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
    /// None while the node is still running, and on a clock that went backwards.
    pub fn duration(&self) -> Option<std::time::Duration> {
        let started = self.started_at?;
        let ended = self.ended_at?;
        (ended - started).try_into().ok()
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
    /// What git says the attempt changed. Authoritative over the event stream.
    #[serde(default)]
    pub files: Vec<FileChange>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{ChangeKind, CostBasis, EvidenceSource};
    use std::str::FromStr;

    pub(crate) fn sample() -> NodeRecord {
        NodeRecord {
            id: NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            run_id: RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAW").unwrap(),
            parent: None,
            logical: NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap(),
            attempt: 2,
            retry_of: Some(NodeId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAX").unwrap()),
            kind: NodeKind::Worker,
            title: "port the parser".into(),
            prompt_path: "nodes/g5fav/prompt.md".into(),
            prompt_sha256: "abc123".into(),
            provider: Provider::Anthropic,
            account: Some(AccountId("main".into())),
            exec: Some("claude-main".into()),
            argv: vec!["-p".into(), "--output-format".into()],
            model: Some("model-a".into()),
            tier: Tier::Mid,
            workspace: WorkspaceRef::Worktree {
                path: "/tmp/wt".into(),
                branch: "swamp/g5faw/g5fav".into(),
                base: "HEAD".into(),
            },
            session: Some(SessionHandle {
                account: AccountId("main".into()),
                id: "0f0f".into(),
                preassigned: true,
            }),
            state: NodeState::Succeeded,
            created_at: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            started_at: Some(time::OffsetDateTime::from_unix_timestamp(1_700_000_010).unwrap()),
            ended_at: Some(time::OffsetDateTime::from_unix_timestamp(1_700_000_070).unwrap()),
            usage: Usage {
                input_tokens: 100,
                cached_input_tokens: 40,
                cache_write_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: 5,
            },
            cost: Some(Cost {
                usd: 0.25,
                basis: CostBasis::Estimated,
            }),
            exit: Some(ExitInfo {
                code: Some(0),
                signal: None,
                duration_ms: 60_000,
            }),
            files: vec![FileChange {
                path: "src/lib.rs".into(),
                kind: ChangeKind::Modify,
                added: 12,
                removed: 3,
                source: EvidenceSource::Git,
            }],
            work: Some(WorkResultRef {
                head: "deadbeef".into(),
                branch: "swamp/g5faw/g5fav".into(),
                patch: "nodes/g5fav/patch.diff".into(),
                insertions: 12,
                deletions: 3,
                empty: false,
                files: Vec::new(),
            }),
            summary: Some("done".into()),
            stream_offset: 4096,
            unparsed_lines: 1,
        }
    }

    #[test]
    fn duration_spans_start_to_end() {
        let n = sample();
        assert_eq!(n.duration(), Some(std::time::Duration::from_secs(60)));
    }

    #[test]
    fn duration_is_none_until_the_node_ends() {
        let mut n = sample();
        n.ended_at = None;
        assert_eq!(n.duration(), None);
        n.started_at = None;
        assert_eq!(n.duration(), None);
    }

    #[test]
    fn duration_is_none_when_the_clock_went_backwards() {
        let mut n = sample();
        n.ended_at = Some(time::OffsetDateTime::from_unix_timestamp(1_699_999_000).unwrap());
        assert_eq!(n.duration(), None);
    }

    #[test]
    fn node_record_round_trips() {
        let n = sample();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(serde_json::from_str::<NodeRecord>(&json).unwrap(), n);
        insta::assert_json_snapshot!(n);
    }
}
