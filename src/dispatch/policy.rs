use crate::config::Config;
use crate::dispatch::account::{Account, AccountState, Health};
use crate::model::core::LimitReached;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub const DEFAULT_QUOTA_WARN: f64 = 0.90;
pub const DEFAULT_QUOTA_STOP: f64 = 0.98;
pub const DEFAULT_NEAR_EXHAUSTION_PENALTY: f64 = 2.0;
pub const DEFAULT_QUOTA_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SelectionPolicy {
    RoundRobin,
    LeastLoaded,
    /// Balances measured utilization, live load and token share, so a pool with no provider
    /// telemetry at all still rotates.
    #[default]
    QuotaAware,
}

/// `[dispatch.weights]`. Every term is in `[0, 1]`, so these are directly comparable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Weights {
    pub util: f64,
    pub load: f64,
    pub share: f64,
    pub weight: f64,
    pub idle: f64,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            util: 0.50,
            load: 0.30,
            share: 0.15,
            weight: 0.05,
            idle: 0.02,
        }
    }
}

/// Everything selection needs out of the config, resolved once per pool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scoring {
    pub weights: Weights,
    pub penalty: f64,
    pub warn_at: f64,
    pub stop_at: f64,
}

impl Default for Scoring {
    fn default() -> Self {
        Self {
            weights: Weights::default(),
            penalty: DEFAULT_NEAR_EXHAUSTION_PENALTY,
            warn_at: DEFAULT_QUOTA_WARN,
            stop_at: DEFAULT_QUOTA_STOP,
        }
    }
}

impl Scoring {
    pub fn from_config(cfg: &Config) -> Self {
        let d = Scoring::default();
        let w = cfg.dispatch.weights.unwrap_or_default();
        Self {
            weights: Weights {
                util: w.util.unwrap_or(d.weights.util),
                load: w.load.unwrap_or(d.weights.load),
                share: w.share.unwrap_or(d.weights.share),
                weight: w.weight.unwrap_or(d.weights.weight),
                idle: w.idle.unwrap_or(d.weights.idle),
            },
            penalty: cfg.dispatch.near_exhaustion_penalty.unwrap_or(d.penalty),
            warn_at: cfg.cooldown.quota_warn_at.unwrap_or(d.warn_at),
            stop_at: cfg.cooldown.quota_stop_at.unwrap_or(d.stop_at),
        }
    }

    /// Raw utilization below the knee, and a steep climb above it: at `stop_at` the penalty
    /// adds a full `penalty` to a term no other term can outweigh.
    pub fn penalised(&self, util: f64) -> f64 {
        if util < self.warn_at {
            return util;
        }
        let span = (self.stop_at - self.warn_at).max(f64::EPSILON);
        util + self.penalty * (util - self.warn_at) / span
    }
}

fn saturation(a: &Account, s: &AccountState) -> f64 {
    match a.max_concurrency {
        Some(c) if c > 0 => s.inflight as f64 / c as f64,
        _ => 0.0,
    }
}

/// Rises with every live node and never reaches 1, so an uncapped account cannot soak up a
/// whole batch while an idle capped one still starts level with it.
fn crowding(s: &AccountState) -> f64 {
    s.inflight as f64 / (s.inflight as f64 + 1.0)
}

pub fn load(a: &Account, s: &AccountState) -> f64 {
    saturation(a, s).max(crowding(s))
}

