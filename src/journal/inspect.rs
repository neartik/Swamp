//! The dispatch surface as data, the JSON shape docs/DISPATCH.md documents.

use crate::dispatch::policy::Ineligible;
use crate::ids::{CallSeq, DispatchId, NodeId};
use crate::journal::fold::{DispatchView, RunView, Scope, Totals};
use crate::model::core::{AccountId, Cost, NodeKind, NodeState, Provider, Tier, Usage};
use crate::model::dispatch::{DispatchState, Phase};
use crate::model::failure::Failure;
use crate::model::node::{ExitInfo, NodeRecord};
use serde::Serialize;
use time::OffsetDateTime;

/// Bumped with the journal schema the shape is folded from.
pub const JSON_SCHEMA: u32 = 2;

/// What the legacy bucket is called wherever an id would be printed.
pub const LEGACY: &str = "legacy";

#[derive(Debug, Clone, Serialize)]
pub struct DispatchList {
    pub schema: u32,
    pub run: Option<String>,
    pub dispatches: Vec<DispatchSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DispatchSummary {
    /// `dsp_…`, or `legacy` for the nodes a schema-1 journal recorded without a dispatch.
    pub id: String,
    pub short: String,
    pub state: DispatchState,
    pub call_seq: Option<CallSeq>,
    pub caller: Option<Caller>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub at: Option<OffsetDateTime>,
    pub age_s: Option<u64>,
    pub wait: Option<bool>,
    pub max_wait_s: Option<u64>,
    pub tasks: u32,
    pub counts: PhaseCounts,
    pub cost: Rollup,
}

#[derive(Debug, Clone, Serialize)]
pub struct Caller {
    #[serde(serialize_with = "prefixed")]
    pub node: NodeId,
    /// `brain`, or `task` for a worker that dispatched work of its own.
    pub kind: &'static str,
    #[serde(serialize_with = "prefixed_opt")]
    pub task: Option<NodeId>,
}

/// Every task of a dispatch by phase; a task counts once, under its current phase.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PhaseCounts {
    pub queued: u32,
    pub blocked: u32,
    pub leased: u32,
    pub running: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub rejected: u32,
}

impl PhaseCounts {
    fn add(&mut self, p: Phase) {
        let n = match p {
            Phase::Queued => &mut self.queued,
            Phase::Blocked => &mut self.blocked,
            Phase::Leased => &mut self.leased,
            Phase::Running => &mut self.running,
            Phase::Succeeded => &mut self.succeeded,
            Phase::Failed => &mut self.failed,
            Phase::Cancelled => &mut self.cancelled,
            Phase::Rejected => &mut self.rejected,
        };
        *n += 1;
    }

    /// Queued, blocked or leased: waiting for a process.
    pub fn waiting(&self) -> u32 {
        self.queued + self.blocked + self.leased
    }

