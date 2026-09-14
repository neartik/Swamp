#![allow(unused_variables)]

pub mod load;
pub mod resolve;
pub mod schema;
pub mod validate;

pub use schema::{
    AccountCfg, BrainCfg, CooldownCfg, DispatchCfg, FailureCfg, JournalCfg, Limits, PricingCfg,
    ProviderCfg, Schema, TierCfg, UiCfg, WorkerCfg, WorkspaceCfg,
};

use crate::error::SwampError;
use crate::model::core::{AccountId, Cost, Provider, Tier, Usage};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use std::time::Duration;

/// The merged, validated configuration. Fields mirror `schema.rs`.
#[derive(Debug, Clone)]
pub struct Config {
    pub version: u32,
    pub limits: Limits,
    pub brain: BrainCfg,
    pub dispatch: DispatchCfg,
    pub cooldown: CooldownCfg,
    pub workspace: WorkspaceCfg,
    pub journal: JournalCfg,
    pub providers: BTreeMap<Provider, ProviderCfg>,
    pub accounts: Vec<AccountCfg>,
    pub tiers: BTreeMap<Tier, TierCfg>,
    pub failure: BTreeMap<Provider, FailureCfg>,
    pub pricing: BTreeMap<String, PricingCfg>,
    pub ui: UiCfg,
    pub profiles: BTreeMap<String, BTreeMap<String, toml::Value>>,
    /// Files that contributed a layer, lowest priority first.
    pub sources: Vec<Utf8PathBuf>,
    /// Non-fatal adjustments made at load time, e.g. shared isolation forcing max_parallel = 1.
    pub warnings: Vec<String>,
}

impl Config {
    /// defaults <- ~/.config/swamp/config.toml <- <repo>/.swamp/config.toml <- SWAMP_* env
    /// <- --config <- flag overrides. Reports ALL validation errors at once.
    pub fn load(
        repo: &Utf8Path,
        explicit: Option<&Utf8Path>,
        profile: Option<&str>,
    ) -> Result<Config, SwampError> {
        todo!("WP1")
    }
    pub fn effective_toml(&self) -> String {
        todo!("WP1")
    }
    pub fn sha256(&self) -> String {
        todo!("WP1")
    }

    pub fn model_for(
        &self,
        p: Provider,
        t: Tier,
        account: Option<&AccountId>,
    ) -> Result<String, SwampError> {
        todo!("WP1")
    }
    pub fn tier_extra(&self, p: Provider, t: Tier) -> BTreeMap<String, String> {
        todo!("WP1")
    }
    pub fn account(&self, id: &AccountId) -> Option<&AccountCfg> {
        todo!("WP1")
    }
    pub fn accounts_for(&self, p: Provider) -> Vec<&AccountCfg> {
        todo!("WP1")
    }
    pub fn provider_order(&self, t: Tier) -> Vec<Provider> {
        todo!("WP1")
    }
    pub fn failure_patterns(&self, p: Provider) -> Result<FailurePatterns, SwampError> {
        todo!("WP1")
    }
    pub fn node_budget_usd(&self, t: Tier) -> Option<f64> {
        todo!("WP1")
    }
    pub fn node_timeout(&self, t: Tier) -> Duration {
        todo!("WP1")
    }
    /// basis = Estimated. `None` when no `[pricing]` row exists, never `Some(0.0)`.
    pub fn estimate_cost(&self, model: &str, u: &Usage) -> Option<Cost> {
        todo!("WP1")
    }
}

/// Compiled once at load, shared by every classifier call.
#[derive(Debug, Clone)]
pub struct FailurePatterns {
    pub rate_limit: regex::RegexSet,
    pub auth: regex::RegexSet,
    pub overloaded: regex::RegexSet,
    /// Source strings, index-aligned with each set, so evidence can name the pattern that fired.
    pub sources: BTreeMap<String, Vec<String>>,
}

impl FailurePatterns {
    pub fn rate_limit_match<'a>(&self, s: &'a str) -> Option<&'a str> {
        todo!("WP1")
    }
    pub fn auth_match<'a>(&self, s: &'a str) -> Option<&'a str> {
        todo!("WP1")
    }
    pub fn overloaded_match<'a>(&self, s: &'a str) -> Option<&'a str> {
        todo!("WP1")
    }
}
