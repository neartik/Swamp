//! WP4: the three layouts of `docs/BOARD.md` §3. Pure: it turns a `Board` into `Line`s and
//! reads no clock and no file of its own.
//!
//! Everything it formats comes from the shared helpers - `ui::fmt`, `watch::gauge_bar`,
//! `watch::shown_health`, `watch::health_word`, `ui::usage` - so the board, `/usage` and
//! `swamp watch` cannot drift apart.

use crate::dispatch::account::Health;
use crate::dispatch::policy::{self, Ineligible, Scoring};
use crate::model::core::{AccountId, LimitScope, LimitWindow, NodeState, Tier};
use crate::ui::board::model::{
    AccountGroup, Board, NodeRow, ProviderGroup, Reason, ReasonForm, Rows, Section, Selection,
    SelectionNote, Summary,
};
use crate::ui::chat::spinner;
use crate::ui::chat::theme::{Glyph, Role, Theme};
use crate::ui::chat::workers::short_model;
use crate::ui::usage::AccountRow;
use crate::ui::{fmt, usage, watch};
use ratatui::text::{Line, Span};
use std::time::Duration as StdDuration;
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;

const SEP: &str = " \u{b7} ";
const TITLE: &str = "swamp board";

/// Below this the bars halve, titles take a line of their own and the quota windows stack.
pub const NARROW: u16 = 52;

// ---------------------------------------------------------------- layout

/// Which cells this width can afford. Drop order, widest first: cost, model, run, tier,
/// tokens. Glyph, id and title never drop; the title moves to its own line instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub width: u16,
    pub bar: usize,
    pub name_w: usize,
    pub title_own_line: bool,
    pub show_tokens: bool,
    pub show_tier: bool,
    pub show_run: bool,
    pub show_model: bool,
    pub show_cost: bool,
    pub show_health_word: bool,
    /// Both quota windows on the account row rather than stacked under it.
    pub windows_inline: bool,
    pub show_clock: bool,
    pub clock_secs: bool,
    pub show_group_counts: bool,
}

pub fn layout_for(width: u16) -> Layout {
    let w = width;
    Layout {
        width,
        bar: if w >= NARROW { 10 } else { 5 },
        name_w: if w >= NARROW { 13 } else { 10 },
        title_own_line: w < NARROW,
        show_tokens: w >= 30,
        show_tier: w >= 34,
        show_run: w >= 84,
        show_model: w >= 92,
        show_cost: w >= 100,
        show_health_word: w >= 96,
        windows_inline: w >= 96,
        show_clock: w >= 40,
        clock_secs: w >= 96,
        show_group_counts: w >= 56,
    }
}

/// Everything a line needs that is not the line itself.
pub struct Ctx<'a> {
    pub l: Layout,
    pub theme: &'a Theme,
    pub now: OffsetDateTime,
    pub tick: u64,
    pub max_age: StdDuration,
    /// The run column only earns its width once two runs are tailed.
    pub multi_run: bool,
    /// The dispatcher's thresholds, so the footer's exclusion lines read the same gate the
    /// pool read.
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
            theme,
            now: b.now,
            tick,
            max_age,
            multi_run: b.runs.len() > 1,
            scoring: b.scoring,
        }
    }

    fn width(&self) -> usize {
        self.l.width as usize
    }

    fn run_col(&self) -> bool {
        self.l.show_run && self.multi_run
    }
}

// ---------------------------------------------------------------- frame

/// Header, tree, waiting, recent, the selected row's dispatch reason, the key hints.
pub fn frame(
    b: &Board,
    width: u16,
    theme: &Theme,
    tick: u64,
    max_age: StdDuration,
) -> Vec<Line<'static>> {
    let c = Ctx::new(b, width, theme, tick, max_age);
    let rows = b.rows();
    let mut out = vec![header(b, &rows, &c), rule(&c)];
    out.extend(body(&rows, &c));
    out.extend(footer(b, &rows, &c));
    out
}

pub fn rule(c: &Ctx) -> Line<'static> {
    Line::from(
        c.theme
            .span(c.theme.g(Glyph::Rule).repeat(c.width()), Role::Meta),
    )
}

// ---------------------------------------------------------------- header

