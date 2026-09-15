use crate::model::core::{
    ChangeKind, EvidenceSource, FileChange, FinalSummary, NodeKind, Provider, Usage,
};
use crate::model::event::WorkerEvent;
use crate::model::failure::Failure;
use crate::model::result::IsolationMode;
use crate::worker::adapter::{
    BrainTransport, Capability, ExitContext, LaunchSpec, McpAttach, ParseOutput, ParseState,
    ProviderAdapter, SessionPlan, raw_line,
};
use crate::worker::classify::truncate;
use serde::Deserialize;
use smallvec::SmallVec;
use std::ffi::OsString;

const READ_ONLY_SANDBOX: &str = "read-only";
const APPROVAL_NEVER: &str = "approval_policy=\"never\"";
const SUMMARY_MAX: usize = 200;
const RESULT_MAX: usize = 4096;

#[derive(Debug, Default, Clone, Copy)]
pub struct CodexAdapter;

impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> Provider {
        Provider::Openai
    }

    fn supports(&self, cap: Capability) -> bool {
        matches!(
            cap,
            Capability::Resume | Capability::McpStdio | Capability::ToolPolicyFlags
        )
    }

    fn build_argv(&self, spec: &LaunchSpec) -> anyhow::Result<Vec<OsString>> {
        let mut a: Vec<OsString> = vec![spec.exec.clone().into(), "exec".into()];
        // `resume <id>` is a subcommand of `exec` and must come before the options.
        if let SessionPlan::Resume(h) = &spec.session {
            a.push("resume".into());
            a.push(h.id.clone().into());
        }
        a.push("--json".into());
        a.push("-m".into());
        a.push(spec.model.clone().into());
        a.push("-C".into());
        a.push(spec.cwd.as_str().into());
        // An unset providers.openai.worker.sandbox must not become `-s ''`, which clap rejects.
        let sandbox = sandbox(spec);
        if !sandbox.trim().is_empty() {
            a.push("-s".into());
            a.push(sandbox.into());
        }
        a.push("-o".into());
        a.push(spec.last_message_path.as_str().into());
        // `codex exec` has NO -a/--ask-for-approval; that flag is top-level only.
        a.push("-c".into());
        a.push(APPROVAL_NEVER.into());

        if spec.kind == NodeKind::Brain
            && let Some(mcp) = &spec.mcp
        {
            for arg in mcp_config_args(mcp) {
                a.push("-c".into());
                a.push(arg.into());
            }
        }
        if !spec.cwd.join(".git").exists() {
            a.push("--skip-git-repo-check".into());
        }
        for (k, v) in &spec.extra {
            a.push("-c".into());
            a.push(format!("{k}=\"{v}\"").into());
        }
        a.extend(spec.extra_args.iter().map(OsString::from));
        a.push("-".into());
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
        // The stream is a human-oriented channel that happens to carry JSON.
        if !t.starts_with('{') {
            return ParseOutput::unparsable();
        }
        let parsed: CodexLine = match serde_json::from_str(t) {
            Ok(v) => v,
            Err(_) => return ParseOutput::unparsable(),
        };
        match parsed {
            CodexLine::ThreadStarted { thread_id } => {
                st.session = Some(thread_id.clone());
                ParseOutput::one(WorkerEvent::SessionStarted {
                    session: thread_id,
                    model: st.model.clone(),
                    auth_hint: None,
                })
            }
            CodexLine::TurnStarted {} => ParseOutput::empty(),
            CodexLine::TurnCompleted { usage } => {
                ParseOutput::one(terminal(st, true, "turn.completed", None, usage_of(&usage)))
            }
            CodexLine::TurnFailed { error } => {
                let so_far = st.usage;
                ParseOutput::one(terminal(
                    st,
                    false,
                    "turn.failed",
                    Some(error.message),
                    so_far,
                ))
            }
            CodexLine::Error { message } => {
                let so_far = st.usage;
                ParseOutput::one(terminal(st, false, "error", Some(message), so_far))
            }
            CodexLine::ItemCompleted { item } => item_events(item, st, true),
            CodexLine::ItemStarted { item } | CodexLine::ItemUpdated { item } => {
                item_events(item, st, false)
            }
            CodexLine::Other => {
                st.unparsed += 1;
                ParseOutput::one(WorkerEvent::Unknown { raw: raw_line(t) })
            }
        }
    }

    fn classify(&self, cx: &ExitContext<'_>) -> Option<Failure> {
        crate::worker::classify::classify(cx)
    }

    /// No streaming stdin: every brain turn is a fresh `exec resume <thread>`.
    fn brain_transport(&self) -> BrainTransport {
        BrainTransport::ResumePerTurn
    }
}

fn sandbox(spec: &LaunchSpec) -> String {
    if spec.isolation == IsolationMode::ReadOnly {
        return READ_ONLY_SANDBOX.to_owned();
    }
    spec.sandbox.clone()
}

fn mcp_config_args(mcp: &McpAttach) -> [String; 2] {
    let args = serde_json::to_string(&mcp.args).unwrap_or_else(|_| "[]".to_owned());
    let name = crate::mcp::server::SERVER_NAME;
    [
        format!("mcp_servers.{name}.command=\"{}\"", mcp.command),
        format!("mcp_servers.{name}.args={args}"),
    ]
}

