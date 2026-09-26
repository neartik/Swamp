//! The three layout tiers of `docs/BOARD.md` §3: a `Board` into `Line`s, no clock or file read.

use crate::dispatch::account::Health;
use crate::dispatch::policy::{self, Ineligible, Scoring};
use crate::journal::inspect;
use crate::model::core::{AccountId, LimitScope, LimitWindow, NodeState, Tier};
use crate::model::dispatch::DispatchState;
use crate::model::failure::Failure;
use crate::ui::board::model::{
    Board, DispatchGroup, NodeRow, Reason, ReasonForm, Rows, Selection, SelectionNote, Summary,
    TaskRow,
};
use crate::ui::chat::spinner;
use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::chat::workers::short_model;
use crate::ui::keys::{self, KeyAction, Surface};
use crate::ui::order::{self, Cell, Drop, SEP};
use crate::ui::usage::{self, AccountRow};
use crate::ui::{delegation, dispatches, fmt, trace, watch};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use std::time::Duration as StdDuration;
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = "swamp board";

/// Wide enough for `~$12.34+`.
const COST_W: usize = 8;

/// Account lines the strip draws before it says how many more there are.
pub const ACCOUNTS_MAX: usize = 6;

/// The body never shrinks below this while the detail block has a line left to give.
pub const BODY_MIN: usize = 8;

// ---------------------------------------------------------------- layout

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Narrow,
    Medium,
    Wide,
}

/// How much of an account's quota the strip draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pool {
    /// Percentages only.
    Compact,
    /// The 5h bar, the 7d percentage.
    Bar,
    /// Both bars, the gate that holds it back, its tokens unless the gate leaves no room.
    Full,
}

/// How much of a blocked task's recorded refusal its stuck line spells out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedLine {
    /// `until 22:54 · main at capacity +1`
    First,
    /// `until 22:54 · main at capacity · alt quota stop`
    All,
    /// `until 22:54 (in 38m) · main at capacity (2/2) · alt quota stop (93%)`
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub band: Band,
    pub min: u16,
    pub header_rows: u8,
    pub tier: bool,
    pub account_w: usize,
    pub model_w: usize,
    pub tokens: bool,
    pub cost_col: bool,
    pub title_min: usize,
    pub indent_cap: usize,
    pub dispatch_id: bool,
    pub blocked: BlockedLine,
    pub pool: Pool,
    pub name_w: usize,
    pub bar: usize,
    pub detail_max: usize,
}

pub const NARROW: Layout = Layout {
    band: Band::Narrow,
    min: 0,
    header_rows: 2,
    tier: false,
    account_w: 0,
    model_w: 0,
    tokens: false,
    cost_col: false,
    title_min: 12,
    indent_cap: 1,
    dispatch_id: false,
    blocked: BlockedLine::First,
    pool: Pool::Compact,
    name_w: 10,
    bar: 0,
    detail_max: 5,
};

pub const MEDIUM: Layout = Layout {
    band: Band::Medium,
    min: 60,
    header_rows: 1,
    tier: false,
    account_w: 8,
    model_w: 0,
    tokens: false,
    cost_col: true,
    title_min: 16,
    indent_cap: 1,
    dispatch_id: true,
    blocked: BlockedLine::All,
    pool: Pool::Bar,
    name_w: 11,
    bar: 10,
    detail_max: 6,
};

pub const WIDE: Layout = Layout {
    band: Band::Wide,
    min: 100,
    header_rows: 1,
    tier: true,
    account_w: 10,
    model_w: 10,
    tokens: true,
    cost_col: true,
    title_min: 24,
    indent_cap: 2,
    dispatch_id: true,
    blocked: BlockedLine::Full,
    pool: Pool::Full,
    name_w: 12,
    bar: 10,
    detail_max: 4,
};

pub const LAYOUTS: [Layout; 3] = [NARROW, MEDIUM, WIDE];

pub fn layout_for(w: u16) -> Layout {
    *LAYOUTS.iter().rev().find(|l| w >= l.min).unwrap_or(&NARROW)
}

/// Everything a line needs that is not the line itself.
pub struct Ctx<'a> {
    pub l: Layout,
    pub width: u16,
    pub theme: &'a Theme,
    pub now: OffsetDateTime,
    pub tick: u64,
    pub max_age: StdDuration,
    /// The brain row names its run once two runs are tailed.
    pub multi_run: bool,
    /// The dispatcher's thresholds, so the gate words read the same gate the pool read.
    pub scoring: Scoring,
}

impl<'a> Ctx<'a> {
    pub fn new(
        b: &Board,
        width: u16,
        theme: &'a Theme,
        tick: u64,
        max_age: StdDuration,
    ) -> Ctx<'a> {
        Ctx {
            l: layout_for(width),
            width,
            theme,
            now: b.now,
            tick,
            max_age,
            multi_run: b.runs.len() > 1,
            scoring: b.scoring,
        }
    }

    fn w(&self) -> usize {
        self.width as usize
    }
}

// ---------------------------------------------------------------- frame

/// One frame at the model's own fold state, the hints bar `hide` at the bottom: `--once`.
pub fn frame(
    b: &Board,
    width: u16,
    theme: &Theme,
    tick: u64,
    max_age: StdDuration,
    hide: &[KeyAction],
) -> Vec<Line<'static>> {
    let c = Ctx::new(b, width, theme, tick, max_age);
    let rows = b.rows();
    let bottom = hints(&c, hide);
    compose(b, &rows, &rows, &c, bottom, None, &mut 0)
}

