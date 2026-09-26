use crate::dispatch::account::AccountState;
use crate::ids::{CallSeq, DispatchId, NodeId, RunId};
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{AccountId, Cost, NodeKind, NodeState, Tier, Usage, WorkspaceRef};
use crate::model::dispatch::{DispatchCounts, DispatchRecord, DispatchState, NodeTransition};
use crate::model::event::WorkerEvent;
use crate::model::node::{ExitInfo, NodeRecord, WorkResultRef};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::{BTreeMap, BTreeSet};
use time::OffsetDateTime;

pub trait Projection {
    type Out;
    fn apply(&mut self, l: &JournalLine);
    fn finish(self) -> Self::Out;
}

#[derive(Debug, Clone)]
pub struct RunHeader {
    pub run: RunId,
    pub swamp_version: String,
    pub schema: u32,
    pub argv: Vec<String>,
    pub cwd: Utf8PathBuf,
    pub repo: Option<Utf8PathBuf>,
    pub base: Option<String>,
    pub config_sha256: String,
    pub task: Option<String>,
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Default)]
pub struct RunView {
    pub header: Option<RunHeader>,
    pub nodes: BTreeMap<NodeId, NodeRecord>,
    pub children: BTreeMap<NodeId, Vec<NodeId>>,
    pub roots: Vec<NodeId>,
    /// Attempt chains, collapsed in the tree view.
    pub by_logical: BTreeMap<NodeId, Vec<NodeId>>,
    pub accounts: BTreeMap<AccountId, AccountState>,
    /// Every dispatch, plus `DispatchId::LEGACY` for nodes journaled without one.
    pub dispatches: BTreeMap<DispatchId, DispatchView>,
    /// Logical tasks, including ones still queued or rejected that have no attempt yet.
    pub tasks: BTreeMap<NodeId, TaskView>,
    pub transitions: BTreeMap<NodeId, Vec<NodeTransition>>,
    pub exited: BTreeSet<NodeId>,
    pub call_seq: Option<CallSeq>,
    /// Only when `with_events`.
    pub events: BTreeMap<NodeId, Vec<WorkerEvent>>,
    pub totals: Usage,
    pub cost_usd: f64,
    /// False if any node's cost is unknown.
    pub cost_complete: bool,
    pub last_seq: u64,
    pub finished: bool,
    /// Set by `load`; without it `NodeEvent` payloads are folded but not retained.
    pub with_events: bool,
    seen: bool,
}

/// One collapsed row: a logical node plus every attempt that served it.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub logical: NodeId,
    pub depth: u32,
    pub title: String,
    pub state: NodeState,
    pub attempts: Vec<NodeId>,
}

#[derive(Debug, Clone)]
pub struct DispatchView {
    pub id: DispatchId,
    /// None for the legacy bucket.
    pub record: Option<DispatchRecord>,
    pub state: DispatchState,
    pub tasks: Vec<NodeId>,
    pub counts: Option<DispatchCounts>,
    pub cost: Option<Cost>,
}

impl DispatchView {
    fn new(id: DispatchId, record: Option<DispatchRecord>) -> Self {
        let tasks = record
            .as_ref()
            .map(|r| r.tasks.iter().map(|t| t.logical).collect())
            .unwrap_or_default();
        DispatchView {
            id,
            record,
            state: DispatchState::Open,
            tasks,
            counts: None,
            cost: None,
        }
    }