/// Codex reports no cost and no quota telemetry: `Cost` stays `None` and is estimated
/// from the `[pricing]` table, if any, one layer up.
fn terminal(
    st: &mut ParseState,
    ok: bool,
    subtype: &str,
    text: Option<String>,
    usage: Usage,
) -> WorkerEvent {
    st.usage = usage;
    let f = FinalSummary {
        ok,
        subtype: subtype.to_owned(),
        text,
        usage,
        cost: None,
        api_error_status: None,
        num_turns: st.last_final.as_ref().map_or(0, |p| p.num_turns) + 1,
        permission_denials: 0,
        denied_tools: Vec::new(),
    };
    st.last_final = Some(f.clone());
    WorkerEvent::Final(f)
}

fn item_events(item: CodexItem, st: &mut ParseState, completed: bool) -> ParseOutput {
    let mut out: SmallVec<[WorkerEvent; 4]> = SmallVec::new();
    match item {
        // Partial text arrives on item.updated; only the completed item is durable.
        CodexItem::AgentMessage { text, .. } if completed => {
            out.push(WorkerEvent::AssistantText { text })
        }
        CodexItem::Reasoning { text, .. } if completed => out.push(WorkerEvent::Thinking { text }),
        CodexItem::AgentMessage { .. } | CodexItem::Reasoning { .. } => {}
        CodexItem::CommandExecution {
            id,
            command,
            exit_code,
            status,
            aggregated_output,
        } => {
            st.tool_names.insert(id.clone(), "shell".to_owned());
            out.push(WorkerEvent::ToolCall {
                id: id.clone(),
                name: "shell".into(),
                summary: truncate(&command, SUMMARY_MAX),
            });
            if completed {
                out.push(WorkerEvent::ToolResult {
                    id,
                    ok: exit_code.unwrap_or(0) == 0,
                    summary: status.unwrap_or_default(),
                    detail: aggregated_output
                        .map(|o| truncate(o.trim_end(), RESULT_MAX))
                        .filter(|o| !o.is_empty()),
                });
            }
        }
        CodexItem::FileChange { changes, .. } if completed => {
            for c in changes {
                let kind = change_kind(&c.kind);
                let path = camino::Utf8PathBuf::from(c.path);
                st.files.push(FileChange {
                    path: path.clone(),
                    kind,
                    added: 0,
                    removed: 0,
                    source: EvidenceSource::EventStream,
                });
                out.push(WorkerEvent::FileChanged { path, kind });
            }
        }
        CodexItem::FileChange { .. } => {}
        CodexItem::McpToolCall { id, server, tool } => {
            let name = format!("{server}__{tool}");
            st.tool_names.insert(id.clone(), name.clone());
            out.push(WorkerEvent::ToolCall {
                id,
                name,
                summary: String::new(),
            });
        }
        CodexItem::WebSearch { id, query } => out.push(WorkerEvent::ToolCall {
            id,
            name: "web_search".into(),
            summary: truncate(&query, SUMMARY_MAX),
        }),
        CodexItem::TodoList { .. } | CodexItem::Other => {}
    }
    ParseOutput::many(out)
}

fn change_kind(k: &str) -> ChangeKind {
    match k {
        "add" => ChangeKind::Add,
        "delete" => ChangeKind::Delete,
        "rename" => ChangeKind::Rename,
        _ => ChangeKind::Modify,
    }
}

/// Codex reports `input_tokens` as the WHOLE prompt with `cached_input_tokens` inside it;
/// everything downstream uses Anthropic semantics, where the two are disjoint.
fn usage_of(u: &CodexUsage) -> Usage {
    Usage {
        input_tokens: u.input_tokens.saturating_sub(u.cached_input_tokens),
        cached_input_tokens: u.cached_input_tokens,
        cache_write_tokens: u.cache_write_input_tokens,
        output_tokens: u.output_tokens,
        reasoning_tokens: u.reasoning_output_tokens,
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum CodexLine {
    #[serde(rename = "thread.started")]
    ThreadStarted {
        #[serde(default)]
        thread_id: String,
    },
    #[serde(rename = "turn.started")]
    TurnStarted {},
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        #[serde(default)]
        usage: CodexUsage,
    },
    #[serde(rename = "turn.failed")]
    TurnFailed { error: CodexError },
    #[serde(rename = "item.started")]
    ItemStarted { item: CodexItem },
    #[serde(rename = "item.updated")]
    ItemUpdated { item: CodexItem },
    #[serde(rename = "item.completed")]
    ItemCompleted { item: CodexItem },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        message: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cached_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    reasoning_output_tokens: u64,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CodexError {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
enum CodexItem {
    AgentMessage {
        #[serde(default)]
        id: String,
        #[serde(default)]
        text: String,
    },
    Reasoning {
        #[serde(default)]
        id: String,
        #[serde(default)]
        text: String,
    },
    CommandExecution {
        #[serde(default)]
        id: String,
        #[serde(default)]
        command: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        aggregated_output: Option<String>,
    },
    FileChange {
        #[serde(default)]
        id: String,
        #[serde(default)]
        changes: Vec<CodexChange>,
    },
    McpToolCall {
        #[serde(default)]
        id: String,
        #[serde(default)]
        server: String,
        #[serde(default)]
        tool: String,
    },
    WebSearch {
        #[serde(default)]
        id: String,
        #[serde(default)]
        query: String,
    },
    TodoList {
        #[serde(default)]
        id: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct CodexChange {
    #[serde(default)]
    path: String,
    #[serde(default)]
    kind: String,
}
