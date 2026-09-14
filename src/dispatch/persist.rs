#![allow(dead_code, unused_variables)]

use crate::dispatch::account::AccountState;
use crate::model::core::AccountId;
use camino::Utf8Path;
use std::collections::BTreeMap;

/// Cross-run, cross-repo, fs4-locked, temp-write-and-rename.
pub fn load_state(path: &Utf8Path) -> anyhow::Result<BTreeMap<AccountId, AccountState>> {
    todo!("WP4")
}

pub fn save_state(path: &Utf8Path, s: &BTreeMap<AccountId, AccountState>) -> anyhow::Result<()> {
    todo!("WP4")
}
