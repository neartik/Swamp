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
use crate::ids::{CallSeq, DispatchId, NodeId, NodeIds};
use crate::journal::{JournalEvent, JournalHandle, RunView};
use crate::model::core::{Cost, CostBasis, NodeKind, NodeState, Provider, Tier};
use crate::model::dispatch::{DispatchCounts, DispatchRecord, TaskRef, settled_state};
use crate::model::failure::Failure;
use crate::model::node::WorkResultRef;
use crate::model::result::{IsolationMode, NodeResult, TaskRequest};
use crate::worker::Executor;
use crate::worker::adapter::{LaunchSpec, SessionPlan};
use crate::workspace::{NodeWorktree, WorkspaceManager};
use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const DEFAULT_MAX_ATTEMPTS: u32 = 3;
const DEFAULT_MAX_NODES_PER_RUN: u32 = 32;
const DEFAULT_MAX_DEPTH: u32 = 2;

pub(crate) fn emit(h: &JournalHandle, node: Option<NodeId>, event: JournalEvent) {
    h.emit(node, event);
}

pub struct DispatchRequest {
    pub id: DispatchId,
    pub caller: NodeId,
    pub call_seq: Option<CallSeq>,
    pub tasks: Vec<TaskRequest>,
    pub wait: bool,
    /// How long the call blocks for results. None blocks until every task settles.
    pub max_wait: Option<Duration>,
}

impl DispatchRequest {
    pub fn new(caller: NodeId, tasks: Vec<TaskRequest>) -> Self {
        DispatchRequest {
            id: DispatchId::new(),
            caller,
            call_seq: None,
            tasks,
            wait: true,
            max_wait: None,
        }
    }
}

pub struct Dispatched {
    pub id: DispatchId,
    /// In request order; `node` is the task's logical id.
    pub results: Vec<NodeResult>,
}

struct TaskEnd {
    state: NodeState,
    /// Every attempt's, where the result carries the last one's.
    cost: Option<Cost>,
}