/// Header, body, accounts strip, detail and bottom line. `shown` is `rows` with the viewer's
/// folds applied; with a `height` the body scrolls to keep the selection in view and the
/// regions under it stay put.
pub fn compose(
    b: &Board,
    shown: &Rows,
    rows: &Rows,
    c: &Ctx,
    bottom: Line<'static>,
    height: Option<usize>,
    scroll: &mut usize,
) -> Vec<Line<'static>> {
    let mut out = header(b, rows, c);
    out.push(rule(c));
    let body = body(shown, &b.selected, rows, c);
    let strip = accounts(rows, &b.selected, c);
    let fixed = out.len() + 1 + strip.len() + 1 + 1;
    let (room, detail_h) = match height {
        None => (body.lines.len(), c.l.detail_max),
        Some(h) => {
            let avail = h.saturating_sub(fixed);
            let detail_h = c.l.detail_max.min(avail.saturating_sub(BODY_MIN)).max(1);
            (avail.saturating_sub(detail_h).max(1), detail_h)
        }
    };
    if let Some(at) = body.selected {
        if at < *scroll {
            *scroll = at;
        } else if at >= *scroll + room {
            *scroll = at + 1 - room;
        }
    }
    *scroll = (*scroll).min(body.lines.len().saturating_sub(room));
    let mut shown_body: Vec<Line<'static>> =
        body.lines.into_iter().skip(*scroll).take(room).collect();
    if height.is_some() {
        shown_body.resize(room, Line::default());
    }
    out.extend(shown_body);
    out.push(rule(c));
    out.extend(strip);
    out.push(rule(c));
    let mut detail = cap(detail(b, rows, c), detail_h, c);
    detail.resize(detail_h, Line::default());
    out.extend(detail);
    out.push(bottom);
    out
}

pub fn rule(c: &Ctx) -> Line<'static> {
    Line::from(
        c.theme
            .span(c.theme.g(Glyph::Rule).repeat(c.w()), Role::Meta),
    )
}

/// `keys::hints` for the board, `k` left out when the board may not cancel.
pub fn hints(c: &Ctx, hide: &[KeyAction]) -> Line<'static> {
    Line::from(c.theme.span(
        fmt::truncate(&keys::hints(Surface::Board, c.w(), hide), c.w()),
        Role::Meta,
    ))
}

/// A prompt or a notice in place of the hints.
pub fn bottom(text: &str, role: Role, bold: bool, c: &Ctx) -> Line<'static> {
    let mut span = c.theme.span(fmt::truncate(text, c.w()), role);
    if bold {
        span.style = span.style.add_modifier(Modifier::BOLD);
    }
    Line::from(span)
}

// ---------------------------------------------------------------- header

/// `swamp board   2 running · 1 stuck · 1 queued · ~$0.43 · observed 4s ago`, on two rows at
/// Narrow; the optional cells drop, in order, when they do not fit.
pub fn header(b: &Board, rows: &Rows, c: &Ctx) -> Vec<Line<'static>> {
    let s = b.summary(rows);
    let cells = header_cells(&s, b.read_budget, c.l.band != Band::Narrow);
    let fresh = freshness(b, c);
    // Within budget the delegation cell is the first to go; past it, it outlasts the cost.
    let over = s.brain.is_some_and(|w| w.over(b.read_budget));
    let drops = if over {
        [
            Drop::Key("queued"),
            Drop::Key("runs"),
            Drop::Key("cost"),
            Drop::Key("brain"),
        ]
    } else {
        [
            Drop::Key("brain"),
            Drop::Key("queued"),
            Drop::Key("runs"),
            Drop::Key("cost"),
        ]
    };
    let mut first = Row::default();
    first.add(c.theme, TITLE, Role::Name);
    if c.l.header_rows == 2 {
        first.tail(c.theme, &fresh.text, fresh.role, c.w());
        let cells = order::fit(cells, c.w(), &drops);
        let mut second = Row::default();
        second.spans.extend(order::spans(&cells, c.theme, c.w()));
        return vec![first.line(c.w()), second.line(c.w())];
    }
    let gap = TITLE.width() + 2;
    let room = c.w().saturating_sub(gap);
    // Freshness never drops: the counts before it give way first.
    let fresh_text = fmt::truncate(&fresh.text, room);
    let rest_room = room.saturating_sub(fresh_text.width() + SEP.width());
    let cells = order::fit(cells, rest_room, &drops);
    let mut spans = order::spans(&cells, c.theme, rest_room);
    let rest_w: usize = spans.iter().map(|s| s.content.width()).sum();
    if rest_w > 0 {
        spans.push(c.theme.span(SEP.to_owned(), Role::Meta));
    }
    spans.push(c.theme.span(fresh_text, fresh.role));
    let total: usize = spans.iter().map(|s| s.content.width()).sum();
    first.to(c.w().saturating_sub(total).max(gap));
    first.spans.extend(spans);
    vec![first.line(c.w())]
}

fn header_cells(s: &Summary, budget: u32, share: bool) -> Vec<Cell> {
    let mut cells = Vec::new();
    if s.runs > 1 || s.hidden_runs > 0 {
        let text = if s.hidden_runs > 0 {
            format!("{} of {} runs", s.runs, s.runs + s.hidden_runs)
        } else {
            format!("{} runs", s.runs)
        };
        cells.push(Cell::new("runs", text, Role::Meta));
    }
    if s.stale_runs > 0 {
        cells.push(Cell::new(
            "stale",
            format!("{} stale", s.stale_runs),
            Role::Err,
        ));
    }
    cells.extend(s.tally.summary_cells(s.cost_usd, s.cost_complete));
    if let Some(w) = &s.brain {
        cells.push(delegation::cell(w, budget, share));
    }
    cells
}

/// How far behind the persisted account snapshot is. Past `quota_max_age` it stops being a
/// lag and becomes the reason dispatch is wrong, so it changes colour.
fn freshness(b: &Board, c: &Ctx) -> Cell {
    let Some(at) = b.accounts_at else {
        return Cell::new("fresh", "not observed", Role::Meta);
    };
    let age: StdDuration = (c.now - at).try_into().unwrap_or(StdDuration::ZERO);
    let text = if age.as_secs() < 60 {
        format!("observed {}s ago", age.as_secs())
    } else {
        format!("observed {} ago", fmt::until(age))
    };
    let role = if age > c.max_age {
        Role::Err
    } else {
        Role::Meta
    };
    Cell::new("fresh", text, role)
}

#[derive(Default)]
pub struct Body {
    pub lines: Vec<Line<'static>>,
    /// The line the selection is on, when it is in the body.
    pub selected: Option<usize>,
}

