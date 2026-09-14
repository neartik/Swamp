pub mod account;
pub mod cooldown;
pub mod persist;
pub mod policy;
pub mod pool;
pub mod retry;

pub use account::{Account, AccountState, Health};
pub use policy::SelectionPolicy;
pub use pool::{AccountPool, Lease, NoCapacity};
pub use retry::{NodeCtx, NodeOutcome, NodeRunner, run_node};

use crate::config::Config;
use crate::ids::{NodeId, NodeIds};
use crate::journal::{JournalEvent, JournalHandle};
use crate::model::core::{NodeKind, Provider, Tier};
use crate::model::failure::Failure;
use crate::model::node::WorkResultRef;
use crate::model::result::{IsolationMode, NodeResult, TaskRequest};
use crate::worker::Executor;
use crate::worker::adapter::{LaunchSpec, SessionPlan};
use crate::workspace::{NodeWorktree, WorkspaceManager};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_ATTEMPTS: u32 = 3;
const DEFAULT_MAX_PARALLEL_DISPATCH: usize = 6;
const DEFAULT_MAX_HIGH_TIER: usize = 2;
const DEFAULT_MAX_NODES_PER_RUN: u32 = 32;
const DEFAULT_MAX_DEPTH: u32 = 2;

/// WP2's writer task owns durability; every producer only enqueues.
pub(crate) fn emit(h: &JournalHandle, node: Option<NodeId>, event: JournalEvent) {
    let _ = h.tx.send((node, event));
}

/// Owns the pool and the semaphores. One per run.
pub struct Dispatcher {
    pub cfg: Arc<Config>,
    pub pool: Arc<AccountPool>,
    pub exec: Arc<Executor>,
    pub workspace: Arc<WorkspaceManager>,
    pub journal: JournalHandle,
    runner: Arc<dyn NodeRunner>,
    /// limits.max_parallel_dispatch: queueing, not rejection.
    batch: Arc<Semaphore>,
    /// limits.max_high_tier_concurrent: the expensive tier gets its own ceiling.
    high_tier: Arc<Semaphore>,
    results: Mutex<HashMap<NodeId, NodeResult>>,
    cancels: Mutex<HashMap<NodeId, CancellationToken>>,
    depths: Mutex<HashMap<NodeId, u32>>,
    spawned: AtomicU32,
    settled: Notify,
}

impl Dispatcher {
    pub fn new(
        cfg: Arc<Config>,
        pool: Arc<AccountPool>,
        exec: Arc<Executor>,
        ws: Arc<WorkspaceManager>,
        journal: JournalHandle,
    ) -> Arc<Self> {
        let runner = Arc::new(ExecRunner {
            exec: Arc::clone(&exec),
            workspace: Arc::clone(&ws),
        });
        Self::with_runner(cfg, pool, exec, ws, journal, runner)
    }

    /// The seam WP4's tests drive: a scripted runner in place of real processes and worktrees.
    pub fn with_runner(
        cfg: Arc<Config>,
        pool: Arc<AccountPool>,
        exec: Arc<Executor>,
        ws: Arc<WorkspaceManager>,
        journal: JournalHandle,
        runner: Arc<dyn NodeRunner>,
    ) -> Arc<Self> {
        let batch = cfg
            .limits
            .max_parallel_dispatch
            .unwrap_or(DEFAULT_MAX_PARALLEL_DISPATCH)
            .max(1);
        let high = cfg
            .limits
            .max_high_tier_concurrent
            .unwrap_or(DEFAULT_MAX_HIGH_TIER)
            .max(1);
        Arc::new(Self {
            cfg,
            pool,
            exec,
            workspace: ws,
            journal,
            runner,
            batch: Arc::new(Semaphore::new(batch)),
            high_tier: Arc::new(Semaphore::new(high)),
            results: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            depths: Mutex::new(HashMap::new()),
            spawned: AtomicU32::new(0),
            settled: Notify::new(),
        })
    }

    pub async fn dispatch_batch(
        self: &Arc<Self>,
        parent: NodeId,
        tasks: Vec<TaskRequest>,
        max_wait: Duration,
    ) -> Vec<NodeResult> {
        let mut ids = Vec::with_capacity(tasks.len());
        let mut handles = Vec::with_capacity(tasks.len());
        for task in tasks {
            let id = NodeId::new();
            ids.push(id);
            let me = Arc::clone(self);
            handles.push(tokio::spawn(async move {
                me.dispatch_as(id, parent, task).await
            }));
        }
        // Never block forever: whatever is still running comes back marked "running".
        let _ = tokio::time::timeout(max_wait, futures::future::join_all(handles)).await;
        self.await_nodes(&ids, Some(Duration::ZERO)).await
    }

    pub async fn dispatch_one(self: &Arc<Self>, parent: NodeId, task: TaskRequest) -> NodeResult {
        self.dispatch_as(NodeId::new(), parent, task).await
    }

