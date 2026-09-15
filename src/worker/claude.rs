use crate::model::core::{
    ChangeKind, Cost, CostBasis, EvidenceSource, FileChange, FinalSummary, LimitScope, LimitStatus,
    LimitWindow, NodeKind, Provider, RateLimitSnapshot, Usage,
};
use crate::model::event::WorkerEvent;
use crate::model::failure::Failure;
use crate::model::result::IsolationMode;
use crate::worker::adapter::{
    BrainTransport, Capability, ExitContext, LaunchSpec, ParseOutput, ParseState, ProviderAdapter,
    SessionPlan, raw_line,
};
use camino::Utf8PathBuf;
use serde::Deserialize;
use smallvec::SmallVec;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use time::OffsetDateTime;

/// Tools whose inputs advertise a file write. Advisory only: git is authoritative at finalize.
const EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
const SUMMARY_MAX: usize = 200;

#[derive(Debug, Default, Clone, Copy)]
pub struct ClaudeAdapter;

impl ProviderAdapter for ClaudeAdapter {
    fn provider(&self) -> Provider {
        Provider::Anthropic
    }

    /// The claude CLI implements every capability the trait knows about.
    fn supports(&self, _cap: Capability) -> bool {
        true
    }

    fn build_argv(&self, spec: &LaunchSpec) -> anyhow::Result<Vec<OsString>> {
        let mut a: Vec<OsString> = vec![spec.exec.clone().into()];
        let mut push = |s: &str| a.push(OsString::from(s));
        push("-p");
        push("--output-format");
        push("stream-json");
        push("--verbose");
        push("--model");
        a.push(spec.model.clone().into());

        match &spec.session {
            SessionPlan::New { preassigned } => {
                a.push("--session-id".into());
                let id = preassigned
                    .clone()
                    .unwrap_or_else(|| spec.node.session_uuid.to_string());
                a.push(id.into());
            }
            SessionPlan::Resume(h) => {
                a.push("--resume".into());
                a.push(h.id.clone().into());
            }
        }

        // An unset config key must not become `--permission-mode ''`, which claude rejects.
        if !spec.permission_mode.trim().is_empty() {
            a.push("--permission-mode".into());
            a.push(spec.permission_mode.clone().into());
        }
        a.push("--permission-prompts".into());
        a.push("none".into());

        match (&spec.kind, &spec.mcp) {
            (NodeKind::Brain, Some(mcp)) => {
                a.push("--input-format".into());
                a.push("stream-json".into());
                a.push("--mcp-config".into());
                a.push(mcp_config_json(mcp).into());
                a.push("--strict-mcp-config".into());
                if spec.partial_messages {
                    a.push("--include-partial-messages".into());
                }
            }
            // No --mcp-config alongside it: workers get exactly zero MCP servers.
            _ => a.push("--strict-mcp-config".into()),
        }

        if let Some(b) = spec.budget_usd {
            a.push("--max-budget-usd".into());
            a.push(format!("{b}").into());
        }
        if let Some(text) = &spec.append_system_prompt {
            a.push("--append-system-prompt".into());
            a.push(text.clone().into());
        }
        // One flag per list: a repeated variadic option overwrites, so the readonly denials
        // and the configured ones have to be merged before they reach the argv.
        let mut allow: Vec<String> = Vec::new();
        if spec.kind == NodeKind::Brain && spec.mcp.is_some() {
            allow.extend(crate::mcp::tools::qualified_names());
        }
        allow.extend(spec.allow_tools.iter().cloned());
        let mut deny: Vec<String> = Vec::new();
        if spec.isolation == IsolationMode::ReadOnly {
            deny.extend(EDIT_TOOLS.iter().map(|t| (*t).to_owned()));
        }
        deny.extend(spec.deny_tools.iter().cloned());
        push_list(&mut a, "--allowed-tools", &allow);
        push_list(&mut a, "--disallowed-tools", &deny);
        for (k, v) in &spec.extra {
            a.push(format!("--{k}").into());
            a.push(v.clone().into());
        }
        a.extend(spec.extra_args.iter().map(OsString::from));
        Ok(a)
    }

    fn env(&self, spec: &LaunchSpec) -> Vec<(OsString, OsString)> {
        spec.env
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    fn parse_line(&self, line: &str, st: &mut ParseState) -> ParseOutput {
        let t = line.trim();
        if !t.starts_with('{') {
            return ParseOutput::unparsable();
        }
        let parsed: ClaudeLine = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => return ParseOutput::unparsable(),
        };
        match parsed {
            ClaudeLine::System(s) => system_event(s, st),
            ClaudeLine::Assistant(m) => assistant_events(m, st),
            ClaudeLine::User(m) => user_events(m, st),
            ClaudeLine::RateLimitEvent { rate_limit_info } => {
                let snap = snapshot(&rate_limit_info);
                st.last_rate_limit = Some(snap.clone());
                ParseOutput::one(WorkerEvent::RateLimit(snap))
            }
            ClaudeLine::Result(r) => result_event(*r, st),
            ClaudeLine::StreamEvent => ParseOutput::empty(),
            ClaudeLine::Other => {
                st.unparsed += 1;
                ParseOutput::one(WorkerEvent::Unknown { raw: raw_line(t) })
            }
        }
    }