    pub fn live(&self) -> u32 {
        self.waiting() + self.running
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Rollup {
    pub usd: f64,
    pub complete: bool,
    pub usage: Usage,
    pub nodes: u32,
    pub failed: u32,
    pub rejected: u32,
}

impl From<Totals> for Rollup {
    fn from(t: Totals) -> Self {
        Rollup {
            usd: t.cost_usd,
            complete: t.cost_complete,
            usage: t.usage,
            nodes: t.nodes,
            failed: t.failed,
            rejected: t.rejected,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DispatchDetail {
    pub schema: u32,
    pub run: Option<String>,
    pub dispatch: DispatchSummary,
    pub tasks: Vec<TaskDetail>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskDetail {
    #[serde(serialize_with = "prefixed")]
    pub node: NodeId,
    pub title: String,
    pub tier: Tier,
    pub dispatch: String,
    #[serde(serialize_with = "prefixed_opt")]
    pub parent: Option<NodeId>,
    pub depth: Option<u32>,
    pub state: Phase,
    pub detail: NodeState,
    pub elapsed_ms: Option<u64>,
    pub account: Option<AccountId>,
    pub model: Option<String>,
    pub provider: Option<Provider>,
    pub pid: Option<i32>,
    pub pgid: Option<i32>,
    pub attempts: Vec<AttemptDetail>,
    /// This task, its attempts and everything it dispatched in turn.
    pub cost: Rollup,
    pub blocked: Option<Blocked>,
    pub rejected: Option<Failure>,
    pub failure: Option<Failure>,
    pub transitions: Vec<Transition>,
    pub dispatches: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttemptDetail {
    #[serde(serialize_with = "prefixed")]
    pub node: NodeId,
    pub attempt: u32,
    pub state: Phase,
    pub account: Option<AccountId>,
    pub model: Option<String>,
    pub provider: Provider,
    pub pid: Option<i32>,
    pub pgid: Option<i32>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub ended_at: Option<OffsetDateTime>,
    pub elapsed_ms: Option<u64>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub exit: Option<ExitInfo>,
    pub failure: Option<Failure>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Blocked {
    #[serde(with = "time::serde::rfc3339")]
    pub until: OffsetDateTime,
    pub why: String,
    pub ineligible: Vec<Refusal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Refusal {
    pub account: AccountId,
    pub reason: Ineligible,
}

#[derive(Debug, Clone, Serialize)]
pub struct Transition {
    pub from: Phase,
    pub to: Phase,
    pub why: String,
}

pub fn list(view: &RunView, now: OffsetDateTime) -> DispatchList {
    DispatchList {
        schema: JSON_SCHEMA,
        run: run_id(view),
        dispatches: view
            .dispatches
            .values()
            .map(|d| summary(view, d, now))
            .collect(),
    }
}

pub fn detail(view: &RunView, id: DispatchId, now: OffsetDateTime) -> Option<DispatchDetail> {
    let d = view.dispatches.get(&id)?;
    Some(DispatchDetail {
        schema: JSON_SCHEMA,
        run: run_id(view),
        dispatch: summary(view, d, now),
        tasks: d.tasks.iter().filter_map(|t| task(view, *t, now)).collect(),
    })
}

pub fn summary(view: &RunView, d: &DispatchView, now: OffsetDateTime) -> DispatchSummary {
    let mut counts = PhaseCounts::default();
    for t in &d.tasks {
        if let Some(s) = view.state_of(*t) {
            counts.add(Phase::from(&s));
        }
    }
    let at = d
        .record
        .as_ref()
        .map(|r| r.at)
        .or_else(|| view.header.as_ref().map(|h| h.started_at));
    DispatchSummary {
        id: label(d.id),
        short: short(d.id),
        state: d.state,
        call_seq: d.record.as_ref().and_then(|r| r.call_seq),
        caller: d.record.as_ref().map(|r| caller(view, r.caller)),
        at,
        age_s: at.map(|a| secs(now - a)),
        wait: d.record.as_ref().map(|r| r.wait),
        max_wait_s: d.record.as_ref().and_then(|r| r.max_wait_s),
        tasks: d.tasks.len() as u32,
        counts,
        cost: view.rollup(Scope::Dispatch(d.id)).into(),
    }
}

/// One task by its logical id or any attempt's.
pub fn task(view: &RunView, id: NodeId, now: OffsetDateTime) -> Option<TaskDetail> {
    let attempts = view.attempts(id);
    let logical = attempts.first().map_or(id, |n| n.logical);
    let t = view.tasks.get(&logical);
    let state = view.state_of(logical)?;
    let latest = attempts.last().copied();
    let (pid, pgid) = process(latest);
    let started = attempts.iter().filter_map(|a| a.started_at).min();
    let ended = attempts.iter().filter_map(|a| a.ended_at).max();
    let blocked = match &state {
        NodeState::Blocked { until, why } => Some(Blocked {
            until: *until,
            why: why.clone(),
            ineligible: t
                .map(|t| {
                    t.ineligible
                        .iter()
                        .map(|(account, reason)| Refusal {
                            account: account.clone(),
                            reason: *reason,
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }),
        _ => None,
    };
    Some(TaskDetail {
        node: logical,
        title: latest
            .map(|n| n.title.clone())
            .or_else(|| t.map(|t| t.title.clone()))
            .unwrap_or_default(),
        tier: latest
            .map(|n| n.tier)
            .or(t.map(|t| t.tier))
            .unwrap_or(Tier::Mid),
        dispatch: t.map_or_else(|| LEGACY.to_owned(), |t| label(t.dispatch)),
        parent: t.and_then(|t| t.parent).or(latest.and_then(|n| n.parent)),
        depth: t
            .and_then(|t| t.depth)
            .or(latest.filter(|n| n.dispatch.is_some()).map(|n| n.depth)),
        state: Phase::from(&state),
        elapsed_ms: started.map(|s| {
            let end = if state.is_terminal() { ended } else { None };
            millis(end.unwrap_or(now) - s)
        }),
        account: latest.and_then(|n| n.account.clone()),
        model: latest.and_then(|n| n.model.clone()),
        provider: latest.map(|n| n.provider),
        pid,
        pgid,
        attempts: attempts.iter().map(|a| attempt(a, now)).collect(),
        cost: view.rollup(Scope::Subtree(logical)).into(),
        blocked,
        rejected: match &state {
            NodeState::Rejected { reason } => Some(reason.clone()),
            _ => None,
        },
        failure: match &state {
            NodeState::Failed { failure } => Some(failure.clone()),
            _ => None,
        },
        transitions: view
            .transitions
            .get(&logical)
            .into_iter()
            .flatten()
            .map(|t| Transition {
                from: t.from,
                to: Phase::from(&t.to),
                why: t.why.clone(),
            })
            .collect(),
        dispatches: issued_by(view, logical).into_iter().map(label).collect(),
        detail: state,
    })
}

/// Dispatches whose caller is `logical` or one of its attempts, oldest first.
pub fn issued_by(view: &RunView, logical: NodeId) -> Vec<DispatchId> {
    view.dispatches
        .values()
        .filter_map(|d| {
            let caller = d.record.as_ref()?.caller;
            let of = view.attempts(caller).first().map_or(caller, |n| n.logical);
            (of == logical).then_some(d.id)
        })
        .collect()
}

pub fn label(id: DispatchId) -> String {
    if id == DispatchId::LEGACY {
        LEGACY.to_owned()
    } else {
        id.to_string()
    }
}

pub fn short(id: DispatchId) -> String {
    if id == DispatchId::LEGACY {
        LEGACY.to_owned()
    } else {
        id.short()
    }
}

/// The run's own brain, or the task a nested caller belongs to.
pub fn caller(view: &RunView, node: NodeId) -> Caller {
    let run_root = view.header.as_ref().is_some_and(|h| h.run.0 == node.0);
    let rec = view.nodes.get(&node);
    if run_root || rec.is_some_and(|n| n.kind == NodeKind::Brain) {
        return Caller {
            node,
            kind: "brain",
            task: None,
        };
    }
    Caller {
        node,
        kind: "task",
        task: Some(rec.map_or(node, |n| n.logical)),
    }
}

fn attempt(n: &NodeRecord, now: OffsetDateTime) -> AttemptDetail {
    let (pid, pgid) = process(Some(n));
    AttemptDetail {
        node: n.id,
        attempt: n.attempt,
        state: Phase::from(&n.state),
        account: n.account.clone(),
        model: n.model.clone(),
        provider: n.provider,
        pid,
        pgid,
        started_at: n.started_at,
        ended_at: n.ended_at,
        elapsed_ms: n.started_at.map(|s| millis(n.ended_at.unwrap_or(now) - s)),
        usage: n.usage,
        cost: n.cost,
        exit: n.exit,
        failure: match &n.state {
            NodeState::Failed { failure } => Some(failure.clone()),
            _ => None,
        },
    }
}

/// The pid and group of an attempt, from the last `ProcessStarted` the fold kept.
fn process(n: Option<&NodeRecord>) -> (Option<i32>, Option<i32>) {
    match n.map(|n| &n.state) {
        Some(NodeState::Running { pid, pgid, .. }) => (Some(*pid), Some(*pgid)),
        Some(NodeState::Orphaned { pid, .. }) => (Some(*pid), Some(*pid)),
        _ => (None, None),
    }
}

fn run_id(view: &RunView) -> Option<String> {
    view.header.as_ref().map(|h| h.run.to_string())
}

fn secs(d: time::Duration) -> u64 {
    d.whole_seconds().max(0) as u64
}

fn millis(d: time::Duration) -> u64 {
    d.whole_milliseconds().max(0) as u64
}

/// Dispatches `spec` names in this run: a full id, a prefix, the short form, or `legacy`.
pub fn match_dispatches(view: &RunView, spec: &str) -> Vec<DispatchId> {
    let spec = spec.trim();
    if spec.eq_ignore_ascii_case(LEGACY) {
        return view
            .dispatches
            .keys()
            .copied()
            .filter(|d| *d == DispatchId::LEGACY)
            .collect();
    }
    view.dispatches
        .keys()
        .copied()
        .filter(|d| *d != DispatchId::LEGACY && d.matches(spec))
        .collect()
}

/// Logical tasks `spec` names, by the logical id or any attempt's: full, prefix or short.
pub fn match_tasks(view: &RunView, spec: &str) -> Vec<NodeId> {
    let spec = spec.trim();
    let mut out: Vec<NodeId> = Vec::new();
    let mut push = |id: NodeId| {
        if !out.contains(&id) {
            out.push(id);
        }
    };
    for t in view.tasks.keys().filter(|t| t.matches(spec)) {
        push(*t);
    }
    for n in view.nodes.values().filter(|n| n.kind != NodeKind::Brain) {
        if n.id.matches(spec) || n.logical.matches(spec) {
            push(n.logical);
        }
    }
    out
}

/// Ids go out as they are printed everywhere else, `nd_…`, so they can be passed straight back.
fn prefixed<S: serde::Serializer>(id: &NodeId, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(id)
}

fn prefixed_opt<S: serde::Serializer>(id: &Option<NodeId>, s: S) -> Result<S::Ok, S::Error> {
    match id {
        Some(id) => s.collect_str(id),
        None => s.serialize_none(),
    }
}
