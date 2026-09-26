//! WP2: what a board frame is made of. Pure by construction: every byte arrives through a
//! loader the caller hands in, so the whole model is testable without a filesystem.
//!
//! Work is grouped by dispatch: each run is its brain row, its open root dispatches with their
//! tasks (and whatever those tasks dispatched in turn), then the dispatches that settled.

use crate::dispatch::policy::{Ineligible, Scoring, SelectionPolicy};
use crate::ids::{CallSeq, DispatchId, NodeId, RunId};
use crate::journal::fold::{DispatchView, Projection, RunView, Scope};
use crate::journal::inspect::{self, AttemptDetail, Rollup};
use crate::journal::paths::RunPaths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{AccountId, Cost, NodeState, Provider, Tier, Usage};
use crate::model::dispatch::DispatchState;
use crate::model::failure::Failure;
use crate::model::node::NodeRecord;
use crate::ui::board::sources::Tail;
use crate::ui::order::{self, Tally};
use crate::ui::usage::AccountRow;
use crate::ui::{dispatches, fmt};
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;
use time::OffsetDateTime;

/// Folding every live journal is linear in the number of runs, so the board tails at most
/// this many, newest first, and says in its header how many it dropped.
pub const MAX_RUNS: usize = 8;

/// How many settled dispatches a run's `recent` keeps.
pub const RECENT: usize = 8;

/// How long a settled dispatch stays in `recent` before it is dropped.
pub const RECENT_TTL: StdDuration = StdDuration::from_secs(5 * 60);

/// Task rows an expanded dispatch draws; the rest of the rank order is counted, not drawn.
pub const DISPATCH_ROWS: usize = 8;

// ---------------------------------------------------------------- selection

/// Why the pool picked this account for this node, as recorded at dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionNote {
    pub account: AccountId,
    pub policy: SelectionPolicy,
    pub reason: Reason,
    pub excluded: Vec<AccountId>,
}

/// Which spelling of `AccountSelected.reason` a journal carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonForm {
    /// WP5's `policy::explain`: `score .41 = util .93×.50 + load .33×.30 ...`, wrapped and
    /// shown term by term.
    Terms,
    /// Every run written before WP5: `format!("score {sc:.4}")`. One line, no terms.
    Score,
    /// Anything else, printed verbatim on one line.
    Other,
}

/// The recorded reason, kept as written. The board never re-scores it: the point of the
/// footer is what the terms were AT dispatch, not what they would be now.
#[derive(Debug, Clone, PartialEq)]
pub struct Reason {
    pub text: String,
    pub score: Option<f64>,
    pub form: ReasonForm,
}

impl Reason {
    pub fn parse(raw: &str) -> Reason {
        let text = fmt::sanitize(raw).trim().to_owned();
        let Some(rest) = text.strip_prefix("score ") else {
            return Reason {
                text,
                score: None,
                form: ReasonForm::Other,
            };
        };
        let mut parts = rest.split_whitespace();
        let score = parts.next().and_then(|h| h.parse::<f64>().ok());
        let form = match (score, parts.next()) {
            (Some(_), None) => ReasonForm::Score,
            (Some(_), Some(_)) => ReasonForm::Terms,
            (None, _) => ReasonForm::Other,
        };
        Reason { text, score, form }
    }
}

/// `RunView` drops `policy` and `reason`, so the board folds `AccountSelected` itself.
pub fn note(map: &mut BTreeMap<NodeId, SelectionNote>, l: &JournalLine) {
    let JournalEvent::AccountSelected {
        account,
        policy,
        reason,
        excluded,
        ..
    } = &l.event
    else {
        return;
    };
    let Some(node) = l.node else {
        return;
    };
    map.insert(
        node,
        SelectionNote {
            account: account.clone(),
            policy: *policy,
            reason: Reason::parse(reason),
            excluded: excluded.clone(),
        },
    );
}

/// One replay of a journal: the shared fold plus the board's own projection, in one pass.
#[derive(Debug, Default)]
pub struct Replay {
    pub view: RunView,
    pub selection: BTreeMap<NodeId, SelectionNote>,
}

impl Projection for Replay {
    type Out = Replay;

    fn apply(&mut self, l: &JournalLine) {
        self.view.apply(l);
        note(&mut self.selection, l);
    }

    fn finish(self) -> Replay {
        self
    }
}

// ---------------------------------------------------------------- runs

/// One tailed run. The board keeps several; `swamp watch` keeps exactly one.
pub struct RunPane {
    pub run: RunId,
    pub paths: RunPaths,
    pub view: RunView,
    pub tailer: Tail,
    /// The run's root node, per `brain::build`.
    pub brain: NodeId,
    pub selection: BTreeMap<NodeId, SelectionNote>,
    /// When the brain stopped being alive while its node was still non-terminal.
    pub stale: Option<OffsetDateTime>,
}

impl RunPane {
    pub fn new(paths: RunPaths, tailer: Tail) -> RunPane {
        RunPane {
            run: paths.run,
            brain: NodeId(paths.run.0),
            paths,
            view: RunView::default(),
            tailer,
            selection: BTreeMap::new(),
            stale: None,
        }
    }

    pub fn seed(&mut self, r: Replay) {
        self.view = r.view;
        self.selection = r.selection;
    }

    pub fn apply(&mut self, lines: &[JournalLine]) {
        for l in lines {
            self.view.apply(l);
            note(&mut self.selection, l);
        }
    }

    /// A journal that was truncated or replaced is re-read from zero; the fold is
    /// idempotent, so the only thing that has to go is what the old bytes built.
    pub fn rewind(&mut self) {
        self.view = RunView::default();
        self.selection.clear();
    }

    /// `alive` is handed in so this stays pure: `sources` supplies the `is_ours` probe.
    pub fn refresh_liveness(&mut self, alive: &dyn Fn(NodeId) -> bool, now: OffsetDateTime) {
        self.view.mark_orphans(alive);
        let brain_working = self
            .view
            .nodes
            .get(&self.brain)
            .is_some_and(|n| !n.state.is_terminal());
        match (brain_working && !alive(self.brain), self.stale) {
            (true, None) => self.stale = Some(now),
            (false, Some(_)) => self.stale = None,
            _ => {}
        }
    }

