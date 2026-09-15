use crate::model::core::{AccountId, Provider, RateLimitSnapshot};
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
    pub max_concurrency: usize,
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
