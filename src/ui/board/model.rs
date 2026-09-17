//! WP2: what a board frame is made of. Pure by construction: every byte arrives through a
//! loader the caller hands in, so the whole model is testable without a filesystem.

use crate::dispatch::policy::{Scoring, SelectionPolicy};
use crate::ids::{NodeId, RunId};
use crate::journal::fold::{Projection, RunView, TreeRow};
use crate::journal::paths::RunPaths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{AccountId, Cost, NodeState, Provider, Tier, Usage};
use crate::ui::board::sources::Tail;
use crate::ui::fmt;
use crate::ui::usage::AccountRow;
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;
use time::OffsetDateTime;

/// Folding every live journal is linear in the number of runs, so the board tails at most
/// this many, newest first, and says in its header how many it dropped.
pub const MAX_RUNS: usize = 8;

/// How many terminal nodes the `recent` section keeps.
pub const RECENT: usize = 8;

/// How long a terminal node stays in `recent` before it is dropped.
pub const RECENT_TTL: StdDuration = StdDuration::from_secs(5 * 60);

/// How many rows the sections with no natural bound draw. A fan-out batch queues hundreds of
/// nodes at once; past this the heading counts them and a frame stays a fixed cost.
pub const SECTION_MAX: usize = 32;

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

    /// The note the footer shows for a collapsed row: the newest attempt that recorded one.
    pub fn note_for(&self, logical: NodeId) -> Option<&SelectionNote> {
        let attempts = self.view.by_logical.get(&logical)?;
        attempts.iter().rev().find_map(|a| self.selection.get(a))
    }

    /// `view.tree()` collapsed by logical id, taking the last attempt as the live record:
    /// the same `latest()` rule `trace.rs` uses, so a retry changes the id in place.
    pub fn rows(&self) -> Vec<NodeRow> {
        self.view
            .tree()
            .iter()
            .filter_map(|r| self.row(r))
            .collect()
    }

    fn row(&self, r: &TreeRow) -> Option<NodeRow> {
        let n = r
            .attempts
            .iter()
            .rev()
            .find_map(|a| self.view.nodes.get(a))?;
        Some(NodeRow {
            run: self.run,
            logical: r.logical,
            id: n.id,
            attempt: n.attempt,
            brain: n.logical == self.brain,
            provider: n.provider,
            account: n.account.clone(),
            tier: n.tier,
            model: n.model.clone(),
            title: fmt::sanitize(&n.title),
            state: n.state.clone(),
            created_at: n.created_at,
            started_at: n.started_at,
            ended_at: n.ended_at,
            usage: n.usage,
            cost: n.cost,
            stale: self.stale.is_some(),
        })
    }
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

/// Where a node is drawn. `Orphaned` stays in flight: its spinner freezes, it does not move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    InFlight,
    Waiting,
    Recent,
}

pub fn section_of(state: &NodeState) -> Section {
    match state {
        NodeState::Running { .. } | NodeState::Leased { .. } | NodeState::Orphaned { .. } => {
            Section::InFlight
        }
        NodeState::Queued | NodeState::Blocked { .. } => Section::Waiting,
        NodeState::Succeeded | NodeState::Failed { .. } | NodeState::Cancelled { .. } => {
            Section::Recent
        }
    }
}

/// One collapsed node row. Every string is already sanitized: a title comes from a model.
#[derive(Debug, Clone)]
pub struct NodeRow {
    pub run: RunId,
    pub logical: NodeId,
    /// The live attempt: the id `swamp diff` and `swamp adopt` take.
    pub id: NodeId,
    pub attempt: u32,
    pub brain: bool,
    pub provider: Provider,
    pub account: Option<AccountId>,
    pub tier: Tier,
    pub model: Option<String>,
    pub title: String,
    pub state: NodeState,
    /// When the node was queued: what a row that has not started yet counts from.
    pub created_at: OffsetDateTime,
    pub started_at: Option<OffsetDateTime>,
    pub ended_at: Option<OffsetDateTime>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub stale: bool,
}

impl NodeRow {
    /// Recomputed every frame, never accumulated: time on the account once a node started,
    /// and time spent waiting before that, which is the number a queued row is judged on.
    pub fn elapsed(&self, now: OffsetDateTime) -> Option<StdDuration> {
        let from = self.started_at.unwrap_or(self.created_at);
        (self.ended_at.unwrap_or(now) - from).try_into().ok()
    }

    pub fn section(&self) -> Section {
        section_of(&self.state)
    }
}

/// One account and the nodes it is running right now.
#[derive(Debug, Clone)]
pub struct AccountGroup {
    pub row: AccountRow,
    pub nodes: Vec<NodeRow>,
}