    /// The note the detail shows for a task: the newest attempt that recorded one. The pool
    /// leases before the attempt's id exists, so its line names the logical id; a journal
    /// that attributed one to an attempt still wins.
    pub fn note_for(&self, logical: NodeId) -> Option<&SelectionNote> {
        let attempts = self.view.by_logical.get(&logical);
        attempts
            .into_iter()
            .flatten()
            .rev()
            .find_map(|a| self.selection.get(a))
            .or_else(|| self.selection.get(&logical))
    }

    /// A task by its logical id, the last attempt taken as the live record: the same
    /// `latest()` rule `trace.rs` uses, so a retry changes the id in place.
    pub fn node_row(&self, logical: NodeId) -> Option<NodeRow> {
        let state = self.view.state_of(logical)?;
        let dispatch = self.view.tasks.get(&logical).map(|t| t.dispatch);
        match self.latest(logical) {
            Some(n) => Some(self.row(n, logical, state, dispatch)),
            None => self.unstarted(logical, state),
        }
    }

    fn latest(&self, logical: NodeId) -> Option<&NodeRecord> {
        self.view
            .by_logical
            .get(&logical)?
            .iter()
            .rev()
            .find_map(|a| self.view.nodes.get(a))
    }

    fn row(
        &self,
        n: &NodeRecord,
        logical: NodeId,
        state: NodeState,
        dispatch: Option<DispatchId>,
    ) -> NodeRow {
        NodeRow {
            run: self.run,
            logical,
            id: n.id,
            attempt: n.attempt,
            brain: n.logical == self.brain,
            provider: n.provider,
            account: n.account.clone(),
            tier: n.tier,
            model: n.model.clone(),
            title: fmt::sanitize(&n.title),
            state,
            created_at: n.created_at,
            started_at: n.started_at,
            ended_at: n.ended_at,
            usage: n.usage,
            cost: n.cost,
            stale: self.stale.is_some(),
            dispatch,
        }
    }

    /// A task queued, blocked or rejected before its first attempt existed. What it counts
    /// its wait from is the dispatch that asked for it.
    fn unstarted(&self, logical: NodeId, state: NodeState) -> Option<NodeRow> {
        let t = self.view.tasks.get(&logical)?;
        let record = self
            .view
            .dispatches
            .get(&t.dispatch)
            .and_then(|d| d.record.as_ref());
        let provider = record
            .and_then(|d| d.tasks.iter().find(|x| x.logical == logical))
            .map_or(Provider::Anthropic, |x| x.provider);
        let created_at = record
            .map(|d| d.at)
            .or(self.view.header.as_ref().map(|h| h.started_at))
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let account = match &state {
            NodeState::Leased { account } => Some(account.clone()),
            _ => None,
        };
        Some(NodeRow {
            run: self.run,
            logical,
            id: logical,
            attempt: 0,
            brain: false,
            provider,
            account,
            tier: t.tier,
            model: None,
            title: fmt::sanitize(&t.title),
            state,
            created_at,
            started_at: None,
            ended_at: None,
            usage: Usage::default(),
            cost: None,
            stale: self.stale.is_some(),
            dispatch: Some(t.dispatch),
        })
    }

    /// One task row: the live attempt, the attempts before it, and why it is stuck if it is.
    pub fn task_row(&self, logical: NodeId, level: usize, now: OffsetDateTime) -> Option<TaskRow> {
        let row = self.node_row(logical)?;
        let attempts = self.view.attempts(logical);
        let prior = attempts
            .iter()
            .take(attempts.len().saturating_sub(1))
            .map(|a| inspect::attempt(a, now))
            .collect();
        let blocked = match &row.state {
            NodeState::Blocked { until, .. } => Some((
                *until,
                self.view
                    .tasks
                    .get(&logical)
                    .map(|t| t.ineligible.clone())
                    .unwrap_or_default(),
            )),
            _ => None,
        };
        let failure = match &row.state {
            NodeState::Failed { failure } => Some(failure.clone()),
            NodeState::Rejected { reason } => Some(reason.clone()),
            _ => None,
        };
        Some(TaskRow {
            row,
            level,
            prior,
            blocked,
            failure,
            nested: Vec::new(),
        })
    }

    /// The brain's row, or `None` for a run that never spawned one.
    pub fn brain_row(&self) -> Option<NodeRow> {
        let n = self.latest(self.brain)?;
        let state = self.view.state_of(self.brain)?;
        Some(self.row(n, self.brain, state, None))
    }

    /// Whether `caller` is this run's brain rather than a task that dispatched work itself.
    fn is_root(&self, d: &DispatchView) -> bool {
        match &d.record {
            None => true,
            Some(r) => {
                r.caller == self.brain || inspect::caller(&self.view, r.caller).kind == "brain"
            }
        }
    }

    /// Dispatches a task issued, by the task's logical id.
    fn issued(&self) -> BTreeMap<NodeId, Vec<&DispatchView>> {
        let mut out: BTreeMap<NodeId, Vec<&DispatchView>> = BTreeMap::new();
        for d in self.view.dispatches.values() {
            if d.id == DispatchId::LEGACY || self.is_root(d) {
                continue;
            }
            let Some(r) = &d.record else { continue };
            let of = self
                .view
                .attempts(r.caller)
                .first()
                .map_or(r.caller, |n| n.logical);
            out.entry(of).or_default().push(d);
        }
        for list in out.values_mut() {
            list.sort_by_key(|d| dispatch_key(d));
        }
        out
    }

    /// The brain row, the open root dispatches, then the ones that settled recently.
    pub fn run_rows(&self, now: OffsetDateTime) -> RunRows {
        let issued = self.issued();
        let mut roots: Vec<&DispatchView> = self
            .view
            .dispatches
            .values()
            .filter(|d| d.id == DispatchId::LEGACY || self.is_root(d))
            .collect();
        roots.sort_by_key(|d| dispatch_key(d));
        let mut active = Vec::new();
        let mut recent = Vec::new();
        for d in roots {
            let g = self.group(d, 0, None, &issued, now);
            match d.state {
                DispatchState::Open => active.push(g),
                DispatchState::Settled => recent.push(g),
            }
        }
        recent.sort_by_key(|g| std::cmp::Reverse((g.settled_at, g.id)));
        let ttl = time::Duration::seconds(RECENT_TTL.as_secs() as i64);
        recent.retain(|g| now - g.settled_at <= ttl);
        recent.truncate(RECENT);
        RunRows {
            run: self.run,
            brain: self.brain_row(),
            active,
            recent,
        }
    }

