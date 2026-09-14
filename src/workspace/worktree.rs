#![allow(dead_code, unused_variables)]

use crate::ids::{NodeId, RunId};
use crate::workspace::git::Git;
use camino::{Utf8Path, Utf8PathBuf};

pub fn branch_name(prefix: &str, run: RunId, node: NodeId, attempt: u32) -> String {
    todo!("WP5")
}

pub async fn add(git: &Git, path: &Utf8Path, branch: &str, base: &str) -> anyhow::Result<()> {
    todo!("WP5")
}

pub async fn remove(git: &Git, path: &Utf8Path, force: bool) -> anyhow::Result<()> {
    todo!("WP5")
}

pub async fn prune(git: &Git) -> anyhow::Result<u32> {
    todo!("WP5")
}

/// link/copy seeds: a worktree that cannot build produces a useless diff, expensively.
pub async fn seed(
    main: &Utf8Path,
    wt: &Utf8Path,
    link: &[String],
    copy: &[String],
) -> anyhow::Result<Vec<String>> {
    todo!("WP5")
}

pub async fn list(git: &Git) -> anyhow::Result<Vec<Utf8PathBuf>> {
    todo!("WP5")
}
