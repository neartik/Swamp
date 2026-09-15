use crate::ids::NodeId;
use crate::model::core::{AccountId, LimitScope, Provider, RateLimitSnapshot, Usage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// A name plus an executable plus an optional non-secret env overlay.
#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    pub provider: Provider,
    pub exec: String,
    /// e.g. CLAUDE_CONFIG_DIR; never a credential.
    pub env: BTreeMap<String, String>,
    pub weight: u32,
    /// Unset is unlimited: the only real ceiling is the one a subscription imposes, and the
    /// user is the one who knows it.
    pub max_concurrency: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountState {
    pub inflight: usize,
    pub health: Health,
    #[serde(with = "time::serde::rfc3339::option")]
    pub cooldown_until: Option<OffsetDateTime>,
    pub consecutive_infra_failures: u32,
    pub quota: Option<RateLimitSnapshot>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used: Option<OffsetDateTime>,
    pub lifetime_nodes: u64,
    pub lifetime_cost_usd: f64,
    /// When this process last changed the entry. The merge into the shared file is
    /// last-writer-wins per account, so a cooldown learned elsewhere is never clobbered.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,

    /// Every token this account ever spent, across runs and repos.
    #[serde(default)]
    pub lifetime_tokens: Usage,
    /// Tokens spent inside the window `window_key` names. Zeroed when the window rolls.
    #[serde(default)]
    pub window_tokens: Usage,
    /// When the current counting window began.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub window_started_at: Option<OffsetDateTime>,
    /// The window `window_tokens` is keyed to. A change means "roll and zero".
    #[serde(default)]
    pub window_key: Option<WindowKey>,
    /// Every bucket the provider reported, keyed by limit id. Display only.
    #[serde(default)]
    pub quota_buckets: BTreeMap<String, RateLimitSnapshot>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub quota_observed_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub quota_source: Option<QuotaSource>,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct WindowKey {
    pub scope: LimitScope,
    #[serde(with = "time::serde::rfc3339")]
    pub resets_at: OffsetDateTime,
}

impl WindowKey {
    /// The window a snapshot keys its counters to: the longest one it carries. Keying on the
    /// tightest window instead would re-key whenever another scope overtook it, zeroing a
    /// counter no window had actually rolled. A snapshot with no reset time keys nothing.
    pub fn of(snap: &RateLimitSnapshot) -> Option<Self> {
        let w = snap
            .named()
            .filter(|w| w.resets_at.is_some())
            .max_by_key(|w| w.minutes())
            .or_else(|| snap.tightest())?;
        Some(Self {
            scope: w.scope,
            resets_at: w.resets_at.or(snap.resets_at)?,
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// claude `rate_limit_event`, in the worker's own stream.
    Telemetry,
    /// codex rollout `event_msg` / `token_count` / `rate_limits`.
    Rollout,
    /// codex `app-server` -> `account/rateLimits/read`.
    AppServer,
    /// Swamp's own token counters against a configured window size. Never a measurement.
    Estimated,
}

impl QuotaSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Telemetry => "telemetry",
            Self::Rollout => "rollout",
            Self::AppServer => "app-server",
            Self::Estimated => "estimated",
        }
    }
}

impl AccountState {
    /// The ONLY place `window_tokens` resets. `resets_at` moving forward is what a rolled
    /// window looks like on the wire, for both providers.
    pub fn roll_window(&mut self, key: Option<WindowKey>, now: OffsetDateTime) -> bool {
        let Some(key) = key else { return false };
        if self.window_key.as_ref() == Some(&key) {
            return false;
        }
        self.window_tokens = Usage::default();
        self.window_started_at = Some(now);
        self.window_key = Some(key);
        true
    }

    /// Everything one snapshot changes: the per-scope merge (a later event carrying fewer
    /// windows must not erase one), the provenance, and the window roll. Returns true when
    /// the window rolled, which is what `AccountUsage { rolled }` reports.
    pub fn apply_quota(
        &mut self,
        snap: RateLimitSnapshot,
        source: QuotaSource,
        now: OffsetDateTime,
    ) -> bool {
        // Two buckets are two allowances: merging across them lets one bucket inherit the
        // other's windows and park a model family that has no limit at all.
        let merged = match &self.quota {
            Some(prev) if prev.limit_id == snap.limit_id => snap.merged_over(prev, now),
            _ => snap,
        };
        let rolled = self.roll_window(WindowKey::of(&merged), now);
        if let Some(id) = merged.limit_id.clone() {
            self.quota_buckets.insert(id, merged.clone());
        }
        self.quota = Some(merged);
        self.quota_observed_at = Some(now);
        self.quota_source = Some(source);
        rolled
    }

    /// Fold a node's committed tokens into both counters.
    pub fn credit_tokens(&mut self, u: &Usage) {
        self.window_tokens.absorb(u);
        self.lifetime_tokens.absorb(u);
    }
}

/// Per-node cumulative totals, waiting to be committed. `observe_usage` is fed a running
/// total and not a delta, because both CLIs report totals and a delta interface would
/// double count; the ledger turns those totals back into one fold per node.
#[derive(Debug, Default)]
pub struct UsageLedger {
    nodes: BTreeMap<(AccountId, NodeId), Entry>,
}

#[derive(Debug, Default)]
struct Entry {
    total: Usage,
    committed: bool,
}

impl UsageLedger {
    /// Idempotent, and monotone per field: a replayed or stale total changes nothing.
    pub fn observe(&mut self, id: &AccountId, node: NodeId, cumulative: Usage) {
        let entry = self.nodes.entry((id.clone(), node)).or_default();
        if entry.committed {
            return;
        }
        entry.total.take_max(&cumulative);
    }

    /// What this node still owes the committed counters. Calling it twice owes nothing.
    pub fn commit(&mut self, id: &AccountId, node: NodeId, final_total: Usage) -> Usage {
        let entry = self.nodes.entry((id.clone(), node)).or_default();
        if entry.committed {
            return Usage::default();
        }
        entry.total.take_max(&final_total);
        entry.committed = true;
        entry.total
    }

    /// Tokens spent by nodes that have not finished yet. A displayed total is
    /// `committed + inflight`.
    pub fn inflight(&self, id: &AccountId) -> Usage {
        let mut out = Usage::default();
        for ((owner, _), e) in &self.nodes {
            if owner == id && !e.committed {
                out.absorb(&e.total);
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    #[default]
    Healthy,
    /// Utilization past quota_warn_at: deprioritize, still usable.
    Degraded,
    /// cooldown_until in the future.
    Cooling,
    /// Out of rotation until a human fixes it.
    AuthBroken,
    /// Config, or `swamp accounts disable`.
    Disabled,
}
