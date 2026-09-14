#![allow(dead_code, unused_variables)]

use crate::config::Config;
use crate::config::schema::AccountCfg;
use crate::error::SwampError;
use camino::Utf8PathBuf;
use std::collections::BTreeMap;

/// `~` and `$VAR` expansion for config paths and account env overlays.
pub fn expand_path(s: &str) -> Utf8PathBuf {
    todo!("WP1")
}

pub fn expand_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    todo!("WP1")
}

/// Resolves `exec` through PATH and records the absolute path.
pub fn resolve_exec(a: &AccountCfg) -> Result<Utf8PathBuf, SwampError> {
    todo!("WP1")
}

pub fn from_schema(s: crate::config::schema::Schema) -> Config {
    todo!("WP1")
}
