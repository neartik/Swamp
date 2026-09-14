#![allow(dead_code, unused_variables)]

use crate::error::SwampError;
use camino::{Utf8Path, Utf8PathBuf};

/// Thin async wrapper over the `git` CLI. Porcelain only, -z everywhere.
#[derive(Debug, Clone)]
pub struct Git {
    pub root: Utf8PathBuf,
}

impl Git {
    pub async fn discover(cwd: &Utf8Path) -> Result<Git, SwampError> {
        todo!("WP5")
    }
    pub async fn run(&self, cwd: &Utf8Path, args: &[&str]) -> anyhow::Result<String> {
        todo!("WP5")
    }
    pub async fn head(&self) -> anyhow::Result<String> {
        todo!("WP5")
    }
    pub async fn is_clean(&self) -> anyhow::Result<bool> {
        todo!("WP5")
    }
    /// The `--include-dirty` base.
    pub async fn stash_create(&self) -> anyhow::Result<Option<String>> {
        todo!("WP5")
    }
    pub async fn version(&self) -> anyhow::Result<(u32, u32)> {
        todo!("WP5")
    }
}
