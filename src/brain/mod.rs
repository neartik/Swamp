pub mod claude;
pub mod codex;
pub mod prompt;

pub use prompt::{BrainMode, system_prompt};

use crate::config::Config;
use crate::dispatch::Lease;
use crate::ids::{NodeId, NodeIds};
use crate::journal::paths::RunPaths;
use crate::journal::{JournalEvent, JournalHandle};
use crate::mcp;
use crate::model::core::{
    AccountId, Cost, NodeKind, NodeState, Provider, SessionHandle, Tier, Usage, WorkspaceRef,
};
use crate::model::event::WorkerEvent;
use crate::model::node::NodeRecord;
use crate::model::result::IsolationMode;
use crate::worker::adapter::{
    BrainTransport, Capability, LaunchSpec, McpAttach, ParseState, ProviderAdapter, SessionPlan,
    adapter_for,
};
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use parking_lot::Mutex;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// The only transport that works on a subscription alone. `api` needs a key and a feature.
const CLI_TRANSPORT: &str = "cli";
const EVENT_QUEUE: usize = 512;
const DEFAULT_TURN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

pub enum BrainEvent {
    Ready { session: String, model: String },
    Text { delta: String },
    Thinking { delta: String },
    ToolCall { id: String, name: String, preview: String },
    ToolDone { id: String, name: String, ok: bool, detail: Option<String> },
    TurnDone { usage: Usage, cost: Option<Cost> },
    Fatal { message: String },
}

#[async_trait]
pub trait Brain: Send {
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn send(&mut self, text: &str) -> anyhow::Result<()>;
    fn events(&mut self) -> &mut mpsc::Receiver<BrainEvent>;
    async fn interrupt(&mut self) -> anyhow::Result<()>;
    async fn shutdown(self: Box<Self>) -> anyhow::Result<()>;
    fn session(&self) -> Option<&SessionHandle>;
}

pub fn build(
    cfg: &Config,
    lease: Lease,
    paths: &RunPaths,
    socket: &Utf8Path,
    journal: JournalHandle,
    resume: Option<SessionHandle>,
    mode: BrainMode,
) -> anyhow::Result<Box<dyn Brain>> {
    let transport = cfg.brain.transport.as_deref().unwrap_or(CLI_TRANSPORT);
    anyhow::ensure!(
        transport == CLI_TRANSPORT,
        "brain.transport = \"{transport}\" is not available: the CLI brain is the only one that \
         runs on a subscription, and no other transport is compiled in"
    );

    let provider = cfg
        .account(&lease.account)
        .map(|a| a.provider)
        .or(cfg.brain.provider)
        .unwrap_or(Provider::Anthropic);
    let tier = cfg.brain.tier.unwrap_or(Tier::High);
    let model = cfg.model_for(provider, tier, Some(&lease.account))?;
    let adapter = adapter_for(provider);

    // The brain is the run's root node: everything it dispatches hangs off this id.
    let node = NodeIds {
        id: NodeId(paths.run.0),
        session_uuid: uuid::Uuid::new_v4(),
    };
    let cwd = repo_root(paths);
    let session = match resume {
        Some(h) => SessionPlan::Resume(h),
        None => SessionPlan::New {
            preassigned: adapter
                .supports(Capability::PreassignedSession)
                .then(|| node.session_uuid.to_string()),
        },
    };

    let spec = LaunchSpec {
        node,
        provider,
        exec: lease.exec.clone(),
        env: lease.env.clone(),
        model: model.clone(),
        tier,
        cwd: cwd.clone(),
        // The brain plans and reads; workers write.
        isolation: IsolationMode::ReadOnly,
        session,
        kind: NodeKind::Brain,
        permission_mode: cfg.brain.permission_mode.clone().unwrap_or_default(),
        sandbox: String::new(),
        budget_usd: cfg.node_budget_usd(tier),
        append_system_prompt: Some(append_system_prompt(cfg, &cwd, mode)),
        allow_tools: cfg.brain.allow_tools.clone(),
        deny_tools: cfg.brain.deny_tools.clone(),
        mcp: Some(McpAttach {
            command: mcp::exe(),
            args: mcp::bridge_args(socket),
        }),
        last_message_path: paths.last_message(node.id),
        extra_args: Vec::new(),
        extra: cfg.tier_extra(provider, tier),
        partial_messages: cfg.brain.include_partial_messages.unwrap_or(false),
        attempt: 1,
    };

    let launch = Launch {
        cfg: Arc::new(cfg.clone()),
        adapter,
        spec,
        journal,
        account: lease.account.clone(),
        model,
        turn_timeout: cfg
            .limits
            .brain_turn_timeout
            .unwrap_or(DEFAULT_TURN_TIMEOUT),
        session: Arc::new(Mutex::new(None)),
        totals: Arc::new(Mutex::new(Totals::default())),
        lease,
    };
    match launch.adapter.brain_transport() {
        BrainTransport::Persistent => Ok(Box::new(claude::ClaudeBrain::new(launch))),
        BrainTransport::ResumePerTurn => Ok(Box::new(codex::CodexBrain::new(launch))),
    }
}