/// Lower is better. None means ineligible right now; every `None` here is a hard gate.
pub fn score(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    pool_window: u64,
    cfg: &Scoring,
    now: OffsetDateTime,
) -> Option<f64> {
    if matches!(s.health, Health::Disabled | Health::AuthBroken) {
        return None;
    }
    // Two independent gates, never one nested inside the other: `swamp accounts enable`
    // rewrites health without touching the timer, and a live cooldown still means wait.
    if s.cooldown_until.is_some_and(|t| t > now) {
        return None;
    }
    // The provider's own authoritative gate: a client must not infer recovery from
    // percentages or reset times, so this outranks both.
    if s.quota.as_ref().and_then(|q| q.ordinary_usage_allowed) == Some(false) {
        return None;
    }
    // Depleted credits or a spend control is not a timer: a human has to act.
    if matches!(
        s.quota.as_ref().and_then(|q| q.reached),
        Some(LimitReached::CreditsDepleted | LimitReached::SpendControl)
    ) {
        return None;
    }
    if let Some(c) = a.max_concurrency
        && s.inflight >= c
    {
        return None;
    }
    // Proactive, before any provider error, and on MEASURED windows only: an estimate is a
    // guess and must never park a working subscription.
    // A window whose reset has passed measures an allowance that has already rolled: gating
    // on it strands the account forever, because only a node running on it can refresh it.
    if s.quota
        .as_ref()
        .and_then(|q| q.measured_utilization_at(now))
        .is_some_and(|u| u >= cfg.stop_at)
    {
        return None;
    }

    let util = s
        .quota
        .as_ref()
        .map_or(0.0, |q| q.worst_utilization_at(now));
    let load = load(a, s);
    let share = s.window_tokens.billable() as f64 / pool_window.max(1) as f64;
    let idle = s.last_used.map_or(1.0, |t| {
        ((now - t).as_seconds_f64() / 3600.0).clamp(0.0, 1.0)
    });
    let w = &cfg.weights;
    Some(match policy {
        SelectionPolicy::RoundRobin => -idle,
        SelectionPolicy::LeastLoaded => load - 0.01 * a.weight as f64,
        SelectionPolicy::QuotaAware => {
            w.util * cfg.penalised(util) + w.load * load + w.share * share
                - w.weight * (a.weight as f64 - 1.0)
                - w.idle * idle
        }
    })
}

/// Two scores this close are a tie: they are small rationals, so this only absorbs
/// floating-point noise.
const TIE: f64 = 1e-9;

/// A total order over eligible accounts. A tied score is settled by the tokens the account
/// has spent over its life and then by a rotating cursor, so two idle accounts alternate
/// instead of the first name always winning. Cost left this chain: it reads 0.0 for every
/// OpenAI account, which silently ranked them all equal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rank {
    pub score: f64,
    pub lifetime_billable: u64,
    pub rotation: usize,
}

impl Rank {
    /// Lower is better, like `score`.
    pub fn compare(&self, other: &Rank) -> std::cmp::Ordering {
        if (self.score - other.score).abs() > TIE {
            return self.score.total_cmp(&other.score);
        }
        self.lifetime_billable
            .cmp(&other.lifetime_billable)
            .then_with(|| self.rotation.cmp(&other.rotation))
    }
}

