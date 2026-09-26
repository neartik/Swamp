use crate::ids::{CallSeq, DispatchId, NodeId};
use crate::journal::fold::RunView;
use crate::journal::inspect::{self, Rollup, TaskDetail};
use crate::model::core::{NodeState, Tier};
use crate::model::dispatch::DispatchState;
use crate::model::node::NodeRecord;
use crate::ui::chat::spinner;
use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::order::{self, Cell, Drop, SEP, Tally};
use crate::ui::{dispatches, fmt, trace, usage};
use ratatui::text::{Line, Span};
use std::collections::BTreeSet;
use std::time::Duration;
use time::OffsetDateTime;

/// Where a board row starts: two spaces, the connector, two spaces.
pub const BODY: u16 = 5;
const ID_WIDTH: usize = 8;

const ACCOUNT_WIDTH: usize = 18;
const ELAPSED_WIDTH: usize = 6;
const COST_WIDTH: usize = 7;
/// Everything but the title; the title takes what is left.
const FIXED: u16 = 62;
pub const MAX_ROWS: usize = 8;

#[derive(Debug, Clone)]
pub struct WorkerRow {
    pub logical: NodeId,
    /// The live attempt, or the logical id before there is one.
    pub id: NodeId,
    /// 0 before the first attempt.
    pub attempt: u32,
    pub title: String,
    pub tier: Tier,
    pub account: String,
    pub state: NodeState,
    pub elapsed: Option<Duration>,
    /// What the row prints, blank when nothing reported a cost.
    pub cost: String,
    /// For a batch with no dispatch behind it, the sum it shows.
    pub usd: Option<f64>,
    /// The `└` lines a live block shows: earlier attempts, the wait, the refusal, the failure.
    pub notes: Vec<(String, Role)>,
    /// `branch {b}   +{i} -{d}   {n} files`, exactly as `swamp trace` words it.
    pub branch: Option<String>,
}

impl WorkerRow {
    pub fn terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// `9g5f09·2` for a retry.
    pub fn short(&self) -> String {
        if self.attempt > 1 {
            format!("{}\u{b7}{}", self.id.short(), self.attempt)
        } else {
            self.id.short()
        }
    }
}

/// One `swamp_dispatch` call and the tasks it owns. Bound to its `DispatchId` once the fold
/// has one; before that, and for a schema-1 run, it owns the brain's children as they appear.
/// A batch with no call behind it is `loose`: a retry, a resumed node, a dispatch the call
/// was never seen for.
#[derive(Debug, Clone)]
pub struct Batch {
    pub tool_id: String,
    /// The call's own head line, so the board and the tool row are one block.
    pub name: String,
    pub preview: String,
    pub ok: Option<bool>,
    pub started: OffsetDateTime,
    pub owned: BTreeSet<NodeId>,
    pub expected: Option<usize>,
    /// The tool call returned: no new node joins this batch.
    pub closed: bool,
    pub loose: bool,
    pub expanded: bool,
    pub dispatch: Option<DispatchId>,
    pub rows: Vec<WorkerRow>,
    pub elapsed: Duration,
    seq: Option<CallSeq>,
    settled: bool,
    rollup: Option<Rollup>,
}

impl Batch {
    pub fn new(tool_id: String, expected: Option<usize>, started: OffsetDateTime) -> Self {
        Batch {
            tool_id,
            name: "swamp_dispatch".to_owned(),
            preview: String::new(),
            ok: None,
            started,
            owned: BTreeSet::new(),
            expected,
            closed: false,
            loose: false,
            expanded: false,
            dispatch: None,
            rows: Vec::new(),
            elapsed: Duration::ZERO,
            seq: None,
            settled: false,
            rollup: None,
        }
    }

    pub fn loose(started: OffsetDateTime) -> Self {
        Batch {
            loose: true,
            ..Batch::new(String::new(), None, started)
        }
    }

    /// A dispatch whose call this chat never saw: nothing more will join it.
    pub fn for_dispatch(id: DispatchId, started: OffsetDateTime) -> Self {
        Batch {
            dispatch: Some(id),
            closed: true,
            ..Batch::loose(started)
        }
    }

