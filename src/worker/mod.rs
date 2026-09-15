pub mod adapter;
pub mod classify;
pub mod claude;
pub mod codex;
pub mod codex_quota;
pub mod follow;
pub mod liveness;
pub mod prompt;
pub mod spawn;

pub use adapter::{
    BrainTransport, Capability, ExitContext, LaunchSpec, McpAttach, ParseOutput, ParseState,
    ProviderAdapter, SessionPlan, adapter_for,
};
pub use spawn::{Detached, NodeIo};

use crate::config::{Config, FailurePatterns};
use crate::ids::NodeId;
use crate::journal::raw::{RawSink, Redactor};
use crate::journal::{JournalEvent, JournalHandle};
use crate::model::core::{
    AccountId, Cost, FileChange, FinalSummary, Provider, RateLimitSnapshot, SessionHandle, Usage,
};
use crate::model::event::WorkerEvent;
use crate::model::failure::Failure;
use crate::model::node::ExitInfo;
use crate::worker::follow::{POLL, follow};
use crate::worker::liveness::wait_exit;
use crate::worker::spawn::{Reaper, spawn_detached, terminate};
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Live telemetry, while the worker still runs: usage and quota reach the account pool from
/// the same consumer loop that journals, instead of only once the node is terminal.
pub trait EventObserver: Send + Sync + 'static {
    fn on_event(&self, event: &WorkerEvent);
}

/// Registered per node rather than passed down `NodeRunner::run`, whose implementors live in
/// packages this one does not own.
pub fn observe_node(node: NodeId, observer: Arc<dyn EventObserver>) -> ObserverGuard {
    observers().lock().insert(node, observer);
    ObserverGuard(node)
}

pub fn observer_for(node: NodeId) -> Option<Arc<dyn EventObserver>> {
    observers().lock().get(&node).cloned()
}

/// Deregisters on drop, so a panicking node cannot leak its observer.
pub struct ObserverGuard(NodeId);

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        observers().lock().remove(&self.0);
    }
}

fn observers() -> &'static Mutex<BTreeMap<NodeId, Arc<dyn EventObserver>>> {
    static OBSERVERS: OnceLock<Mutex<BTreeMap<NodeId, Arc<dyn EventObserver>>>> = OnceLock::new();
    OBSERVERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

const STDERR_TAIL: usize = 64;
const DEFAULT_GRACE: Duration = Duration::from_secs(5);
const EVENT_QUEUE: usize = 256;

pub struct RunOutcome {
    pub failure: Option<Failure>,
    pub exit: Option<ExitInfo>,
    pub session: Option<SessionHandle>,
    pub usage: Usage,
    /// What the ACCOUNT spent: `usage` is the main model alone, and a side-call is billed to
    /// the same subscription.
    pub account_usage: Usage,
    pub cost: Option<Cost>,
    pub summary: Option<String>,
    pub files: Vec<FileChange>,
    pub rate_limit: Option<RateLimitSnapshot>,
    pub stream_offset: u64,
    pub unparsed_lines: u32,
    pub permission_denials: u32,
}

/// What the ACCOUNT spent over a finished run. `usage` is the main model alone; every
/// side-call `modelUsage` reports was billed to the same subscription.
pub fn account_total(st: &ParseState) -> Usage {
    st.last_final
        .as_ref()
        .map_or(st.usage, FinalSummary::account_usage)
}

pub struct Executor {
    pub journal: JournalHandle,
    pub cfg: Arc<Config>,
}

/// Everything `execute` needs that is not policy. Paths and the journal are explicit so the
/// supervision loop can be exercised without a live run directory.
pub struct ExecReq<'a> {
    pub adapter: Arc<dyn ProviderAdapter>,
    pub spec: &'a LaunchSpec,
    pub io: &'a NodeIo,
    pub patterns: &'a FailurePatterns,
    pub sink: &'a mut RawSink,
    pub journal: Option<&'a JournalHandle>,
    /// `Some((pid, offset))` adopts a process that is already running.
    pub resume: Option<(i32, u64)>,
    pub timeout: Duration,
    pub grace: Duration,
    /// journal.max_line_bytes: a base64 blob on one line must truncate, not OOM.
    pub max_line: usize,
    pub cancel: CancellationToken,
    /// Fed every parsed event as it is journaled.
    pub observer: Option<Arc<dyn EventObserver>>,
}