    fn add(&mut self, logical: NodeId) {
        if !self.tasks.contains(&logical) {
            self.tasks.push(logical);
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskView {
    pub logical: NodeId,
    pub dispatch: DispatchId,
    pub parent: Option<NodeId>,
    pub title: String,
    pub tier: Tier,
    /// None in schema 1, which did not journal depth.
    pub depth: Option<u32>,
    /// None in schema 1, where the latest attempt is the task's state.
    pub state: Option<NodeState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Dispatch(DispatchId),
    /// A logical id, or any attempt of it.
    Task(NodeId),
    Subtree(NodeId),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Totals {
    pub nodes: u32,
    pub failed: u32,
    pub rejected: u32,
    pub usage: Usage,
    pub cost_usd: f64,
    pub cost_complete: bool,
}

impl RunView {
    /// Pure and idempotent: replaying the same prefix always yields the same state.
    pub fn apply(&mut self, l: &JournalLine) {
        if self.seen && l.seq <= self.last_seq {
            return;
        }
        self.seen = true;
        self.last_seq = self.last_seq.max(l.seq);
        self.fold(l);
        self.recompute();
    }

    fn fold(&mut self, l: &JournalLine) {
        match &l.event {
            JournalEvent::RunStarted {
                swamp_version,
                schema,
                argv,
                cwd,
                repo,
                base,
                config_sha256,
                task,
            } => {
                // `swamp resume` appends a second RunStarted to the same journal; the run's
                // origin is the first one, task, base and start time included.
                if self.header.is_some() {
                    return;
                }
                self.header = Some(RunHeader {
                    run: l.run,
                    swamp_version: swamp_version.clone(),
                    schema: *schema,
                    argv: argv.clone(),
                    cwd: cwd.clone(),
                    repo: repo.clone(),
                    base: base.clone(),
                    config_sha256: config_sha256.clone(),
                    task: task.clone(),
                    started_at: l.at,
                });
            }
            JournalEvent::NodeSpawned { node } => {
                self.link_task(node);
                self.spawn((**node).clone());
            }
            JournalEvent::AccountSelected { account, exec, .. } => {
                self.accounts.entry(account.clone()).or_default();
                if let Some(n) = self.node_mut(l) {
                    n.account = Some(account.clone());
                    n.exec = Some(exec.clone());
                }
            }
            JournalEvent::ModelResolved { tier, model, .. } => {
                if let Some(n) = self.node_mut(l) {
                    n.tier = *tier;
                    n.model = Some(model.clone());
                }
            }
            JournalEvent::ProcessStarted {
                pid, pgid, argv, ..
            } => {
                let at = l.at;
                if let Some(n) = self.node_mut(l) {
                    n.argv = argv.clone();
                    n.started_at = Some(at);
                    n.state = NodeState::Running {
                        pid: *pid,
                        pgid: *pgid,
                        since: at,
                    };
                }
            }
            JournalEvent::SessionBound { session } => {
                if let Some(n) = self.node_mut(l) {
                    n.session = Some(session.clone());
                }
            }
            JournalEvent::NodeEvent { offset, event } => {
                let with_events = self.with_events;
                let node = l.node;
                if let Some(n) = self.node_mut(l) {
                    n.stream_offset = n.stream_offset.max(*offset);
                }
                if with_events && let Some(id) = node {
                    self.events.entry(id).or_default().push(event.clone());
                }
            }
            JournalEvent::NodeUsage { usage, cost } => {
                if let Some(n) = self.node_mut(l) {
                    n.usage = *usage;
                    if cost.is_some() {
                        n.cost = *cost;
                    }
                }
            }
            JournalEvent::NodeFiles { files } => {
                if let Some(n) = self.node_mut(l) {
                    n.files = files.clone();
                }
            }
            JournalEvent::NodeBlocked { until, why, .. } => {
                let blocked = NodeState::Blocked {
                    until: *until,
                    why: why.clone(),
                };
                if let Some(t) = self.tracked_task_mut(l.node) {
                    t.state = Some(blocked.clone());
                }
                if let Some(n) = self.node_mut(l) {
                    n.state = blocked;
                }
            }
            JournalEvent::NodeRetry { .. } | JournalEvent::ProviderSwitch { .. } => {}
            JournalEvent::WorktreeCreated { path, branch, base } => {
                if let Some(n) = self.node_mut(l) {
                    n.workspace = WorkspaceRef::Worktree {
                        path: path.clone(),
                        branch: branch.clone(),
                        base: base.clone(),
                    };
                }
            }
            JournalEvent::DiffCaptured {
                patch,
                head,
                files,
                insertions,
                deletions,
            } => {
                if let Some(n) = self.node_mut(l) {
                    let branch = match &n.workspace {
                        WorkspaceRef::Worktree { branch, .. } => branch.clone(),
                        _ => String::new(),
                    };
                    n.work = Some(WorkResultRef {
                        head: head.clone(),
                        branch,
                        patch: patch.clone(),
                        insertions: *insertions,
                        deletions: *deletions,
                        empty: *files == 0,
                        files: Vec::new(),
                    });
                }
            }
            JournalEvent::NodeFinished {
                state,
                exit,
                usage,
                cost,
                work,
                summary,
                files,
                unparsed_lines,
            } => {
                let at = l.at;
                if let Some(n) = self.node_mut(l) {
                    n.state = state.clone();
                    n.exit = *exit;
                    n.usage = *usage;
                    if cost.is_some() {
                        n.cost = *cost;
                    }
                    if work.is_some() {
                        n.work = work.clone();
                    }
                    if summary.is_some() {
                        n.summary = summary.clone();
                    }
                    if !files.is_empty() {
                        n.files = files.clone();
                    }
                    n.unparsed_lines = *unparsed_lines;
                    n.ended_at = Some(at);
                }
            }
            JournalEvent::AccountHealth {
                account,
                health,
                cooldown_until,
                quota,
                quota_observed_at,
                quota_source,
            } => {
                let s = self.accounts.entry(account.clone()).or_default();
                s.health = *health;
                s.cooldown_until = *cooldown_until;
                s.quota = quota.clone();
                s.quota_observed_at = *quota_observed_at;
                s.quota_source = *quota_source;
            }
            JournalEvent::AccountUsage {
                account,
                window,
                lifetime,
                window_key,
                rolled,
                source,
            } => {
                let at = l.at;
                let s = self.accounts.entry(account.clone()).or_default();
                s.window_tokens = *window;
                s.lifetime_tokens = *lifetime;
                s.window_key = window_key.clone();
                if *rolled {
                    s.window_started_at = Some(at);
                }
                if source.is_some() {
                    s.quota_source = *source;
                }
            }
            JournalEvent::BrainToolCall { call_seq, .. } => self.saw_call(*call_seq),
            JournalEvent::DispatchIssued { record } => {
                self.saw_call(record.call_seq);
                for t in &record.tasks {
                    self.tasks.entry(t.logical).or_insert_with(|| TaskView {
                        logical: t.logical,
                        dispatch: record.id,
                        parent: Some(record.caller),
                        title: t.title.clone(),
                        tier: t.tier,
                        depth: None,
                        state: Some(NodeState::Queued),
                    });
                }
                self.dispatches
                    .entry(record.id)
                    .or_insert_with(|| DispatchView::new(record.id, Some((**record).clone())));
            }
            JournalEvent::TaskQueued {
                logical,
                dispatch,
                title,
                tier,
                depth,
            } => {
                let d = *dispatch;
                let caller = self.dispatch_view(d).record.as_ref().map(|r| r.caller);
                self.dispatch_view(d).add(*logical);
                let t = self.tasks.entry(*logical).or_insert_with(|| TaskView {
                    logical: *logical,
                    dispatch: d,
                    parent: caller,
                    title: title.clone(),
                    tier: *tier,
                    depth: None,
                    state: None,
                });
                t.title = title.clone();
                t.tier = *tier;
                t.depth = Some(*depth);
                t.state.get_or_insert(NodeState::Queued);
            }
            JournalEvent::DispatchRejected {
                dispatch,
                logical,
                reason,
            } => {
                let record = self
                    .dispatches
                    .get(dispatch)
                    .and_then(|d| d.record.as_ref());
                let caller = record.map(|r| r.caller);
                let issued = record
                    .and_then(|r| r.tasks.iter().find(|t| t.logical == *logical))
                    .map(|t| (t.title.clone(), t.tier));
                self.dispatch_view(*dispatch).add(*logical);
                let t = self.tasks.entry(*logical).or_insert_with(|| {
                    let (title, tier) = issued.unwrap_or((String::new(), Tier::Mid));
                    TaskView {
                        logical: *logical,
                        dispatch: *dispatch,
                        parent: caller,
                        title,
                        tier,
                        depth: None,
                        state: None,
                    }
                });
                t.state = Some(NodeState::Rejected {
                    reason: reason.clone(),
                });
            }
            JournalEvent::NodeStateChanged { from, to, why } => {
                let Some(id) = l.node else { return };
                self.transitions
                    .entry(id)
                    .or_default()
                    .push(NodeTransition {
                        from: *from,
                        to: to.clone(),
                        why: why.clone(),
                    });
                // An attempt's state comes from its payload events; a task's from this.
                if !self.nodes.contains_key(&id)
                    && let Some(t) = self.tracked_task_mut(Some(id))
                {
                    t.state = Some(to.clone());
                }
            }
            JournalEvent::ProcessExited { code, signal } => {
                let at = l.at;
                if let Some(id) = l.node {
                    self.exited.insert(id);
                }
                if let Some(n) = self.node_mut(l)
                    && n.exit.is_none()
                {
                    let ran = n.started_at.and_then(|s| (at - s).try_into().ok());
                    n.exit = Some(ExitInfo {
                        code: *code,
                        signal: *signal,
                        duration_ms: ran.map_or(0, |d: std::time::Duration| d.as_millis() as u64),
                    });
                }
            }
            JournalEvent::DispatchSettled {
                dispatch,
                counts,
                cost,
            } => {
                let d = self.dispatch_view(*dispatch);
                d.state = DispatchState::Settled;
                d.counts = Some(*counts);
                d.cost = *cost;
            }
            JournalEvent::BrainTurn { .. }
            | JournalEvent::Note { .. }
            | JournalEvent::Adopted { .. } => {}
            JournalEvent::RunFinished { .. } => self.finished = true,
        }
    }

    fn spawn(&mut self, node: NodeRecord) {
        let id = node.id;
        match node.parent {
            Some(parent) => {
                let kids = self.children.entry(parent).or_default();
                if !kids.contains(&id) {
                    kids.push(id);
                }
            }
            None => {
                if !self.roots.contains(&id) {
                    self.roots.push(id);
                }
            }
        }
        let chain = self.by_logical.entry(node.logical).or_default();
        if !chain.contains(&id) {
            chain.push(id);
        }
        self.nodes.insert(id, node);
    }

    fn node_mut(&mut self, l: &JournalLine) -> Option<&mut NodeRecord> {
        self.nodes.get_mut(&l.node?)
    }

    /// A task whose state is journaled; a schema-1 task only mirrors its attempts.
    fn tracked_task_mut(&mut self, id: Option<NodeId>) -> Option<&mut TaskView> {
        self.tasks.get_mut(&id?).filter(|t| t.state.is_some())
    }

    fn dispatch_view(&mut self, id: DispatchId) -> &mut DispatchView {
        self.dispatches
            .entry(id)
            .or_insert_with(|| DispatchView::new(id, None))
    }

    fn saw_call(&mut self, seq: Option<CallSeq>) {
        self.call_seq = self.call_seq.max(seq);
    }

    /// A worker node without a dispatch lands in the legacy bucket.
    fn link_task(&mut self, node: &NodeRecord) {
        if node.kind == NodeKind::Brain {
            return;
        }
        let d = node.dispatch.unwrap_or(DispatchId::LEGACY);
        self.dispatch_view(d).add(node.logical);
        self.tasks.entry(node.logical).or_insert_with(|| TaskView {
            logical: node.logical,
            dispatch: d,
            parent: node.parent,
            title: node.title.clone(),
            tier: node.tier,
            depth: node.dispatch.map(|_| node.depth),
            state: None,
        });
    }

    /// Totals are derived, never accumulated, so any prefix folds to the same numbers.
    pub fn recompute(&mut self) {
        let mut totals = Usage::default();
        let mut cost = 0.0;
        let mut complete = true;
        for n in self.nodes.values() {
            totals.absorb(&n.usage);
            match n.cost {
                Some(c) => cost += c.usd,
                None => complete = false,
            }
        }
        self.totals = totals;
        self.cost_usd = cost;
        self.cost_complete = complete;
        // The legacy bucket has no settle event of its own: it closes with the run.
        let finished = self.finished;
        if let Some(d) = self.dispatches.get_mut(&DispatchId::LEGACY) {
            d.state = if finished {
                DispatchState::Settled
            } else {
                DispatchState::Open
            };
        }
    }

    pub fn load(dir: &Utf8Path, with_events: bool) -> anyhow::Result<Self> {
        let journal = if dir.is_file() {
            dir.to_path_buf()
        } else {
            dir.join("journal.jsonl")
        };
        let view = RunView {
            with_events,
            ..RunView::default()
        };
        crate::journal::reader::replay(&journal, view)
    }

    /// Running nodes with no ProcessExited, no NodeFinished and a dead pid become Orphaned.
    pub fn mark_orphans(&mut self, alive: &dyn Fn(NodeId) -> bool) {
        for (id, n) in self.nodes.iter_mut() {
            if let NodeState::Running { pid, .. } = n.state
                && !self.exited.contains(id)
                && !alive(*id)
            {
                n.state = NodeState::Orphaned {
                    pid,
                    stream_offset: n.stream_offset,
                };
            }
        }
    }

    pub fn tree(&self) -> Vec<TreeRow> {
        let (roots, children) = self.logical_tree();
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for r in roots {
            self.walk(r, 0, &children, &mut seen, &mut out);
        }
        out
    }

    /// Roots and children by logical id; a task with no attempt yet hangs off its caller.
    fn logical_tree(&self) -> (Vec<NodeId>, BTreeMap<NodeId, Vec<NodeId>>) {
        let mut children: BTreeMap<NodeId, Vec<NodeId>> = BTreeMap::new();
        for (parent, kids) in &self.children {
            let entry = children.entry(self.logical_of(*parent)).or_default();
            for kid in kids {
                let logical = self.logical_of(*kid);
                if !entry.contains(&logical) {
                    entry.push(logical);
                }
            }
        }
        let mut roots = Vec::new();
        for r in &self.roots {
            let logical = self.logical_of(*r);
            if !roots.contains(&logical) {
                roots.push(logical);
            }
        }
        for t in self.tasks.values() {
            if self.by_logical.contains_key(&t.logical) {
                continue;
            }
            let siblings = match t.parent.filter(|p| self.nodes.contains_key(p)) {
                Some(p) => children.entry(self.logical_of(p)).or_default(),
                None => &mut roots,
            };
            if !siblings.contains(&t.logical) {
                siblings.push(t.logical);
            }
        }
        (roots, children)
    }

    fn walk(
        &self,
        logical: NodeId,
        depth: u32,
        children: &BTreeMap<NodeId, Vec<NodeId>>,
        seen: &mut BTreeSet<NodeId>,
        out: &mut Vec<TreeRow>,
    ) {
        if !seen.insert(logical) {
            return;
        }
        if let Some(row) = self.row(logical, depth) {
            out.push(row);
        }
        if let Some(kids) = children.get(&logical) {
            for kid in kids {
                self.walk(*kid, depth + 1, children, seen, out);
            }
        }
    }

    fn row(&self, logical: NodeId, depth: u32) -> Option<TreeRow> {
        let attempts = self.by_logical.get(&logical).cloned().unwrap_or_default();
        let latest = attempts.iter().rev().find_map(|a| self.nodes.get(a));
        let title = match (latest, self.tasks.get(&logical)) {
            (Some(n), _) => n.title.clone(),
            (None, Some(t)) => t.title.clone(),
            (None, None) => return None,
        };
        Some(TreeRow {
            logical,
            depth,
            title,
            state: self.state_of(logical)?,
            attempts,
        })
    }

    /// The latest attempt's state while it is live, else the task's journaled one.
    pub fn state_of(&self, id: NodeId) -> Option<NodeState> {
        let logical = self.logical_of(id);
        let latest = self
            .by_logical
            .get(&logical)
            .and_then(|a| a.iter().rev().find_map(|a| self.nodes.get(a)));
        match (
            self.tasks.get(&logical).and_then(|t| t.state.as_ref()),
            latest,
        ) {
            (Some(task), _) if task.is_terminal() => Some(task.clone()),
            (_, Some(n)) if !n.state.is_terminal() => Some(n.state.clone()),
            (Some(task), _) => Some(task.clone()),
            (None, Some(n)) => Some(n.state.clone()),
            (None, None) => None,
        }
    }

    /// Every attempt that served a task, oldest first. Takes the logical id or any attempt's.
    pub fn attempts(&self, id: NodeId) -> Vec<&NodeRecord> {
        self.by_logical
            .get(&self.logical_of(id))
            .map(|ids| ids.iter().filter_map(|a| self.nodes.get(a)).collect())
            .unwrap_or_default()
    }

    pub fn rollup(&self, scope: Scope) -> Totals {
        let logicals: Vec<NodeId> = match scope {
            Scope::Dispatch(id) => self
                .dispatches
                .get(&id)
                .map(|d| d.tasks.clone())
                .unwrap_or_default(),
            Scope::Task(id) => vec![self.logical_of(id)],
            Scope::Subtree(id) => {
                let (_, children) = self.logical_tree();
                let mut out = Vec::new();
                let mut stack = vec![self.logical_of(id)];
                while let Some(l) = stack.pop() {
                    if out.contains(&l) {
                        continue;
                    }
                    out.push(l);
                    stack.extend(children.get(&l).into_iter().flatten().rev());
                }
                out
            }
        };
        let mut t = Totals {
            cost_complete: true,
            ..Totals::default()
        };
        for logical in logicals {
            match self.state_of(logical) {
                None => continue,
                Some(NodeState::Rejected { .. }) => t.rejected += 1,
                Some(NodeState::Failed { .. }) => {
                    t.nodes += 1;
                    t.failed += 1;
                }
                Some(_) => t.nodes += 1,
            }
            for n in self.attempts(logical) {
                t.usage.absorb(&n.usage);
                match n.cost {
                    Some(c) => t.cost_usd += c.usd,
                    None => t.cost_complete = false,
                }
            }
        }
        t
    }

    fn logical_of(&self, id: NodeId) -> NodeId {
        self.nodes.get(&id).map_or(id, |n| n.logical)
    }

    pub fn totals(&self) -> Totals {
        let rows = self.tree();
        let rejected = rows
            .iter()
            .filter(|r| matches!(r.state, NodeState::Rejected { .. }))
            .count() as u32;
        Totals {
            nodes: rows.len() as u32 - rejected,
            failed: rows
                .iter()
                .filter(|r| matches!(r.state, NodeState::Failed { .. }))
                .count() as u32,
            rejected,
            usage: self.totals,
            cost_usd: self.cost_usd,
            cost_complete: self.cost_complete,
        }
    }
}

impl Projection for RunView {
    type Out = RunView;
    fn apply(&mut self, l: &JournalLine) {
        RunView::apply(self, l);
    }
    fn finish(self) -> RunView {
        self
    }
}

/// Byte-budgeted compact rendering for the brain's swamp_status tool.
/// Same fold, different output.
pub struct LlmDigest {
    pub max_bytes: usize,
    view: RunView,
    paths: Option<crate::journal::paths::RunPaths>,
}

impl LlmDigest {
    pub fn new(max_bytes: usize) -> Self {
        LlmDigest {
            max_bytes,
            view: RunView::default(),
            paths: None,
        }
    }

    /// Without the run directory a node whose process is gone still reads as `running`.
    pub fn with_paths(mut self, paths: crate::journal::paths::RunPaths) -> Self {
        self.paths = Some(paths);
        self
    }
}

impl Projection for LlmDigest {
    type Out = String;

    fn apply(&mut self, l: &JournalLine) {
        self.view.apply(l);
    }

    fn finish(mut self) -> Self::Out {
        if let Some(paths) = self.paths.clone() {
            self.view
                .mark_orphans(&|id| crate::worker::liveness::is_ours(&paths.pidfile(id)));
        }
        let t = self.view.totals();
        let run = self
            .view
            .header
            .as_ref()
            .map(|h| h.run.short())
            .unwrap_or_else(|| "?".to_owned());
        let cost = if t.cost_complete {
            format!("~${:.2}", t.cost_usd)
        } else {
            format!("~${:.2}+", t.cost_usd)
        };
        let state = if self.view.finished {
            "finished"
        } else {
            "running"
        };
        let mut out = format!(
            "run {run} {state} nodes {} failed {} in {} out {} cost {cost}\n",
            t.nodes,
            t.failed,
            tokens(t.usage.input_tokens),
            tokens(t.usage.output_tokens),
        );

        let rows = self.view.tree();
        let (failed, rest): (Vec<&TreeRow>, Vec<&TreeRow>) = rows
            .iter()
            .partition(|r| matches!(r.state, NodeState::Failed { .. }));
        let mut skipped = 0usize;
        // Failures are the point of the digest: they are emitted before anything optional.
        for r in failed.iter().chain(rest.iter()) {
            let line = self.line(r);
            if out.len() + line.len() <= self.max_bytes {
                out.push_str(&line);
            } else {
                skipped += 1;
            }
        }
        if skipped > 0 {
            let more = format!("+{skipped} more\n");
            if out.len() + more.len() <= self.max_bytes {
                out.push_str(&more);
            }
        }
        while out.len() > self.max_bytes {
            out.pop();
        }
        out
    }
}

impl LlmDigest {
    fn line(&self, r: &TreeRow) -> String {
        let mark = match &r.state {
            NodeState::Failed { failure } => format!("FAIL {}", failure.kind()),
            NodeState::Succeeded => "ok".to_owned(),
            NodeState::Cancelled { .. } => "cancelled".to_owned(),
            NodeState::Running { .. } => "running".to_owned(),
            NodeState::Orphaned { .. } => "orphaned".to_owned(),
            NodeState::Blocked { .. } => "blocked".to_owned(),
            NodeState::Leased { .. } => "leased".to_owned(),
            NodeState::Queued => "queued".to_owned(),
            NodeState::Rejected { reason } => format!("REJECTED {}", reason.kind()),
        };
        let indent = "  ".repeat(r.depth as usize);
        let attempts = r.attempts.len();
        let title = clip(&r.title, 60);
        if attempts > 1 {
            format!(
                "{indent}{} {title} [{mark}] x{attempts}\n",
                r.logical.short()
            )
        } else {
            format!("{indent}{} {title} [{mark}]\n", r.logical.short())
        }
    }
}

fn clip(s: &str, max: usize) -> String {
    let flat = s.replace(['\n', '\r'], " ");
    if flat.chars().count() <= max {
        return flat;
    }
    flat.chars().take(max.saturating_sub(1)).collect::<String>() + "~"
}

fn tokens(n: u64) -> String {
    match n {
        0..=9_999 => n.to_string(),
        10_000..=999_999 => format!("{:.0}k", n as f64 / 1_000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}