/// `swamp board   2 runs · 4 in flight · 3 accounts · ~$0.47 · fresh 0.4s · 14:07:22`, the
/// optional cells dropping left to right as the pane narrows.
pub fn header(b: &Board, rows: &Rows, c: &Ctx) -> Line<'static> {
    let s = b.summary(rows);
    let mut cells: Vec<(String, Role, u8)> = vec![(runs_cell(&s), Role::Meta, 0)];
    if s.stale_runs > 0 {
        cells.push((format!("{} stale", s.stale_runs), Role::Err, 0));
    }
    cells.push((format!("{} in flight", s.in_flight), Role::Meta, 0));
    if c.l.show_health_word {
        cells.push((count(s.accounts, "account"), Role::Meta, 2));
        if s.cost_usd > 0.0 {
            cells.push((cost_total(&s), Role::Meta, 3));
        }
    }
    if let Some((text, role)) = freshness(b, c) {
        cells.push((text, role, 1));
    }
    if c.l.show_clock {
        cells.push((
            if c.l.clock_secs {
                fmt::clock(c.now)
            } else {
                fmt::clock_hm(c.now)
            },
            Role::Meta,
            4,
        ));
    }

    // Drop the widest-priority cells until what is left fits beside the title.
    for drop in [3u8, 2, 1, 4] {
        if TITLE.width() + 2 + joined_width(&cells) <= c.width() {
            break;
        }
        cells.retain(|(_, _, p)| *p != drop);
    }

    let mut r = Row::default();
    r.add(c.theme, TITLE, Role::Name);
    let tail: usize = joined_width(&cells);
    r.to(c.width().saturating_sub(tail));
    for (i, (text, role, _)) in cells.iter().enumerate() {
        if i > 0 {
            r.add(c.theme, SEP, Role::Meta);
        }
        r.add(c.theme, text, *role);
    }
    r.line(c.width())
}

fn joined_width(cells: &[(String, Role, u8)]) -> usize {
    cells.iter().map(|(t, _, _)| t.width()).sum::<usize>()
        + SEP.width() * cells.len().saturating_sub(1)
}

fn runs_cell(s: &Summary) -> String {
    if s.hidden_runs > 0 {
        return format!("{} of {} runs", s.runs, s.runs + s.hidden_runs);
    }
    count(s.runs, "run")
}

fn cost_total(s: &Summary) -> String {
    format!("~${:.2}", s.cost_usd)
}

fn count(n: usize, word: &str) -> String {
    let plural = if n == 1 { "" } else { "s" };
    format!("{n} {word}{plural}")
}

/// How far behind the persisted account snapshot is. Past `quota_max_age` it stops being a
/// lag and becomes the reason dispatch is wrong, so it changes colour and wording.
fn freshness(b: &Board, c: &Ctx) -> Option<(String, Role)> {
    let at = b.accounts_at?;
    let age: StdDuration = (c.now - at).try_into().unwrap_or(StdDuration::ZERO);
    if age > c.max_age {
        return Some((format!("observed {} ago", fmt::until(age)), Role::Err));
    }
    if age.as_secs() < 10 {
        return Some((format!("fresh {:.1}s", age.as_secs_f64()), Role::Meta));
    }
    Some((format!("fresh {}", fmt::duration(age)), Role::Meta))
}

// ---------------------------------------------------------------- body

/// Provider -> account -> the nodes that account is running, then waiting, then recent.
pub fn body(rows: &Rows, c: &Ctx) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut spin = 0usize;
    for group in &rows.providers {
        out.push(provider_line(group, c));
        for account in &group.accounts {
            out.extend(account_lines(account, c));
            for n in &account.nodes {
                out.extend(node_lines(n, Section::InFlight, &mut spin, c));
            }
        }
    }
    if !rows.orphan_nodes.is_empty() {
        out.push(heading(
            &section_word(
                "unknown accounts",
                rows.orphan_nodes.len(),
                rows.orphan_total,
            ),
            c,
        ));
        for n in &rows.orphan_nodes {
            out.extend(node_lines(n, Section::InFlight, &mut spin, c));
        }
    }
    if !rows.waiting.is_empty() {
        out.push(Line::default());
        out.push(heading(
            &section_word("waiting", rows.waiting.len(), rows.waiting_total),
            c,
        ));
        for n in &rows.waiting {
            out.extend(node_lines(n, Section::Waiting, &mut spin, c));
        }
    }
    if !rows.recent.is_empty() {
        out.push(heading(&format!("recent {}", rows.recent.len()), c));
        for n in &rows.recent {
            out.extend(node_lines(n, Section::Recent, &mut spin, c));
        }
    }
    out
}