    fn classify(&self, cx: &ExitContext<'_>) -> Option<Failure> {
        crate::worker::classify::classify(cx)
    }

    fn brain_transport(&self) -> BrainTransport {
        BrainTransport::Persistent
    }
}

fn mcp_config_json(mcp: &crate::worker::adapter::McpAttach) -> String {
    serde_json::json!({
        "mcpServers": {
            crate::mcp::server::SERVER_NAME: { "command": mcp.command.as_str(), "args": mcp.args }
        }
    })
    .to_string()
}

/// `--allowed-tools A B C`, deduplicated and in order; nothing at all when the list is empty,
/// since a bare flag with no values is an argv error.
fn push_list(a: &mut Vec<OsString>, flag: &str, tools: &[String]) {
    let mut seen = BTreeSet::new();
    let unique: Vec<&String> = tools.iter().filter(|t| seen.insert(t.as_str())).collect();
    if unique.is_empty() {
        return;
    }
    a.push(flag.into());
    a.extend(unique.into_iter().map(OsString::from));
}

fn system_event(s: SystemLine, st: &mut ParseState) -> ParseOutput {
    if let Some(m) = s.model.clone() {
        st.model = Some(m);
    }
    let Some(session) = s.session_id.filter(|_| s.subtype == "init") else {
        return ParseOutput::empty();
    };
    st.session = Some(session.clone());
    ParseOutput::one(WorkerEvent::SessionStarted {
        session,
        model: st.model.clone(),
        auth_hint: s.api_key_source,
    })
}

fn assistant_events(m: MsgEnvelope, st: &mut ParseState) -> ParseOutput {
    let mut out: SmallVec<[WorkerEvent; 4]> = SmallVec::new();
    if let Some(model) = m.message.model.clone() {
        st.model = Some(model);
    }
    for b in m.message.content {
        match b {
            Block::Text { text } => out.push(WorkerEvent::AssistantText { text }),
            Block::Thinking { thinking } => out.push(WorkerEvent::Thinking { text: thinking }),
            Block::ToolUse { id, name, input } => {
                st.tool_names.insert(id.clone(), name.clone());
                let summary = tool_summary(&input);
                if EDIT_TOOLS.contains(&name.as_str())
                    && let Some(path) = file_path(&input)
                {
                    st.files.push(FileChange {
                        path: path.clone(),
                        kind: ChangeKind::Modify,
                        added: 0,
                        removed: 0,
                        source: EvidenceSource::EventStream,
                    });
                    out.push(WorkerEvent::ToolCall { id, name, summary });
                    out.push(WorkerEvent::FileChanged {
                        path,
                        kind: ChangeKind::Modify,
                    });
                    continue;
                }
                out.push(WorkerEvent::ToolCall { id, name, summary });
            }
            Block::ToolResult {
                tool_use_id,
                is_error,
            } => out.push(tool_result(tool_use_id, is_error, st)),
            Block::Other => {}
        }
    }
    if let Some(u) = m.message.usage {
        let u = usage_of(&u);
        st.usage.absorb(&u);
        out.push(WorkerEvent::Usage(u));
    }
    ParseOutput::many(out)
}

fn user_events(m: MsgEnvelope, st: &mut ParseState) -> ParseOutput {
    let mut out: SmallVec<[WorkerEvent; 4]> = SmallVec::new();
    for b in m.message.content {
        if let Block::ToolResult {
            tool_use_id,
            is_error,
        } = b
        {
            out.push(tool_result(tool_use_id, is_error, st));
        }
    }
    ParseOutput::many(out)
}

fn tool_result(id: String, is_error: bool, st: &ParseState) -> WorkerEvent {
    let summary = st.tool_names.get(&id).cloned().unwrap_or_default();
    WorkerEvent::ToolResult {
        id,
        ok: !is_error,
        summary,
    }
}

fn result_event(r: ResultLine, st: &mut ParseState) -> ParseOutput {
    let usage = usage_of(&r.usage);
    let final_summary = FinalSummary {
        ok: !r.is_error && r.subtype == "success",
        subtype: r.subtype,
        text: r.result,
        usage,
        cost: r.total_cost_usd.map(|usd| Cost {
            usd,
            basis: CostBasis::Reported,
        }),
        api_error_status: r.api_error_status,
        num_turns: r.num_turns,
        permission_denials: r.permission_denials.len() as u32,
        denied_tools: denied_tools(&r.permission_denials),
    };
    if let Some(s) = r.session_id {
        st.session = Some(s);
    }
    // The result line carries run totals, so it replaces the per-message sum rather than adding.
    if usage.billable() > 0 {
        st.usage = usage;
    }
    st.last_final = Some(final_summary.clone());
    ParseOutput::one(WorkerEvent::Final(final_summary))
}