    fn group(
        &self,
        d: &DispatchView,
        level: usize,
        caller: Option<NodeId>,
        issued: &BTreeMap<NodeId, Vec<&DispatchView>>,
        now: OffsetDateTime,
    ) -> DispatchGroup {
        let mut tasks: Vec<TaskRow> = if d.id == DispatchId::LEGACY {
            // Schema 1 has no dispatch order to rank within: the tree is the order.
            self.view
                .tree()
                .into_iter()
                .filter(|r| r.logical != self.brain && d.tasks.contains(&r.logical))
                .filter_map(|r| {
                    self.task_row(r.logical, level + (r.depth as usize).saturating_sub(1), now)
                })
                .collect()
        } else {
            let mut rows: Vec<(usize, TaskRow)> = d
                .tasks
                .iter()
                .filter_map(|t| self.task_row(*t, level, now))
                .enumerate()
                .collect();
            rows.sort_by_key(|(i, t)| (order::rank(&t.row.state), *i));
            rows.into_iter().map(|(_, t)| t).collect()
        };
        let mut tally = Tally::default();
        for t in &tasks {
            tally.add(&t.row.state);
        }
        let settled_at = tasks
            .iter()
            .filter_map(|t| t.row.ended_at)
            .max()
            .or(d.record.as_ref().map(|r| r.at))
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        let hidden = tasks.len().saturating_sub(DISPATCH_ROWS);
        tasks.truncate(DISPATCH_ROWS);
        if level < dispatches::MAX_NESTING {
            for t in &mut tasks {
                for sub in issued.get(&t.row.logical).into_iter().flatten() {
                    let by = sub.record.as_ref().map(|r| r.caller);
                    t.nested.push(self.group(sub, t.level + 1, by, issued, now));
                }
            }
        }
        let at = d
            .record
            .as_ref()
            .map(|r| r.at)
            .or(self.view.header.as_ref().map(|h| h.started_at))
            .unwrap_or(OffsetDateTime::UNIX_EPOCH);
        DispatchGroup {
            run: self.run,
            id: d.id,
            seq: d.record.as_ref().and_then(|r| r.call_seq),
            caller,
            level,
            state: d.state,
            at,
            settled_at,
            tally,
            cost: self.view.rollup(Scope::Dispatch(d.id)).into(),
            tasks,
            hidden,
            expanded: d.state == DispatchState::Open,
        }
    }

    /// Every task of every open dispatch, the legacy bucket included while it is open.
    pub fn open_tally(&self) -> Tally {
        let mut t = Tally::default();
        for d in self
            .view
            .dispatches
            .values()
            .filter(|d| d.state == DispatchState::Open)
        {
            for task in &d.tasks {
                if let Some(s) = self.view.state_of(*task) {
                    t.add(&s);
                }
            }
        }
        t
    }
}

/// Stable dispatch order: by call, then by when it was issued; the legacy bucket last.
fn dispatch_key(d: &DispatchView) -> (bool, bool, Option<CallSeq>, OffsetDateTime, DispatchId) {
    let r = d.record.as_ref();
    (
        d.id == DispatchId::LEGACY,
        r.and_then(|r| r.call_seq).is_none(),
        r.and_then(|r| r.call_seq),
        r.map_or(OffsetDateTime::UNIX_EPOCH, |r| r.at),
        d.id,
    )
}

/// A run is live while it has not written `RunFinished` and either a non-terminal node's
/// pidfile is still ours or its control socket is still there.
pub fn is_live(view: &RunView, alive: &dyn Fn(NodeId) -> bool, socket: bool) -> bool {
    if view.finished {
        return false;
    }
    if socket {
        return true;
    }
    view.nodes
        .values()
        .any(|n| !n.state.is_terminal() && alive(n.id))
}

// ---------------------------------------------------------------- rows

/// One task, or the brain. Every string is already sanitized: a title comes from a model.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub run: RunId,
    pub logical: NodeId,
    /// The live attempt: the id `swamp diff` and `swamp adopt` take. The logical id until
    /// the task has one.
    pub id: NodeId,
    /// 0 for a task that never started.
    pub attempt: u32,
    pub brain: bool,
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub tier: Tier,
    pub model: Option<String>,
    pub title: String,
    pub state: NodeState,
    /// When the attempt was created, or the dispatch was issued for a task with none.
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub ended_at: Option<OffsetDateTime>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub stale: bool,
    pub dispatch: Option<DispatchId>,
}

impl NodeRow {
    /// Recomputed every frame, never accumulated: time on the account once a task started,
    /// its wait before that, and nothing for a task that ended without ever starting.
    pub fn elapsed(&self, now: OffsetDateTime) -> Option<StdDuration> {
        match self.started_at {
            Some(from) => (self.ended_at.unwrap_or(now) - from).try_into().ok(),
            None if self.state.is_terminal() => None,
            None => (now - self.created_at).try_into().ok(),
        }
    }