impl Body {
    fn push(&mut self, line: Line<'static>, selected: bool) {
        if selected {
            self.selected = Some(self.lines.len());
        }
        self.lines.push(line);
    }

    fn blank(&mut self) {
        self.lines.push(Line::default());
    }
}

/// Per run: the brain, its open dispatches with their tasks, then `recent`.
pub fn body(shown: &Rows, sel: &Selection, all: &Rows, c: &Ctx) -> Body {
    let mut out = Body::default();
    let mut spin = 0usize;
    if shown.runs.is_empty() {
        out.push(
            Line::from(c.theme.span("  no live runs", Role::Meta)),
            false,
        );
    }
    for (i, run) in shown.runs.iter().enumerate() {
        if i > 0 {
            out.blank();
        }
        let mut started = false;
        if let Some(brain) = &run.brain {
            let on = *sel == brain.selection();
            out.push(brain_line(brain, on, c), on);
            started = true;
        }
        for g in &run.active {
            if started {
                out.blank();
            }
            started = true;
            group_lines(g, false, sel, &mut spin, all, c, &mut out);
        }
        if !run.recent.is_empty() {
            if started {
                out.blank();
            }
            let mut r = Row::default();
            r.pad(2);
            r.add(c.theme, "recent", Role::Meta);
            out.push(r.line(c.w()), false);
            for g in &run.recent {
                group_lines(g, true, sel, &mut spin, all, c, &mut out);
            }
        }
    }
    out
}

fn group_lines(
    g: &DispatchGroup,
    recent: bool,
    sel: &Selection,
    spin: &mut usize,
    all: &Rows,
    c: &Ctx,
    out: &mut Body,
) {
    let on = *sel == g.selection();
    out.push(dispatch_line(g, recent, on, c), on);
    if !g.expanded {
        return;
    }
    for t in &g.tasks {
        let on = *sel == t.row.selection();
        out.push(task_line(t, recent, on, spin, c), on);
        if let Some(line) = stuck_line(t, all, c) {
            out.push(line, false);
        }
        for sub in &t.nested {
            group_lines(sub, recent, sel, spin, all, c, out);
        }
    }
    if g.hidden > 0 {
        let mut r = Row::default();
        r.pad(4 + 4 * g.level.min(c.l.indent_cap));
        r.add(
            c.theme,
            &format!("\u{2026} +{} more{SEP}enter lists all", g.hidden),
            Role::Meta,
        );
        out.push(r.line(c.w()), false);
    }
}

fn gutter(r: &mut Row, selected: bool, c: &Ctx) {
    if selected {
        r.add(c.theme, c.theme.g(Glyph::Select), Role::Accent);
    } else {
        r.pad(1);
    }
}

fn brain_line(n: &NodeRow, selected: bool, c: &Ctx) -> Line<'static> {
    let t = c.theme;
    let mut r = Row::default();
    gutter(&mut r, selected, c);
    r.pad(1);
    let working = matches!(
        n.state,
        NodeState::Running { .. } | NodeState::Leased { .. }
    );
    let (glyph, role) = match (n.stale, working) {
        (true, _) => (t.g(Glyph::Orphaned), Role::Err),
        (false, true) => (t.g(Glyph::Brain), Role::Run),
        (false, false) => (t.g(Glyph::Brain), Role::Meta),
    };
    r.add(t, glyph, role);
    r.pad(1);
    let id = if c.multi_run {
        n.run.short()
    } else {
        "brain".to_owned()
    };
    r.add(t, &fmt::pad(&id, 10), Role::Text);
    r.pad(1);
    if c.l.tier {
        r.pad(7);
    }
    let title = if working { "orchestrating" } else { &n.title };
    let title_role = if n.state.is_terminal() {
        Role::Meta
    } else {
        Role::Name
    };
    finish_row(r, title, title_role, n, c)
}

fn task_line(
    t: &TaskRow,
    recent: bool,
    selected: bool,
    spin: &mut usize,
    c: &Ctx,
) -> Line<'static> {
    let theme = c.theme;
    let n = &t.row;
    let l = t.level.min(c.l.indent_cap);
    let mut r = Row::default();
    gutter(&mut r, selected, c);
    r.pad(3 + 4 * l);
    let (glyph, role) = node_glyph(n, spin, c);
    r.add(theme, glyph, role);
    r.pad(1);
    let finished = recent || matches!(n.state, NodeState::Succeeded | NodeState::Cancelled { .. });
    let id_role = if finished || n.state.is_terminal() {
        Role::Meta
    } else {
        Role::Text
    };
    r.add(theme, &fmt::pad(&n.short(), 8), id_role);
    r.pad(1);
    if c.l.tier {
        let role = if n.tier == Tier::High && !finished {
            Role::TierHi
        } else {
            Role::Meta
        };
        r.add(theme, &format!("[{}]", fmt::pad(n.tier.as_str(), 4)), role);
        r.pad(1);
    }
    let title_role = if finished {
        Role::Meta
    } else if n.state.is_terminal() {
        Role::Text
    } else {
        Role::Name
    };
    finish_row(r, &n.title, title_role, n, c)
}

/// The flexible title, then the tier's right block.
fn finish_row(mut r: Row, title: &str, role: Role, n: &NodeRow, c: &Ctx) -> Line<'static> {
    let right = right_cells(n, c);
    let right_w: usize = right.iter().map(|s| s.width() + 1).sum();
    let title_w = c.w().saturating_sub(r.w + right_w).max(1);
    r.add(c.theme, &fmt::pad(title, title_w), role);
    for cell in right {
        r.pad(1);
        r.add(c.theme, &cell, Role::Meta);
    }
    r.line(c.w())
}