/// Everything both transports need. The lease rides along: dropping the brain returns the
/// account to the pool.
pub struct Launch {
    pub cfg: Arc<Config>,
    pub adapter: Arc<dyn ProviderAdapter>,
    pub spec: LaunchSpec,
    pub journal: JournalHandle,
    pub account: AccountId,
    pub model: String,
    pub turn_timeout: Duration,
    pub session: Arc<Mutex<Option<SessionHandle>>>,
    pub totals: Arc<Mutex<Totals>>,
    pub lease: Lease,
}

/// What the brain has spent so far, across every turn of the session.
#[derive(Debug, Default, Clone, Copy)]
pub struct Totals {
    pub usage: Usage,
    pub usd: f64,
    pub basis: Option<crate::model::core::CostBasis>,
    /// How much of `usd` has already reached the account pool.
    pub credited_usd: f64,
}

impl Totals {
    pub fn cost(&self) -> Option<Cost> {
        self.basis.map(|basis| Cost {
            usd: self.usd,
            basis,
        })
    }
}

impl Launch {
    pub fn node(&self) -> NodeId {
        self.spec.node.id
    }

    /// The handle Swamp can journal before the process exists, when the provider lets Swamp
    /// choose the session id. Codex mints its own, so this is None there.
    pub fn planned_session(&self) -> Option<SessionHandle> {
        match &self.spec.session {
            SessionPlan::Resume(h) => Some(h.clone()),
            SessionPlan::New { preassigned } => preassigned.clone().map(|id| SessionHandle {
                account: self.account.clone(),
                id,
                preassigned: true,
            }),
        }
    }

    pub fn latest_session(&self) -> Option<SessionHandle> {
        self.session.lock().clone()
    }

    /// Hands this turn's spend to the pool. The node itself is counted once, at shutdown.
    pub fn credit_turn(&self) {
        if let Some(cost) = self.uncredited() {
            self.lease.pool().credit(&self.account, cost);
        }
    }

    fn uncredited(&self) -> Option<Cost> {
        let mut t = self.totals.lock();
        let usd = t.usd - t.credited_usd;
        t.credited_usd = t.usd;
        t.basis.map(|basis| Cost { usd, basis })
    }

    /// One NodeSpawned so the brain shows up in the run tree like any other node.
    pub async fn journal_spawn(&self) -> anyhow::Result<()> {
        let record = NodeRecord {
            id: self.node(),
            run_id: self.journal.run(),
            parent: None,
            logical: self.node(),
            attempt: 1,
            retry_of: None,
            kind: NodeKind::Brain,
            title: "brain".to_owned(),
            prompt_path: self.journal.paths().prompt(self.node()),
            prompt_sha256: String::new(),
            provider: self.spec.provider,
            account: Some(self.account.clone()),
            exec: Some(self.spec.exec.clone()),
            argv: Vec::new(),
            model: Some(self.model.clone()),
            tier: self.spec.tier,
            workspace: WorkspaceRef::ReadOnly {
                path: self.spec.cwd.clone(),
            },
            session: self.planned_session(),
            state: NodeState::Queued,
            created_at: OffsetDateTime::now_utc(),
            started_at: None,
            ended_at: None,
            usage: Usage::default(),
            cost: None,
            exit: None,
            files: Vec::new(),
            work: None,
            summary: None,
            stream_offset: 0,
            unparsed_lines: 0,
        };
        self.journal
            .emit_durable(
                Some(self.node()),
                JournalEvent::NodeSpawned {
                    node: Box::new(record),
                },
            )
            .await?;
        Ok(())
    }

