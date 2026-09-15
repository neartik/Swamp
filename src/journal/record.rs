use crate::dispatch::account::{Health, QuotaSource, WindowKey};
use crate::dispatch::policy::SelectionPolicy;
use crate::ids::{NodeId, RunId};
use crate::model::core::{
    AccountId, Cost, FileChange, NodeState, Provider, RateLimitSnapshot, SessionHandle, Tier, Usage,
};
use crate::model::event::WorkerEvent;
use crate::model::failure::Failure;
use crate::model::node::{ExitInfo, NodeRecord, WorkResultRef};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalLine {
    pub seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub run: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<NodeId>,
    #[serde(flatten)]
    pub event: JournalEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum JournalEvent {
    RunStarted {
        swamp_version: String,
        schema: u32,
        argv: Vec<String>,
        cwd: Utf8PathBuf,
        repo: Option<Utf8PathBuf>,
        base: Option<String>,
        config_sha256: String,
        task: Option<String>,
    },
    /// Full snapshot at creation. Everything after is a delta.
    /// Serialized as `record`: `node` is already taken by the line-level node id, and a
    /// flattened duplicate key makes the line impossible to read back.
    NodeSpawned {
        #[serde(rename = "record")]
        node: Box<NodeRecord>,
    },
    AccountSelected {
        account: AccountId,
        exec: String,
        policy: SelectionPolicy,
        reason: String,
        excluded: Vec<AccountId>,
    },
    ModelResolved {
        tier: Tier,
        model: String,
        extra: BTreeMap<String, String>,
    },
    ProcessStarted {
        pid: i32,
        pgid: i32,
        argv: Vec<String>,
        env_overrides: BTreeMap<String, String>,
        cwd: Utf8PathBuf,
    },
    SessionBound {
        session: SessionHandle,
    },
    /// Normalized event plus the byte offset in stream.jsonl it was parsed from.
    /// (last offset, raw file) is a complete crash-safe resume point for the parser.
    NodeEvent {
        offset: u64,
        event: WorkerEvent,
    },
    NodeUsage {
        usage: Usage,
        cost: Option<Cost>,
    },
    NodeFiles {
        files: Vec<FileChange>,
    },
    NodeBlocked {
        #[serde(with = "time::serde::rfc3339")]
        until: OffsetDateTime,
        why: String,
    },
    NodeRetry {
        attempt: u32,
        reason: Failure,
        rotate: bool,
    },
    ProviderSwitch {
        from: Provider,
        to: Provider,
    },
    WorktreeCreated {
        path: Utf8PathBuf,
        branch: String,
        base: String,
    },
    DiffCaptured {
        patch: Utf8PathBuf,
        head: String,
        files: u32,
        insertions: u32,
        deletions: u32,
    },
    NodeFinished {
        state: NodeState,
        exit: Option<ExitInfo>,
        usage: Usage,
        cost: Option<Cost>,
        work: Option<WorkResultRef>,
        summary: Option<String>,
        files: Vec<FileChange>,
        unparsed_lines: u32,
    },
    AccountHealth {
        account: AccountId,
        health: Health,
        #[serde(with = "time::serde::rfc3339::option")]
        cooldown_until: Option<OffsetDateTime>,
        quota: Option<RateLimitSnapshot>,
        #[serde(default, with = "time::serde::rfc3339::option")]
        quota_observed_at: Option<OffsetDateTime>,
        #[serde(default)]
        quota_source: Option<QuotaSource>,
    },
    /// Emitted on every commit_usage and on every window roll, so `swamp replay --reparse`
    /// re-derives token counters from the journal like everything else.
    AccountUsage {
        account: AccountId,
        window: Usage,
        lifetime: Usage,
        #[serde(default)]
        window_key: Option<WindowKey>,
        rolled: bool,
        #[serde(default)]
        source: Option<QuotaSource>,
    },
    BrainTurn {
        role: TurnRole,
        text: String,
    },
    BrainToolCall {
        tool: String,
        args_sha256: String,
        args_path: Utf8PathBuf,
    },
    Adopted {
        into: String,
        commit: String,
        conflicts: Vec<Utf8PathBuf>,
    },
    Note {
        author: NoteAuthor,
        text: String,
    },
    /// Written on graceful shutdown. Its ABSENCE is what marks a run interrupted.
    RunFinished {
        state: NodeState,
        nodes: u32,
        usage: Usage,
        cost_usd: Option<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteAuthor {
    User,
    Brain,
    Swamp,
}

/// Bumped only for a breaking change; every additive change keeps it.
pub const SCHEMA_VERSION: u32 = 1;