/// Blank, never `-`, when a cell does not apply.
fn right_cells(n: &NodeRow, c: &Ctx) -> Vec<String> {
    let mut out = Vec::new();
    if c.l.account_w > 0 {
        let account = n.account.as_ref().map(|a| fmt::sanitize(&a.0));
        out.push(fmt::pad(account.as_deref().unwrap_or(""), c.l.account_w));
    }
    if c.l.model_w > 0 {
        let model = n.model.as_deref().map(short_model).unwrap_or_default();
        out.push(fmt::pad(&model, c.l.model_w));
    }
    let elapsed = n.elapsed(c.now).map(fmt::duration).unwrap_or_default();
    out.push(right(&elapsed, 6));
    if c.l.tokens {
        out.push(right(&tokens(n.spend.usage.billable(), false, c), 7));
    }
    if c.l.cost_col {
        out.push(right(&dispatches::cost_cell(&n.spend), COST_W));
    }
    out
}

/// `↓ 1.2k`; blank for zero unless `always`.
fn tokens(n: u64, always: bool, c: &Ctx) -> String {
    if n == 0 && !always {
        return String::new();
    }
    format!("{} {}", c.theme.g(Glyph::TokenArrow), fmt::tokens(n))
}

fn node_glyph(n: &NodeRow, spin: &mut usize, c: &Ctx) -> (&'static str, Role) {
    let live = matches!(
        n.state,
        NodeState::Running { .. } | NodeState::Leased { .. }
    );
    // Only the rows that were animating freeze; a queued task still reads as queued.
    if n.stale && live {
        return (c.theme.g(Glyph::Orphaned), Role::Err);
    }
    if matches!(n.state, NodeState::Running { .. }) {
        let frame = spinner::worker_frame(c.theme, c.tick, *spin);
        *spin += 1;
        return (frame, Role::Run);
    }
    (c.theme.state_glyph(&n.state), c.theme.state_role(&n.state))
}

fn dispatch_line(g: &DispatchGroup, recent: bool, selected: bool, c: &Ctx) -> Line<'static> {
    let t = c.theme;
    let l = g.level.min(c.l.indent_cap);
    let mut r = Row::default();
    gutter(&mut r, selected, c);
    r.pad(1 + 4 * l);
    let fold = if g.expanded {
        Glyph::Fold
    } else {
        Glyph::Folded
    };
    r.add(t, t.g(fold), Role::Meta);
    r.pad(1);

    let label_role = if recent || g.state == DispatchState::Settled {
        Role::Meta
    } else {
        Role::Name
    };
    let mut cells = order::label_cells(g.seq, g.id, label_role);
    if !c.l.dispatch_id {
        cells.retain(|cell| cell.key != "id");
    }
    if let Some(by) = g.caller {
        cells.push(Cell::new("by", format!("by {}", by.short()), Role::Meta));
    }
    cells.push(Cell::new(
        "tasks",
        dispatches::tasks_word(g.tally.total() as u32),
        Role::Meta,
    ));
    cells.extend(g.tally.cells());
    if recent && let Some(reason) = reason_cell(g) {
        cells.push(Cell::new("reason", reason, Role::Err));
    }
    let cost = dispatches::cost_cell(&g.cost);
    if !c.l.cost_col && !cost.is_empty() {
        cells.push(Cell::new("cost", cost.clone(), Role::Meta));
    }

    let mut right_block: Vec<String> = Vec::new();
    if c.l.cost_col {
        let age = if recent {
            String::new()
        } else {
            let age: StdDuration = (c.now - g.at).try_into().unwrap_or(StdDuration::ZERO);
            fmt::duration(age)
        };
        right_block.push(right(&age, 6));
        if c.l.tokens {
            right_block.push(right(&tokens(g.cost.usage.billable(), false, c), 7));
        }
        right_block.push(right(&cost, COST_W));
    }
    let right_w: usize = right_block.iter().map(|s| s.width() + 1).sum();
    let blank = right_block.iter().all(|s| s.trim().is_empty());
    let room = if blank {
        c.w().saturating_sub(r.w)
    } else {
        c.w().saturating_sub(r.w + right_w + 1)
    };
    let cells = order::fit(
        cells,
        room,
        &[
            Drop::Key("tasks"),
            Drop::Tail,
            Drop::Key("id"),
            Drop::Key("cost"),
        ],
    );
    let spans = order::spans(&cells, t, room);
    r.w += spans.iter().map(|s| s.content.width()).sum::<usize>();
    r.spans.extend(spans);
    if !blank {
        r.to(c.w().saturating_sub(right_w));
        for cell in right_block {
            r.pad(1);
            r.add(t, &cell, Role::Meta);
        }
    }
    r.line(c.w())
}

/// Why a settled dispatch went wrong, from its first failed or rejected task.
fn reason_cell(g: &DispatchGroup) -> Option<String> {
    let first = g.tasks.iter().find_map(|t| failure(&t.row.state))?;
    let more = (g.tally.failed + g.tally.rejected).saturating_sub(1);
    let tail = if more > 0 {
        format!(" +{more}")
    } else {
        String::new()
    };
    Some(format!("{}{tail}", trace::failure_short(first)))
}

/// Why a failed task failed, or why a rejected one was refused.
fn failure(s: &NodeState) -> Option<&Failure> {
    match s {
        NodeState::Failed { failure } => Some(failure),
        NodeState::Rejected { reason } => Some(reason),
        _ => None,
    }
}

/// The second line a stuck task gets: why it is waiting, or why it ended.
fn stuck_line(t: &TaskRow, all: &Rows, c: &Ctx) -> Option<Line<'static>> {
    let (text, role) = match &t.row.state {
        NodeState::Blocked { until, .. } => {
            (blocked_text(*until, &t.ineligible, all, c), Role::Meta)
        }
        NodeState::Failed { failure } => (trace::failure_short(failure), Role::Err),
        NodeState::Rejected { reason } => (
            format!("rejected: {}", trace::failure_short(reason)),
            Role::Err,
        ),
        NodeState::Orphaned { pid, .. } => (format!("orphaned: pid {pid} is gone"), Role::Err),
        _ => return None,
    };
    let col = 6 + 4 * t.level.min(c.l.indent_cap);
    let mut r = Row::default();
    r.pad(col);
    r.add(
        c.theme,
        &fmt::truncate(&fmt::sanitize(&text), c.w().saturating_sub(col)),
        role,
    );
    Some(r.line(c.w()))
}