#[derive(Debug, Clone)]
pub struct ProviderGroup {
    pub provider: Option<Provider>,
    pub accounts: Vec<AccountGroup>,
}

/// Everything a frame draws, in draw order.
#[derive(Debug, Clone, Default)]
pub struct Rows {
    pub providers: Vec<ProviderGroup>,
    /// In flight on an account no `accounts.json` entry names, so it has no group of its own.
    pub orphan_nodes: Vec<NodeRow>,
    pub waiting: Vec<NodeRow>,
    pub recent: Vec<NodeRow>,
    /// In flight across every tailed run, `focus` included: what is drawn is a view, what an
    /// account is carrying is a fact.
    pub in_flight: usize,
    /// How many rows the two capped sections had before `SECTION_MAX`.
    pub orphan_total: usize,
    pub waiting_total: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Summary {
    pub runs: usize,
    pub hidden_runs: usize,
    pub stale_runs: usize,
    pub in_flight: usize,
    pub waiting: usize,
    pub accounts: usize,
    pub cost_usd: f64,
    pub cost_complete: bool,
}

// ---------------------------------------------------------------- board

/// What the cursor is on. Held by identity, not by index, so a rediscovery or a new node
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
}

impl Selection {
    pub fn run(&self) -> Option<RunId> {
        match self {
            Selection::Node { run, .. } => Some(*run),
            _ => None,
        }
    }
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
    /// `tab`: draw only this run's nodes. `None` merges every tailed run.
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