impl Executor {
    pub fn new(journal: JournalHandle, cfg: Arc<Config>) -> Self {
        Self { journal, cfg }
    }

    pub fn adapter(&self, p: Provider) -> Arc<dyn ProviderAdapter> {
        adapter_for(p)
    }

    /// Spawns detached, journals ProcessStarted, follows to the terminal event, classifies.
    pub async fn run(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        self.drive(spec, None, timeout, cancel).await
    }

    pub async fn resume_from(
        &self,
        spec: &LaunchSpec,
        pid: i32,
        offset: u64,
    ) -> anyhow::Result<RunOutcome> {
        self.drive(
            spec,
            Some((pid, offset)),
            self.cfg.node_timeout(spec.tier),
            CancellationToken::new(),
        )
        .await
    }

    async fn drive(
        &self,
        spec: &LaunchSpec,
        resume: Option<(i32, u64)>,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        let paths = self.journal.paths();
        let node = spec.node.id;
        let io = NodeIo {
            node,
            prompt: paths.prompt(node),
            stdout: paths.stream(node),
            stderr: paths.stderr(node),
            pidfile: paths.pidfile(node),
            depth: depth_from_env(),
        };
        let patterns = self.cfg.failure_patterns(spec.provider)?;
        let redact = Arc::new(Redactor::new(&self.cfg.journal.redact)?);
        let mut sink = RawSink::open(paths, node, redact).await?;

        let (extra_args, refused) =
            adapter::gate_unsafe_args(&spec.extra_args, self.cfg.limits.unsafe_ack == Some(true));
        if !refused.is_empty() {
            tracing::error!(node = %node, args = ?refused, "dropping unsafe args: limits.unsafe_ack is not set");
        }
        let mut spec = spec.clone();
        spec.extra_args = extra_args;

        let mut outcome = execute(ExecReq {
            adapter: self.adapter(spec.provider),
            spec: &spec,
            io: &io,
            patterns: &patterns,
            sink: &mut sink,
            journal: Some(&self.journal),
            resume,
            timeout,
            grace: self.cfg.limits.grace_period.unwrap_or(DEFAULT_GRACE),
            max_line: self
                .cfg
                .journal
                .max_line_bytes
                .unwrap_or(crate::worker::classify::MAX_LINE),
            cancel,
            observer: observer_for(spec.node.id),
        })
        .await?;
        sink.flush().await?;
        if outcome.cost.is_none() {
            outcome.cost = self.cfg.estimate_cost(&spec.model, &outcome.usage);
        }
        Ok(outcome)
    }
}