fn blocked_text(
    until: OffsetDateTime,
    list: &[(AccountId, Ineligible)],
    all: &Rows,
    c: &Ctx,
) -> String {
    let mut out = if c.l.blocked == BlockedLine::Full {
        fmt::until_at(until, c.now)
    } else {
        format!("until {}", fmt::clock_day(until, c.now))
    };
    match c.l.blocked {
        BlockedLine::First => {
            if let Some((id, g)) = list.first() {
                out.push_str(&format!("{SEP}{} {}", id.0, g.word()));
                if list.len() > 1 {
                    out.push_str(&format!(" +{}", list.len() - 1));
                }
            }
        }
        BlockedLine::All => {
            for (id, g) in list {
                out.push_str(&format!("{SEP}{} {}", id.0, g.word()));
            }
        }
        BlockedLine::Full => {
            for (id, g) in list {
                let text = recorded_text(*g, all.account(id), c);
                out.push_str(&format!("{SEP}{} {text}", id.0));
            }
        }
    }
    out
}

/// A recorded refusal, its figures only while the live gate still agrees with it.
fn recorded_text(g: Ineligible, row: Option<&AccountRow>, c: &Ctx) -> String {
    let row = row.filter(|r| gate_of(r, c) == Some(g));
    usage::ineligible_text(g, row, c.now)
}

// ---------------------------------------------------------------- accounts

/// One line per account, `+N more` past `ACCOUNTS_MAX`.
pub fn accounts(rows: &Rows, sel: &Selection, c: &Ctx) -> Vec<Line<'static>> {
    let list = &rows.accounts;
    if list.is_empty() {
        return vec![Line::from(c.theme.span("  no accounts", Role::Meta))];
    }
    let keep = if list.len() > ACCOUNTS_MAX {
        ACCOUNTS_MAX - 1
    } else {
        list.len()
    };
    let flight_w = list[..keep]
        .iter()
        .map(|r| flight(r).width())
        .max()
        .unwrap_or(0)
        .max(3);
    let mut out: Vec<Line<'static>> = list[..keep]
        .iter()
        .map(|r| {
            let on = *sel == Selection::Account(r.account.clone());
            account_line(r, on, flight_w, c)
        })
        .collect();
    if list.len() > keep {
        out.push(Line::from(c.theme.span(
            fmt::truncate(
                &format!("  +{} more{SEP}a lists all", list.len() - keep),
                c.w(),
            ),
            Role::Meta,
        )));
    }
    out
}

/// `g ● main        2/2   5h ▇▇▇▇▇▇▇░░░  71% ↻ 38m     7d  32%`, one path per pool.
pub fn account_line(r: &AccountRow, selected: bool, flight_w: usize, c: &Ctx) -> Line<'static> {
    let t = c.theme;
    let health = watch::shown_health(r.health, r.cooldown_until, c.now);
    let role = usage::health_role(health);
    let mut line = Row::default();
    gutter(&mut line, selected, c);
    line.pad(1);
    line.add(t, health_glyph(t, health), role);
    line.pad(1);
    line.add(
        t,
        &fmt::pad(&fmt::sanitize(&r.account.0), c.l.name_w),
        Role::Name,
    );
    line.pad(1);
    let flight = flight(r);
    let fill = flight_w.saturating_sub(flight.width());
    line.pad(fill);
    line.add(t, &flight, Role::Meta);

    let out_of_service = matches!(
        health,
        Health::Cooling | Health::AuthBroken | Health::Disabled
    );
    if out_of_service && c.l.pool != Pool::Full {
        line.pad(if c.l.pool == Pool::Compact { 2 } else { 3 });
        line.add(t, &service_word(r, health, c), Role::Err);
        return line.line(c.w());
    }
    let w5 = usage::window_for(&r.quota, LimitScope::FiveHour, c.now);
    let w7 = usage::window_for(&r.quota, LimitScope::SevenDay, c.now);
    match c.l.pool {
        Pool::Compact => {
            line.pad(2);
            line.add(t, "5h ", Role::Meta);
            line.add(t, &right(&usage::pct_cell(w5), 4), pct_role(r, w5, c));
            line.pad(2);
            line.add(t, "7d ", Role::Meta);
            line.add(t, &right(&usage::pct_cell(w7), 4), pct_role(r, w7, c));
        }
        Pool::Bar => {
            line.pad(3);
            window(&mut line, "5h", w5, r, c);
            line.pad(2);
            line.add(t, "7d ", Role::Meta);
            line.add(t, &right(&usage::pct_cell(w7), 4), pct_role(r, w7, c));
        }
        Pool::Full => {
            line.pad(3);
            window(&mut line, "5h", w5, r, c);
            line.pad(2);
            window(&mut line, "7d", w7, r, c);
            line.pad(2);
            let (status, role) = status_cell(r, health, c);
            let tok = tokens(r.window_tokens.billable(), true, c);
            // The gate says why the account is out; its tokens give way first.
            let room = c.w().saturating_sub(line.w + tok.width() + 1);
            line.add(t, &status, role);
            if status.width() <= room {
                line.tail(t, &tok, Role::Meta, c.w());
            }
        }
    }
    line.line(c.w())
}

/// `2/2`, `10/12`, `0/-` without a limit.
fn flight(r: &AccountRow) -> String {
    let max = r
        .max_concurrency
        .map_or_else(|| "-".to_owned(), |m| m.to_string());
    format!("{}/{max}", r.inflight)
}

/// `5h ▇▇▇▇▇▇▇░░░  71% ↻ 38m   `
fn window(line: &mut Row, label: &str, w: Option<&LimitWindow>, r: &AccountRow, c: &Ctx) {
    let t = c.theme;
    let role = pct_role(r, w, c);
    line.add(t, &format!("{label} "), Role::Meta);
    // No window: one `-` where the percentage goes, the bar and the reset left blank.
    let bar = match w {
        Some(w) => gauge(w.utilization, c.l.bar, t),
        None => " ".repeat(c.l.bar),
    };
    line.add(t, &bar, role);
    line.pad(1);
    line.add(t, &right(&usage::pct_cell(w), 4), role);
    line.pad(1);
    if w.is_none() {
        line.pad(reset_mark(t).width() + 1 + 6);
        return;
    }
    line.add(t, reset_mark(t), Role::Meta);
    line.pad(1);
    line.add(t, &fmt::pad(&reset_cell(w, c), 6), Role::Meta);
}

