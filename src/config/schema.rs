use crate::model::core::{AccountId, Provider, Tier};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// Raw deserialization target: one layer of a config.toml, before merging.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    /// Skipped when zero: a layer that never mentioned `version` must not clobber the one
    /// that did when the layers are merged through TOML.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub version: u32,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub brain: BrainCfg,
    #[serde(default)]
    pub dispatch: DispatchCfg,
    #[serde(default)]
    pub cooldown: CooldownCfg,
    #[serde(default)]
    pub workspace: WorkspaceCfg,
    #[serde(default)]
    pub journal: JournalCfg,
    #[serde(default)]
    pub providers: BTreeMap<Provider, ProviderCfg>,
    #[serde(default)]
    pub accounts: Vec<AccountCfg>,
    #[serde(default)]
    pub tiers: BTreeMap<Tier, TierCfg>,
    #[serde(default)]
    pub failure: BTreeMap<Provider, FailureCfg>,
    #[serde(default)]
    pub pricing: BTreeMap<String, PricingCfg>,
    #[serde(default)]
    pub ui: UiCfg,
    #[serde(default)]
    pub profiles: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_nodes_per_run: Option<u32>,
    pub max_depth: Option<u32>,
    #[serde(default, with = "humantime_serde")]
    pub worker_timeout: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub brain_turn_timeout: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub grace_period: Option<Duration>,
    pub max_prompt_bytes: Option<usize>,
    pub max_result_bytes: Option<usize>,
    pub unsafe_ack: Option<bool>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrainCfg {
    pub transport: Option<String>,
    pub provider: Option<Provider>,
    pub account: Option<AccountId>,
    pub tier: Option<Tier>,
    pub reserve_brain_slot: Option<bool>,
    pub permission_mode: Option<String>,
    pub include_partial_messages: Option<bool>,
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub deny_tools: Vec<String>,
    pub system_prompt_file: Option<Utf8PathBuf>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchCfg {
    pub policy: Option<crate::dispatch::policy::SelectionPolicy>,
    pub max_attempts: Option<u32>,
    pub cross_provider_failover: Option<bool>,
    pub default_provider: Option<Provider>,
    pub default_tier: Option<Tier>,
    /// Out-of-band probe threshold for `/usage` and dispatch.
    #[serde(default, with = "humantime_serde")]
    pub quota_max_age: Option<Duration>,
    pub near_exhaustion_penalty: Option<f64>,
    #[serde(default)]
    pub weights: Option<WeightsCfg>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WeightsCfg {
    pub util: Option<f64>,
    pub load: Option<f64>,
    pub share: Option<f64>,
    pub weight: Option<f64>,
    pub idle: Option<f64>,
}

/// Ceiling for every `[cooldown]` duration: past a year the timer is a typo, and
/// `OffsetDateTime + Duration` panics once the sum runs off the calendar.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(365 * 24 * 60 * 60);

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CooldownCfg {
    #[serde(default, with = "humantime_serde")]
    pub min: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub max: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub default: Option<Duration>,
    pub breaker_threshold: Option<u32>,
    pub quota_warn_at: Option<f64>,
    pub quota_stop_at: Option<f64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceCfg {
    pub isolation: Option<crate::model::result::IsolationMode>,
    pub root: Option<Utf8PathBuf>,
    pub base: Option<String>,
    pub branch_prefix: Option<String>,
    pub include_dirty: Option<bool>,
    pub require_clean: Option<bool>,
    pub commit_on_success: Option<bool>,
    pub commit_template: Option<String>,
    pub keep_on_failure: Option<bool>,
    #[serde(default)]
    pub link: Vec<String>,
    #[serde(default)]
    pub copy: Vec<String>,
    pub post_create: Option<String>,
    #[serde(default, with = "humantime_serde")]
    pub post_create_timeout: Option<Duration>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalCfg {
    pub fsync: Option<String>,
    pub max_line_bytes: Option<usize>,
    pub keep_runs: Option<u32>,
    #[serde(default, with = "humantime_serde")]
    pub keep_runs_for: Option<Duration>,
    #[serde(default)]
    pub redact: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCfg {
    pub adapter: Option<String>,
    #[serde(default)]
    pub models: BTreeMap<Tier, String>,
    #[serde(default)]
    pub tier_extra: BTreeMap<Tier, BTreeMap<String, String>>,
    #[serde(default)]
    pub worker: WorkerCfg,
    /// openai only: auto | rollout | app-server | none.
    pub quota_source: Option<String>,
    #[serde(default, with = "humantime_serde")]
    pub estimated_window: Option<Duration>,
    /// 0 = no estimate, render "-".
    pub estimated_window_tokens: Option<u64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCfg {
    pub permission_mode: Option<String>,
    pub sandbox: Option<String>,
    /// Rendered as one `--allowed-tools` / `--disallowed-tools` flag, like the brain's.
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub deny_tools: Vec<String>,
    /// Appended after Swamp's built-in worker role prompt.
    pub system_prompt_file: Option<Utf8PathBuf>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub readonly_args: Vec<String>,
}

impl WorkerCfg {
    /// The one place read-only policy turns into argv, so no launch path can forget it.
    pub fn args_for(&self, isolation: crate::model::result::IsolationMode) -> Vec<String> {
        let mut out = self.args.clone();
        if isolation == crate::model::result::IsolationMode::ReadOnly {
            out.extend(self.readonly_args.clone());
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountCfg {
    pub id: AccountId,
    pub provider: Provider,
    pub exec: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub weight: Option<u32>,
    pub max_concurrency: Option<usize>,
    /// Per-account tier override: a plan without the top model maps high to something else.
    #[serde(default)]
    pub models: BTreeMap<Tier, String>,
    /// Which quota bucket this account routes against, e.g. codex's `limit_id`.
    pub limit_id: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TierCfg {
    #[serde(default)]
    pub provider_order: Vec<Provider>,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureCfg {
    #[serde(default)]
    pub rate_limit: Vec<String>,
    #[serde(default)]
    pub auth: Vec<String>,
    #[serde(default)]
    pub overloaded: Vec<String>,
}

/// USD per 1M tokens. Only used to estimate cost where the CLI reports none.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingCfg {
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub input: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub cached_input: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub output: f64,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_zero_f64(n: &f64) -> bool {
    *n == 0.0
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCfg {
    pub refresh_hz: Option<u16>,
    pub tree_width: Option<u16>,
    /// `swamp chat --board`: how many columns the tmux split gets.
    pub board_width: Option<u16>,
    pub show_thinking: Option<bool>,
    pub tail_lines: Option<usize>,
    /// auto | truecolor | ansi256 | plain
    pub chat_theme: Option<String>,
    pub collapse_lines: Option<usize>,
    pub chat_history: Option<usize>,
}