/// `waiting 12`, or `waiting 32 of 480` once `SECTION_MAX` hid the rest, the same shape
/// the header uses for the runs it could not tail.
fn section_word(word: &str, shown: usize, total: usize) -> String {
    if total > shown {
        return format!("{word} {shown} of {total}");
    }
    format!("{word} {total}")
}

fn heading(text: &str, c: &Ctx) -> Line<'static> {
    let mut r = Row::default();
    r.add(c.theme, text, Role::Name);
    r.line(c.width())
}

fn provider_line(g: &ProviderGroup, c: &Ctx) -> Line<'static> {
    let name = match g.provider {
        Some(p) => p.as_str().to_owned(),
        None => "not in config".to_owned(),
    };
    let mut r = Row::default();
    r.add(c.theme, &name, Role::Name);
    if c.l.show_group_counts {
        let mut tail = count(g.accounts.len(), "account");
        if c.l.windows_inline {
            let flight: usize = g.accounts.iter().map(|a| a.nodes.len()).sum();
            tail = format!("{tail}{SEP}{flight} in flight");
        }
        r.tail(c.theme, &tail, Role::Meta, c.width());
    }
    r.line(c.width())
}

// ---------------------------------------------------------------- accounts

fn account_lines(g: &AccountGroup, c: &Ctx) -> Vec<Line<'static>> {
    let r = &g.row;
    let health = watch::shown_health(r.health, r.cooldown_until, c.now);
    let role = usage::health_role(health);
    let w5 = usage::window_for(&r.quota, LimitScope::FiveHour, c.now);
    let w7 = usage::window_for(&r.quota, LimitScope::SevenDay, c.now);
    let quota_role = if stale_quota(r, c) { Role::Meta } else { role };
    let flight = format!(
        "{}/{}",
        r.inflight,
        r.max_concurrency
            .map(|m| m.to_string())
            .unwrap_or_else(|| "-".to_owned())
    );

    let mut first = Row::default();
    first.add(c.theme, health_glyph(c.theme, health), role);
    first.add(c.theme, " ", Role::Meta);
    first.add(c.theme, &fmt::pad(&r.account.0, c.l.name_w), Role::Name);
    first.add(c.theme, " ", Role::Meta);
    if c.l.show_health_word {
        first.add(c.theme, &fmt::pad(watch::health_word(health), 11), role);
    }
    first.add(c.theme, &right(&flight, 4), Role::Meta);
    first.add(c.theme, "  ", Role::Meta);
    let window_col = first.w;
    first.add(c.theme, &window_cell("5h", w5, c), quota_role);
    let mut out = Vec::new();
    if c.l.windows_inline {
        first.add(c.theme, "  ", Role::Meta);
        first.add(c.theme, &window_cell("7d", w7, c), quota_role);
        first.tail(c.theme, &token_cell(r), Role::Meta, c.width());
        out.push(first.line(c.width()));
    } else {
        if !c.l.title_own_line {
            first.tail(c.theme, &token_cell(r), Role::Meta, c.width());
        }
        out.push(first.line(c.width()));
        let mut second = Row::default();
        second.to(window_col);
        second.add(c.theme, &window_cell("7d", w7, c), quota_role);
        out.push(second.line(c.width()));
    }

    let status = status_parts(g, health, role, c);
    if !status.is_empty() {
        let mut cell = Row::default();
        for (i, (text, role)) in status.iter().enumerate() {
            if i > 0 {
                cell.add(c.theme, SEP, Role::Meta);
            }
            cell.add(c.theme, text, *role);
        }
        // The bar row takes it when it fits whole; a clipped status word says nothing.
        let used: usize = out
            .last()
            .map(|l| l.spans.iter().map(|s| s.content.width()).sum())
            .unwrap_or(c.width());
        if !c.l.windows_inline && used + 2 + cell.w <= c.width() {
            let mut last = out.pop().expect("the 7d row");
            last.spans
                .push(Span::raw(" ".repeat(c.width() - used - cell.w)));
            last.spans.extend(cell.spans);
            out.push(last);
        } else {
            let mut line = Row::default();
            line.to(4);
            line.w += cell.w;
            line.spans.extend(cell.spans);
            out.push(line.line(c.width()));
        }
    }
    out
}

