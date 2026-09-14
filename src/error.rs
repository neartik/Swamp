#![allow(unused_variables)]

use crate::model::core::{Provider, Tier};

#[derive(Debug, thiserror::Error)]
pub enum SwampError {
    #[error(
        "no {provider:?} account available: {excluded} excluded by failover, {cooling} cooling down"
    )]
    NoAccountAvailable {
        provider: Provider,
        excluded: usize,
        cooling: usize,
    },
    #[error(
        "no model configured for {provider:?} tier {tier:?}; set providers.<p>.models.<t> in swamp.toml"
    )]
    TierUnmapped { provider: Provider, tier: Tier },
    #[error("executable `{exec}` for account `{id}` not found in PATH")]
    ExecNotFound { id: String, exec: String },
    #[error("all {attempts} attempts exhausted for task `{title}`")]
    ExhaustedAttempts { title: String, attempts: u32 },
    #[error(
        "not a git repository: {0} (worktree isolation requires git; use isolation = \"shared\")"
    )]
    NotAGitRepo(camino::Utf8PathBuf),
    #[error("refusing to run: working tree is dirty. Commit, stash, or pass --include-dirty")]
    DirtyTree,
    #[error("config invalid:\n{0}")]
    ConfigInvalid(String),
    #[error("swamp is already running for this repo (pid {pid})")]
    AlreadyRunning { pid: i32 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Scripts branch on these. 3 vs 4 is "try again in an hour" vs "your task is broken".
pub fn exit_code(e: &anyhow::Error) -> i32 {
    todo!("WP1")
}
