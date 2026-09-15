use crate::ids::NodeId;
use crate::journal::fold::RunView;
use crate::model::core::{Cost, NodeState, Tier};
use crate::model::node::NodeRecord;
use crate::ui::chat::spinner;
use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::{fmt, trace};
use ratatui::text::{Line, Span};
use std::collections::BTreeSet;
use std::time::Duration;
use time::OffsetDateTime;

/// Where a board row starts: two spaces, the connector, two spaces.
pub const BODY: u16 = 5;
const ID_WIDTH: usize = 6;

const ACCOUNT_WIDTH: usize = 18;
const ELAPSED_WIDTH: usize = 6;
const COST_WIDTH: usize = 7;
/// Everything but the title; the title takes what is left.
const FIXED: u16 = 60;
pub const MAX_ROWS: usize = 8;

#[derive(Debug, Clone)]
pub struct WorkerRow {
    pub logical: NodeId,
    pub id: NodeId,
    pub title: String,
    pub tier: Tier,
    pub account: String,
    pub state: NodeState,
    pub elapsed: Option<Duration>,
    pub cost: Option<Cost>,
    /// `branch {b}   +{i} -{d}   {n} files`, exactly as `swamp trace` words it.
    pub branch: Option<String>,
    pub failure: Option<String>,
}

impl WorkerRow {
    pub fn terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Queued first, finished last: a collapsed board shows what is still owed.
    fn rank(&self) -> u8 {
        match self.state {
            NodeState::Queued | NodeState::Blocked { .. } => 0,
            NodeState::Leased { .. } => 1,
            NodeState::Running { .. } => 2,
            _ => 3,
        }
    }
}

