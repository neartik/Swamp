pub mod load;
pub mod resolve;
pub mod schema;
pub mod validate;

pub use schema::{
    AccountCfg, BrainCfg, CooldownCfg, DispatchCfg, FailureCfg, JournalCfg, Limits, PricingCfg,
    ProviderCfg, Schema, TierCfg, UiCfg, WeightsCfg, WorkerCfg, WorkspaceCfg,
};

use crate::error::SwampError;
use crate::model::core::{AccountId, Cost, CostBasis, Provider, Tier, Usage};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use std::time::Duration;

/// Used only if every layer somehow lost `limits.worker_timeout`.
const FALLBACK_NODE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

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
    /// Non-fatal adjustments made at load time.
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
        let mut layers = vec![load::default_layer()];
        let mut sources = Vec::new();

        let file_layer = |path: Utf8PathBuf,
                          layers: &mut Vec<load::Layer>,
                          sources: &mut Vec<Utf8PathBuf>|
         -> Result<(), SwampError> {
            layers.push(load::read_layer(&path)?);
            sources.push(path);
            Ok(())
        };

        if let Some(user) = load::user_config_path()
            && user.is_file()
        {
            file_layer(user, &mut layers, &mut sources)?;
        }
        let repo_cfg = load::repo_config_path(repo);
        if repo_cfg.is_file() {
            file_layer(repo_cfg, &mut layers, &mut sources)?;
        }
        layers.push(load::env_layer()?);
        if let Some(explicit) = explicit {
            file_layer(explicit.to_owned(), &mut layers, &mut sources)?;
        }

        let mut schema = load::merge(layers);
        if let Some(profile) = profile {
            load::apply_profile(&mut schema, profile)?;
        }

        let mut cfg = resolve::from_schema(schema);
        cfg.sources = sources;
        validate::validate(&mut cfg)?;
        Ok(cfg)
    }

    /// The merged config as TOML. `swamp config show --effective` annotates it with origins.
    pub fn effective_toml(&self) -> String {
        toml::to_string_pretty(&self.to_schema()).unwrap_or_default()
    }

    pub fn sha256(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.effective_toml().as_bytes());
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn model_for(
        &self,
        p: Provider,
        t: Tier,
        account: Option<&AccountId>,
    ) -> Result<String, SwampError> {
        if let Some(a) = account.and_then(|a| self.account(a))
            && let Some(m) = a.models.get(&t)
        {
            return Ok(m.clone());
        }
        self.providers
            .get(&p)
            .and_then(|pc| pc.models.get(&t))
            .cloned()
            .ok_or(SwampError::TierUnmapped {
                provider: p,
                tier: t,
            })
    }

    pub fn tier_extra(&self, p: Provider, t: Tier) -> BTreeMap<String, String> {
        self.providers
            .get(&p)
            .and_then(|pc| pc.tier_extra.get(&t))
            .cloned()
            .unwrap_or_default()
    }

    pub fn account(&self, id: &AccountId) -> Option<&AccountCfg> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    pub fn accounts_for(&self, p: Provider) -> Vec<&AccountCfg> {
        self.accounts.iter().filter(|a| a.provider == p).collect()
    }

    /// An explicit `tiers.<t>.provider_order` is taken verbatim: a tier that lists one provider
    /// is opting out of the others.
    pub fn provider_order(&self, t: Tier) -> Vec<Provider> {
        if let Some(order) = self.tiers.get(&t).map(|tc| &tc.provider_order)
            && !order.is_empty()
        {
            return order.clone();
        }
        let mut order: Vec<Provider> = self.dispatch.default_provider.into_iter().collect();
        for p in self.providers.keys() {
            if !order.contains(p) {
                order.push(*p);
            }
        }
        order
    }

    pub fn failure_patterns(&self, p: Provider) -> Result<FailurePatterns, SwampError> {
        let empty = FailureCfg::default();
        let cfg = self.failure.get(&p).unwrap_or(&empty);
        let compile = |field: &str, pats: &[String]| -> Result<regex::RegexSet, SwampError> {
            regex::RegexSet::new(pats)
                .map_err(|e| SwampError::ConfigInvalid(format!("  failure.{p}.{field}: {e}")))
        };
        Ok(FailurePatterns {
            rate_limit: compile("rate_limit", &cfg.rate_limit)?,
            auth: compile("auth", &cfg.auth)?,
            overloaded: compile("overloaded", &cfg.overloaded)?,
            sources: BTreeMap::from([
                ("rate_limit".to_owned(), cfg.rate_limit.clone()),
                ("auth".to_owned(), cfg.auth.clone()),
                ("overloaded".to_owned(), cfg.overloaded.clone()),
            ]),
        })
    }

    pub fn node_timeout(&self, t: Tier) -> Duration {
        self.tiers
            .get(&t)
            .and_then(|tc| tc.timeout)
            .or(self.limits.worker_timeout)
            .unwrap_or(FALLBACK_NODE_TIMEOUT)
    }

    /// How old a quota reading may be before `/usage` calls it stale and dispatch reprobes.
    pub fn quota_max_age(&self) -> Duration {
        self.dispatch
            .quota_max_age
            .unwrap_or(crate::dispatch::policy::DEFAULT_QUOTA_MAX_AGE)
    }

    /// `providers.<p>.quota_source`: "auto | rollout | app-server | none". Every out-of-band
    /// probe honours it, not just the one dispatch runs.
    pub fn quota_source(&self, p: crate::model::core::Provider) -> &str {
        self.providers
            .get(&p)
            .and_then(|c| c.quota_source.as_deref())
            .unwrap_or("auto")
    }

    /// Whether `account/rateLimits/read` may be spawned at all for this provider.
    pub fn probes_app_server(&self, p: crate::model::core::Provider) -> bool {
        matches!(self.quota_source(p), "auto" | "app-server")
    }

    /// basis = Estimated. `None` when no `[pricing]` row exists, never `Some(0.0)`.
    pub fn estimate_cost(&self, model: &str, u: &Usage) -> Option<Cost> {
        let row = self.pricing.get(model)?;
        let per_million = u.input_tokens as f64 * row.input
            + u.cached_input_tokens as f64 * row.cached_input
            + u.output_tokens as f64 * row.output;
        Some(Cost {
            usd: per_million / 1_000_000.0,
            basis: CostBasis::Estimated,
        })
    }

    fn to_schema(&self) -> Schema {
        Schema {
            version: self.version,
            limits: self.limits.clone(),
            brain: self.brain.clone(),
            dispatch: self.dispatch.clone(),
            cooldown: self.cooldown.clone(),
            workspace: self.workspace.clone(),
            journal: self.journal.clone(),
            providers: self.providers.clone(),
            accounts: self.accounts.clone(),
            tiers: self.tiers.clone(),
            failure: self.failure.clone(),
            pricing: self.pricing.clone(),
            ui: self.ui.clone(),
            profiles: self.profiles.clone(),
        }
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
        matching_line(&self.rate_limit, s)
    }
    pub fn auth_match<'a>(&self, s: &'a str) -> Option<&'a str> {
        matching_line(&self.auth, s)
    }
    pub fn overloaded_match<'a>(&self, s: &'a str) -> Option<&'a str> {
        matching_line(&self.overloaded, s)
    }
}

/// The evidence a classification is built on: the narrowest slice of `s` that still matches.
fn matching_line<'a>(set: &regex::RegexSet, s: &'a str) -> Option<&'a str> {
    if set.is_empty() || !set.is_match(s) {
        return None;
    }
    Some(s.lines().find(|l| set.is_match(l)).unwrap_or(s))
}