/// What the account row says beyond its bars: when it comes back, and `· stale` when the run
/// behind one of its nodes lost its brain, so the marker sits on the affected row rather than
/// only in the header's total.
fn status_parts(g: &AccountGroup, health: Health, role: Role, c: &Ctx) -> Vec<(String, Role)> {
    let mut out = Vec::new();
    if let Some(text) = status_cell(&g.row, health, c) {
        out.push((text, role));
    }
    if g.nodes.iter().any(|n| n.stale) {
        out.push(("stale".to_owned(), Role::Err));
    }
    out
}

/// An account whose percentages are older than `quota_max_age` renders them in `meta`: a
/// stale number is what makes dispatch wrong, so it must not read as measured truth.
fn stale_quota(r: &AccountRow, c: &Ctx) -> bool {
    match r.quota_observed_at {
        None => r.quota.is_some(),
        Some(at) => StdDuration::try_from(c.now - at).unwrap_or(StdDuration::ZERO) > c.max_age,
    }
}

/// `5h ▇▇▇▇▇▇▇░░░  71%  ↻ 38m`, or all dashes when no window was ever measured.
fn window_cell(label: &str, w: Option<&LimitWindow>, c: &Ctx) -> String {
    let bar = match w {
        Some(w) => gauge(w.utilization, c.l.bar, c.theme),
        None => fmt::pad("-", c.l.bar),
    };
    let pct = right(&usage::pct_cell(w), 4);
    let reset = reset_cell(w, c);
    if c.l.title_own_line {
        return format!("{label} {bar} {pct} {}{reset}", reset_mark(c.theme));
    }
    format!(
        "{label} {bar} {pct} {} {}",
        reset_mark(c.theme),
        fmt::pad(&reset, 6)
    )
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

fn token_cell(r: &AccountRow) -> String {
    let n = r.window_tokens.billable();
    format!("\u{2193} {}", fmt::tokens(n))
}

/// The one thing the health word cannot say on its own: when the account comes back.
fn status_cell(r: &AccountRow, health: Health, c: &Ctx) -> Option<String> {
    let until = r
        .cooldown_until
        .filter(|t| *t > c.now)
        .map(|t| format!("until {}", fmt::clock_day(t, c.now)));
    if c.l.show_health_word {
        return until;
    }
    if health == Health::Healthy {
        return None;
    }
    let word = watch::health_word(health);
    Some(match until {
        Some(u) => format!("{word} {u}"),
        None => word.to_owned(),
    })
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

// ---------------------------------------------------------------- nodes

fn node_lines(n: &NodeRow, section: Section, spin: &mut usize, c: &Ctx) -> Vec<Line<'static>> {
    let theme = c.theme;
    let role = theme.state_role(&n.state);
    let mut r = Row::default();
    r.add(theme, "  ", Role::Meta);
    r.add(theme, node_glyph(n, section, spin, c), role);
    r.add(theme, " ", Role::Meta);
    r.add(theme, &fmt::pad(&id_cell(n, c), 8), Role::Text);
    r.add(theme, " ", Role::Meta);
    if c.run_col() {
        r.add(theme, &fmt::pad(&n.run.short(), 6), Role::Run);
        r.add(theme, " ", Role::Meta);
    }
    if c.l.show_tier {
        r.add(theme, &tier_cell(n, c), tier_role(n));
        r.add(theme, " ", Role::Meta);
    }

    let mut tail: Vec<(String, Role)> = Vec::new();
    if c.l.show_model {
        tail.push((fmt::pad(&model_cell(n), 12), Role::Meta));
    }
    tail.push((right(&elapsed_cell(n, c), 6), Role::Meta));
    if c.l.show_tokens {
        tail.push((right(&node_tokens(n, section, c), 7), Role::Meta));
    }
    if c.l.show_cost {
        tail.push((right(&fmt::cost(n.cost), 7), Role::Meta));
    }
    let tail_w: usize = tail.iter().map(|(t, _)| t.width() + 1).sum();

    let title_w = c.width().saturating_sub(r.w + tail_w);
    if !c.l.title_own_line && title_w > 0 {
        r.add(
            theme,
            &fmt::pad(title_cell(n), title_w),
            title_role(section),
        );
    }
    r.to(c.width().saturating_sub(tail_w));
    for (text, role) in tail {
        r.add(theme, " ", Role::Meta);
        r.add(theme, &text, role);
    }
    let mut out = vec![r.line(c.width())];

    // The title never disappears; at 40 columns it takes a line of its own. `recent` is a
    // fading history strip and keeps to one row per node.
    if c.l.title_own_line && section != Section::Recent {
        let mut line = Row::default();
        line.to(4);
        line.add(
            theme,
            &fmt::truncate(title_cell(n), c.width().saturating_sub(4)),
            title_role(section),
        );
        out.push(line.line(c.width()));
    }
    for text in blocked_lines(n, c) {
        let mut line = Row::default();
        line.to(4);
        line.add(theme, &text, Role::Meta);
        out.push(line.line(c.width()));
    }
    out
}

fn node_glyph(n: &NodeRow, section: Section, spin: &mut usize, c: &Ctx) -> &'static str {
    if n.brain {
        return match (&n.state, n.stale) {
            (_, true) => c.theme.g(Glyph::Orphaned),
            (NodeState::Running { .. } | NodeState::Leased { .. }, _) if c.theme.ascii => "*",
            (NodeState::Running { .. } | NodeState::Leased { .. }, _) => "\u{25c6}",
            (s, _) => c.theme.state_glyph(s),
        };
    }
    if section == Section::InFlight && !n.stale && matches!(n.state, NodeState::Running { .. }) {
        let frame = spinner::worker_frame(c.theme, c.tick, *spin);
        *spin += 1;
        return frame;
    }
    // Only the rows that were animating freeze; a queued node still reads as queued.
    if n.stale
        && matches!(
            n.state,
            NodeState::Running { .. } | NodeState::Leased { .. }
        )
    {
        return c.theme.g(Glyph::Orphaned);
    }
    c.theme.state_glyph(&n.state)
}