    /// Durable on purpose: a brain that crashes after launch is only resumable if the id
    /// reached the disk before the process did.
    pub async fn journal_session(&self, handle: SessionHandle) -> anyhow::Result<()> {
        *self.session.lock() = Some(handle.clone());
        self.journal
            .emit_durable(
                Some(self.node()),
                JournalEvent::SessionBound { session: handle },
            )
            .await?;
        Ok(())
    }

    pub fn spawn(&self, argv: &[std::ffi::OsString], stdin: Stdio) -> anyhow::Result<Child> {
        let (program, rest) = argv
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("adapter produced an empty argv"))?;
        let mut cmd = Command::new(program);
        cmd.args(rest)
            .current_dir(&self.spec.cwd)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in self.adapter.env(&self.spec) {
            cmd.env(k, v);
        }
        let child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start the brain `{}`: {e}", self.spec.exec))?;
        if let Some(pid) = child.id() {
            self.journal.emit(
                Some(self.node()),
                JournalEvent::ProcessStarted {
                    pid: pid as i32,
                    pgid: pid as i32,
                    argv: argv
                        .iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect(),
                    env_overrides: self.spec.env.clone(),
                    cwd: self.spec.cwd.clone(),
                },
            );
        }
        Ok(child)
    }
}

/// How far the raw stream got, and whether the provider's terminal event ever arrived.
pub(crate) struct Driven {
    pub offset: u64,
    pub finished: bool,
}

/// Reads one process's stdout to EOF: mirrors raw lines into the run tree, journals every
/// normalized event, and forwards what the chat UI needs.
pub(crate) async fn drive<R: AsyncRead + Unpin>(
    launch: &Launch,
    tx: &mpsc::Sender<BrainEvent>,
    out: R,
    from_offset: u64,
) -> Driven {
    let node = launch.node();
    let mut state = ParseState::default();
    let mut offset = from_offset;
    let mut finished = false;
    let mut raw = open_append(&launch.journal.paths().stream(node)).await;
    let mut lines = BufReader::new(out).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(file) = raw.as_mut() {
            let _ = file.write_all(line.as_bytes()).await;
            let _ = file.write_all(b"\n").await;
        }
        let parsed = launch.adapter.parse_line(&line, &mut state);
        for event in parsed.events {
            launch.journal.emit(
                Some(node),
                JournalEvent::NodeEvent {
                    offset,
                    event: event.clone(),
                },
            );
            if let WorkerEvent::SessionStarted { session, .. } = &event {
                let handle = SessionHandle {
                    account: launch.account.clone(),
                    id: session.clone(),
                    preassigned: matches!(&launch.spec.session, SessionPlan::New { preassigned } if preassigned.is_some()),
                };
                if launch.latest_session().as_ref() != Some(&handle)
                    && let Err(e) = launch.journal_session(handle).await
                {
                    tracing::warn!("cannot journal the brain session: {e}");
                }
            }
            if let WorkerEvent::RateLimit(snap) = &event {
                launch
                    .lease
                    .pool()
                    .observe_quota(&launch.account, snap.clone());
            }
            if let WorkerEvent::Final(f) = &event {
                finished = true;
                let cost = f
                    .cost
                    .or_else(|| launch.cfg.estimate_cost(&launch.model, &f.usage));
                let totals = {
                    let mut t = launch.totals.lock();
                    t.usage.absorb(&f.usage);
                    if let Some(c) = cost {
                        t.usd += c.usd;
                        t.basis = Some(c.basis);
                    }
                    *t
                };
                launch.journal.emit(
                    Some(node),
                    JournalEvent::NodeUsage {
                        usage: totals.usage,
                        cost: totals.cost(),
                    },
                );
                // Per turn, so a chat that runs for hours is visible to selection long
                // before it shuts down.
                launch.credit_turn();
            }
            if let Some(out) = brain_event(launch, &event)
                && tx.send(out).await.is_err()
            {
                return Driven { offset, finished };
            }
        }
        offset += line.len() as u64 + 1;
    }
    Driven { offset, finished }
}