/// `score` plus its tie-breakers. `rotation` is the caller's position in a list rotated by a
/// per-pool cursor, which is what makes sequential runs alternate.
pub fn rank(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    pool_window: u64,
    cfg: &Scoring,
    now: OffsetDateTime,
    rotation: usize,
) -> Option<Rank> {
    Some(Rank {
        score: score(policy, a, s, pool_window, cfg, now)?,
        lifetime_billable: s.lifetime_tokens.billable(),
        rotation,
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

    fn account(id: &str, max_concurrency: Option<usize>) -> Account {
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
                ..Default::default()
            }],
            resets_at: None,
            ..Default::default()
        }
    }

    fn scoring() -> Scoring {
        Scoring::default()
    }

    #[test]
    fn a_saturated_or_parked_account_is_ineligible() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", Some(2));
        let cfg = scoring();
        let mut s = AccountState {
            inflight: 2,
            ..Default::default()
        };
        assert_eq!(
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
            None
        );

        s.inflight = 0;
        for health in [Health::Disabled, Health::AuthBroken] {
            s.health = health;
            assert_eq!(
                score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
                None
            );
        }

        s.health = Health::Cooling;
        s.cooldown_until = Some(now + Duration::from_secs(60));
        assert_eq!(
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
            None
        );

        // An expired cooldown is usable again without anyone clearing the flag.
        s.cooldown_until = Some(now - Duration::from_secs(60));
        assert!(score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now).is_some());
    }

    /// An account with no ceiling is bounded by headroom, never by a number.
    #[test]
    fn an_uncapped_account_is_never_saturated() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", None);
        let s = AccountState {
            inflight: 99,
            ..Default::default()
        };
        let got =
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &scoring(), now).expect("eligible");
        assert!(got < 1.0, "crowding must stay under 1: {got}");
    }

    #[test]
    fn quota_stop_makes_an_account_ineligible_before_any_error() {
        let now = OffsetDateTime::now_utc();
        let a = account("main", Some(2));
        let s = AccountState {
            quota: Some(util(0.99)),
            ..Default::default()
        };
        let mut cfg = scoring();
        assert_eq!(
            score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now),
            None
        );
        cfg.stop_at = 1.0;
        assert!(score(SelectionPolicy::LeastLoaded, &a, &s, 0, &cfg, now).is_some());
    }

    #[test]
    fn quota_aware_degrades_to_least_loaded_without_telemetry() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("a", Some(2)), account("b", Some(2)));
        let cfg = scoring();
        let idle = AccountState::default();
        let busy = AccountState {
            inflight: 1,
            ..Default::default()
        };
        let sa = score(SelectionPolicy::QuotaAware, &a, &idle, 0, &cfg, now).unwrap();
        let sb = score(SelectionPolicy::QuotaAware, &b, &busy, 0, &cfg, now).unwrap();
        assert!(sa < sb, "{sa} < {sb}");
    }

    /// Two idle accounts scored identically, so selection fell back to the map order and
    /// every sequential run went to the same one.
    #[test]
    fn a_tie_goes_to_the_account_with_fewer_lifetime_tokens() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let cfg = scoring();
        let used = AccountState {
            lifetime_tokens: crate::model::core::Usage {
                input_tokens: 5_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let fresh = AccountState::default();
        let ra = rank(SelectionPolicy::LeastLoaded, &a, &used, 0, &cfg, now, 0).unwrap();
        let rb = rank(SelectionPolicy::LeastLoaded, &b, &fresh, 0, &cfg, now, 1).unwrap();
        assert_eq!(ra.compare(&rb), std::cmp::Ordering::Greater);
        assert_eq!(rb.compare(&ra), std::cmp::Ordering::Less);
    }

    /// Equal on every counter: the rotating cursor decides, and it is deterministic.
    #[test]
    fn a_full_tie_falls_back_to_the_rotating_cursor() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let cfg = scoring();
        let idle = AccountState::default();
        let first = rank(SelectionPolicy::LeastLoaded, &a, &idle, 0, &cfg, now, 1).unwrap();
        let second = rank(SelectionPolicy::LeastLoaded, &b, &idle, 0, &cfg, now, 0).unwrap();
        assert_eq!(first.compare(&second), std::cmp::Ordering::Greater);
    }

    /// Load still outranks lifetime history: a busy account is never preferred.
    #[test]
    fn lifetime_history_never_outweighs_current_load() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", Some(2)), account("main", Some(2)));
        let cfg = scoring();
        let idle_but_used = AccountState {
            lifetime_tokens: crate::model::core::Usage {
                input_tokens: 99_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let busy_and_fresh = AccountState {
            inflight: 1,
            ..Default::default()
        };
        let ra = rank(
            SelectionPolicy::LeastLoaded,
            &a,
            &idle_but_used,
            0,
            &cfg,
            now,
            0,
        )
        .unwrap();
        let rb = rank(
            SelectionPolicy::LeastLoaded,
            &b,
            &busy_and_fresh,
            0,
            &cfg,
            now,
            1,
        )
        .unwrap();
        assert_eq!(ra.compare(&rb), std::cmp::Ordering::Less);
    }

    #[test]
    fn the_penalty_knee_starts_at_the_warning_threshold() {
        let cfg = scoring();
        assert!((cfg.penalised(0.50) - 0.50).abs() < 1e-9);
        assert!((cfg.penalised(0.90) - 0.90).abs() < 1e-9);
        assert!((cfg.penalised(0.93) - 1.68).abs() < 1e-9);
        assert!((cfg.penalised(0.98) - 2.98).abs() < 1e-9);
    }
}