fn id_cell(n: &NodeRow, c: &Ctx) -> String {
    if n.brain {
        // The brain's node id is the run's, so the short id is what tells two apart.
        return if c.multi_run && !c.run_col() {
            n.id.short()
        } else {
            "brain".to_owned()
        };
    }
    if n.attempt > 1 {
        return format!("{}\u{b7}{}", n.id.short(), n.attempt);
    }
    n.id.short()
}

fn tier_cell(n: &NodeRow, c: &Ctx) -> String {
    if n.brain && !c.l.show_health_word {
        return fmt::pad("-", 4);
    }
    if c.l.show_health_word {
        if n.brain {
            return fmt::pad("-", 6);
        }
        return format!("[{}]", fmt::pad(n.tier.as_str(), 4));
    }
    fmt::pad(n.tier.as_str(), 4)
}

fn tier_role(n: &NodeRow) -> Role {
    if n.tier == Tier::High && !n.brain {
        Role::TierHi
    } else {
        Role::Meta
    }
}

/// The brain's recorded title is the word "brain", which the id cell already says; the board
/// shows the working verb instead (`docs/BOARD.md` section 3).
fn title_cell(n: &NodeRow) -> &str {
    if n.brain
        && matches!(
            n.state,
            NodeState::Running { .. } | NodeState::Leased { .. }
        )
    {
        return "orchestrating";
    }
    &n.title
}

fn title_role(section: Section) -> Role {
    match section {
        Section::Recent => Role::Meta,
        _ => Role::Name,
    }
}

fn model_cell(n: &NodeRow) -> String {
    n.model
        .as_deref()
        .map(short_model)
        .unwrap_or_else(|| "-".to_owned())
}

fn elapsed_cell(n: &NodeRow, c: &Ctx) -> String {
    n.elapsed(c.now)
        .map(fmt::duration)
        .unwrap_or_else(|| "-".to_owned())
}

fn node_tokens(n: &NodeRow, section: Section, c: &Ctx) -> String {
    // §3.2: a node the pool parked has no tokens to show, so the cell names the wait instead
    // of a dash, in the band where it is the row's last cell and the title is beside it.
    if section == Section::Waiting
        && matches!(n.state, NodeState::Blocked { .. })
        && !c.l.title_own_line
        && !c.l.show_cost
    {
        return "blocked".to_owned();
    }
    let billable = n.usage.billable();
    if billable == 0 {
        return "-".to_owned();
    }
    format!("{} {}", c.theme.g(Glyph::TokenArrow), fmt::tokens(billable))
}

