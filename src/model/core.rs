#![allow(unused_variables)]

use crate::model::failure::Failure;
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Anthropic,
    Openai,
}

/// Ordered so `Tier::High > Tier::Low` works for policy checks.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Low,
    Mid,
    High,
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!("WP1")
    }
}

impl std::str::FromStr for Provider {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        todo!("WP1")
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!("WP1")
    }
}

impl std::str::FromStr for Tier {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        todo!("WP1")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Root,
    Brain,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NodeState {
    Queued,
    /// Every eligible account is cooling; the scheduler sleeps until `until`.
    Blocked {
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
        why: String,
    },
    Leased {
        account: AccountId,
    },
    Running {
        pid: i32,
        pgid: i32,
        #[serde(with = "time::serde::rfc3339")]
        since: OffsetDateTime,
    },
    /// Process outlived a supervisor crash, or vice versa. Recovery decides adopt vs resume.
    Orphaned {
        pid: i32,
        stream_offset: u64,
    },
    Succeeded,
    Failed {
        failure: Failure,
    },
    Cancelled {
        by: CancelSource,
    },
}

impl NodeState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelSource {
    User,
    Brain,
    Budget,
    Timeout,
    Shutdown,
}

/// A resume handle. ALWAYS carried with the account that minted it: a claude session id
/// created under CLAUDE_CONFIG_DIR=A does not exist under B. If this invariant breaks,
/// resume silently starts a fresh conversation while Swamp believes it has context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub account: AccountId,
    pub id: String,
    pub preassigned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkspaceRef {
    /// Fresh `git worktree` on a `swamp/<run>/<node>` branch. Default for anything that writes.
    Worktree {
        path: Utf8PathBuf,
        branch: String,
        base: String,
    },
    /// The user's real checkout, serialized behind a repo-wide write mutex. Opt-in.
    Shared { path: Utf8PathBuf },
    /// The checkout with provider-level write denial. Used for the brain and review nodes.
    ReadOnly { path: Utf8PathBuf },
}

impl WorkspaceRef {
    pub fn path(&self) -> &camino::Utf8Path {
        match self {
            Self::Worktree { path, .. } | Self::Shared { path } | Self::ReadOnly { path } => path,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl Usage {
    pub fn absorb(&mut self, o: &Usage) {
        self.input_tokens += o.input_tokens;
        self.cached_input_tokens += o.cached_input_tokens;
        self.cache_write_tokens += o.cache_write_tokens;
        self.output_tokens += o.output_tokens;
        self.reasoning_tokens += o.reasoning_tokens;
    }
    pub fn billable(&self) -> u64 {
        self.input_tokens + self.cache_write_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    pub usd: f64,
    pub basis: CostBasis,
}

/// `Reported` = the CLI told us (claude `total_cost_usd`). Note that on a subscription this is
/// list-price equivalence, not money billed. `Estimated` = we multiplied tokens by the
/// `[pricing]` table because the CLI reports none. Absent cost renders as `-`, never `$0.00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    Reported,
    Estimated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
    Rename,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileChange {
    pub path: Utf8PathBuf,
    pub kind: ChangeKind,
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    /// Git is authoritative. EventStream is a live-progress estimate and may be wrong.
    pub source: EvidenceSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    Git,
    EventStream,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    FiveHour,
    SevenDay,
    Minute,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitStatus {
    Allowed,
    Warning,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LimitWindow {
    pub scope: LimitScope,
    /// 0.0 ..= 1.0
    pub utilization: f64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
}

/// The real sample carries five_hour = 0.06 AND seven_day = 0.64 in the same event.
/// Collapsing to one window throws away the one that is actually near exhaustion,
/// so every window is kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub status: LimitStatus,
    pub windows: Vec<LimitWindow>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub resets_at: Option<OffsetDateTime>,
}

impl RateLimitSnapshot {
    pub fn worst_utilization(&self) -> f64 {
        self.windows
            .iter()
            .map(|w| w.utilization)
            .fold(0.0, f64::max)
    }
    pub fn soonest_reset(&self) -> Option<OffsetDateTime> {
        self.windows
            .iter()
            .filter_map(|w| w.resets_at)
            .min()
            .or(self.resets_at)
    }
    pub fn worst_scope(&self) -> LimitScope {
        self.windows
            .iter()
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
            .map_or(LimitScope::Unknown, |w| w.scope)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalSummary {
    pub ok: bool,
    /// Raw provider terminal marker: claude `result.subtype`, codex `turn.completed|failed`.
    pub subtype: String,
    pub text: Option<String>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub api_error_status: Option<i64>,
    #[serde(default)]
    pub num_turns: u32,
    /// Under `--permission-prompts none` anything that would prompt is denied. A worker then
    /// writes a confident summary of work it never did. Non-empty is a failure signal.
    #[serde(default)]
    pub permission_denials: u32,
}
