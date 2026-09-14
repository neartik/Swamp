#![allow(dead_code, unused_variables)]

use crate::config::Config;
use crate::journal::paths::Paths;

pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Error,
}

pub async fn checks(cfg: &Config, paths: &Paths, probe: bool, schema: bool) -> Vec<Check> {
    todo!("WP7")
}

/// Removes stale worktrees, sockets and pidfiles.
pub async fn reap(paths: &Paths) -> anyhow::Result<u32> {
    todo!("WP7")
}