/// Why a waiting node is waiting: `fmt::blocked_notice`'s own segments, wrapped, so the board
/// and chat word an exhausted pool the same way.
fn blocked_lines(n: &NodeRow, c: &Ctx) -> Vec<String> {
    let NodeState::Blocked { until, .. } = &n.state else {
        return Vec::new();
    };
    if c.l.title_own_line {
        return vec![format!(
            "{} at limit{SEP}{}",
            n.provider,
            fmt::clock_hm(*until)
        )];
    }
    let notice = fmt::blocked_notice(n.provider, *until, c.now, "");
    let room = c.width().saturating_sub(4);
    let mut out: Vec<String> = Vec::new();
    for part in notice.split(SEP).filter(|p| !p.trim().is_empty()) {
        match out.last_mut() {
            Some(last) if last.width() + SEP.width() + part.width() <= room => {
                last.push_str(SEP);
                last.push_str(part);
            }
            _ => out.push(part.to_owned()),
        }
    }
    out
}

// ---------------------------------------------------------------- footer

/// The rule, the selected row's dispatch reason, the rule, the key hints. `rows` comes in
/// because the exclusion lines are re-derived against the counts a frame actually shows.
pub fn footer(b: &Board, rows: &Rows, c: &Ctx) -> Vec<Line<'static>> {
    let mut out = vec![rule(c)];
    out.extend(reason_lines(b, rows, c));
    out.push(rule(c));
    out.push(hints(b, c));
    out
}

fn reason_lines(b: &Board, rows: &Rows, c: &Ctx) -> Vec<Line<'static>> {
    match &b.selected {
        Selection::Node { run, logical } => {
            let Some(note) = b.note_for(*run, *logical) else {
                return vec![note_missing(c)];
            };
            let head = match b.pane(*run).and_then(|p| p.view.by_logical.get(logical)) {
                Some(attempts) => attempts.last().copied().unwrap_or(*logical),
                None => *logical,
            };
            selected_node_lines(&head.short(), note, rows, c)
        }
        Selection::Account(id) => vec![account_reason(rows, id, c)],
        Selection::None => vec![Line::from(c.theme.span(
            fmt::truncate("select a row to see why its account won", c.width()),
            Role::Meta,
        ))],
    }
}

fn note_missing(c: &Ctx) -> Line<'static> {
    Line::from(c.theme.span(
        fmt::truncate("no dispatch record for this row", c.width()),
        Role::Meta,
    ))
}

fn selected_node_lines(
    short: &str,
    note: &SelectionNote,
    rows: &Rows,
    c: &Ctx,
) -> Vec<Line<'static>> {
    let head = format!("{short} \u{2192} {}", fmt::sanitize(&note.account.0));
    let mut out = Vec::new();
    let indent = if c.l.title_own_line {
        2
    } else {
        head.width() + 3
    };
    let room = c.width().saturating_sub(indent).max(8);

    let (first, rest) = split_reason(&note.reason, c);
    let mut chunks = wrap(&first, room);
    let mut r = Row::default();
    r.add(c.theme, &head, Role::Accent);
    r.add(c.theme, "   ", Role::Meta);
    if !c.l.title_own_line {
        r.to(indent);
    }
    if !chunks.is_empty() {
        r.add(c.theme, &chunks.remove(0), Role::Meta);
    }
    out.push(r.line(c.width()));

    chunks.extend(wrap(&rest, room));
    for chunk in chunks {
        let mut line = Row::default();
        line.to(indent);
        line.add(c.theme, &chunk, Role::Meta);
        out.push(line.line(c.width()));
    }
    for (text, role) in excluded_lines(note, rows, c) {
        for chunk in wrap(&text, room) {
            let mut line = Row::default();
            line.to(indent);
            line.add(c.theme, &chunk, role);
            out.push(line.line(c.width()));
        }
    }
    out
}