/// A stale number must not read as measured truth; one past `stop_at` is the gate.
fn pct_role(r: &AccountRow, w: Option<&LimitWindow>, c: &Ctx) -> Role {
    if stale_quota(r, c) {
        return Role::Meta;
    }
    match w {
        Some(w) if w.utilization >= c.scoring.stop_at => Role::Err,
        _ => Role::Meta,
    }
}

fn service_word(r: &AccountRow, health: Health, c: &Ctx) -> String {
    match health {
        Health::Cooling => usage::ineligible_text(Ineligible::Cooling, Some(r), c.now),
        Health::AuthBroken => "auth broken".to_owned(),
        Health::Disabled => "disabled".to_owned(),
        h => watch::health_word(h).to_owned(),
    }
}

/// At Wide: the gate that holds the account back, else its health when it is not healthy.
fn status_cell(r: &AccountRow, health: Health, c: &Ctx) -> (String, Role) {
    if let Some(g) = gate_of(r, c) {
        return (usage::ineligible_text(g, Some(r), c.now), gate_role(g));
    }
    if health == Health::Healthy {
        return (String::new(), Role::Meta);
    }
    (
        watch::health_word(health).to_owned(),
        usage::health_role(health),
    )
}

fn gate_role(g: Ineligible) -> Role {
    if g == Ineligible::AtCapacity {
        Role::Meta
    } else {
        Role::Err
    }
}

fn stale_quota(r: &AccountRow, c: &Ctx) -> bool {
    match r.quota_observed_at {
        None => r.quota.is_some(),
        Some(at) => StdDuration::try_from(c.now - at).unwrap_or(StdDuration::ZERO) > c.max_age,
    }
}

fn reset_mark(theme: &Theme) -> &'static str {
    if theme.ascii { "~" } else { "\u{21bb}" }
}

fn reset_cell(w: Option<&LimitWindow>, c: &Ctx) -> String {
    let Some(at) = w.and_then(|w| w.resets_at) else {
        return "-".to_owned();
    };
    let left = StdDuration::try_from(at - c.now).unwrap_or(StdDuration::ZERO);
    let text = fmt::until(left);
    if w.is_some_and(|w| w.measured) {
        text
    } else {
        format!("~{text}")
    }
}

/// `watch::gauge_bar`-shaped: the same rounding, the board's own glyphs and width.
pub fn gauge(util: f64, cells: usize, theme: &Theme) -> String {
    let (on, off) = if theme.ascii {
        ('#', '-')
    } else {
        ('\u{2587}', '\u{2591}')
    };
    let filled = (util.clamp(0.0, 1.0) * cells as f64).round() as usize;
    format!(
        "{}{}",
        on.to_string().repeat(filled),
        off.to_string().repeat(cells - filled)
    )
}

fn health_glyph(theme: &Theme, h: Health) -> &'static str {
    match h {
        Health::Healthy => theme.g(Glyph::Bullet),
        Health::Degraded if theme.ascii => "!",
        Health::Degraded => "\u{25d0}",
        Health::Cooling => theme.g(Glyph::Failed),
        Health::AuthBroken => theme.g(Glyph::Cancelled),
        Health::Disabled => theme.g(Glyph::Queued),
    }
}

/// The dispatcher's own gate over the row's live numbers, `inflight` recounted from journals.
fn gate_of(r: &AccountRow, c: &Ctx) -> Option<Ineligible> {
    policy::gate(
        r.health,
        r.cooldown_until,
        r.quota.as_ref(),
        r.inflight,
        r.max_concurrency,
        &c.scoring,
        c.now,
    )
}

/// A detail line: styled pieces, the indent already in the first one.
type Segs = Vec<(String, Role)>;

/// What the selected row is, and why.
pub fn detail(b: &Board, rows: &Rows, c: &Ctx) -> Vec<Line<'static>> {
    let lines = match &b.selected {
        Selection::Node { run, logical } => match rows.brain(*run, *logical) {
            Some(brain) => brain_detail(brain, c),
            None => {
                let task = rows
                    .task(*run, *logical)
                    .cloned()
                    .or_else(|| b.pane(*run).and_then(|p| p.task_row(*logical, 0, b.now)));
                match task {
                    Some(t) => task_detail(b, &t, rows, c),
                    None => vec![plain("nothing running", Role::Meta)],
                }
            }
        },
        Selection::Dispatch { run, id } => match rows.group(*run, *id) {
            Some(g) => dispatch_detail(g, c),
            None => vec![plain(
                &format!("dispatch {} is gone", inspect::short(*id)),
                Role::Meta,
            )],
        },
        Selection::Account(id) => account_detail(rows, id, c),
        Selection::None => vec![plain("nothing running", Role::Meta)],
    };
    lines.into_iter().map(|segs| seg_line(segs, c)).collect()
}

/// At most `n` lines; the last one kept ends with ` …` when some were cut.
pub fn cap(mut lines: Vec<Line<'static>>, n: usize, c: &Ctx) -> Vec<Line<'static>> {
    if lines.len() <= n {
        return lines;
    }
    lines.truncate(n);
    if let Some(last) = lines.pop() {
        let mut r = Row::from_line(Row::from_line(last).line(c.w().saturating_sub(2)));
        r.add(c.theme, " \u{2026}", Role::Meta);
        lines.push(r.line(c.w()));
    }
    lines
}

fn plain(text: &str, role: Role) -> Segs {
    vec![(format!("  {text}"), role)]
}

fn seg_line(segs: Segs, c: &Ctx) -> Line<'static> {
    let mut r = Row::default();
    for (text, role) in segs {
        r.add(c.theme, &fmt::sanitize(&text), role);
    }
    r.line(c.w())
}