fn brain_event(launch: &Launch, event: &WorkerEvent) -> Option<BrainEvent> {
    match event {
        WorkerEvent::SessionStarted { session, model, .. } => Some(BrainEvent::Ready {
            session: session.clone(),
            model: model.clone().unwrap_or_else(|| launch.model.clone()),
        }),
        WorkerEvent::AssistantText { text } => Some(BrainEvent::Text {
            delta: text.clone(),
        }),
        WorkerEvent::Thinking { text } => Some(BrainEvent::Thinking {
            delta: text.clone(),
        }),
        WorkerEvent::ToolCall { id, name, summary } => Some(BrainEvent::ToolCall {
            id: id.clone(),
            name: name.clone(),
            preview: summary.clone(),
        }),
        // `summary` on a result is the name the parser looked up from the call's id.
        WorkerEvent::ToolResult {
            id,
            ok,
            summary,
            detail,
        } => Some(BrainEvent::ToolDone {
            id: id.clone(),
            name: summary.clone(),
            ok: *ok,
            detail: detail.clone(),
        }),
        WorkerEvent::Final(f) if f.ok => Some(BrainEvent::TurnDone {
            usage: f.usage,
            cost: f
                .cost
                .or_else(|| launch.cfg.estimate_cost(&launch.model, &f.usage)),
        }),
        WorkerEvent::Final(f) => Some(BrainEvent::Fatal {
            message: f
                .text
                .clone()
                .unwrap_or_else(|| format!("the brain ended the turn with `{}`", f.subtype)),
        }),
        _ => None,
    }
}

/// Stderr is not part of the protocol, so it goes to the node's own log, never to stdout.
pub(crate) fn drain_stderr(launch: &Launch, err: tokio::process::ChildStderr) {
    let path = launch.journal.paths().stderr(launch.node());
    tokio::spawn(async move {
        let mut file = open_append(&path).await;
        let mut lines = BufReader::new(err).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match file.as_mut() {
                Some(f) => {
                    let _ = f.write_all(line.as_bytes()).await;
                    let _ = f.write_all(b"\n").await;
                }
                None => tracing::debug!("brain stderr: {line}"),
            }
        }
    });
}

pub(crate) async fn finish(launch: &Launch, state: NodeState) {
    let totals = *launch.totals.lock();
    let failure = match &state {
        NodeState::Failed { failure } => Some(failure.clone()),
        _ => None,
    };
    launch.journal.emit(
        Some(launch.node()),
        JournalEvent::NodeFinished {
            state,
            exit: None,
            usage: totals.usage,
            cost: totals.cost(),
            work: None,
            summary: None,
            files: Vec::new(),
            unparsed_lines: 0,
        },
    );
    // The brain is one node per run, and the pool has to see it: spend it does not know
    // about is spend it cannot route around.
    launch
        .lease
        .pool()
        .report(&launch.account, failure.as_ref(), launch.uncredited());
}

async fn open_append(path: &Utf8Path) -> Option<tokio::fs::File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok()?;
    }
    tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .ok()
}

/// `<repo>/.swamp/runs/<run>` is the only shape `Paths` builds, so the repo is three up.
fn repo_root(paths: &RunPaths) -> Utf8PathBuf {
    paths
        .dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(Utf8Path::to_path_buf)
        .unwrap_or_else(|| paths.dir.clone())
}

/// Swamp's own contract, plus the operator's brain file if there is one. DESIGN 10: Swamp
/// reads the file and passes its text, because the `-file` flag spellings are not documented.
fn append_system_prompt(cfg: &Config, repo: &Utf8Path, mode: BrainMode) -> String {
    let mut text = system_prompt(cfg, mode);
    let Some(file) = &cfg.brain.system_prompt_file else {
        return text;
    };
    let path = if file.is_absolute() {
        file.clone()
    } else {
        repo.join(file)
    };
    if let Ok(extra) = std::fs::read_to_string(&path)
        && !extra.trim().is_empty()
    {
        text.push_str("\n\n## Project instructions\n\n");
        text.push_str(extra.trim_end());
    }
    text
}

/// SIGTERM, then SIGKILL after the grace period: the brain owns a terminal, not a worktree.
pub(crate) async fn terminate(mut child: Child) -> anyhow::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    let _ = child.start_kill();
    match tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await {
        Ok(_) => Ok(()),
        Err(_) => {
            let _ = child.kill().await;
            Ok(())
        }
    }
}