    /// Rows are rebuilt from the fold every poll: elapsed is recomputed, never accumulated.
    pub fn refresh(&mut self, view: &RunView, now: OffsetDateTime) {
        let rows: Vec<WorkerRow> = match self.dispatch {
            Some(id) => match inspect::detail(view, id, now) {
                Some(d) => {
                    self.seq = d.dispatch.call_seq;
                    self.settled = d.dispatch.state == DispatchState::Settled;
                    self.rollup = Some(d.dispatch.cost);
                    let at = d.dispatch.at;
                    d.tasks.iter().map(|t| task_row(view, t, at, now)).collect()
                }
                None => Vec::new(),
            },
            None => self
                .owned
                .iter()
                .filter_map(|logical| row(view, *logical, now))
                .collect(),
        };
        let mut ranked: Vec<(usize, WorkerRow)> = rows.into_iter().enumerate().collect();
        ranked.sort_by_key(|(i, r)| (order::rank(&r.state), *i));
        self.rows = ranked.into_iter().map(|(_, r)| r).collect();
        // A batch is at least as old as its oldest task: a resumed run has tasks that started
        // before this process ever opened the board.
        let oldest = self.rows.iter().filter_map(|r| r.elapsed).max();
        let since: Duration = (now - self.started).try_into().unwrap_or(Duration::ZERO);
        self.elapsed = oldest.unwrap_or(Duration::ZERO).max(since);
    }

    /// Committed only when the call is done and its dispatch settled, or, with no dispatch
    /// behind it, every owned task is terminal.
    pub fn done(&self) -> bool {
        if self.dispatch.is_some() {
            return self.closed && self.settled;
        }
        self.closed && !self.rows.is_empty() && self.rows.iter().all(WorkerRow::terminal)
    }

    pub fn tally(&self) -> Tally {
        let mut t = Tally::default();
        for r in &self.rows {
            t.add(&r.state);
        }
        t
    }

    /// Running or leased tasks, what the working line and `esc` count.
    pub fn running(&self) -> usize {
        self.tally().running
    }

    /// The dispatch's rollup, or the rows' own costs summed.
    fn cost(&self) -> String {
        if let Some(r) = &self.rollup {
            let cost = dispatches::cost(r);
            return if cost == "-" { String::new() } else { cost };
        }
        let mut usd = 0.0;
        let mut complete = true;
        for r in &self.rows {
            match r.usd {
                Some(c) => usd += c,
                None => complete = false,
            }
        }
        format!("~${usd:.2}{}", if complete { "" } else { "+" })
    }

    /// `#1 9g5f18`, or the short id without a call number.
    fn label(&self) -> Vec<Cell> {
        let Some(id) = self.dispatch else {
            return Vec::new();
        };
        match self.seq {
            Some(seq) => vec![
                Cell::new("label", format!("#{seq}"), Role::Name),
                Cell::new("id", id.short(), Role::Meta).joined(),
            ],
            None => vec![Cell::new("label", id.short(), Role::Name)],
        }
    }