    pub async fn await_nodes(&self, ids: &[NodeId], timeout: Option<Duration>) -> Vec<NodeResult> {
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        loop {
            let pending = {
                let results = self.results.lock();
                ids.iter()
                    .filter(|id| results.get(id).is_none_or(|r| r.state == "running"))
                    .count()
            };
            if pending == 0 {
                break;
            }
            match deadline {
                Some(at) if tokio::time::Instant::now() >= at => break,
                Some(at) => {
                    tokio::select! {
                        _ = self.settled.notified() => {}
                        _ = tokio::time::sleep_until(at) => {}
                    }
                }
                None => self.settled.notified().await,
            }
        }
        let results = self.results.lock();
        ids.iter()
            .filter_map(|id| results.get(id).cloned())
            .collect()
    }

    pub async fn cancel(&self, id: NodeId) -> anyhow::Result<()> {
        let token = self.cancels.lock().get(&id).cloned();
        match token {
            Some(t) => {
                t.cancel();
                Ok(())
            }
            None => anyhow::bail!("no dispatched node {id}"),
        }
    }

    pub fn result(&self, id: NodeId) -> Option<NodeResult> {
        self.results.lock().get(&id).cloned()
    }

    pub fn pool(&self) -> &Arc<AccountPool> {
        &self.pool
    }

    async fn dispatch_as(
        self: &Arc<Self>,
        id: NodeId,
        parent: NodeId,
        task: TaskRequest,
    ) -> NodeResult {
        let tier = task
            .tier
            .or(self.cfg.dispatch.default_tier)
            .unwrap_or(Tier::Mid);
        let order = self.provider_order(&task, tier);
        let provider = order.first().copied().unwrap_or(Provider::Anthropic);

        if let Some(reason) = self.reject_reason(&task, parent) {
            return self.settle(finished(id, &task, tier, provider, None, Some(reason)));
        }

        let depth = self.depths.lock().get(&parent).copied().unwrap_or(0) + 1;
        self.depths.lock().insert(id, depth);
        self.results
            .lock()
            .insert(id, running(id, &task, tier, provider));

        let _batch = self.batch.clone().acquire_owned().await;
        let _high = match tier {
            Tier::High => Some(self.high_tier.clone().acquire_owned().await),
            _ => None,
        };

        let cancel = CancellationToken::new();
        self.cancels.lock().insert(id, cancel.clone());
        self.spawned.fetch_add(1, Ordering::Relaxed);

        let cx = NodeCtx {
            cfg: Arc::clone(&self.cfg),
            pool: Arc::clone(&self.pool),
            runner: Arc::clone(&self.runner),
            journal: self.journal.clone(),
            provider_order: order,
            cross_provider: self.cfg.dispatch.cross_provider_failover.unwrap_or(false),
            max_attempts: self
                .cfg
                .dispatch
                .max_attempts
                .unwrap_or(DEFAULT_MAX_ATTEMPTS)
                .max(1),
            deadline: tokio::time::Instant::now() + self.cfg.node_timeout(tier),
            parent: Some(parent),
            logical: id,
            cancel,
        };
        let spec = self.launch_spec(&task, tier, provider);
        let outcome = run_node(&cx, spec, &task).await;
        self.settle(from_outcome(id, &task, tier, provider, outcome))
    }

    /// Hard limits: refusing is the answer, not queueing.
    fn reject_reason(&self, task: &TaskRequest, parent: NodeId) -> Option<Failure> {
        if !task.deps.is_empty() {
            return Some(Failure::WorkerError {
                subtype: "unsupported".into(),
                detail: "TaskRequest.deps is not supported in v1: dispatch a batch of independent \
                         tasks and sequence them from the brain"
                    .into(),
            });
        }
        let max_depth = self.cfg.limits.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
        let depth = self.depths.lock().get(&parent).copied().unwrap_or(0) + 1;
        if depth > max_depth {
            return Some(Failure::WorkerError {
                subtype: "max_depth".into(),
                detail: format!("nesting depth {depth} exceeds limits.max_depth = {max_depth}"),
            });
        }
        let max_nodes = self
            .cfg
            .limits
            .max_nodes_per_run
            .unwrap_or(DEFAULT_MAX_NODES_PER_RUN);
        if self.spawned.load(Ordering::Relaxed) >= max_nodes {
            return Some(Failure::WorkerError {
                subtype: "max_nodes_per_run".into(),
                detail: format!("run already spawned limits.max_nodes_per_run = {max_nodes} nodes"),
            });
        }
        None
    }

    fn provider_order(&self, task: &TaskRequest, tier: Tier) -> Vec<Provider> {
        match task.provider {
            Some(p) => {
                let mut order = vec![p];
                order.extend(
                    self.cfg
                        .provider_order(tier)
                        .into_iter()
                        .filter(|q| *q != p),
                );
                order
            }
            None => {
                let order = self.cfg.provider_order(tier);
                if order.is_empty() {
                    vec![Provider::Anthropic]
                } else {
                    order
                }
            }
        }
    }