/// The supervision loop: spawn (or adopt), tail the raw stream to EOF, classify what is left.
/// Timeout and cancellation work by killing the process group; the follower then drains the
/// file and stops on its own, so no event is lost to a cancelled future.
pub async fn execute(req: ExecReq<'_>) -> anyhow::Result<RunOutcome> {
    let ExecReq {
        adapter,
        spec,
        io,
        patterns,
        sink,
        journal,
        resume,
        timeout,
        grace,
        max_line,
        cancel,
        observer,
    } = req;

    let (pid, pgid, offset) = match resume {
        Some((pid, offset)) => (pid, pid, offset),
        None => {
            let argv = adapter.build_argv(spec)?;
            let env = adapter.env(spec);
            let d = spawn_detached(&argv, &env, &spec.cwd, io)?;
            if let Some(j) = journal {
                j.emit(
                    Some(io.node),
                    JournalEvent::ProcessStarted {
                        pid: d.pid,
                        pgid: d.pgid,
                        argv: argv
                            .iter()
                            .map(|a| a.to_string_lossy().into_owned())
                            .collect(),
                        env_overrides: spec.env.clone(),
                        cwd: spec.cwd.clone(),
                    },
                );
            }
            (d.pid, d.pgid, 0)
        }
    };

    let alive_flag = Arc::new(AtomicBool::new(true));
    let deadline_hit = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();

    let waiter = tokio::spawn({
        let alive_flag = alive_flag.clone();
        async move {
            let exit = wait_exit(pid, POLL).await;
            alive_flag.store(false, Ordering::SeqCst);
            let _ = done_tx.send(());
            exit
        }
    });
    let supervisor = tokio::spawn({
        let deadline_hit = deadline_hit.clone();
        async move {
            tokio::select! {
                _ = done_rx => {}
                _ = tokio::time::sleep(timeout) => {
                    deadline_hit.store(true, Ordering::SeqCst);
                    let _ = terminate(pgid, grace, Reaper::Elsewhere).await;
                }
                _ = cancel.cancelled() => {
                    let _ = terminate(pgid, grace, Reaper::Elsewhere).await;
                }
            }
        }
    });

    let (tx, mut rx) = mpsc::channel(EVENT_QUEUE);
    let consumer = tokio::spawn({
        let journal = journal.cloned();
        async move {
            while let Some((node, event, offset)) = rx.recv().await {
                if let Some(o) = &observer {
                    o.on_event(&event);
                }
                if let Some(j) = &journal {
                    j.emit(Some(node), JournalEvent::NodeEvent { offset, event });
                }
            }
        }
    });

    let mut st = ParseState::default();
    let alive: Arc<dyn Fn() -> bool + Send + Sync> = {
        let flag = alive_flag.clone();
        Arc::new(move || flag.load(Ordering::SeqCst))
    };
    let stream_offset = follow(
        io.node,
        &io.stdout,
        offset,
        adapter.clone(),
        &mut st,
        sink,
        tx,
        alive,
        max_line,
    )
    .await?;
    let _ = consumer.await;
    let exit = waiter.await.ok().flatten();
    // NOT aborted: the direct child can die on SIGTERM while a grandchild in the same group
    // does not, and aborting here would drop `terminate` before it ever sends SIGKILL.
    let _ = supervisor.await;

    st.stderr_tail = stderr_tail(&io.stderr);
    let failure = adapter
        .classify(&ExitContext {
            exit,
            state: &st,
            patterns,
            deadline_hit: deadline_hit.load(Ordering::SeqCst),
        })
        .map(|f| refine(f, timeout, stream_offset));

    let summary = st
        .last_final
        .as_ref()
        .and_then(|f| f.text.clone())
        .or_else(|| last_message(spec));
    Ok(RunOutcome {
        failure,
        exit,
        session: st.session.clone().map(|id| session_handle(spec, id)),
        usage: st.usage,
        account_usage: account_total(&st),
        cost: st.last_final.as_ref().and_then(|f| f.cost),
        summary,
        files: st.files.clone(),
        rate_limit: st.last_rate_limit.clone(),
        stream_offset,
        unparsed_lines: st.unparsed,
        permission_denials: st.last_final.map_or(0, |f| f.permission_denials),
    })
}

/// The classifier works from the stream alone; these three numbers come from the launch.
fn refine(f: Failure, timeout: Duration, offset: u64) -> Failure {
    match f {
        Failure::Timeout { .. } => Failure::Timeout {
            after_s: timeout.as_secs(),
        },
        Failure::Truncated { .. } => Failure::Truncated { offset },
        other => other,
    }
}

/// A session handle is only usable together with the account that minted it, and the launch
/// spec does not name one: dispatch stamps the leasing account onto this before journaling.
fn session_handle(spec: &LaunchSpec, id: String) -> SessionHandle {
    match &spec.session {
        SessionPlan::Resume(h) => SessionHandle {
            account: h.account.clone(),
            id,
            preassigned: h.preassigned,
        },
        SessionPlan::New { preassigned } => SessionHandle {
            account: AccountId(String::new()),
            id,
            preassigned: preassigned.is_some(),
        },
    }
}

/// Codex writes its own final answer to `-o <file>`, which is parse-independent.
fn last_message(spec: &LaunchSpec) -> Option<String> {
    let text = std::fs::read_to_string(&spec.last_message_path).ok()?;
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn stderr_tail(path: &camino::Utf8Path) -> std::collections::VecDeque<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return std::collections::VecDeque::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .take(STDERR_TAIL)
        .rev()
        .map(|l| (*l).to_owned())
        .collect()
}

fn depth_from_env() -> u32 {
    std::env::var("SWAMP_DEPTH")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0)
}