    pub fn render(&self, width: u16, t: &Theme, tick: u64, committed: bool) -> Vec<Line<'static>> {
        let mut out = vec![self.head(width, t), self.headline(width, t, tick)];
        let full = committed || self.expanded;
        let shown = if full {
            self.rows.len()
        } else {
            self.rows.len().min(MAX_ROWS)
        };
        for (i, row) in self.rows.iter().take(shown).enumerate() {
            out.push(self.row_line(row, i, width, t, tick));
            out.extend(self.details(row, full, width, t));
        }
        let hidden = self.rows.len() - shown;
        if hidden > 0 {
            out.push(Line::from(t.span(
                format!("     … +{hidden} more (ctrl+o to expand)"),
                Role::Meta,
            )));
        }
        if committed {
            out.push(self.totals(width, t));
        }
        out
    }

    /// `● swamp_dispatch(2 tasks)`, or `● workers` for tasks no call claimed.
    fn head(&self, width: u16, t: &Theme) -> Line<'static> {
        let role = match self.ok {
            _ if !self.closed => Role::Run,
            Some(true) | None => Role::Ok,
            Some(false) => Role::Err,
        };
        if self.loose {
            return Line::from(vec![
                t.span(format!("{} ", t.g(Glyph::Bullet)), role),
                t.span("workers".to_owned(), Role::Name),
            ]);
        }
        let arg = crate::ui::chat::blocks::tool_args::preview(&self.name, &self.preview, width);
        Line::from(vec![
            t.span(format!("{} ", t.g(Glyph::Bullet)), role),
            t.span(self.name.clone(), Role::Name),
            t.span(format!("({arg})"), Role::Meta),
        ])
    }

    /// `⎿  ⠹ #1 9g5f18 · 2 running · 1 blocked · ~$0.34 · 3m20s`, cut to fit.
    fn headline(&self, width: u16, t: &Theme, tick: u64) -> Line<'static> {
        let lead = vec![
            t.span("  ", Role::Text),
            t.span(t.g(Glyph::Connector), Role::Meta),
            t.span("  ", Role::Text),
        ];
        // Nothing has reached the fold yet: the call is out, the tasks are not.
        if self.rows.is_empty() && !self.closed {
            let expected = self
                .expected
                .map(|n| format!("{n} queued"))
                .unwrap_or_else(|| "queued".to_owned());
            let mut spans = lead;
            spans.push(t.span(spinner::worker_frame(t, tick, 0).to_owned(), Role::Run));
            spans.push(t.span(format!(" {expected} · starting…"), Role::Meta));
            return Line::from(spans);
        }
        let tally = self.tally();
        let (glyph, role) = if tally.running > 0 {
            (spinner::worker_frame(t, tick, 0).to_owned(), Role::Run)
        } else if tally.blocked > 0 {
            (t.g(Glyph::Blocked).to_owned(), Role::Err)
        } else if tally.queued > 0 {
            (t.g(Glyph::Queued).to_owned(), Role::Meta)
        } else if tally.done > 0 && tally.done == tally.total() {
            (t.g(Glyph::Succeeded).to_owned(), Role::Ok)
        } else if tally.rejected > 0 && tally.rejected == tally.total() {
            (t.g(Glyph::Rejected).to_owned(), Role::Err)
        } else {
            (t.g(Glyph::Failed).to_owned(), Role::Err)
        };
        let mut cells = self.label();
        cells.extend(tally.cells());
        let cost = self.cost();
        if !cost.is_empty() {
            cells.push(Cell::new("cost", cost, Role::Meta));
        }
        cells.push(Cell::new(
            "elapsed",
            fmt::duration(self.elapsed),
            Role::Meta,
        ));
        let room = (width as usize).saturating_sub(7);
        let cells = order::fit(
            cells,
            room,
            &[Drop::Tail, Drop::Key("id"), Drop::Key("cost")],
        );
        let mut spans = lead;
        spans.push(t.span(glyph, role));
        spans.push(t.span(" ", Role::Text));
        spans.extend(order::spans(&cells, t, room));
        Line::from(spans)
    }

    fn row_line(&self, r: &WorkerRow, i: usize, width: u16, t: &Theme, tick: u64) -> Line<'static> {
        let cols = Columns::for_width(width);
        let glyph = match r.state {
            NodeState::Running { .. } => spinner::worker_frame(t, tick, i).to_owned(),
            _ => t.state_glyph(&r.state).to_owned(),
        };
        let mut spans = vec![
            Span::raw(" ".repeat(BODY as usize)),
            t.span(glyph, t.state_role(&r.state)),
            Span::raw(" "),
            t.span(fmt::pad(&r.short(), ID_WIDTH), Role::Meta),
            Span::raw("  "),
        ];
        if cols.tier {
            let role = if r.tier == Tier::High {
                Role::TierHi
            } else {
                Role::Meta
            };
            spans.push(t.span(format!("[{}]", fmt::pad(r.tier.as_str(), 4)), role));
            spans.push(Span::raw("  "));
        }
        let title_role = if r.terminal() { Role::Meta } else { Role::Name };
        spans.push(t.span(fmt::pad(&r.title, cols.flex as usize), title_role));
        spans.push(Span::raw("  "));
        if cols.account {
            spans.push(t.span(fmt::pad(&r.account, ACCOUNT_WIDTH), Role::Meta));
            spans.push(Span::raw("  "));
        }
        let elapsed = r.elapsed.map(fmt::duration).unwrap_or_default();
        spans.push(t.span(format!("{elapsed:>ELAPSED_WIDTH$}"), Role::Meta));
        if cols.cost {
            spans.push(Span::raw("  "));
            spans.push(t.span(format!("{:>COST_WIDTH$}", r.cost), Role::Meta));
        }
        Line::from(spans)
    }

    /// The `└` lines under a row: always its notes, and its branch once the block is final.
    fn details(&self, r: &WorkerRow, full: bool, width: u16, t: &Theme) -> Vec<Line<'static>> {
        let room = width.saturating_sub(BODY + 4) as usize;
        let branch = r
            .branch
            .iter()
            .filter(|_| full)
            .map(|b| (b.clone(), Role::Meta));
        r.notes
            .iter()
            .cloned()
            .chain(branch)
            .map(|(text, role)| {
                Line::from(vec![
                    Span::raw("       "),
                    t.span(t.g(Glyph::Detail), Role::Meta),
                    t.span(fmt::truncate(&text, room), role),
                ])
            })
            .collect()
    }

    /// `1 task · 1 rejected · 0s   (swamp dispatch 9g5f1c)`
    fn totals(&self, width: u16, t: &Theme) -> Line<'static> {
        let tally = self.tally();
        let mut parts = vec![dispatches::tasks_word(tally.total() as u32)];
        let phrase = order::text(&tally.cells());
        if !phrase.is_empty() {
            parts.push(phrase);
        }
        parts.push(fmt::duration(self.elapsed));
        let cost = self.cost();
        if !cost.is_empty() {
            parts.push(cost);
        }
        let text = parts.join(SEP);
        let mut spans = vec![
            Span::raw("     "),
            t.span(
                fmt::truncate(&text, width.saturating_sub(BODY) as usize),
                Role::Meta,
            ),
        ];
        // The one string in the block meant to be copied.
        let copy = match self.rows.iter().find(|r| r.branch.is_some()) {
            Some(adopt) => Some(format!("swamp adopt {}", adopt.id.short())),
            None if tally.failed + tally.rejected + tally.cancelled > 0 => self
                .dispatch
                .map(|id| format!("swamp dispatch {}", id.short())),
            None => None,
        };
        if let Some(copy) = copy {
            spans.push(t.span(format!("   ({copy})"), Role::Accent));
        }
        Line::from(spans)
    }
}