    /// Drops a selection or a focus that names a run the board no longer tails.
    pub fn clamp(&mut self) {
        if self.focus.is_some_and(|r| self.pane(r).is_none()) {
            self.focus = None;
        }
        if let Selection::Node { run, logical } = &self.selected {
            let gone = self
                .pane(*run)
                .is_none_or(|p| !p.view.by_logical.contains_key(logical));
            if gone {
                self.selected = Selection::None;
            }
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

    /// Provider -> account -> its in-flight nodes, then waiting, then recent.
    pub fn rows(&self) -> Rows {
        let mut by_account: BTreeMap<AccountId, Vec<NodeRow>> = BTreeMap::new();
        let mut loose: Vec<NodeRow> = Vec::new();
        let mut waiting: Vec<NodeRow> = Vec::new();
        let mut recent: Vec<NodeRow> = Vec::new();
        for pane in self.panes() {
            for row in pane.rows() {
                match row.section() {
                    Section::InFlight => match &row.account {
                        Some(id) => by_account.entry(id.clone()).or_default().push(row),
                        None => loose.push(row),
                    },
                    Section::Waiting => waiting.push(row),
                    Section::Recent => recent.push(row),
                }
            }
        }

        let (carried, in_flight) = self.in_flight_counts(&by_account, &loose);
        let mut groups: Vec<(Option<Provider>, Vec<AccountGroup>)> = Vec::new();
        for row in &self.accounts {
            let nodes = by_account.remove(&row.account).unwrap_or_default();
            let mut row = row.clone();
            // `persist::merge_state` zeroes `inflight` in the file, because it is one
            // process's runtime state: the only honest count is the one the journals show.
            row.inflight = carried.get(&row.account).copied().unwrap_or(0);
            let slot = match groups.iter_mut().find(|(p, _)| *p == row.provider) {
                Some(slot) => slot,
                None => {
                    groups.push((row.provider, Vec::new()));
                    groups.last_mut().expect("just pushed")
                }
            };
            slot.1.push(AccountGroup { row, nodes });
        }
        // `None` is the `not in config` group and sorts last, the same rule `ui::usage` uses.
        groups.sort_by_key(|(p, _)| (p.is_none(), *p));
        loose.extend(by_account.into_values().flatten());

        recent.sort_by_key(|r| std::cmp::Reverse(r.ended_at));
        recent.retain(|r| {
            r.ended_at.is_none_or(|e| {
                (self.now - e)
                    .try_into()
                    .is_ok_and(|age: StdDuration| age <= RECENT_TTL)
            })
        });
        recent.truncate(RECENT);

        // Longest wait first, so the rows a cap hides are the ones that just arrived.
        waiting.sort_by_key(|r| (r.created_at, r.id));
        let waiting_total = waiting.len();
        let orphan_total = loose.len();
        waiting.truncate(SECTION_MAX);
        loose.truncate(SECTION_MAX);

        Rows {
            providers: groups
                .into_iter()
                .map(|(provider, accounts)| ProviderGroup { provider, accounts })
                .collect(),
            orphan_nodes: loose,
            waiting,
            recent,
            in_flight,
            orphan_total,
            waiting_total,
        }
    }

    /// In-flight nodes per account, and in total, over every tailed run. `focus` narrows what
    /// a frame draws; an account running three nodes in the run `tab` hid is still at three.
    fn in_flight_counts(
        &self,
        drawn: &BTreeMap<AccountId, Vec<NodeRow>>,
        loose: &[NodeRow],
    ) -> (BTreeMap<AccountId, usize>, usize) {
        if self.focus.is_none() {
            let counts = drawn.iter().map(|(id, n)| (id.clone(), n.len())).collect();
            let total = drawn.values().map(Vec::len).sum::<usize>() + loose.len();
            return (counts, total);
        }
        let mut counts: BTreeMap<AccountId, usize> = BTreeMap::new();
        let mut total = 0usize;
        for pane in &self.runs {
            for row in pane.rows() {
                if row.section() != Section::InFlight {
                    continue;
                }
                total += 1;
                if let Some(id) = &row.account {
                    *counts.entry(id.clone()).or_default() += 1;
                }
            }
        }
        (counts, total)
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
            in_flight: rows.in_flight,
            waiting: rows.waiting_total,
            accounts: self.accounts.len(),
            cost_usd,
            cost_complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::core::{NodeKind, WorkspaceRef};
    use crate::model::node::NodeRecord;
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

    fn queued(seq: u64, node: NodeId, title: &str) -> JournalLine {
        JournalLine {
            seq,
            at: fx::at(seq as i64),
            run: fx::run_id(),
            node: Some(node),
            event: JournalEvent::NodeSpawned {
                node: Box::new(NodeRecord {
                    id: node,
                    run_id: fx::run_id(),
                    parent: Some(fx::id(0)),
                    logical: node,
                    attempt: 1,
                    retry_of: None,
                    kind: NodeKind::Worker,
                    title: title.to_owned(),
                    prompt_path: Utf8PathBuf::from("prompt.md"),
                    prompt_sha256: String::new(),
                    provider: Provider::Anthropic,
                    account: None,
                    exec: None,
                    argv: Vec::new(),
                    model: None,
                    tier: Tier::Mid,
                    workspace: WorkspaceRef::ReadOnly {
                        path: Utf8PathBuf::from("/repo"),
                    },
                    session: None,
                    state: NodeState::Queued,
                    created_at: fx::at(seq as i64),
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
                }),
            },
        }
    }

    /// The whole point of the board: account -> what that account is working on.
    #[test]
    fn in_flight_nodes_group_under_the_account_running_them() {
        let b = board(
            vec![pane(&fx::running())],
            vec![
                account_row("main", Some(Provider::Anthropic)),
                account_row("alt", Some(Provider::Anthropic)),
                account_row("codex-main", Some(Provider::Openai)),
            ],
        );
        let rows = b.rows();

        assert_eq!(rows.providers.len(), 2);
        assert_eq!(rows.providers[0].provider, Some(Provider::Anthropic));
        let main = &rows.providers[0].accounts[0];
        assert_eq!(main.row.account.0, "main");
        // The brain leases an account like any worker, and it is drawn first.
        let titles: Vec<&str> = main.nodes.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, vec!["brain", "add pagination to /users"]);
        assert!(main.nodes[0].brain);
        // The file's own `inflight` is always zero; the count comes from the journals.
        assert_eq!(main.row.inflight, 2);
        assert_eq!(rows.providers[0].accounts[1].row.inflight, 1);
        assert_eq!(rows.providers[1].accounts[0].row.inflight, 0);
        assert!(rows.orphan_nodes.is_empty());

        let s = b.summary(&rows);
        assert_eq!(s.in_flight, 3);
        assert_eq!(s.accounts, 3);
    }

    /// An account `accounts.json` has never heard of still has to show its work.
    #[test]
    fn a_node_on_an_unknown_account_is_never_dropped() {
        let b = board(vec![pane(&fx::running())], Vec::new());
        let rows = b.rows();
        assert!(rows.providers.is_empty());
        assert_eq!(rows.orphan_nodes.len(), 3);
    }

    #[test]
    fn terminal_nodes_move_to_recent_and_age_out() {
        let mut b = board(
            vec![pane(&fx::fixture())],
            vec![account_row("main", Some(Provider::Anthropic))],
        );
        let rows = b.rows();
        assert_eq!(rows.recent.len(), 2);
        // Newest ended first.
        assert_eq!(rows.recent[0].ended_at, Some(fx::at(140)));
        assert_eq!(rows.providers[0].accounts[0].nodes.len(), 1, "brain only");

        b.now = fx::at(140) + time::Duration::seconds(RECENT_TTL.as_secs() as i64 + 1);
        assert!(b.rows().recent.is_empty());
    }

    #[test]
    fn a_queued_node_waits() {
        let mut lines = fx::running();
        lines.push(queued(9, fx::id(7), "rebuild the index"));
        let b = board(
            vec![pane(&lines)],
            vec![account_row("main", Some(Provider::Anthropic))],
        );
        let rows = b.rows();
        assert_eq!(rows.waiting.len(), 1);
        assert_eq!(rows.waiting[0].title, "rebuild the index");
        // A node that has not started yet still has a wait, and it is the number that says
        // how badly the pool is stuck: §3.1 shows it in the elapsed cell.
        assert_eq!(
            rows.waiting[0].elapsed(b.now),
            Some(StdDuration::from_secs(191))
        );
    }

    /// A backlog is unbounded; a frame is not. The heading keeps the true total.
    #[test]
    fn the_waiting_section_is_capped_and_says_so() {
        let mut lines = fx::running();
        for n in 0..(SECTION_MAX as u64 + 5) {
            lines.push(queued(10 + n, fx::id(10 + n as u8), "rebuild the index"));
        }
        let b = board(
            vec![pane(&lines)],
            vec![account_row("main", Some(Provider::Anthropic))],
        );
        let rows = b.rows();
        assert_eq!(rows.waiting.len(), SECTION_MAX);
        assert_eq!(rows.waiting_total, SECTION_MAX + 5);
        assert_eq!(b.summary(&rows).waiting, SECTION_MAX + 5);
        // Oldest wait first, so the rows the cap hides are the ones that just arrived.
        assert!(rows.waiting[0].created_at <= rows.waiting[1].created_at);
    }

    /// `tab` chooses what is drawn, never what is true: an account at capacity in the run the
    /// focus hid must not read as having headroom here.
    #[test]
    fn focus_never_shrinks_an_account_count() {
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
            vec![pane(&fx::running()), second],
            vec![account_row("main", Some(Provider::Anthropic))],
        );

        let merged = b.rows();
        assert_eq!(merged.providers[0].accounts[0].row.inflight, 4);
        assert_eq!(merged.in_flight, 6);

        b.focus = Some(fx::run_id());
        let rows = b.rows();
        assert_eq!(
            rows.providers[0].accounts[0].nodes.len(),
            2,
            "one run drawn"
        );
        assert_eq!(
            rows.providers[0].accounts[0].row.inflight, 4,
            "both runs counted"
        );
        assert_eq!(b.summary(&rows).in_flight, 6);
    }

    #[test]
    fn elapsed_is_recomputed_from_started_at() {
        let b = board(
            vec![pane(&fx::running())],
            vec![account_row("main", Some(Provider::Anthropic))],
        );
        let row = &b.rows().providers[0].accounts[0].nodes[1];
        assert_eq!(row.elapsed(b.now), Some(StdDuration::from_secs(190)));
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

    /// Notes are keyed by attempt; a retry's own note is the one the footer shows.
    #[test]
    fn the_newest_attempt_owns_the_note() {
        let mut lines = fx::running();
        lines.push(selected(9, fx::id(2), "score 0.9000", &[]));
        let mut pane = pane(&lines);
        // A retry keeps the logical id and takes a new attempt id.
        let mut retry = match &lines[3].event {
            JournalEvent::NodeSpawned { node } => (**node).clone(),
            _ => unreachable!("fixture line 3 spawns a node"),
        };
        retry.id = fx::id(8);
        retry.attempt = 2;
        pane.apply(&[
            JournalLine {
                seq: 10,
                at: fx::at(10),
                run: fx::run_id(),
                node: Some(fx::id(8)),
                event: JournalEvent::NodeSpawned {
                    node: Box::new(retry),
                },
            },
            selected(11, fx::id(8), "score .41 = util .93×.50", &[]),
        ]);

        let note = pane
            .note_for(fx::id(2))
            .expect("a note for the logical row");
        assert_eq!(note.reason.form, ReasonForm::Terms);
        let row = pane
            .rows()
            .into_iter()
            .find(|r| r.logical == fx::id(2))
            .expect("the collapsed row");
        assert_eq!(row.id, fx::id(8), "the live attempt is the one diff takes");
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
        // Orphaned is not terminal: the node stays in flight with a frozen glyph.
        assert_eq!(pane.rows()[0].section(), Section::InFlight);
        assert!(pane.rows()[0].stale);

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

    /// `tab` narrows every section, not just the tree.
    #[test]
    fn focus_draws_one_run() {
        let mut b = board(
            vec![pane(&fx::running())],
            vec![account_row("main", Some(Provider::Anthropic))],
        );
        b.focus = Some(fx::run_id());
        assert_eq!(b.rows().providers[0].accounts[0].nodes.len(), 2);

        b.focus = Some(RunId::default());
        assert_eq!(b.rows().providers[0].accounts[0].nodes.len(), 0);
        b.clamp();
        assert_eq!(b.focus, None);
    }
}