    /// `9g5f09·2` for a retry, the plain short id otherwise.
    pub fn short(&self) -> String {
        if self.attempt > 1 {
            format!("{}\u{b7}{}", self.id.short(), self.attempt)
        } else {
            self.id.short()
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub row: NodeRow,
    /// 0 for a root dispatch's tasks, one more per dispatch nested under a task.
    pub level: usize,
    /// Every attempt before the live one, oldest first.
    pub prior: Vec<AttemptDetail>,
    /// Until when, and why each account refused it, as the pool recorded.
    pub blocked: Option<(OffsetDateTime, Vec<(AccountId, Ineligible)>)>,
    /// The failure of a failed task, or the reason a rejected one was refused.
    pub failure: Option<Failure>,
    /// What this task dispatched in turn.
    pub nested: Vec<DispatchGroup>,
}

#[derive(Debug, Clone)]
pub struct DispatchGroup {
    pub run: RunId,
    pub id: DispatchId,
    pub seq: Option<CallSeq>,
    /// The attempt that issued a nested dispatch; `None` for the brain's own.
    pub caller: Option<NodeId>,
    pub level: usize,
    pub state: DispatchState,
    pub at: OffsetDateTime,
    /// The last task's end, or when it was issued: what `recent` sorts and ages by.
    pub settled_at: OffsetDateTime,
    /// Every task, including the ones past `DISPATCH_ROWS`.
    pub tally: Tally,
    pub cost: Rollup,
    /// In rank order, capped at `DISPATCH_ROWS`.
    pub tasks: Vec<TaskRow>,
    /// How many tasks the cap left out.
    pub hidden: usize,
    pub expanded: bool,
}

impl DispatchGroup {
    /// `#3`, the short id when there is no call behind it, or `legacy`.
    pub fn label(&self) -> String {
        if self.id == DispatchId::LEGACY {
            return inspect::LEGACY.to_owned();
        }
        match self.seq {
            Some(seq) => format!("#{seq}"),
            None => self.id.short(),
        }
    }

    /// The label and, when it is a call number, the short id beside it.
    pub fn full_label(&self) -> String {
        match (self.id == DispatchId::LEGACY, self.seq) {
            (false, Some(_)) => format!("{} {}", self.label(), self.id.short()),
            _ => self.label(),
        }
    }

    pub fn is_legacy(&self) -> bool {
        self.id == DispatchId::LEGACY
    }
}

#[derive(Debug, Clone)]
pub struct RunRows {
    pub run: RunId,
    pub brain: Option<NodeRow>,
    pub active: Vec<DispatchGroup>,
    pub recent: Vec<DispatchGroup>,
}

/// Everything a frame draws, in draw order.
#[derive(Debug, Clone, Default)]
pub struct Rows {
    /// The focused run, or every tailed run.
    pub runs: Vec<RunRows>,
    /// `in flight` recounted from every tailed run's journal.
    pub accounts: Vec<AccountRow>,
    /// Every open dispatch of every tailed run, `focus` or not: what the header counts.
    pub tally: Tally,
}

impl Rows {
    /// Every task row the runs hold, nested and recent ones included, in draw order.
    pub fn tasks(&self) -> Vec<&TaskRow> {
        fn walk<'a>(g: &'a DispatchGroup, out: &mut Vec<&'a TaskRow>) {
            for t in &g.tasks {
                out.push(t);
                for sub in &t.nested {
                    walk(sub, out);
                }
            }
        }
        let mut out = Vec::new();
        for r in &self.runs {
            for g in r.active.iter().chain(&r.recent) {
                walk(g, &mut out);
            }
        }
        out
    }

    pub fn groups(&self) -> Vec<&DispatchGroup> {
        fn walk<'a>(g: &'a DispatchGroup, out: &mut Vec<&'a DispatchGroup>) {
            out.push(g);
            for t in &g.tasks {
                for sub in &t.nested {
                    walk(sub, out);
                }
            }
        }
        let mut out = Vec::new();
        for r in &self.runs {
            for g in r.active.iter().chain(&r.recent) {
                walk(g, &mut out);
            }
        }
        out
    }

    pub fn task(&self, run: RunId, logical: NodeId) -> Option<&TaskRow> {
        self.tasks()
            .into_iter()
            .find(|t| t.row.run == run && t.row.logical == logical)
    }

    pub fn group(&self, run: RunId, id: DispatchId) -> Option<&DispatchGroup> {
        self.groups()
            .into_iter()
            .find(|g| g.run == run && g.id == id)
    }

    pub fn brain(&self, run: RunId, logical: NodeId) -> Option<&NodeRow> {
        self.runs
            .iter()
            .filter_map(|r| r.brain.as_ref())
            .find(|b| b.run == run && b.logical == logical)
    }

    pub fn account(&self, id: &AccountId) -> Option<&AccountRow> {
        self.accounts.iter().find(|r| &r.account == id)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub runs: usize,
    pub hidden_runs: usize,
    pub stale_runs: usize,
    pub tally: Tally,
    pub accounts: usize,
    pub cost_usd: f64,
    pub cost_complete: bool,
}

// ---------------------------------------------------------------- board

/// What the cursor is on. Held by identity, not by index, so a rediscovery or a new task
/// never moves it under the user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Selection {
    #[default]
    None,
    Account(AccountId),
    Node {
        run: RunId,
        logical: NodeId,
    },
    Dispatch {
        run: RunId,
        id: DispatchId,
    },
}

impl Selection {
    pub fn run(&self) -> Option<RunId> {
        match self {
            Selection::Node { run, .. } | Selection::Dispatch { run, .. } => Some(*run),
            _ => None,
        }
    }
}

/// Where the cursor goes until the user moves it: the first task stuck for good, else the
/// first blocked one, else the first running one, else the brain.
pub fn attention(rows: &Rows) -> Option<Selection> {
    fn walk<'a>(g: &'a DispatchGroup, out: &mut Vec<&'a TaskRow>) {
        if !g.expanded {
            return;
        }
        for t in &g.tasks {
            out.push(t);
            for sub in &t.nested {
                walk(sub, out);
            }
        }
    }
    let mut shown = Vec::new();
    for r in &rows.runs {
        for g in &r.active {
            walk(g, &mut shown);
        }
    }
    let pick = |want: &dyn Fn(&NodeState) -> bool| {
        shown
            .iter()
            .find(|t| want(&t.row.state))
            .map(|t| Selection::Node {
                run: t.row.run,
                logical: t.row.logical,
            })
    };
    pick(&|s| order::rank(s) == 0)
        .or_else(|| pick(&|s| matches!(s, NodeState::Blocked { .. })))
        .or_else(|| pick(&|s| order::rank(s) == 1))
        .or_else(|| {
            let b = rows.runs.iter().find_map(|r| r.brain.as_ref())?;
            Some(Selection::Node {
                run: b.run,
                logical: b.logical,
            })
        })
}