fn brain_detail(n: &NodeRow, c: &Ctx) -> Vec<Segs> {
    let model = n
        .model
        .as_deref()
        .map(short_model)
        .unwrap_or_else(|| "-".into());
    let account = n.account.as_ref().map_or("-", |a| a.0.as_str());
    let elapsed = n.elapsed(c.now).map(fmt::duration).unwrap_or_default();
    vec![plain(
        &format!(
            "brain{SEP}run {}{SEP}{model} on {account}{SEP}{elapsed}",
            n.run.short()
        ),
        Role::Meta,
    )]
}

fn task_detail(b: &Board, t: &TaskRow, rows: &Rows, c: &Ctx) -> Vec<Segs> {
    let n = &t.row;
    let id = n.short();
    match &n.state {
        NodeState::Blocked { until, .. } => {
            let mut out = vec![vec![
                (format!("  {id}"), Role::Text),
                (
                    format!(" blocked{SEP}{}", fmt::until_at(*until, c.now)),
                    Role::Meta,
                ),
            ]];
            let list = &t.ineligible;
            let longest = list.iter().map(|(a, _)| a.0.width()).max().unwrap_or(0);
            for (account, g) in list {
                out.push(vec![
                    (
                        format!("    {}  ", fmt::pad(&account.0, longest)),
                        Role::Meta,
                    ),
                    (recorded_text(*g, rows.account(account), c), gate_role(*g)),
                ]);
            }
            out
        }
        NodeState::Queued => {
            let waited = n.elapsed(c.now).map(fmt::duration).unwrap_or_default();
            vec![plain(
                &format!("{id} queued{SEP}waiting {waited}"),
                Role::Meta,
            )]
        }
        NodeState::Rejected { reason } => vec![plain(
            &format!("{id} rejected{SEP}{}", trace::failure_detail(reason)),
            Role::Err,
        )],
        NodeState::Cancelled { by } => vec![plain(
            &format!(
                "{id} cancelled by {}",
                crate::dispatch::cancel::source_word(*by)
            ),
            Role::Meta,
        )],
        _ => ran_detail(b, t, rows, c),
    }
}

/// A task that got an account: why that one, what went wrong, who else was passed over.
fn ran_detail(b: &Board, t: &TaskRow, rows: &Rows, c: &Ctx) -> Vec<Segs> {
    let n = &t.row;
    let id = n.short();
    let note = b.note_for(n.run, n.logical);
    let narrow = c.l.band == Band::Narrow;
    let mut first = vec![(format!("  {id}"), Role::Text)];
    let head_w = match note {
        Some(note) => {
            let arrow = format!(" \u{2192} {}", fmt::sanitize(&note.account.0));
            let w = id.width() + arrow.width();
            first.push((arrow, Role::Accent));
            w
        }
        None => id.width(),
    };
    let indent = if narrow { 4 } else { 2 + head_w + 3 };
    let room = c.w().saturating_sub(indent).max(8);
    let mut chunks: Vec<Segs> = Vec::new();
    match note {
        Some(note) => {
            let (score, rest) = split_reason(&note.reason, narrow);
            let head_room = if narrow {
                c.w().saturating_sub(2 + head_w + 3).max(8)
            } else {
                room
            };
            let mut lines = wrap(&[(score, Role::Meta)], head_room);
            if !lines.is_empty() {
                first.push(("   ".to_owned(), Role::Meta));
                first.extend(lines.remove(0));
            }
            chunks.extend(lines);
            chunks.extend(wrap(&[(rest, Role::Meta)], room));
        }
        None => first.push(("   no dispatch record for this task".to_owned(), Role::Meta)),
    }
    if let NodeState::Failed { failure } = &n.state {
        chunks.extend(wrap(
            &[(
                format!("failed: {}", trace::failure_detail(failure)),
                Role::Err,
            )],
            room,
        ));
    }
    if let Some(note) = note {
        chunks.extend(passed_over(t, note, rows, room, narrow, c));
    }
    for line in order::attempt_lines(&t.prior) {
        chunks.extend(wrap(&[(line, Role::Meta)], room));
    }
    let mut out = vec![first];
    for mut segs in chunks {
        segs.insert(0, (" ".repeat(indent), Role::Meta));
        out.push(segs);
    }
    out
}

struct Passed {
    text: String,
    role: Role,
    live: bool,
    eligible: bool,
}

/// §3.4: every account passed over, the gate recorded at block time, else the live one.
fn passed_over(
    t: &TaskRow,
    note: &SelectionNote,
    rows: &Rows,
    room: usize,
    narrow: bool,
    c: &Ctx,
) -> Vec<Segs> {
    let recorded = &t.ineligible;
    let passed = |text: String, role: Role, live: bool, eligible: bool| Passed {
        text,
        role,
        live,
        eligible,
    };
    let items: Vec<Passed> = note
        .excluded
        .iter()
        .map(|id| {
            let name = fmt::sanitize(&id.0);
            let row = rows.account(id);
            if let Some((_, g)) = recorded.iter().find(|(a, _)| a == id) {
                let text = recorded_text(*g, row, c);
                return passed(format!("{name} {text}"), gate_role(*g), false, false);
            }
            let Some(r) = row else {
                return passed(
                    format!("{name} not in accounts.json"),
                    Role::Err,
                    true,
                    false,
                );
            };
            match gate_of(r, c) {
                Some(g) => passed(
                    format!("{name} {}", usage::ineligible_text(g, Some(r), c.now)),
                    gate_role(g),
                    true,
                    false,
                ),
                None => passed(format!("{name} eligible now"), Role::Meta, true, true),
            }
        })
        .collect();
    if items.is_empty() {
        return Vec::new();
    }
    let all_live = items.iter().all(|i| i.live);
    let tag = |live: bool, eligible: bool| {
        if live && !eligible && !all_live {
            " (now)"
        } else {
            ""
        }
    };
    let prefix = if all_live {
        "passed over (now): "
    } else {
        "passed over: "
    };
    if narrow {
        let mut out = vec![vec![(prefix.trim_end().to_owned(), Role::Meta)]];
        for i in items {
            out.extend(wrap(
                &[(format!("{}{}", i.text, tag(i.live, i.eligible)), i.role)],
                room,
            ));
        }
        return out;
    }
    let mut segs: Segs = vec![(prefix.trim_end().to_owned(), Role::Meta)];
    for (n, i) in items.into_iter().enumerate() {
        if n > 0 {
            segs.push(("\u{b7}".to_owned(), Role::Meta));
        }
        segs.push((format!("{}{}", i.text, tag(i.live, i.eligible)), i.role));
    }
    wrap(&segs, room)
}

