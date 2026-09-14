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
/// 5 = merge conflict, 6 = cancelled, 7 = budget exceeded: set by the cmd layer.
pub fn exit_code(e: &anyhow::Error) -> i32 {
    match e.downcast_ref::<SwampError>() {
        Some(SwampError::ConfigInvalid(_)) => 2,
        Some(SwampError::NoAccountAvailable { .. }) => 3,
        Some(SwampError::ExhaustedAttempts { .. }) => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::Provider;

    #[test]
    fn exit_codes_are_stable() {
        let cases: [(SwampError, i32); 5] = [
            (SwampError::ConfigInvalid("bad".into()), 2),
            (
                SwampError::NoAccountAvailable {
                    provider: Provider::Anthropic,
                    excluded: 1,
                    cooling: 2,
                },
                3,
            ),
            (
                SwampError::ExhaustedAttempts {
                    title: "t".into(),
                    attempts: 3,
                },
                4,
            ),
            (SwampError::DirtyTree, 1),
            (SwampError::AlreadyRunning { pid: 7 }, 1),
        ];
        for (err, code) in cases {
            assert_eq!(exit_code(&anyhow::Error::new(err)), code);
        }
        assert_eq!(exit_code(&anyhow::anyhow!("something else")), 1);
    }

    #[test]
    fn a_wrapped_error_keeps_its_code() {
        let e = anyhow::Error::new(SwampError::ConfigInvalid("bad".into())).context("loading");
        assert_eq!(exit_code(&e), 2);
    }
}