/// What `follow` pins to: the running task that started last.
pub fn newest_running(rows: &Rows) -> Option<Selection> {
    rows.tasks()
        .into_iter()
        .filter(|t| order::rank(&t.row.state) == 1)
        .max_by_key(|t| (t.row.started_at, t.row.id))
        .map(|t| Selection::Node {
            run: t.row.run,
            logical: t.row.logical,
        })
}

pub struct Board {
    /// Newest first, capped at `MAX_RUNS`.
    pub runs: Vec<RunPane>,
    /// Machine-wide, the same `AccountRow` values `/usage` and `swamp usage` render.
    pub accounts: Vec<AccountRow>,
    pub scoring: Scoring,
    pub policy: SelectionPolicy,
    pub selected: Selection,
    pub now: OffsetDateTime,
    /// When `accounts.json` was last read, for the header's freshness.
    pub accounts_at: Option<OffsetDateTime>,
    /// Live runs discovery found beyond `MAX_RUNS`.
    pub hidden_runs: usize,
    /// `tab`: draw only this run. `None` merges every tailed run.
    pub focus: Option<RunId>,
}

impl Board {
    pub fn new(scoring: Scoring, policy: SelectionPolicy, now: OffsetDateTime) -> Board {
        Board {
            runs: Vec::new(),
            accounts: Vec::new(),
            scoring,
            policy,
            selected: Selection::None,
            now,
            accounts_at: None,
            hidden_runs: 0,
            focus: None,
        }
    }

    pub fn pane(&self, run: RunId) -> Option<&RunPane> {
        self.runs.iter().find(|p| p.run == run)
    }

    pub fn pane_mut(&mut self, run: RunId) -> Option<&mut RunPane> {
        self.runs.iter_mut().find(|p| p.run == run)
    }

    /// Brings the tailed set in line with `wanted`, keeping the panes it already has and
    /// asking `open` only for the runs that are new. `open` is the only I/O, and it is the
    /// caller's.
    pub fn sync<F>(&mut self, wanted: &[RunId], mut open: F) -> bool
    where
        F: FnMut(RunId) -> anyhow::Result<RunPane>,
    {
        let before: Vec<RunId> = self.runs.iter().map(|p| p.run).collect();
        if before == wanted {
            return false;
        }
        let mut kept: BTreeMap<RunId, RunPane> = std::mem::take(&mut self.runs)
            .into_iter()
            .map(|p| (p.run, p))
            .collect();
        for run in wanted {
            match kept.remove(run) {
                Some(pane) => self.runs.push(pane),
                None => match open(*run) {
                    Ok(pane) => self.runs.push(pane),
                    Err(e) => tracing::warn!("board: cannot tail run {run}: {e:#}"),
                },
            }
        }
        self.clamp();
        true
    }

    /// Drops a selection or a focus that names something the board no longer tails.
    pub fn clamp(&mut self) {
        if self.focus.is_some_and(|r| self.pane(r).is_none()) {
            self.focus = None;
        }
        let gone = match &self.selected {
            Selection::Node { run, logical } => self.pane(*run).is_none_or(|p| {
                !p.view.by_logical.contains_key(logical) && !p.view.tasks.contains_key(logical)
            }),
            Selection::Dispatch { run, id } => self
                .pane(*run)
                .is_none_or(|p| !p.view.dispatches.contains_key(id)),
            _ => false,
        };
        if gone {
            self.selected = Selection::None;
        }
    }

    fn panes(&self) -> impl Iterator<Item = &RunPane> {
        self.runs
            .iter()
            .filter(|p| self.focus.is_none_or(|r| r == p.run))
    }

    pub fn note_for(&self, run: RunId, logical: NodeId) -> Option<&SelectionNote> {
        self.pane(run)?.note_for(logical)
    }

    pub fn selected_note(&self) -> Option<&SelectionNote> {
        match &self.selected {
            Selection::Node { run, logical } => self.note_for(*run, *logical),
            _ => None,
        }
    }

    pub fn rows(&self) -> Rows {
        let mut tally = Tally::default();
        for p in &self.runs {
            tally.absorb(&p.open_tally());
        }
        let carried = self.in_flight_counts();
        let accounts = self
            .accounts
            .iter()
            .map(|r| {
                let mut r = r.clone();
                // `persist::merge_state` zeroes `inflight` in the file, because it is one
                // process's runtime state: the only honest count is the one the journals show.
                r.inflight = carried.get(&r.account).copied().unwrap_or(0);
                r
            })
            .collect();
        Rows {
            runs: self.panes().map(|p| p.run_rows(self.now)).collect(),
            accounts,
            tally,
        }
    }

    /// Tasks and brains on each account over every tailed run. `focus` narrows what a frame
    /// draws; an account running three nodes in the run `tab` hid is still at three.
    fn in_flight_counts(&self) -> BTreeMap<AccountId, usize> {
        let mut counts: BTreeMap<AccountId, usize> = BTreeMap::new();
        for pane in &self.runs {
            for logical in pane.view.by_logical.keys() {
                let Some(state) = pane.view.state_of(*logical) else {
                    continue;
                };
                let account = match &state {
                    NodeState::Leased { account } => Some(account.clone()),
                    NodeState::Running { .. } | NodeState::Orphaned { .. } => {
                        pane.latest(*logical).and_then(|n| n.account.clone())
                    }
                    _ => None,
                };
                if let Some(id) = account {
                    *counts.entry(id).or_default() += 1;
                }
            }
        }
        counts
    }