/// `permission_denials[].tool_name`, in the order the CLI reported them.
fn denied_tools(denials: &[serde_json::Value]) -> Vec<String> {
    denials
        .iter()
        .filter_map(|d| d.get("tool_name")?.as_str())
        .map(str::to_owned)
        .collect()
}

fn usage_of(u: &ClaudeUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens,
        cached_input_tokens: u.cache_read_input_tokens,
        cache_write_tokens: u.cache_creation_input_tokens,
        output_tokens: u.output_tokens,
        reasoning_tokens: u
            .output_tokens_details
            .as_ref()
            .map_or(0, |d| d.thinking_tokens),
    }
}

fn snapshot(info: &RateLimitInfo) -> RateLimitSnapshot {
    RateLimitSnapshot {
        status: match info.status.as_str() {
            "rejected" => LimitStatus::Rejected,
            "warning" | "warn" => LimitStatus::Warning,
            _ => LimitStatus::Allowed,
        },
        windows: if info.windows.is_empty() {
            info.kind
                .iter()
                .map(|k| LimitWindow {
                    scope: scope_of(k),
                    utilization: 0.0,
                    resets_at: reset_time(info.resets_at),
                })
                .collect()
        } else {
            info.windows
                .iter()
                .map(|(name, w)| LimitWindow {
                    scope: scope_of(name),
                    utilization: w.utilization,
                    resets_at: reset_time(Some(w.resets_at)),
                })
                .collect()
        },
        resets_at: reset_time(info.resets_at),
    }
}

fn reset_time(secs: Option<i64>) -> Option<OffsetDateTime> {
    secs.and_then(|t| OffsetDateTime::from_unix_timestamp(t).ok())
}

fn scope_of(name: &str) -> LimitScope {
    match name {
        "five_hour" => LimitScope::FiveHour,
        "seven_day" => LimitScope::SevenDay,
        "minute" => LimitScope::Minute,
        _ => LimitScope::Unknown,
    }
}

fn file_path(input: &serde_json::Value) -> Option<Utf8PathBuf> {
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("notebook_path"))
        .and_then(|v| v.as_str())
        .map(Utf8PathBuf::from)
}

fn tool_summary(input: &serde_json::Value) -> String {
    for key in ["file_path", "path", "notebook_path", "command", "pattern"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return crate::worker::classify::truncate(s, SUMMARY_MAX);
        }
    }
    crate::worker::classify::truncate(&input.to_string(), SUMMARY_MAX)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeLine {
    System(SystemLine),
    Assistant(MsgEnvelope),
    User(MsgEnvelope),
    RateLimitEvent {
        rate_limit_info: RateLimitInfo,
    },
    Result(Box<ResultLine>),
    /// --include-partial-messages chunks: nothing downstream consumes a delta, and counting
    /// thousands of them as unparsed would bury the real noise.
    StreamEvent,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct SystemLine {
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// "none" proves subscription auth. doctor asserts it.
    #[serde(default, rename = "apiKeySource")]
    api_key_source: Option<String>,
}

#[derive(Deserialize)]
struct RateLimitInfo {
    #[serde(default)]
    status: String,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
    #[serde(default, rename = "rateLimitType")]
    kind: Option<String>,
    #[serde(default, rename = "unifiedWindows")]
    windows: BTreeMap<String, Window>,
}

#[derive(Deserialize, Clone, Copy)]
struct Window {
    #[serde(default)]
    utilization: f64,
    #[serde(default, rename = "resetsAt")]
    resets_at: i64,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct MsgEnvelope {
    message: Msg,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
}

#[derive(Deserialize)]
struct Msg {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    content: Vec<Block>,
    #[serde(default)]
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Block {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    ToolUse {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    ToolResult {
        #[serde(default)]
        tool_use_id: String,
        #[serde(default)]
        is_error: bool,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    output_tokens_details: Option<OutDetails>,
}

#[derive(Deserialize, Default)]
struct OutDetails {
    #[serde(default)]
    thinking_tokens: u64,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ResultLine {
    #[serde(default)]
    subtype: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    usage: ClaudeUsage,
    #[serde(default)]
    api_error_status: Option<i64>,
    #[serde(default)]
    num_turns: u32,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    permission_denials: Vec<serde_json::Value>,
    #[serde(default)]
    duration_ms: u64,
}
