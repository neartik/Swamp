#![allow(dead_code, unused_variables)]

pub mod adopt;
pub mod diff;
pub mod git;
pub mod worktree;

pub use adopt::{AdoptResult, MergeStrategy, adopt};
pub use diff::DiffSummary;
pub use git::Git;

use crate::config::Config;
use crate::ids::NodeId;
use crate::journal::JournalHandle;
use crate::journal::paths::Paths;
use crate::model::core::Tier;
use crate::model::node::WorkResultRef;
use camino::Utf8PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct NodeWorktree {
    pub node: NodeId,
    pub path: Utf8PathBuf,
    pub branch: String,
    pub base: String,
}

/// Owns the global git gate: concurrent `git worktree add` contends on .git/worktrees.
pub struct WorkspaceManager {
    pub git: Git,
    pub paths: Arc<Paths>,
    pub cfg: Arc<Config>,
    pub journal: JournalHandle,
}

impl WorkspaceManager {
    pub async fn new(
        git: Git,
        paths: Arc<Paths>,
        cfg: Arc<Config>,
        journal: JournalHandle,
    ) -> anyhow::Result<Arc<Self>> {
        todo!("WP5")
    }
    pub async fn base_commit(
        &self,
        requested: Option<&str>,
        include_dirty: bool,
    ) -> anyhow::Result<String> {
        todo!("WP5")
    }
    /// Fresh worktree per attempt, seeded and serialized behind a mutex plus a flock.
    pub async fn create(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree> {
        todo!("WP5")
    }
    pub async fn finalize(
        &self,
        wt: &NodeWorktree,
        title: &str,
        tier: Tier,
    ) -> anyhow::Result<Option<WorkResultRef>> {
        todo!("WP5")
    }
    pub async fn remove(&self, wt: &NodeWorktree, force: bool) -> anyhow::Result<()> {
        todo!("WP5")
    }
    pub async fn prune(&self) -> anyhow::Result<u32> {
        todo!("WP5")
    }
    pub async fn list(&self) -> anyhow::Result<Vec<NodeWorktree>> {
        todo!("WP5")
    }
    /// Shared isolation: at most one process mutating the user's real tree.
    pub async fn shared_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        todo!("WP5")
    }
}