    fn launch_spec(&self, task: &TaskRequest, tier: Tier, provider: Provider) -> LaunchSpec {
        let worker = self
            .cfg
            .providers
            .get(&provider)
            .map(|p| p.worker.clone())
            .unwrap_or_default();
        LaunchSpec {
            node: NodeIds {
                id: NodeId::new(),
                session_uuid: uuid::Uuid::new_v4(),
            },
            provider,
            exec: String::new(),
            env: Default::default(),
            model: String::new(),
            tier,
            cwd: self.journal.paths.dir.clone(),
            isolation: task
                .isolation
                .or(self.cfg.workspace.isolation)
                .unwrap_or(IsolationMode::Worktree),
            session: SessionPlan::New { preassigned: None },
            kind: NodeKind::Worker,
            permission_mode: worker.permission_mode.clone().unwrap_or_default(),
            sandbox: worker.sandbox.clone().unwrap_or_default(),
            budget_usd: self.cfg.node_budget_usd(tier),
            append_system_prompt: None,
            allow_tools: Vec::new(),
            deny_tools: Vec::new(),
            mcp: None,
            last_message_path: self.journal.paths.dir.join("last-message.txt"),
            extra_args: worker.args.clone(),
            attempt: 1,
        }
    }

    fn settle(&self, result: NodeResult) -> NodeResult {
        self.results.lock().insert(result.node, result.clone());
        self.settled.notify_waiters();
        result
    }
}

fn running(id: NodeId, task: &TaskRequest, tier: Tier, provider: Provider) -> NodeResult {
    NodeResult {
        node: id,
        title: task.title.clone(),
        ok: false,
        state: "running",
        tier,
        provider,
        account: None,
        model: None,
        attempts: 0,
        summary: None,
        files: Vec::new(),
        branch: None,
        patch: None,
        insertions: 0,
        deletions: 0,
        usage: Default::default(),
        cost: None,
        duration_ms: 0,
        failure: None,
        permission_denials: 0,
    }
}

fn finished(
    id: NodeId,
    task: &TaskRequest,
    tier: Tier,
    provider: Provider,
    summary: Option<String>,
    failure: Option<Failure>,
) -> NodeResult {
    NodeResult {
        ok: failure.is_none(),
        state: if failure.is_none() {
            "succeeded"
        } else {
            "failed"
        },
        summary,
        failure,
        ..running(id, task, tier, provider)
    }
}

fn from_outcome(
    id: NodeId,
    task: &TaskRequest,
    tier: Tier,
    provider: Provider,
    outcome: NodeOutcome,
) -> NodeResult {
    let last = outcome.attempts.last();
    let work: Option<WorkResultRef> = last.and_then(|r| r.work.clone());
    let mut out = finished(
        id,
        task,
        last.map_or(tier, |r| r.tier),
        last.map_or(provider, |r| r.provider),
        outcome.outcome.as_ref().and_then(|o| o.summary.clone()),
        outcome.failure.clone(),
    );
    out.attempts = outcome.attempts.len() as u32;
    out.account = last.and_then(|r| r.account.clone());
    out.model = last.and_then(|r| r.model.clone());
    out.branch = work
        .as_ref()
        .map(|w| w.branch.clone())
        .or_else(|| last.and_then(branch_of));
    out.patch = work.as_ref().map(|w| w.patch.clone());
    out.insertions = work.as_ref().map_or(0, |w| w.insertions);
    out.deletions = work.as_ref().map_or(0, |w| w.deletions);
    if let Some(o) = outcome.outcome.as_ref() {
        out.usage = o.usage;
        out.cost = o.cost;
        out.files = o.files.clone();
        out.permission_denials = o.permission_denials;
    }
    out.duration_ms = last
        .and_then(|r| r.duration())
        .map_or(0, |d| d.as_millis() as u64);
    out
}

fn branch_of(r: &crate::model::node::NodeRecord) -> Option<String> {
    match &r.workspace {
        crate::model::core::WorkspaceRef::Worktree { branch, .. } => Some(branch.clone()),
        _ => None,
    }
}

/// The production runner: real worktrees, real detached processes.
struct ExecRunner {
    exec: Arc<Executor>,
    workspace: Arc<WorkspaceManager>,
}

#[async_trait]
impl NodeRunner for ExecRunner {
    async fn workspace(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree> {
        self.workspace.create(logical, attempt).await
    }
    async fn run(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<crate::worker::RunOutcome> {
        self.exec.run(spec, timeout, cancel).await
    }
    async fn finalize(
        &self,
        wt: &NodeWorktree,
        title: &str,
        tier: Tier,
    ) -> anyhow::Result<Option<WorkResultRef>> {
        self.workspace.finalize(wt, title, tier).await
    }
}