fn dispatch_detail(g: &DispatchGroup, c: &Ctx) -> Vec<Segs> {
    let by = match g.caller {
        Some(caller) => caller.short(),
        None => "brain".to_owned(),
    };
    let state = dispatches::state_word(g.state);
    let age: StdDuration = (c.now - g.at).try_into().unwrap_or(StdDuration::ZERO);
    let mut phrase = vec![dispatches::tasks_word(g.tally.total() as u32)];
    let counts = order::text(&g.tally.cells());
    if !counts.is_empty() {
        phrase.push(counts);
    }
    let cost = dispatches::cost_cell(&g.cost);
    if !cost.is_empty() {
        phrase.push(cost);
    }
    let mut head: Segs = order::label_cells(g.seq, g.id, label_role(g))
        .into_iter()
        .enumerate()
        .map(|(i, cell)| {
            let lead = if i == 0 { "  " } else { cell.glue };
            (format!("{lead}{}", cell.text), cell.role)
        })
        .collect();
    head.push((
        format!("{SEP}by {by}{SEP}{state}{SEP}{} ago", fmt::duration(age)),
        Role::Meta,
    ));
    vec![
        head,
        plain(&phrase.join(SEP), Role::Meta),
        plain(
            &format!("swamp dispatch {}", inspect::short(g.id)),
            Role::Accent,
        ),
    ]
}

fn label_role(g: &DispatchGroup) -> Role {
    if g.state == DispatchState::Open {
        Role::Name
    } else {
        Role::Meta
    }
}

fn account_detail(rows: &Rows, id: &AccountId, c: &Ctx) -> Vec<Segs> {
    // From `rows`, not `board.accounts`: `inflight` there is the zero the state file carries.
    let Some(r) = rows.account(id) else {
        return vec![plain("no such account in accounts.json", Role::Meta)];
    };
    let health = watch::shown_health(r.health, r.cooldown_until, c.now);
    let provider = r
        .provider
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "not in config".to_owned());
    let mut out = vec![plain(
        &format!(
            "{} \u{2192} {provider}{SEP}{}{SEP}{} in flight{SEP}{}",
            fmt::sanitize(&r.account.0),
            watch::health_word(health),
            r.inflight,
            tokens(r.window_tokens.billable(), true, c)
        ),
        Role::Meta,
    )];
    if let Some(g) = gate_of(r, c) {
        out.push(plain(
            &usage::ineligible_text(g, Some(r), c.now),
            gate_role(g),
        ));
    }
    out
}

/// At Narrow the score stays on the head line and the terms wrap under it.
fn split_reason(reason: &Reason, narrow: bool) -> (String, String) {
    if !narrow || reason.form != ReasonForm::Terms {
        return (reason.text.clone(), String::new());
    }
    match reason.text.split_once(" = ") {
        Some((score, terms)) => (score.to_owned(), terms.to_owned()),
        None => (reason.text.clone(), String::new()),
    }
}

/// Greedy word wrap that keeps each word's role; an overlong word is cut when drawn.
fn wrap(segs: &[(String, Role)], room: usize) -> Vec<Segs> {
    let mut out: Vec<Segs> = Vec::new();
    let mut line: Segs = Vec::new();
    let mut w = 0usize;
    for (text, role) in segs {
        for word in text.split(' ').filter(|w| !w.is_empty()) {
            let ww = word.width();
            if w > 0 && w + 1 + ww > room {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            if w > 0 {
                line.push((" ".to_owned(), Role::Meta));
                w += 1;
            }
            line.push((word.to_owned(), *role));
            w += ww;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

// ---------------------------------------------------------------- cells

/// A line under construction: spans plus the width they already occupy.
#[derive(Default)]
struct Row {
    spans: Vec<Span<'static>>,
    w: usize,
}

impl Row {
    fn from_line(line: Line<'static>) -> Row {
        let w = line.spans.iter().map(|s| s.content.width()).sum();
        Row {
            spans: line.spans,
            w,
        }
    }

    fn add(&mut self, theme: &Theme, text: &str, role: Role) {
        if text.is_empty() {
            return;
        }
        self.w += text.width();
        self.spans.push(theme.span(text.to_owned(), role));
    }

    fn pad(&mut self, n: usize) {
        if n > 0 {
            self.w += n;
            self.spans.push(Span::raw(" ".repeat(n)));
        }
    }

    fn to(&mut self, col: usize) {
        self.pad(col.saturating_sub(self.w));
    }

    fn tail(&mut self, theme: &Theme, text: &str, role: Role, width: usize) {
        let text = fmt::truncate(text, width.saturating_sub(self.w + 1));
        self.to(width.saturating_sub(text.width()));
        self.add(theme, &text, role);
    }

    /// Nothing leaves this module without a width check: a title or a model name is worker
    /// text, and a line that overruns the pane is how a row repaints its neighbour.
    fn line(self, width: usize) -> Line<'static> {
        let mut out: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        for s in self.spans {
            if used >= width {
                break;
            }
            let w = s.content.width();
            if used + w <= width {
                used += w;
                out.push(s);
                continue;
            }
            let cut = fmt::truncate(&s.content, width - used);
            used += cut.width();
            out.push(Span::styled(cut, s.style));
        }
        Line::from(out)
    }
}

/// Right-aligned in `w`, width aware and never letting a control character through.
fn right(s: &str, w: usize) -> String {
    let cell = fmt::truncate(s, w);
    format!("{}{cell}", " ".repeat(w.saturating_sub(cell.width())))
}
