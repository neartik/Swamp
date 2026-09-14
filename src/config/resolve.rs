use crate::config::Config;
use crate::config::schema::AccountCfg;
use crate::error::SwampError;
use camino::Utf8PathBuf;
use std::collections::BTreeMap;

/// `~` and `$VAR` expansion for config paths and account env overlays.
pub fn expand_path(s: &str) -> Utf8PathBuf {
    Utf8PathBuf::from(shellexpand::full(s).map_or_else(|_| s.to_owned(), |v| v.into_owned()))
}

pub fn expand_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .map(|(k, v)| {
            (
                k.clone(),
                shellexpand::full(v).map_or_else(|_| v.clone(), |e| e.into_owned()),
            )
        })
        .collect()
}

/// Resolves `exec` through PATH and records the absolute path.
pub fn resolve_exec(a: &AccountCfg) -> Result<Utf8PathBuf, SwampError> {
    let found = which::which(&a.exec).map_err(|_| SwampError::ExecNotFound {
        id: a.id.0.clone(),
        exec: a.exec.clone(),
    })?;
    Utf8PathBuf::from_path_buf(found).map_err(|p| SwampError::ExecNotFound {
        id: a.id.0.clone(),
        exec: p.to_string_lossy().into_owned(),
    })
}

pub fn from_schema(s: crate::config::schema::Schema) -> Config {
    Config {
        version: s.version,
        limits: s.limits,
        brain: s.brain,
        dispatch: s.dispatch,
        cooldown: s.cooldown,
        workspace: s.workspace,
        journal: s.journal,
        providers: s.providers,
        accounts: s.accounts,
        tiers: s.tiers,
        failure: s.failure,
        pricing: s.pricing,
        ui: s.ui,
        profiles: s.profiles,
        sources: Vec::new(),
        warnings: Vec::new(),
    }
}