/// Owns the pool and the semaphores. One per run.
pub struct Dispatcher {
    pub cfg: Arc<Config>,
    pub pool: Arc<AccountPool>,
    pub exec: Arc<Executor>,
    pub workspace: Arc<WorkspaceManager>,
    pub journal: JournalHandle,
    runner: Arc<dyn NodeRunner>,
    results: Mutex<HashMap<NodeId, NodeResult>>,
    cancels: Mutex<HashMap<NodeId, CancellationToken>>,
    depths: Mutex<HashMap<NodeId, u32>>,
    /// SWAMP_DEPTH of this process: a nested swamp starts counting where its parent left off.
    base_depth: AtomicU32,
    spawned: AtomicU32,
    call_seq: AtomicU64,
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
        Arc::new(Self {
            cfg,
            pool,
            exec,
            workspace: ws,
            journal,
            runner,
            results: Mutex::new(HashMap::new()),
            cancels: Mutex::new(HashMap::new()),
            depths: Mutex::new(HashMap::new()),
            base_depth: AtomicU32::new(0),
            spawned: AtomicU32::new(0),
            call_seq: AtomicU64::new(0),
            settled: Notify::new(),
        })
    }

    /// Continues a run's depths, node budget and tool call sequence from its journal.
    pub fn seed(&self, view: &RunView) {
        {
            let mut depths = self.depths.lock();
            for t in view.tasks.values() {
                if let Some(d) = t.depth {
                    depths.insert(t.logical, d);
                }
            }
            for n in view.nodes.values().filter(|n| n.dispatch.is_some()) {
                depths.insert(n.id, n.depth);
            }
        }
        let spawned = view
            .tasks
            .keys()
            .filter(|t| !matches!(view.state_of(**t), Some(NodeState::Rejected { .. })))
            .count() as u32;
        self.spawned.fetch_max(spawned, Ordering::Relaxed);
        if let Some(seq) = view.call_seq {
            self.call_seq.fetch_max(seq.0, Ordering::Relaxed);
        }
    }

    pub fn next_call_seq(&self) -> CallSeq {
        CallSeq(self.call_seq.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// Every task starts at once; each then queues on its own account's capacity.
    pub async fn dispatch(self: &Arc<Self>, req: DispatchRequest) -> Dispatched {
        let planned: Vec<(NodeId, Tier, Vec<Provider>, TaskRequest)> = req
            .tasks
            .into_iter()
            .map(|task| {
                let tier = self.tier_of(&task);
                (NodeId::new(), tier, self.provider_order(&task, tier), task)
            })
            .collect();
        let record = DispatchRecord {
            id: req.id,
            run: self.journal.run(),
            caller: req.caller,
            call_seq: req.call_seq,
            wait: req.wait,
            max_wait_s: req.max_wait.map(|d| d.as_secs()),
            tasks: planned
                .iter()
                .map(|(logical, tier, order, task)| TaskRef {
                    logical: *logical,
                    title: task.title.clone(),
                    tier: *tier,
                    provider: primary_provider(order),
                })
                .collect(),
            at: time::OffsetDateTime::now_utc(),
        };
        journal_issued(&self.journal, record).await;

        let ids: Vec<NodeId> = planned.iter().map(|(logical, ..)| *logical).collect();
        // Every id answers, even one whose task has not been polled when the wait ends.
        {
            let mut results = self.results.lock();
            for (logical, tier, order, task) in &planned {
                results.insert(
                    *logical,
                    running(*logical, task, *tier, primary_provider(order)),
                );
            }
        }
        let handles: Vec<_> = planned
            .into_iter()
            .map(|(logical, tier, order, task)| {
                let me = Arc::clone(self);
                let (dispatch, caller) = (req.id, req.caller);
                tokio::spawn(async move {
                    me.dispatch_as(logical, dispatch, caller, tier, order, task)
                        .await
                })
            })
            .collect();
        let settler = {
            let me = Arc::clone(self);
            let (dispatch, caller) = (req.id, req.caller);
            tokio::spawn(async move {
                let ends: Vec<TaskEnd> = futures::future::join_all(handles)
                    .await
                    .into_iter()
                    .filter_map(Result::ok)
                    .collect();
                me.settle_dispatch(dispatch, caller, &ends).await;
            })
        };
        // Never block forever: whatever is still running comes back marked "running".
        match req.max_wait {
            Some(max) => {
                let _ = tokio::time::timeout(max, settler).await;
            }
            None => {
                let _ = settler.await;
            }
        }
        Dispatched {
            id: req.id,
            results: self.await_nodes(&ids, Some(Duration::ZERO)).await,
        }
    }

    pub async fn dispatch_batch(
        self: &Arc<Self>,
        parent: NodeId,
        tasks: Vec<TaskRequest>,
        max_wait: Duration,
    ) -> Vec<NodeResult> {
        let req = DispatchRequest {
            max_wait: Some(max_wait),
            ..DispatchRequest::new(parent, tasks)
        };
        self.dispatch(req).await.results
    }

    pub async fn dispatch_one(self: &Arc<Self>, parent: NodeId, task: TaskRequest) -> NodeResult {
        self.dispatch(DispatchRequest::new(parent, vec![task]))
            .await
            .results
            .pop()
            .expect("a dispatch that blocks until settled has every result")
    }

    pub async fn await_nodes(&self, ids: &[NodeId], timeout: Option<Duration>) -> Vec<NodeResult> {
        let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
        loop {
            // Registered BEFORE the pending count is read: `settle` notifies only waiters that
            // have already subscribed, and a result landing in that window would be missed.
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
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
                        _ = &mut settled => {}
                        _ = tokio::time::sleep_until(at) => {}
                    }
                }
                None => settled.await,
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

    pub fn set_base_depth(&self, depth: u32) {
        self.base_depth.store(depth, Ordering::Relaxed);
    }

    /// Every node still running, for `esc esc` and shutdown. Settled nodes are dropped rather
    /// than cancelled, so the count is the number of workers actually stopped and a later
    /// dispatch on the same run is untouched.
    pub fn cancel_all(&self) -> usize {
        // The brain is the run's own root: cancelling it would end the session, not a worker.
        let brain = NodeId(self.journal.run().0);
        let settled: HashSet<NodeId> = {
            let results = self.results.lock();
            results
                .iter()
                .filter(|(_, r)| r.state != "running")
                .map(|(id, _)| *id)
                .collect()
        };
        let mut n = 0;
        self.cancels.lock().retain(|id, token| {
            if *id == brain {
                return true;
            }
            if settled.contains(id) {
                return false;
            }
            if !token.is_cancelled() {
                token.cancel();
                n += 1;
            }
            true
        });
        n
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
        dispatch: DispatchId,
        parent: NodeId,
        tier: Tier,
        order: Vec<Provider>,
        task: TaskRequest,
    ) -> TaskEnd {
        let provider = primary_provider(&order);

        if let Some(reason) = self.reject_reason(&task, parent) {
            self.settle(finished(
                id,
                &task,
                tier,
                provider,
                None,
                Some(reason.clone()),
            ));
            if let Err(e) = self
                .journal
                .emit_durable(
                    Some(id),
                    JournalEvent::DispatchRejected {
                        dispatch,
                        logical: id,
                        reason: reason.clone(),
                    },
                )
                .await
            {
                tracing::warn!(node = %id.short(), "cannot journal DispatchRejected: {e}");
            }
            return TaskEnd {
                state: NodeState::Rejected { reason },
                cost: None,
            };
        }

        let depth = self.depth_of(parent) + 1;
        self.depths.lock().insert(id, depth);

        let cancel = CancellationToken::new();
        self.cancels.lock().insert(id, cancel.clone());

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
            dispatch,
            depth,
            cancel,
        };
        let spec = self.launch_spec(&task, tier, provider);
        let outcome = run_node(&cx, spec, &task).await;
        let cost = total_cost(outcome.attempts.iter().map(|a| a.cost));
        let state = settled_state(outcome.failure.as_ref());
        self.settle(from_outcome(id, &task, tier, provider, outcome));
        TaskEnd { state, cost }
    }

    async fn settle_dispatch(&self, dispatch: DispatchId, caller: NodeId, ends: &[TaskEnd]) {
        let mut counts = DispatchCounts::default();
        for end in ends {
            counts.count(&end.state);
        }
        let cost = total_cost(ends.iter().map(|e| e.cost));
        journal_settled(&self.journal, dispatch, caller, counts, cost).await;
    }

    fn tier_of(&self, task: &TaskRequest) -> Tier {
        task.tier
            .or(self.cfg.dispatch.default_tier)
            .unwrap_or(Tier::Mid)
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
        let depth = self.depth_of(parent) + 1;
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
        // Reserved atomically: two tasks of one batch must not share the last slot.
        if self
            .spawned
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < max_nodes).then_some(n + 1)
            })
            .is_err()
        {
            return Some(Failure::WorkerError {
                subtype: "max_nodes_per_run".into(),
                detail: format!("run already spawned limits.max_nodes_per_run = {max_nodes} nodes"),
            });
        }
        None
    }

    fn depth_of(&self, parent: NodeId) -> u32 {
        self.depths
            .lock()
            .get(&parent)
            .copied()
            .unwrap_or_else(|| self.base_depth.load(Ordering::Relaxed))
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
        let isolation = task
            .isolation
            .or(self.cfg.workspace.isolation)
            .unwrap_or(IsolationMode::Worktree);
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
            isolation,
            session: SessionPlan::New { preassigned: None },
            kind: NodeKind::Worker,
            permission_mode: worker.permission_mode.clone().unwrap_or_default(),
            sandbox: worker.sandbox.clone().unwrap_or_default(),
            append_system_prompt: None,
            allow_tools: worker.allow_tools.clone(),
            deny_tools: worker.deny_tools.clone(),
            mcp: None,
            last_message_path: self.journal.paths.dir.join("last-message.txt"),
            extra_args: worker.args_for(isolation),
            extra: self.cfg.tier_extra(provider, tier),
            partial_messages: false,
            attempt: 1,
        }
    }

    fn settle(&self, result: NodeResult) {
        self.results.lock().insert(result.node, result);
        self.settled.notify_waiters();
    }
}