/// §3.4: every account the pool passed over, with the gate that holds it back right now. The
/// recorded reason says they were excluded at dispatch; an account that would be taken today
/// disagrees with it, and says so with `now`.
fn excluded_lines(note: &SelectionNote, rows: &Rows, c: &Ctx) -> Vec<(String, Role)> {
    note.excluded
        .iter()
        .map(|id| {
            let name = fmt::sanitize(&id.0);
            let Some(r) = account_row(rows, id) else {
                return (
                    format!("{name} excluded{SEP}not in accounts.json"),
                    Role::Err,
                );
            };
            match gate_of(r, c) {
                Some(g) => (
                    format!("{name} ineligible ({})", gate_text(r, g, c)),
                    Role::Err,
                ),
                None => (format!("{name} eligible now"), Role::Meta),
            }
        })
        .collect()
}

fn account_row<'a>(rows: &'a Rows, id: &AccountId) -> Option<&'a AccountRow> {
    rows.providers
        .iter()
        .flat_map(|g| &g.accounts)
        .map(|a| &a.row)
        .find(|r| &r.account == id)
}

/// The dispatcher's own gate, over the row's live numbers: `inflight` here is the journal
/// count `rows()` recomputed, not the zero the state file carries.
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

fn gate_text(r: &AccountRow, g: Ineligible, c: &Ctx) -> String {
    match g {
        Ineligible::Cooling => match r.cooldown_until {
            Some(t) => format!("cooldown until {}", fmt::clock_day(t, c.now)),
            None => g.word().to_owned(),
        },
        Ineligible::AtCapacity => format!(
            "at {}/{}",
            r.inflight,
            r.max_concurrency.unwrap_or(r.inflight)
        ),
        Ineligible::QuotaStop => match r
            .quota
            .as_ref()
            .and_then(|q| q.measured_utilization_at(c.now))
        {
            Some(u) => format!("quota {}%", (u * 100.0).round() as i64),
            None => g.word().to_owned(),
        },
        g => g.word().to_owned(),
    }
}

/// 40 columns keeps the score on the head line and wraps the terms under it; wider layouts
/// keep the recorded string whole and let it wrap on the hanging indent.
fn split_reason(reason: &Reason, c: &Ctx) -> (String, String) {
    if !c.l.title_own_line || reason.form != ReasonForm::Terms {
        return (reason.text.clone(), String::new());
    }
    match reason.text.split_once(" = ") {
        Some((score, terms)) => (score.to_owned(), terms.to_owned()),
        None => (reason.text.clone(), String::new()),
    }
}

fn wrap(text: &str, room: usize) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    textwrap::wrap(&fmt::sanitize(text), room.max(8))
        .into_iter()
        .map(|c| c.into_owned())
        .collect()
}

fn account_reason(rows: &Rows, id: &AccountId, c: &Ctx) -> Line<'static> {
    // From `rows`, not `board.accounts`: `inflight` there is the zero the state file carries.
    let Some(r) = account_row(rows, id) else {
        return note_missing(c);
    };
    let health = watch::shown_health(r.health, r.cooldown_until, c.now);
    let provider = r
        .provider
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "not in config".to_owned());
    let text = format!(
        "{} \u{2192} {provider}{SEP}{}{SEP}{} in flight{SEP}{}",
        fmt::sanitize(&r.account.0),
        watch::health_word(health),
        r.inflight,
        token_cell(r)
    );
    Line::from(c.theme.span(fmt::truncate(&text, c.width()), Role::Meta))
}

fn hints(b: &Board, c: &Ctx) -> Line<'static> {
    let keys: &[&str] = if c.l.show_health_word {
        &[
            "\u{2191}\u{2193} select",
            "enter trace",
            "tab run",
            "a accounts",
            "r raw",
            "q quit",
        ]
    } else if c.l.show_group_counts {
        &[
            "\u{2191}\u{2193} select",
            "\u{21b5} trace",
            "tab run",
            "a accounts",
            "q quit",
        ]
    } else {
        &["\u{2191}\u{2193}", "\u{21b5} trace", "tab run", "q quit"]
    };
    let mut r = Row::default();
    r.add(c.theme, &keys.join(SEP), Role::Meta);
    if c.l.show_run {
        let shorts: Vec<String> = b.runs.iter().map(|p| p.run.short()).collect();
        if !shorts.is_empty() {
            r.tail(c.theme, &shorts.join(" "), Role::Run, c.width());
        }
    }
    r.line(c.width())
}

// ---------------------------------------------------------------- cells

/// A line under construction: spans plus the width they already occupy.
#[derive(Default)]
struct Row {
    spans: Vec<Span<'static>>,
    w: usize,
}

impl Row {
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
