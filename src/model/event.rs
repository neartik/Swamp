use crate::model::core::{ChangeKind, FinalSummary, RateLimitSnapshot, Usage};
use camino::Utf8PathBuf;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

/// Provider-neutral. Every adapter normalizes into this; the journal, the TUI and the brain
/// only ever see this shape. Raw provider lines live untouched in nodes/<id>/stream.jsonl.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum WorkerEvent {
    SessionStarted {
        session: String,
        model: Option<String>,
        auth_hint: Option<String>,
    },
    AssistantText {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        summary: String,
    },
    ToolResult {
        id: String,
        ok: bool,
        summary: String,
    },
    FileChanged {
        path: Utf8PathBuf,
        kind: ChangeKind,
    },
    Usage(Usage),
    RateLimit(RateLimitSnapshot),
    Final(FinalSummary),
    /// Never dropped and never fatal: a CLI schema change degrades instead of breaking.
    Unknown {
        raw: RawLine,
    },
}

/// `Box<RawValue>` with structural equality, so `WorkerEvent` can keep `PartialEq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RawLine(pub Box<RawValue>);

impl PartialEq for RawLine {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}
