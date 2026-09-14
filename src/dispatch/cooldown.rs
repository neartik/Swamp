use crate::config::CooldownCfg;
use crate::model::failure::Failure;
use std::time::Duration;
use time::OffsetDateTime;

const FALLBACK_MIN: Duration = Duration::from_secs(60);
const FALLBACK_MAX: Duration = Duration::from_secs(6 * 60 * 60);
const FALLBACK_DEFAULT: Duration = Duration::from_secs(15 * 60);
const FALLBACK_BREAKER: u32 = 3;
/// Transient upstream capacity: long enough to let the spike pass, short enough to keep working.
const OVERLOADED: Duration = Duration::from_secs(30);
/// 2^16 * 15m is already far past cooldown.max; anything more only risks overflow.
const MAX_SHIFT: u32 = 16;

pub fn cooldown_for(
    f: &Failure,
    consecutive: u32,
    cfg: &CooldownCfg,
    now: OffsetDateTime,
) -> Option<Duration> {
    let min = cfg.min.unwrap_or(FALLBACK_MIN);
    let max = cfg.max.unwrap_or(FALLBACK_MAX).max(min);
    let base = cfg.default.unwrap_or(FALLBACK_DEFAULT);
    let breaker = cfg.breaker_threshold.unwrap_or(FALLBACK_BREAKER).max(1);

    match f {
        Failure::RateLimited { resets_at, .. } => {
            let d = match resets_at {
                // The provider told us when it reopens; trust it over any backoff curve.
                Some(at) if *at > now => Duration::try_from(*at - now).unwrap_or(base),
                // Clock skew and past timestamps fall back to the configured default.
                _ => backoff(base, consecutive),
            };
            Some(d.clamp(min, max))
        }
        // Re-auth is a human action: parking the account is what matters, not a timer.
        Failure::AuthExpired { .. } => None,
        Failure::Overloaded { .. } => Some(OVERLOADED),
        Failure::Crashed { .. } | Failure::Truncated { .. } if consecutive >= breaker => {
            Some(base.clamp(min, max))
        }
        _ => None,
    }
}

fn backoff(base: Duration, consecutive: u32) -> Duration {
    let shift = consecutive.saturating_sub(1).min(MAX_SHIFT);
    base.saturating_mul(1u32 << shift)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::LimitScope;
    use crate::model::failure::Detector;

    fn cfg() -> CooldownCfg {
        CooldownCfg {
            min: Some(Duration::from_secs(60)),
            max: Some(Duration::from_secs(6 * 60 * 60)),
            default: Some(Duration::from_secs(15 * 60)),
            breaker_threshold: Some(3),
            quota_warn_at: Some(0.90),
            quota_stop_at: Some(0.98),
        }
    }

    fn rate_limited(resets_at: Option<OffsetDateTime>) -> Failure {
        Failure::RateLimited {
            resets_at,
            scope: LimitScope::FiveHour,
            detected_by: Detector::Telemetry,
            evidence: "usage limit reached".into(),
        }
    }

    #[test]
    fn provider_reset_time_wins_over_the_backoff_curve() {
        let now = OffsetDateTime::now_utc();
        let d = cooldown_for(
            &rate_limited(Some(now + Duration::from_secs(1200))),
            1,
            &cfg(),
            now,
        )
        .unwrap();
        assert!(
            d.abs_diff(Duration::from_secs(1200)) < Duration::from_secs(2),
            "{d:?}"
        );
    }

    #[test]
    fn a_reset_time_outside_the_window_is_clamped() {
        let now = OffsetDateTime::now_utc();
        let far = cooldown_for(
            &rate_limited(Some(now + Duration::from_secs(10 * 3600))),
            1,
            &cfg(),
            now,
        );
        assert_eq!(far, Some(Duration::from_secs(6 * 3600)));

        let past = cooldown_for(
            &rate_limited(Some(now - Duration::from_secs(3600))),
            1,
            &cfg(),
            now,
        );
        assert_eq!(past, Some(Duration::from_secs(15 * 60)));
    }

    #[test]
    fn repeated_rate_limits_back_off_exponentially_then_clamp() {
        let now = OffsetDateTime::now_utc();
        let at = |n| cooldown_for(&rate_limited(None), n, &cfg(), now).unwrap();
        assert_eq!(at(1), Duration::from_secs(15 * 60));
        assert_eq!(at(2), Duration::from_secs(30 * 60));
        assert_eq!(at(3), Duration::from_secs(60 * 60));
        assert_eq!(at(9), Duration::from_secs(6 * 3600));
        assert_eq!(at(u32::MAX), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn overload_is_short_and_carries_no_penalty() {
        let now = OffsetDateTime::now_utc();
        let f = Failure::Overloaded {
            detail: "529".into(),
        };
        assert_eq!(cooldown_for(&f, 1, &cfg(), now), Some(OVERLOADED));
        assert_eq!(cooldown_for(&f, 7, &cfg(), now), Some(OVERLOADED));
    }

    #[test]
    fn crashes_only_cool_once_the_breaker_trips() {
        let now = OffsetDateTime::now_utc();
        let f = Failure::Crashed { signal: Some(9) };
        assert_eq!(cooldown_for(&f, 2, &cfg(), now), None);
        assert_eq!(
            cooldown_for(&f, 3, &cfg(), now),
            Some(Duration::from_secs(15 * 60))
        );
    }

    #[test]
    fn task_failures_never_cool_an_account() {
        let now = OffsetDateTime::now_utc();
        for f in [
            Failure::WorkerError {
                subtype: "error_during_execution".into(),
                detail: "tests failed".into(),
            },
            Failure::AuthExpired {
                detail: "expired".into(),
                detected_by: Detector::Pattern,
            },
            Failure::Timeout { after_s: 10 },
            Failure::PermissionDenied { denials: 3 },
            Failure::BudgetExceeded {
                limit_usd: 1.0,
                spent_usd: 2.0,
            },
            Failure::NoCapacity {
                detail: "none".into(),
            },
        ] {
            assert_eq!(cooldown_for(&f, 5, &cfg(), now), None, "{f:?}");
        }
    }
}