    pub fn summary(&self, rows: &Rows) -> Summary {
        let mut cost_usd = 0.0;
        let mut cost_complete = true;
        // The header counts the tailed set, never the focused one: `2 runs` and the totals
        // beside it have to describe the same thing.
        for pane in &self.runs {
            cost_usd += pane.view.cost_usd;
            cost_complete &= pane.view.cost_complete;
        }
        Summary {
            runs: self.runs.len(),
            hidden_runs: self.hidden_runs,
            stale_runs: self.runs.iter().filter(|p| p.stale.is_some()).count(),
            tally: rows.tally,
            accounts: self.accounts.len(),
            cost_usd,
            cost_complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::CallSeq;
    use crate::model::dispatch::{DispatchRecord, Phase, TaskRef};
    use crate::ui::board::sources::Tail;
    use crate::ui::chat::tests_support as fx;
    use camino::Utf8PathBuf;
    use std::str::FromStr;

    fn paths() -> RunPaths {
        RunPaths {
            run: fx::run_id(),
            dir: Utf8PathBuf::from("/repo/.swamp/runs/x"),
            sock_dir: Utf8PathBuf::from("/home/.swamp/sock"),
        }
    }

    fn pane(lines: &[JournalLine]) -> RunPane {
        let mut pane = RunPane::new(paths(), Tail::detached("/repo/.swamp/runs/x/journal.jsonl"));
        pane.apply(lines);
        pane
    }

    fn account_row(id: &str, provider: Option<Provider>) -> AccountRow {
        AccountRow {
            provider,
            account: AccountId(id.to_owned()),
            exec: format!("claude-{id}"),
            in_config: provider.is_some(),
            health: crate::dispatch::account::Health::Healthy,
            inflight: 7,
            max_concurrency: Some(4),
            cooldown_until: None,
            quota: None,
            quota_buckets: BTreeMap::new(),
            quota_observed_at: None,
            quota_source: None,
            window_tokens: Usage::default(),
            window_started_at: None,
            lifetime_tokens: Usage::default(),
            lifetime_nodes: 0,
            cost_usd: 0.0,
            cost_basis: None,
        }
    }

    fn board(panes: Vec<RunPane>, accounts: Vec<AccountRow>) -> Board {
        let mut b = Board::new(Scoring::default(), SelectionPolicy::default(), fx::now());
        b.runs = panes;
        b.accounts = accounts;
        b
    }

    fn line(seq: u64, node: NodeId, event: JournalEvent) -> JournalLine {
        JournalLine {
            seq,
            at: fx::at(100),
            run: fx::run_id(),
            node: Some(node),
            event,
        }
    }

    fn changed(seq: u64, node: NodeId, to: NodeState) -> JournalLine {
        line(
            seq,
            node,
            JournalEvent::NodeStateChanged {
                from: Phase::Queued,
                to,
                why: String::new(),
            },
        )
    }

    fn selected(seq: u64, node: NodeId, reason: &str, excluded: &[&str]) -> JournalLine {
        JournalLine {
            seq,
            at: fx::at(seq as i64),
            run: fx::run_id(),
            node: Some(node),
            event: JournalEvent::AccountSelected {
                account: AccountId("alt".into()),
                exec: "claude-alt".into(),
                policy: SelectionPolicy::QuotaAware,
                reason: reason.to_owned(),
                excluded: excluded.iter().map(|e| AccountId((*e).into())).collect(),
            },
        }
    }

    fn shorts(g: &DispatchGroup) -> Vec<String> {
        g.tasks.iter().map(|t| t.row.short()).collect()
    }

    /// Tasks sit under the dispatch that asked for them, dispatches in call order.
    #[test]
    fn tasks_group_under_their_dispatch_in_call_order() {
        let b = board(vec![pane(&fx::p4_journal())], Vec::new());
        let rows = b.rows();
        let run = &rows.runs[0];
        assert_eq!(run.brain.as_ref().map(|r| r.short()), Some("9g5fav".into()));
        assert_eq!(run.active.len(), 1);
        assert_eq!(run.active[0].label(), "#1");
        assert_eq!(run.active[0].full_label(), "#1 9g5f18");
        assert_eq!(run.recent.len(), 1, "the rejected dispatch settled");
        assert_eq!(run.recent[0].seq, Some(CallSeq(2)));
        assert_eq!(
            run.active[0].tally.cells().len(),
            4,
            "{:?}",
            run.active[0].tally
        );
        assert!((run.active[0].cost.usd - 0.34).abs() < 1e-9);
    }

    /// Failures first, then running, then blocked, then queued, then done.
    #[test]
    fn tasks_are_ranked_inside_a_dispatch() {
        let mut lines = fx::p4_journal();
        let b = board(vec![pane(&lines)], Vec::new());
        assert_eq!(
            shorts(&b.rows().runs[0].active[0]),
            vec!["9g5f01", "9g5f09·2", "9g5f04", "9g5f0a", "9g5f05"]
        );

        let seq = lines.len() as u64 + 10;
        lines.push(changed(
            seq,
            fx::p4_task(4),
            NodeState::Failed {
                failure: Failure::Timeout { after_s: 60 },
            },
        ));
        let b = board(vec![pane(&lines)], Vec::new());
        let group = b.rows().runs[0].active[0].clone();
        assert_eq!(
            shorts(&group),
            vec!["9g5f0a", "9g5f01", "9g5f09·2", "9g5f04", "9g5f05"]
        );
        assert_eq!(
            order::text(&group.tally.cells()),
            "1 failed · 2 running · 1 blocked · 1 done"
        );
    }

    /// Settled means recent; an open dispatch whose tasks all ended is still open.
    #[test]
    fn only_a_settled_dispatch_moves_to_recent() {
        let mut lines = fx::p4_journal();
        let base = lines.len() as u64 + 10;
        for n in 1..=4u8 {
            lines.push(changed(
                base + n as u64,
                fx::p4_task(n),
                NodeState::Succeeded,
            ));
        }
        let b = board(vec![pane(&lines)], Vec::new());
        let rows = b.rows();
        assert_eq!(rows.runs[0].active.len(), 1, "no DispatchSettled yet");
        assert_eq!(rows.runs[0].active[0].tally.done, 5);
        assert!(rows.runs[0].active[0].expanded);
        assert!(!rows.runs[0].recent[0].expanded, "recent starts folded");

        let mut later = b;
        later.now = fx::at(159) + time::Duration::seconds(RECENT_TTL.as_secs() as i64 + 1);
        assert!(later.rows().runs[0].recent.is_empty(), "recent ages out");
    }

    /// A task that dispatched work of its own carries that dispatch right under it.
    #[test]
    fn a_nested_dispatch_hangs_under_its_task() {
        let mut lines = fx::p4_journal();
        let seq = lines.len() as u64 + 10;
        lines.push(line(
            seq,
            fx::nid("01"),
            JournalEvent::DispatchIssued {
                record: Box::new(DispatchRecord {
                    id: fx::did("1k"),
                    run: fx::run_id(),
                    caller: fx::nid("01"),
                    call_seq: Some(CallSeq(3)),
                    wait: true,
                    max_wait_s: None,
                    tasks: vec![TaskRef {
                        logical: fx::nid("1m"),
                        title: "split the handler".into(),
                        tier: Tier::Low,
                        provider: Provider::Anthropic,
                    }],
                    at: fx::at(100),
                }),
            },
        ));
        let b = board(vec![pane(&lines)], Vec::new());
        let rows = b.rows();
        let run = &rows.runs[0];
        assert_eq!(run.active.len(), 1, "a nested dispatch is not a root");
        let task = &run.active[0].tasks[0];
        assert_eq!(task.row.short(), "9g5f01");
        assert_eq!(task.nested.len(), 1);
        let nested = &task.nested[0];
        assert_eq!(nested.label(), "#3");
        assert_eq!(nested.caller, Some(fx::nid("01")));
        assert_eq!(nested.level, 1);
        assert_eq!(nested.tasks[0].level, 1);
        assert_eq!(rows.tally.queued, 2, "an open nested dispatch counts");
    }

    /// A schema-1 run has no dispatches: one legacy bucket, in tree order, no brain in it.
    #[test]
    fn a_legacy_run_groups_into_one_bucket_in_tree_order() {
        let b = board(vec![pane(&fx::fixture())], Vec::new());
        let rows = b.rows();
        let run = &rows.runs[0];
        assert_eq!(run.active.len(), 1);
        let legacy = &run.active[0];
        assert!(legacy.is_legacy());
        assert_eq!(legacy.label(), "legacy");
        assert_eq!(
            shorts(legacy),
            vec!["9g5f01", "9g5f02"],
            "tree order, not rank"
        );
        assert!(run.brain.is_some());
        assert_eq!(rows.tally.failed, 1);
    }

    #[test]
    fn the_default_selection_is_the_first_stuck_task() {
        let b = board(vec![pane(&fx::p4_journal())], Vec::new());
        assert_eq!(
            attention(&b.rows()),
            Some(Selection::Node {
                run: fx::run_id(),
                logical: fx::p4_task(3),
            }),
            "nothing failed, so the blocked task"
        );
        let b = board(vec![pane(&fx::fixture())], Vec::new());
        assert_eq!(
            attention(&b.rows()),
            Some(Selection::Node {
                run: fx::run_id(),
                logical: fx::id(2),
            }),
            "the failed worker"
        );
    }

    /// `tab` chooses what is drawn, never what is counted.
    #[test]
    fn header_tallies_sum_every_run_whatever_the_focus() {
        let other = RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FBZ").expect("run id");
        let mut second = RunPane::new(
            RunPaths {
                run: other,
                dir: Utf8PathBuf::from("/repo/.swamp/runs/y"),
                sock_dir: Utf8PathBuf::from("/home/.swamp/sock"),
            },
            Tail::detached("/repo/.swamp/runs/y/journal.jsonl"),
        );
        second.apply(&fx::running());
        let mut b = board(
            vec![pane(&fx::p4_journal()), second],
            vec![
                account_row("main", Some(Provider::Anthropic)),
                account_row("alt", Some(Provider::Anthropic)),
            ],
        );
        let merged = b.rows();
        assert_eq!(merged.tally.running, 4);
        assert_eq!(merged.tally.stuck(), 1);
        // Two brains and a worker of each run on main, a worker of each run on alt.
        assert_eq!(
            merged
                .account(&AccountId("main".into()))
                .map(|r| r.inflight),
            Some(4)
        );
        assert_eq!(
            merged.account(&AccountId("alt".into())).map(|r| r.inflight),
            Some(2)
        );

        b.focus = Some(fx::run_id());
        let rows = b.rows();
        assert_eq!(rows.runs.len(), 1, "one run drawn");
        assert_eq!(rows.tally, merged.tally, "both runs counted");
        assert_eq!(rows.accounts[0].inflight, 4);
        assert_eq!(b.summary(&rows).tally.running, 4);
    }

    #[test]
    fn an_unstarted_rejected_task_has_no_elapsed() {
        let b = board(vec![pane(&fx::p4_journal())], Vec::new());
        let rows = b.rows();
        let rejected = &rows.runs[0].recent[0].tasks[0];
        assert!(matches!(rejected.row.state, NodeState::Rejected { .. }));
        assert_eq!(rejected.row.elapsed(b.now), None);
        assert!(rejected.failure.is_some());

        let blocked = rows
            .task(fx::run_id(), fx::p4_task(3))
            .expect("blocked task");
        assert_eq!(
            blocked.row.elapsed(b.now),
            Some(StdDuration::from_secs(200))
        );
        let (_, why) = blocked.blocked.as_ref().expect("recorded ineligible");
        assert_eq!(why.len(), 2);

        let retried = rows
            .task(fx::run_id(), fx::p4_task(2))
            .expect("retried task");
        assert_eq!(
            retried.row.elapsed(b.now),
            Some(StdDuration::from_secs(168))
        );
        assert_eq!(retried.prior.len(), 1);
        assert_eq!(
            order::attempt_lines(&retried.prior),
            vec!["attempt 1 9g5f08 on main: rate_limited (five_hour) after 41s"]
        );
    }

    #[test]
    fn the_new_reason_form_keeps_its_terms() {
        let raw = "score .41 = util .93×.50 + load .33×.30 + share .12×.15 − weight .00";
        let mut lines = fx::running();
        lines.push(selected(9, fx::id(2), raw, &["main"]));
        let pane = pane(&lines);

        let note = pane.note_for(fx::id(2)).expect("a note for the node");
        assert_eq!(note.reason.form, ReasonForm::Terms);
        assert_eq!(note.reason.score, Some(0.41));
        assert_eq!(note.reason.text, raw);
        assert_eq!(note.excluded, vec![AccountId("main".into())]);
        assert_eq!(note.policy, SelectionPolicy::QuotaAware);
    }

    /// Journals written before WP5 carry `format!("score {sc:.4}")` and nothing else.
    #[test]
    fn the_legacy_score_form_degrades_to_one_line() {
        let mut lines = fx::running();
        lines.push(selected(9, fx::id(2), "score 0.4100", &[]));
        let pane = pane(&lines);

        let note = pane.note_for(fx::id(2)).expect("a note for the node");
        assert_eq!(note.reason.form, ReasonForm::Score);
        assert_eq!(note.reason.score, Some(0.41));
        assert!(note.excluded.is_empty());
    }

    /// A title, and now a reason, can carry whatever a model emitted.
    #[test]
    fn a_reason_never_carries_control_characters() {
        let r = Reason::parse("score \u{1b}[2J0.41");
        assert!(!r.text.contains('\u{1b}'));
        assert_eq!(r.form, ReasonForm::Other);
    }

    #[test]
    fn a_reason_the_board_cannot_parse_is_kept_verbatim() {
        let r = Reason::parse("round robin");
        assert_eq!(r.form, ReasonForm::Other);
        assert_eq!(r.text, "round robin");
        assert_eq!(r.score, None);
    }

    /// A real lease is taken before the attempt has an id, so `AccountSelected` names the
    /// logical id. The detail has to find it there too.
    #[test]
    fn a_note_attributed_to_the_logical_id_still_reaches_the_row() {
        let mut lines = fx::p4_journal();
        let seq = lines.len() as u64 + 10;
        lines.push(selected(
            seq,
            fx::p4_task(1),
            "score .41 = util .93×.50",
            &[],
        ));
        let pane = pane(&lines);
        let note = pane
            .note_for(fx::p4_task(1))
            .expect("a note for the logical row");
        assert_eq!(note.reason.form, ReasonForm::Terms);
    }

    /// Notes are keyed by attempt; a retry's own note is the one the detail shows.
    #[test]
    fn the_newest_attempt_owns_the_note() {
        let mut lines = fx::p4_journal();
        let seq = lines.len() as u64 + 10;
        lines.push(selected(seq, fx::nid("08"), "score 0.9000", &[]));
        let pane = pane(&lines);
        let note = pane.note_for(fx::p4_task(2)).expect("a note for the task");
        assert_eq!(
            note.reason.form,
            ReasonForm::Terms,
            "the retry's, not attempt 1's"
        );
        let row = pane.node_row(fx::p4_task(2)).expect("the task row");
        assert_eq!(
            row.id,
            fx::nid("09"),
            "the live attempt is the one diff takes"
        );
        assert_eq!(row.attempt, 2);
    }

    #[test]
    fn a_dead_brain_makes_the_run_stale_and_freezes_its_nodes() {
        let mut pane = pane(&fx::running());
        pane.refresh_liveness(&|_| true, fx::now());
        assert_eq!(pane.stale, None);

        pane.refresh_liveness(&|_| false, fx::now());
        assert_eq!(pane.stale, Some(fx::now()));
        assert!(matches!(
            pane.view.nodes[&pane.brain].state,
            NodeState::Orphaned { .. }
        ));
        let brain = pane.brain_row().expect("the brain row");
        assert!(brain.stale);
        assert!(matches!(brain.state, NodeState::Orphaned { .. }));

        // The brain came back (a restart adopted it): the header stops saying stale.
        pane.refresh_liveness(&|_| true, fx::now());
        assert_eq!(pane.stale, None);
    }

    #[test]
    fn liveness_takes_the_socket_or_a_live_pidfile() {
        let view = fx::view_of(fx::running());
        assert!(is_live(&view, &|_| false, true), "socket alone is enough");
        assert!(is_live(&view, &|_| true, false), "a live worker is enough");
        assert!(!is_live(&view, &|_| false, false));

        let mut finished = fx::view_of(fx::running());
        finished.apply(&JournalLine {
            seq: 99,
            at: fx::at(99),
            run: fx::run_id(),
            node: None,
            event: JournalEvent::RunFinished {
                state: NodeState::Succeeded,
                nodes: 2,
                usage: Usage::default(),
                cost_usd: None,
            },
        });
        assert!(
            !is_live(&finished, &|_| true, true),
            "finished outranks both"
        );
    }

    #[test]
    fn sync_keeps_the_panes_it_already_tails() {
        let mut b = board(vec![pane(&fx::running())], Vec::new());
        b.selected = Selection::Node {
            run: fx::run_id(),
            logical: fx::id(1),
        };
        let mut opened = 0;
        let changed = b.sync(&[fx::run_id()], |_| {
            opened += 1;
            Ok(RunPane::new(paths(), Tail::detached("/nowhere")))
        });
        assert!(!changed);
        assert_eq!(opened, 0);
        assert_eq!(b.selected.run(), Some(fx::run_id()));

        // The run fell out of the live set: the selection must not point into nothing.
        assert!(b.sync(&[], |_| unreachable!("nothing to open")));
        assert!(b.runs.is_empty());
        assert_eq!(b.selected, Selection::None);
    }

    #[test]
    fn a_selected_dispatch_that_disappears_is_dropped() {
        let mut b = board(vec![pane(&fx::p4_journal())], Vec::new());
        b.selected = Selection::Dispatch {
            run: fx::run_id(),
            id: fx::did("18"),
        };
        b.clamp();
        assert!(matches!(b.selected, Selection::Dispatch { .. }));
        b.selected = Selection::Dispatch {
            run: fx::run_id(),
            id: fx::did("99"),
        };
        b.clamp();
        assert_eq!(b.selected, Selection::None);
    }

    #[test]
    fn focus_draws_one_run() {
        let mut b = board(vec![pane(&fx::running())], Vec::new());
        b.focus = Some(fx::run_id());
        assert_eq!(b.rows().runs.len(), 1);

        b.focus = Some(RunId::default());
        assert!(b.rows().runs.is_empty());
        b.clamp();
        assert_eq!(b.focus, None);
    }
}
