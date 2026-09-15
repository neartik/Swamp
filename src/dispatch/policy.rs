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

/// Two scores this close are a tie: they are small rationals, so this only absorbs
/// floating-point noise.
const TIE: f64 = 1e-9;

/// A total order over eligible accounts. A tied score is settled by lifetime load and then by
/// a rotating cursor, so two idle accounts alternate instead of the first name always winning.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rank {
    pub score: f64,
    pub lifetime_nodes: u64,
    pub lifetime_cost_usd: f64,
    pub rotation: usize,
}

impl Rank {
    /// Lower is better, like `score`.
    pub fn compare(&self, other: &Rank) -> std::cmp::Ordering {
        if (self.score - other.score).abs() > TIE {
            return self.score.total_cmp(&other.score);
        }
        self.lifetime_nodes
            .cmp(&other.lifetime_nodes)
            .then_with(|| self.lifetime_cost_usd.total_cmp(&other.lifetime_cost_usd))
            .then_with(|| self.rotation.cmp(&other.rotation))
    }
}

/// `score` plus its tie-breakers. `rotation` is the caller's position in a list rotated by a
/// per-pool cursor, which is what makes sequential runs alternate.
pub fn rank(
    policy: SelectionPolicy,
    a: &Account,
    s: &AccountState,
    quota_stop_at: f64,
    now: OffsetDateTime,
    rotation: usize,
) -> Option<Rank> {
    Some(Rank {
        score: score(policy, a, s, quota_stop_at, now)?,
        lifetime_nodes: s.lifetime_nodes,
        lifetime_cost_usd: s.lifetime_cost_usd,
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
                ..Default::default()
            }],
            resets_at: None,
            ..Default::default()
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

    /// Two idle accounts scored identically, so selection fell back to the map order and
    /// every sequential run went to the same one.
    #[test]
    fn a_tie_goes_to_the_account_with_fewer_lifetime_nodes() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", 2), account("main", 2));
        let used = AccountState {
            lifetime_nodes: 5,
            ..Default::default()
        };
        let fresh = AccountState::default();
        let ra = rank(SelectionPolicy::LeastLoaded, &a, &used, 0.98, now, 0).unwrap();
        let rb = rank(SelectionPolicy::LeastLoaded, &b, &fresh, 0.98, now, 1).unwrap();
        assert_eq!(ra.compare(&rb), std::cmp::Ordering::Greater);
        assert_eq!(rb.compare(&ra), std::cmp::Ordering::Less);
    }

    /// Equal on every counter: the rotating cursor decides, and it is deterministic.
    #[test]
    fn a_full_tie_falls_back_to_the_rotating_cursor() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", 2), account("main", 2));
        let idle = AccountState::default();
        let first = rank(SelectionPolicy::LeastLoaded, &a, &idle, 0.98, now, 1).unwrap();
        let second = rank(SelectionPolicy::LeastLoaded, &b, &idle, 0.98, now, 0).unwrap();
        assert_eq!(first.compare(&second), std::cmp::Ordering::Greater);
    }

    /// Load still outranks lifetime history: a busy account is never preferred.
    #[test]
    fn lifetime_history_never_outweighs_current_load() {
        let now = OffsetDateTime::now_utc();
        let (a, b) = (account("alt", 2), account("main", 2));
        let idle_but_used = AccountState {
            lifetime_nodes: 99,
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
            0.98,
            now,
            0,
        )
        .unwrap();
        let rb = rank(
            SelectionPolicy::LeastLoaded,
            &b,
            &busy_and_fresh,
            0.98,
            now,
            1,
        )
        .unwrap();
        assert_eq!(ra.compare(&rb), std::cmp::Ordering::Less);
    }
}
