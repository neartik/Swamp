use crate::ids::{NodeId, RunId};
use crate::model::core::{
    AccountId, Cost, FileChange, NodeKind, NodeState, Provider, SessionHandle, Tier, Usage,
    WorkspaceRef,
};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeRecord {
    pub id: NodeId,
    pub run_id: RunId,
    pub parent: Option<NodeId>,
    /// Stable across retries: attempt 2 is a new NodeId but the same `logical`.
    /// The tree collapses attempts into one row with an attempt chain.
    pub logical: NodeId,
    pub attempt: u32,
    /// Set when this node is a failover retry of a sibling.
    pub retry_of: Option<NodeId>,
    pub kind: NodeKind,
    pub title: String,

    /// The prompt is on disk, not inline: it can be large and it is the exact bytes fed to fd0.
    pub prompt_path: Utf8PathBuf,
    pub prompt_sha256: String,

    pub provider: Provider,
    pub account: Option<AccountId>,
    pub exec: Option<String>,
    pub argv: Vec<String>,
    pub model: Option<String>,
    pub tier: Tier,

    pub workspace: WorkspaceRef,
    pub session: Option<SessionHandle>,
    pub state: NodeState,

    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,

    pub usage: Usage,
    pub cost: Option<Cost>,
    pub exit: Option<ExitInfo>,
    pub files: Vec<FileChange>,
    pub work: Option<WorkResultRef>,
    pub summary: Option<String>,
    /// Byte offset consumed so far in nodes/<id>/stream.jsonl. Restart resumes exactly here.
    pub stream_offset: u64,
    pub unparsed_lines: u32,
}

impl NodeRecord {
    pub fn duration(&self) -> Option<std::time::Duration> {
        todo!("WP1")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkResultRef {
    pub head: String,
    pub branch: String,
    pub patch: Utf8PathBuf,
    pub insertions: u32,
    pub deletions: u32,
    pub empty: bool,
}