/// Durable, and before any task of the dispatch starts.
pub(crate) async fn journal_issued(journal: &JournalHandle, record: DispatchRecord) {
    let (id, caller) = (record.id, record.caller);
    let event = JournalEvent::DispatchIssued {
        record: Box::new(record),
    };
    if let Err(e) = journal.emit_durable(Some(caller), event).await {
        tracing::warn!("cannot journal DispatchIssued {id}: {e}");
    }
}

pub(crate) async fn journal_settled(
    journal: &JournalHandle,
    dispatch: DispatchId,
    caller: NodeId,
    counts: DispatchCounts,
    cost: Option<Cost>,
) {
    let event = JournalEvent::DispatchSettled {
        dispatch,
        counts,
        cost,
    };
    if let Err(e) = journal.emit_durable(Some(caller), event).await {
        tracing::warn!("cannot journal DispatchSettled {dispatch}: {e}");
    }
}

pub(crate) fn primary_provider(order: &[Provider]) -> Provider {
    order.first().copied().unwrap_or(Provider::Anthropic)
}

/// Known costs summed; estimated as soon as any part is. None when nothing reported one.
pub(crate) fn total_cost(costs: impl Iterator<Item = Option<Cost>>) -> Option<Cost> {
    costs.flatten().reduce(|a, b| Cost {
        usd: a.usd + b.usd,
        basis: if a.basis == CostBasis::Reported && b.basis == CostBasis::Reported {
            CostBasis::Reported
        } else {
            CostBasis::Estimated
        },
    })
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
        state: match &failure {
            None => "succeeded",
            Some(Failure::Cancelled { .. }) => "cancelled",
            Some(_) => "failed",
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
    // A worker that never announced an edit still left a patch; git is what the brain gets.
    if out.files.is_empty()
        && let Some(r) = last
    {
        out.files = r.files.clone();
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
