#![allow(dead_code, unused_variables)]

use crate::error::SwampError;
use crate::ids::{NodeId, RunId};
use camino::{Utf8Path, Utf8PathBuf};

/// Repo root, its `.swamp`, and the machine-wide `~/.swamp`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub repo: Utf8PathBuf,
    pub dot_swamp: Utf8PathBuf,
    pub home_swamp: Utf8PathBuf,
}

impl Paths {
    /// Walks up to the git root.
    pub fn discover(cwd: &Utf8Path) -> Result<Paths, SwampError> {
        todo!("WP2")
    }
    pub fn run_dir(&self, run: RunId) -> Utf8PathBuf {
        todo!("WP2")
    }
    /// ~/.swamp/worktrees/<repo>-<hash8>
    pub fn worktree_root(&self) -> Utf8PathBuf {
        todo!("WP2")
    }
    /// ~/.swamp/accounts.json
    pub fn accounts_state(&self) -> Utf8PathBuf {
        todo!("WP2")
    }
    /// .git/info/exclude, not .gitignore.
    pub fn ensure_git_excluded(&self) -> anyhow::Result<()> {
        todo!("WP2")
    }
    /// Newest first.
    pub fn list_runs(&self) -> anyhow::Result<Vec<RunId>> {
        todo!("WP2")
    }
    /// id | prefix | "last" | "-2"
    pub fn resolve_run(&self, spec: &str) -> anyhow::Result<RunId> {
        todo!("WP2")
    }
}

#[derive(Debug, Clone)]
pub struct RunPaths {
    pub run: RunId,
    pub dir: Utf8PathBuf,
}

impl RunPaths {
    pub fn journal(&self) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn node_dir(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn prompt(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn stream(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn stderr(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn noise(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn last_message(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn patch(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn pidfile(&self, n: NodeId) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn socket(&self) -> Utf8PathBuf {
        todo!("WP2")
    }
    pub fn link_last(&self) -> anyhow::Result<()> {
        todo!("WP2")
    }
}
