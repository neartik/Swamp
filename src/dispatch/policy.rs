use crate::dispatch::account::{Account, AccountState, Health};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionPolicy {
    RoundRobin,
    /// v1 default: the only telemetry that feeds QuotaAware exists for one provider.
    #[default]
    LeastLoaded,
    QuotaAware,
}

/// Lower is better. None means ineligible.
pub fn score(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    quota_stop_at: f64,
    now: OffsetDateTime,
) -> Option<f64> {
    match s.health {
        Health::Disabled | Health::AuthBroken => return None,
        Health::Cooling if s.cooldown_until.is_some_and(|t| t > now) => return None,
        _ => {}
    }
    if s.inflight >= a.max_concurrency {
        return None;
    }
    let util = s.quota.as_ref().map_or(0.0, |q| q.worst_utilization());
    // Proactive, before any error: we stop using an account before the provider stops us.
    if util >= quota_stop_at {
        return None;
    }
    let load = s.inflight as f64 / a.max_concurrency.max(1) as f64;
    let idle = s.last_used.map_or(f64::MAX, |t| (now - t).as_seconds_f64());
    Some(match policy {
        SelectionPolicy::RoundRobin => -idle,
        SelectionPolicy::LeastLoaded => load - 0.01 * a.weight as f64,
        SelectionPolicy::QuotaAware => {
            0.65 * util + 0.35 * load - 0.05 * a.weight as f64 - 0.02 * (idle / 3600.0).min(1.0)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{
        AccountId, LimitScope, LimitStatus, LimitWindow, Provider, RateLimitSnapshot,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn account(id: &str, max_concurrency: usize) -> Account {
        Account {
            id: AccountId(id.into()),
            provider: Provider::Anthropic,
            exec: format!("claude-{id}"),
            env: BTreeMap::new(),
            weight: 1,
            max_concurrency,
        }
    }

    fn util(v: f64) -> RateLimitSnapshot {
        RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: v,
                resets_at: None,
            }],
            resets_at: None,
        }
    }

    #[test]
    fn a_saturated_or_parked_account_is_ineligible() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", 2);
        let mut s = AccountState {
            inflight: 2,
            ..Default::default()
        };
        assert_eq!(score(SelectionPolicy::LeastLoaded, &a, &s, 0.98, now), None);

        s.inflight = 0;
        for health in [Health::Disabled, Health::AuthBroken] {
            s.health = health;
            assert_eq!(score(SelectionPolicy::LeastLoaded, &a, &s, 0.98, now), None);
        }

        s.health = Health::Cooling;
        s.cooldown_until = Some(now + Duration::from_secs(60));
        assert_eq!(score(SelectionPolicy::LeastLoaded, &a, &s, 0.98, now), None);

        // An expired cooldown is usable again without anyone clearing the flag.
        s.cooldown_until = Some(now - Duration::from_secs(60));
        assert!(score(SelectionPolicy::LeastLoaded, &a, &s, 0.98, now).is_some());
    }

    #[test]
    fn quota_stop_makes_an_account_ineligible_before_any_error() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", 2);
        let s = AccountState {
            quota: Some(util(0.99)),
            ..Default::default()
        };
        assert_eq!(score(SelectionPolicy::LeastLoaded, &a, &s, 0.98, now), None);
        assert!(score(SelectionPolicy::LeastLoaded, &a, &s, 1.0, now).is_some());
    }

    #[test]
    fn quota_aware_degrades_to_least_loaded_without_telemetry() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("a", 2), account("b", 2));
        let idle = AccountState::default();
        let busy = AccountState {
            inflight: 1,
            ..Default::default()
        };
        let sa = score(SelectionPolicy::QuotaAware, &a, &idle, 0.98, now).unwrap();
        let sb = score(SelectionPolicy::QuotaAware, &b, &busy, 0.98, now).unwrap();
        assert!(sa < sb, "{sa} < {sb}");
    }
}