/// Which optional cells survive at this width, and what is left for the title.
struct Columns {
    tier: bool,
    account: bool,
    cost: bool,
    flex: u16,
}

impl Columns {
    fn for_width(width: u16) -> Columns {
        Columns {
            tier: width >= 62,
            account: width >= 78,
            cost: width >= 50,
            flex: width.saturating_sub(FIXED).clamp(12, 40),
        }
    }
}

/// Logical children of the brain node, deduplicated, in fold order.
pub fn brain_children(view: &RunView, brain: NodeId) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = Vec::new();
    for kid in view.children.get(&brain).into_iter().flatten() {
        let logical = view.nodes.get(kid).map_or(*kid, |n| n.logical);
        if !out.contains(&logical) {
            out.push(logical);
        }
    }
    out
}

/// A task of a bound dispatch, from the same `inspect` shape `swamp dispatch --json` prints.
fn task_row(
    view: &RunView,
    t: &TaskDetail,
    at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> WorkerRow {
    let latest = t.attempts.last();
    let rec = latest.and_then(|a| view.nodes.get(&a.node));
    let elapsed = match latest {
        Some(a) if a.started_at.is_some() => a.elapsed_ms.map(Duration::from_millis),
        _ if t.detail.is_terminal() => None,
        _ => at.and_then(|at| (now - at).try_into().ok()),
    };
    let cost = dispatches::cost(&t.cost);
    let mut notes: Vec<(String, Role)> =
        order::attempt_lines(&t.attempts[..t.attempts.len().saturating_sub(1)])
            .into_iter()
            .map(|l| (l, Role::Meta))
            .collect();
    if let Some(b) = &t.blocked {
        let secs = (b.until - now).whole_seconds().max(0) as u64;
        let mut line = format!(
            "until {} (in {})",
            fmt::clock_day(b.until, now),
            fmt::until(Duration::from_secs(secs))
        );
        for r in &b.ineligible {
            let word = usage::ineligible_text(r.reason, None, false, now);
            line.push_str(&format!("{SEP}{} {word}", r.account.0));
        }
        notes.push((line, Role::Meta));
    }
    if let Some(reason) = &t.rejected {
        notes.push((
            format!("rejected: {}", trace::failure_short(reason)),
            Role::Err,
        ));
    }
    if let Some(f) = &t.failure {
        notes.push((trace::failure_detail(f), Role::Err));
    }
    WorkerRow {
        logical: t.node,
        id: latest.map_or(t.node, |a| a.node),
        attempt: latest.map_or(0, |a| a.attempt),
        title: fmt::sanitize(&t.title),
        tier: t.tier,
        account: rec.map(account_cell).unwrap_or_default(),
        state: t.detail.clone(),
        elapsed,
        cost: if cost == "-" { String::new() } else { cost },
        usd: Some(t.cost.usd),
        notes,
        branch: rec.and_then(branch_line),
    }
}

/// A retry changes the row's short id in place: the id shown is always the one `swamp diff`
/// and `swamp adopt` take.
fn row(view: &RunView, logical: NodeId, now: OffsetDateTime) -> Option<WorkerRow> {
    let chain = view.by_logical.get(&logical)?;
    let rec = chain.iter().rev().find_map(|a| view.nodes.get(a))?;
    let notes = match &rec.state {
        NodeState::Failed { failure } => vec![(trace::failure_detail(failure), Role::Err)],
        _ => Vec::new(),
    };
    Some(WorkerRow {
        logical,
        id: rec.id,
        attempt: rec.attempt,
        title: fmt::sanitize(&rec.title),
        tier: rec.tier,
        account: account_cell(rec),
        state: rec.state.clone(),
        elapsed: elapsed(rec, now),
        cost: rec.cost.map(|_| fmt::cost(rec.cost)).unwrap_or_default(),
        usd: rec.cost.map(|c| c.usd),
        notes,
        branch: branch_line(rec),
    })
}

fn elapsed(rec: &NodeRecord, now: OffsetDateTime) -> Option<Duration> {
    if let Some(d) = rec.duration() {
        return Some(d);
    }
    let started = rec.started_at?;
    (now - started).try_into().ok()
}

/// `main/sonnet-4`: the provider prefix goes before the account id does, and the model keeps
/// the part a human reads.
fn account_cell(rec: &NodeRecord) -> String {
    let account = trace::account_cell(rec);
    let account = account.rsplit('/').next().unwrap_or(&account).to_owned();
    let Some(model) = rec.model.as_deref() else {
        return account;
    };
    let cell = format!("{account}/{}", short_model(model));
    if cell.chars().count() <= ACCOUNT_WIDTH {
        cell
    } else {
        fmt::truncate(&cell, ACCOUNT_WIDTH)
    }
}

pub(crate) fn short_model(model: &str) -> String {
    let mut parts: Vec<&str> = model.split('-').collect();
    if parts
        .last()
        .is_some_and(|p| p.len() == 8 && p.chars().all(|c| c.is_ascii_digit()))
    {
        parts.pop();
    }
    if parts.len() > 2 && matches!(parts[0], "claude" | "gpt" | "o") {
        parts.remove(0);
    }
    parts.join("-")
}

/// Omitted, never zeroed, when the attempt changed nothing; `trace` words it the same way.
fn branch_line(rec: &NodeRecord) -> Option<String> {
    let w = rec.work.as_ref().filter(|w| !w.empty)?;
    let files = match rec.files.len() {
        0 => String::new(),
        1 => "   1 file".to_owned(),
        n => format!("   {n} files"),
    };
    Some(format!(
        "branch {}   +{} -{}{files}",
        w.branch, w.insertions, w.deletions
    ))
}

/// `{"tasks":[...]}` -> how many rows the board should expect before the fold shows them.
pub fn expected_tasks(preview: &str) -> Option<usize> {
    let v: serde_json::Value = serde_json::from_str(preview).ok()?;
    Some(v.get("tasks")?.as_array()?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::chat::markdown::text_of;
    use crate::ui::chat::tests_support::{fixture, id, view_of};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_200).expect("time")
    }

    #[test]
    fn a_board_commits_only_after_the_last_owned_node_is_terminal() {
        let view = view_of(fixture());
        let mut b = Batch::new("t1".into(), Some(2), now());
        b.owned.extend(brain_children(&view, id(0)));
        b.refresh(&view, now());
        assert_eq!(b.rows.len(), 2);
        assert!(!b.done(), "the call is still open");
        b.closed = true;
        assert!(b.done(), "both fixture nodes are terminal");
    }

    #[test]
    fn the_columns_drop_in_the_documented_order() {
        assert!(Columns::for_width(100).account);
        assert!(!Columns::for_width(77).account);
        assert!(Columns::for_width(62).tier);
        assert!(!Columns::for_width(61).tier);
        assert!(Columns::for_width(50).cost);
        assert!(!Columns::for_width(49).cost);
        assert_eq!(Columns::for_width(100).flex, 38);
        assert_eq!(Columns::for_width(102).flex, 40);
        assert_eq!(Columns::for_width(74).flex, 12);
        assert_eq!(Columns::for_width(62).flex, 12);
    }

    #[test]
    fn a_collapsed_board_keeps_the_headline_totals_honest() {
        let view = view_of(fixture());
        let mut b = Batch::new("t1".into(), None, now());
        b.owned.extend(brain_children(&view, id(0)));
        b.refresh(&view, now());
        // Twelve rows, eight shown: the headline still counts every one.
        let one = b.rows[0].clone();
        while b.rows.len() < 12 {
            b.rows.push(one.clone());
        }
        let lines = b.render(100, &Theme::plain(), 0, false);
        let text: Vec<String> = lines.iter().map(text_of).collect();
        assert!(text[1].contains("✘ 11 failed · 1 done"), "{text:?}");
        assert!(text.iter().any(|l| l.contains("+4 more")), "{text:?}");
    }

    /// A bound block reads its dispatch: every task has a row from the moment it is
    /// journaled, ranked, and the block waits for `DispatchSettled` to commit.
    #[test]
    fn a_bound_batch_rows_every_task_of_its_dispatch() {
        use crate::ui::chat::tests_support as fx;
        let view = view_of(fx::p4_journal());
        let mut b = Batch::new("d1".into(), Some(5), fx::now());
        b.dispatch = Some(fx::did("18"));
        b.refresh(&view, fx::now());
        let ids: Vec<String> = b.rows.iter().map(WorkerRow::short).collect();
        assert_eq!(
            ids,
            vec!["9g5f01", "9g5f09·2", "9g5f04", "9g5f0a", "9g5f05"]
        );
        assert_eq!(b.rows[1].cost, "~$0.22", "the task rollup, both attempts");
        assert_eq!(b.running(), 2);
        b.closed = true;
        assert!(!b.done(), "open until DispatchSettled");

        let mut rejected = Batch::for_dispatch(fx::did("1c"), fx::now());
        rejected.refresh(&view, fx::now());
        assert!(rejected.done());
        assert_eq!(rejected.rows[0].elapsed, None);
    }

    #[test]
    fn a_model_id_shortens_to_the_part_a_human_reads() {
        assert_eq!(short_model("claude-sonnet-4-20250514"), "sonnet-4");
        assert_eq!(short_model("fake-mid"), "fake-mid");
        assert_eq!(expected_tasks(r#"{"tasks":[{},{}]}"#), Some(2));
        assert_eq!(expected_tasks("not json"), None);
    }
}