/// One `swamp_dispatch` call and the nodes it owns. A batch with no call behind it is
/// `loose`: a retry or a resumed node, rendered the same way.
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
    pub rows: Vec<WorkerRow>,
    pub elapsed: Duration,
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
            rows: Vec::new(),
            elapsed: Duration::ZERO,
        }
    }

    pub fn loose(started: OffsetDateTime) -> Self {
        Batch {
            loose: true,
            ..Batch::new(String::new(), None, started)
        }
    }

    /// Rows are rebuilt from the fold every poll: elapsed is recomputed, never accumulated.
    pub fn refresh(&mut self, view: &RunView, now: OffsetDateTime) {
        self.rows = self
            .owned
            .iter()
            .filter_map(|logical| row(view, *logical, now))
            .collect();
        // A batch is at least as old as its oldest node: a resumed run has nodes that
        // started before this process ever opened the board.
        let oldest = self.rows.iter().filter_map(|r| r.elapsed).max();
        let since: Duration = (now - self.started).try_into().unwrap_or(Duration::ZERO);
        self.elapsed = oldest.unwrap_or(Duration::ZERO).max(since);
    }

    /// Committed only when the call is done and every owned node is terminal.
    pub fn done(&self) -> bool {
        self.closed && !self.rows.is_empty() && self.rows.iter().all(WorkerRow::terminal)
    }

    pub fn cost(&self) -> (f64, bool) {
        let mut usd = 0.0;
        let mut complete = true;
        for r in &self.rows {
            match r.cost {
                Some(c) => usd += c.usd,
                None => complete = false,
            }
        }
        (usd, complete)
    }

    fn counts(&self) -> (usize, usize, usize) {
        let done = self
            .rows
            .iter()
            .filter(|r| matches!(r.state, NodeState::Succeeded))
            .count();
        let failed = self
            .rows
            .iter()
            .filter(|r| !matches!(r.state, NodeState::Succeeded) && r.terminal())
            .count();
        let running = self.rows.len() - done - failed;
        (running, done, failed)
    }

    pub fn running(&self) -> usize {
        self.counts().0
    }

    pub fn render(&self, width: u16, t: &Theme, tick: u64, committed: bool) -> Vec<Line<'static>> {
        let mut out = vec![self.head(width, t), self.headline(width, t, tick)];
        let visible: Vec<(usize, &WorkerRow)> = self.visible(committed);
        for (i, row) in &visible {
            out.push(self.row_line(row, *i, width, t, tick));
            if committed || self.expanded {
                out.extend(self.details(row, width, t));
            }
        }
        let hidden = self.rows.len() - visible.len();
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

    /// The least-advanced rows survive a collapse; the headline still counts them all.
    fn visible(&self, committed: bool) -> Vec<(usize, &WorkerRow)> {
        let mut idx: Vec<(usize, &WorkerRow)> = self.rows.iter().enumerate().collect();
        if committed || self.expanded || idx.len() <= MAX_ROWS {
            return idx;
        }
        idx.sort_by_key(|(i, r)| (r.rank(), *i));
        idx.truncate(MAX_ROWS);
        idx.sort_by_key(|(i, _)| *i);
        idx
    }

    /// `● swamp_dispatch(2 tasks)`, or `● workers` for nodes no call claimed.
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

    fn headline(&self, width: u16, t: &Theme, tick: u64) -> Line<'static> {
        let (running, done, failed) = self.counts();
        let (usd, complete) = self.cost();
        let cost = format!("~${usd:.2}{}", if complete { "" } else { "+" });
        // Nothing has reached the fold yet: the call is out, the nodes are not.
        if self.rows.is_empty() && !self.closed {
            let expected = self
                .expected
                .map(|n| format!("{n} queued"))
                .unwrap_or_else(|| "queued".to_owned());
            return Line::from(vec![
                t.span("  ", Role::Text),
                t.span(t.g(Glyph::Connector), Role::Meta),
                t.span("  ", Role::Text),
                t.span(spinner::worker_frame(t, tick, 0).to_owned(), Role::Run),
                t.span(format!(" {expected} · starting…"), Role::Meta),
            ]);
        }
        let (glyph, role) = if running > 0 {
            (spinner::worker_frame(t, tick, 0).to_owned(), Role::Run)
        } else if done > 0 {
            (t.g(Glyph::Succeeded).to_owned(), Role::Ok)
        } else {
            (t.g(Glyph::Failed).to_owned(), Role::Err)
        };
        let failures = if failed > 0 {
            format!("{} {failed} failed · ", t.g(Glyph::Failed))
        } else {
            String::new()
        };
        let body = if running > 0 {
            format!(
                "{running} running · {done} done · {cost} · {}",
                fmt::duration(self.elapsed)
            )
        } else {
            format!(
                "{done} done · {failures}{cost} · {}",
                fmt::duration(self.elapsed)
            )
        };
        let text = fmt::truncate(&body, width.saturating_sub(BODY + 2) as usize);
        Line::from(vec![
            t.span("  ", Role::Text),
            t.span(t.g(Glyph::Connector), Role::Meta),
            t.span("  ", Role::Text),
            t.span(glyph, role),
            t.span(" ", Role::Text),
            t.span(text, Role::Meta),
        ])
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
            t.span(fmt::pad(&r.id.short(), ID_WIDTH), Role::Meta),
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
        spans.push(t.span(fmt::pad(&r.title, cols.flex as usize), Role::Name));
        spans.push(Span::raw("  "));
        if cols.account {
            spans.push(t.span(fmt::pad(&r.account, ACCOUNT_WIDTH), Role::Meta));
            spans.push(Span::raw("  "));
        }
        let elapsed = r
            .elapsed
            .map(fmt::duration)
            .unwrap_or_else(|| "-".to_owned());
        spans.push(t.span(format!("{elapsed:>ELAPSED_WIDTH$}"), Role::Meta));
        if cols.cost {
            spans.push(Span::raw("  "));
            spans.push(t.span(format!("{:>COST_WIDTH$}", fmt::cost(r.cost)), Role::Meta));
        }
        Line::from(spans)
    }

    /// The `└` lines under a finished row: the branch to adopt, or why it failed.
    fn details(&self, r: &WorkerRow, width: u16, t: &Theme) -> Vec<Line<'static>> {
        let room = width.saturating_sub(BODY + 4) as usize;
        let mut out = Vec::new();
        if let Some(failure) = &r.failure {
            out.push(Line::from(vec![
                Span::raw("       "),
                t.span(t.g(Glyph::Detail), Role::Meta),
                t.span(fmt::truncate(failure, room), Role::Err),
            ]));
        }
        if let Some(branch) = &r.branch {
            out.push(Line::from(vec![
                Span::raw("       "),
                t.span(t.g(Glyph::Detail), Role::Meta),
                t.span(fmt::truncate(branch, room), Role::Meta),
            ]));
        }
        out
    }

    fn totals(&self, width: u16, t: &Theme) -> Line<'static> {
        let (_, _, failed) = self.counts();
        let (usd, complete) = self.cost();
        let nodes = self.rows.len();
        let plural = if nodes == 1 { "node" } else { "nodes" };
        let text = format!(
            "{nodes} {plural} · {failed} failed · {} · ~${usd:.2}{}",
            fmt::duration(self.elapsed),
            if complete { "" } else { "+" }
        );
        let mut spans = vec![
            Span::raw("     "),
            t.span(
                fmt::truncate(&text, width.saturating_sub(BODY) as usize),
                Role::Meta,
            ),
        ];
        // The one string in the block meant to be copied.
        if let Some(adopt) = self.rows.iter().find(|r| r.branch.is_some()) {
            spans.push(t.span(
                format!("   (swamp adopt {})", adopt.id.short()),
                Role::Accent,
            ));
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

/// A retry changes the row's short id in place: the id shown is always the one `swamp diff`
/// and `swamp adopt` take.
fn row(view: &RunView, logical: NodeId, now: OffsetDateTime) -> Option<WorkerRow> {
    let chain = view.by_logical.get(&logical)?;
    let rec = chain.iter().rev().find_map(|a| view.nodes.get(a))?;
    Some(WorkerRow {
        logical,
        id: rec.id,
        title: rec.title.clone(),
        tier: rec.tier,
        account: account_cell(rec),
        state: rec.state.clone(),
        elapsed: elapsed(rec, now),
        cost: rec.cost,
        branch: branch_line(rec),
        failure: match &rec.state {
            NodeState::Failed { failure } => Some(trace::failure_detail(failure)),
            _ => None,
        },
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

fn short_model(model: &str) -> String {
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
        assert_eq!(Columns::for_width(100).flex, 40);
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
        assert!(text[1].contains("11 done · ✘ 1 failed"), "{text:?}");
        assert!(text.iter().any(|l| l.contains("+4 more")), "{text:?}");
    }

    #[test]
    fn a_model_id_shortens_to_the_part_a_human_reads() {
        assert_eq!(short_model("claude-sonnet-4-20250514"), "sonnet-4");
        assert_eq!(short_model("fake-mid"), "fake-mid");
        assert_eq!(expected_tasks(r#"{"tasks":[{},{}]}"#), Some(2));
        assert_eq!(expected_tasks("not json"), None);
    }
}
