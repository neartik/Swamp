use crate::config::FailurePatterns;
use crate::ids::NodeIds;
use crate::model::core::{
    FileChange, FinalSummary, NodeKind, Provider, RateLimitSnapshot, SessionHandle, Tier, Usage,
};
use crate::model::event::{RawLine, WorkerEvent};
use crate::model::failure::Failure;
use crate::model::node::ExitInfo;
use crate::model::result::IsolationMode;
use camino::Utf8PathBuf;
use serde_json::value::RawValue;
use smallvec::SmallVec;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub node: NodeIds,
    pub provider: Provider,
    pub exec: String,
    pub env: BTreeMap<String, String>,
    pub model: String,
    pub tier: Tier,
    pub cwd: Utf8PathBuf,
    pub isolation: IsolationMode,
    pub session: SessionPlan,
    /// Worker or Brain; changes the argv.
    pub kind: NodeKind,
    pub permission_mode: String,
    pub sandbox: String,
    pub budget_usd: Option<f64>,
    pub append_system_prompt: Option<String>,
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    pub mcp: Option<McpAttach>,
    pub last_message_path: Utf8PathBuf,
    pub extra_args: Vec<String>,
    /// providers.<p>.tier_extra.<tier>: rendered per adapter, never passed through verbatim.
    pub extra: BTreeMap<String, String>,
    /// brain.include_partial_messages; meaningless for a worker.
    pub partial_messages: bool,
    pub attempt: u32,
}

#[derive(Debug, Clone)]
pub enum SessionPlan {
    New { preassigned: Option<String> },
    Resume(SessionHandle),
}

#[derive(Debug, Clone)]
pub struct McpAttach {
    pub command: Utf8PathBuf,
    pub args: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ParseState {
    pub session: Option<String>,
    pub model: Option<String>,
    pub usage: Usage,
    pub last_rate_limit: Option<RateLimitSnapshot>,
    pub last_final: Option<FinalSummary>,
    /// tool_use_id -> name, to label results.
    pub tool_names: HashMap<String, String>,
    pub files: Vec<FileChange>,
    /// Last 64 lines.
    pub stderr_tail: VecDeque<String>,
    pub unparsed: u32,
}

/// One input line can fan out to several normalized events (a claude assistant message
/// routinely carries a text block AND a tool_use block). Returning Option would drop the rest.
pub struct ParseOutput {
    pub events: SmallVec<[WorkerEvent; 4]>,
    pub noise: bool,
}

pub struct ExitContext<'a> {
    pub exit: Option<ExitInfo>,
    pub state: &'a ParseState,
    /// Compiled RegexSets from config, field-upgradeable.
    pub patterns: &'a FailurePatterns,
    pub deadline_hit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Resume,
    PreassignedSession,
    McpStdio,
    NativeBudget,
    StreamingStdin,
    ReportedCost,
    QuotaTelemetry,
    ToolPolicyFlags,
}

/// Deliberately sync and object-safe: an adapter is a pure argv builder plus a line parser
/// plus a classifier. All async lives in `spawn.rs` / `follow.rs`, which makes adapters
/// trivially testable against the recorded sample streams with no tokio runtime.
pub trait ProviderAdapter: Send + Sync + 'static {
    fn provider(&self) -> Provider;
    fn supports(&self, cap: Capability) -> bool;
    fn build_argv(&self, spec: &LaunchSpec) -> anyhow::Result<Vec<OsString>>;
    fn env(&self, spec: &LaunchSpec) -> Vec<(OsString, OsString)>;
    fn parse_line(&self, line: &str, st: &mut ParseState) -> ParseOutput;
    /// None == success.
    fn classify(&self, cx: &ExitContext<'_>) -> Option<Failure>;
    fn brain_transport(&self) -> BrainTransport;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainTransport {
    Persistent,
    ResumePerTurn,
}

impl ParseOutput {
    pub fn empty() -> Self {
        Self {
            events: SmallVec::new(),
            noise: false,
        }
    }
    /// A line neither provider could make sense of: kept verbatim in noise.log.
    pub fn unparsable() -> Self {
        Self {
            events: SmallVec::new(),
            noise: true,
        }
    }
    pub fn one(ev: WorkerEvent) -> Self {
        let mut events = SmallVec::new();
        events.push(ev);
        Self {
            events,
            noise: false,
        }
    }
    pub fn many(events: SmallVec<[WorkerEvent; 4]>) -> Self {
        Self {
            events,
            noise: false,
        }
    }
}

/// Raw JSON kept for `WorkerEvent::Unknown`; the caller has already checked it parses.
pub(crate) fn raw_line(line: &str) -> RawLine {
    RawLine(
        RawValue::from_string(line.to_owned())
            .unwrap_or_else(|_| RawValue::from_string("null".to_owned()).expect("null is json")),
    )
}

/// `--dangerously-*` is inert unless the operator acknowledged it in config. Returns the
/// arguments to use and the ones that were refused, so the caller can report them.
pub fn gate_unsafe_args(args: &[String], unsafe_ack: bool) -> (Vec<String>, Vec<String>) {
    if unsafe_ack {
        return (args.to_vec(), Vec::new());
    }
    let (dropped, kept): (Vec<String>, Vec<String>) = args
        .iter()
        .cloned()
        .partition(|a| a.starts_with("--dangerously"));
    (kept, dropped)
}

pub fn adapter_for(p: Provider) -> Arc<dyn ProviderAdapter> {
    static CLAUDE: OnceLock<Arc<dyn ProviderAdapter>> = OnceLock::new();
    static CODEX: OnceLock<Arc<dyn ProviderAdapter>> = OnceLock::new();
    match p {
        Provider::Anthropic => CLAUDE
            .get_or_init(|| Arc::new(crate::worker::claude::ClaudeAdapter))
            .clone(),
        Provider::Openai => CODEX
            .get_or_init(|| Arc::new(crate::worker::codex::CodexAdapter))
            .clone(),
    }
}
