use crate::dispatch::account::AccountState;
use crate::ids::{NodeId, RunId};
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{AccountId, NodeState, Usage, WorkspaceRef};
use crate::model::event::WorkerEvent;
use crate::model::failure::Failure;
use crate::model::node::{NodeRecord, WorkResultRef};
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

#[derive(Debug, Default, Clone, Copy)]
pub struct Totals {
    pub nodes: u32,
    pub failed: u32,
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
            JournalEvent::NodeSpawned { node } => self.spawn((**node).clone()),
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
            JournalEvent::NodeBlocked { until, why } => {
                if let Some(n) = self.node_mut(l) {
                    n.state = NodeState::Blocked {
                        until: *until,
                        why: why.clone(),
                    };
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
            } => {
                let s = self.accounts.entry(account.clone()).or_default();
                s.health = *health;
                s.cooldown_until = *cooldown_until;
                s.quota = quota.clone();
            }
            JournalEvent::BrainTurn { .. }
            | JournalEvent::BrainToolCall { .. }
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

    /// Running nodes with no NodeFinished and a dead pid become Orphaned.
    pub fn mark_orphans(&mut self, alive: &dyn Fn(NodeId) -> bool) {
        for (id, n) in self.nodes.iter_mut() {
            if let NodeState::Running { pid, .. } = n.state
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
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for r in roots {
            self.walk(r, 0, &children, &mut seen, &mut out);
        }
        out
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
        let attempts = self.by_logical.get(&logical)?.clone();
        let latest = attempts.iter().rev().find_map(|a| self.nodes.get(a))?;
        Some(TreeRow {
            logical,
            depth,
            title: latest.title.clone(),
            state: latest.state.clone(),
            attempts,
        })
    }

    fn logical_of(&self, id: NodeId) -> NodeId {
        self.nodes.get(&id).map_or(id, |n| n.logical)
    }

    pub fn totals(&self) -> Totals {
        let rows = self.tree();
        Totals {
            nodes: rows.len() as u32,
            failed: rows
                .iter()
                .filter(|r| matches!(r.state, NodeState::Failed { .. }))
                .count() as u32,
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
            NodeState::Failed { failure } => format!("FAIL {}", failure_kind(failure)),
            NodeState::Succeeded => "ok".to_owned(),
            NodeState::Cancelled { .. } => "cancelled".to_owned(),
            NodeState::Running { .. } => "running".to_owned(),
            NodeState::Orphaned { .. } => "orphaned".to_owned(),
            NodeState::Blocked { .. } => "blocked".to_owned(),
            NodeState::Leased { .. } => "leased".to_owned(),
            NodeState::Queued => "queued".to_owned(),
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

fn failure_kind(f: &Failure) -> &'static str {
    match f {
        Failure::RateLimited { .. } => "rate_limited",
        Failure::AuthExpired { .. } => "auth_expired",
        Failure::Overloaded { .. } => "overloaded",
        Failure::BudgetExceeded { .. } => "budget_exceeded",
        Failure::Timeout { .. } => "timeout",
        Failure::WorkerError { .. } => "worker_error",
        Failure::PermissionDenied { .. } => "permission_denied",
        Failure::Crashed { .. } => "crashed",
        Failure::Truncated { .. } => "truncated",
        Failure::NoCapacity { .. } => "no_capacity",
        Failure::Cancelled { .. } => "cancelled",
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
