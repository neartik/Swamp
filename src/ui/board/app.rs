//! WP6: the keys, the pagers and the loop of `docs/BOARD.md` §4-5.
//!
//! The pane state is pure and testable: `on_key` takes a `Board` and returns what the outer
//! loop has to do, and `lines` turns the two of them into a frame. Only `run_tui` touches a
//! terminal, and it enters the alternate screen behind `watch::TerminalGuard`, so neither a
//! panic nor a ctrl+c leaves a wrecked tty. The one thing it changes is a confirmed cancel,
//! run through `ui::actions` off the loop.

use crate::dispatch::cancel::dispatch_tasks;
use crate::ids::{DispatchId, NodeId, RunId};
use crate::journal::inspect;
use crate::journal::paths::{self, write_board_pid};
use crate::model::core::NodeState;
use crate::ui::actions::{CancelDone, CancelTarget, spawn_cancel};
use crate::ui::board::model::{Board, DispatchGroup, Rows, Selection, attention, newest_running};
use crate::ui::board::render;
use crate::ui::board::sources::Sources;
use crate::ui::chat::theme::{Role, Theme};
use crate::ui::keys::{self, KeyAction, Surface};
use crate::ui::order;
use crate::ui::trace::{TraceOpts, render as trace_render};
use crate::ui::{dispatches, fmt, usage, watch};
use camino::{Utf8Path, Utf8PathBuf};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration as StdDuration, Instant};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// `ui.refresh_hz` default, the same one `swamp watch` uses.
const REFRESH_HZ: u16 = 20;

/// What an idle board waits on: nothing is animating, so the account poll is the only clock.
const IDLE_PERIOD: StdDuration = StdDuration::from_secs(1);

/// How many journal lines `r` shows, per §4.
const RAW_LINES: usize = 200;

/// How much of a journal `r` reads at a time, and the most it will ever read: a run whose
/// lines are enormous stops at the cap rather than pulling the whole file into the loop.
const RAW_CHUNK: u64 = 64 * 1024;
const RAW_MAX: u64 = 4 * 1024 * 1024;

/// How long a notice stays in place of the hints.
pub const NOTICE_TTL: StdDuration = StdDuration::from_secs(5);

// ---------------------------------------------------------------- state

/// What a keypress asks the outer loop to do. Everything that needs no file is already done
/// by the time this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    LoadRaw(RunId),
    Cancel(CancelTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerKind {
    Trace,
    Dispatch,
    Raw,
    Keys,
}

/// A full-pane scroll view over text: a trace, a dispatch, a raw journal tail, the keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pager {
    pub kind: PagerKind,
    /// What follows the kind word on the title line.
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub run: Option<RunId>,
}

/// Everything the board draws that is not the board itself.
#[derive(Debug)]
pub struct App {
    /// `ui.board_actions`: whether `k` may cancel at all.
    pub actions: bool,
    /// Folds the viewer chose, over the model's own open-expanded, settled-folded default.
    pub folds: BTreeMap<(RunId, DispatchId), bool>,
    /// The selection follows `attention` until the user moves it.
    pub touched: bool,
    /// `a`: the `/usage` table instead of the tree.
    pub accounts_only: bool,
    /// `f`: pin the selection to the newest running task.
    pub follow: bool,
    pub scroll: usize,
    pub overlay: Option<Pager>,
    /// `k` asked about this; the next key answers.
    pub confirm: Option<CancelTarget>,
    /// Shown in place of the hints until it expires.
    pub notice: Option<(String, Role, Instant)>,
    pub quit: bool,
}

impl Default for App {
    fn default() -> App {
        App::new(true)
    }
}

impl App {
    pub fn new(actions: bool) -> App {
        App {
            actions,
            folds: BTreeMap::new(),
            touched: false,
            accounts_only: false,
            follow: false,
            scroll: 0,
            overlay: None,
            confirm: None,
            notice: None,
            quit: false,
        }
    }

    /// Per frame: drop a selection the journals no longer carry, then re-pin it: to the
    /// newest running task under `follow`, to what needs attention until the user moved.
    pub fn sync(&mut self, board: &mut Board) {
        board.clamp();
        let rows = self.visible(&board.rows());
        let pick = if self.follow {
            newest_running(&rows)
        } else if !self.touched {
            attention(&rows)
        } else {
            None
        };
        if let Some(sel) = pick {
            board.selected = sel;
        }
    }

    /// `rows` with the viewer's folds applied.
    pub fn visible(&self, rows: &Rows) -> Rows {
        fn apply(g: &mut DispatchGroup, folds: &BTreeMap<(RunId, DispatchId), bool>) {
            if let Some(open) = folds.get(&(g.run, g.id)) {
                g.expanded = *open;
            }
            for t in &mut g.tasks {
                for sub in &mut t.nested {
                    apply(sub, folds);
                }
            }
        }
        let mut out = rows.clone();
        for run in &mut out.runs {
            for g in run.active.iter_mut().chain(run.recent.iter_mut()) {
                apply(g, &self.folds);
            }
        }
        out
    }

    /// Every row the cursor can land on, in draw order.
    pub fn targets(&self, rows: &Rows) -> Vec<Selection> {
        fn walk(g: &DispatchGroup, out: &mut Vec<Selection>) {
            out.push(Selection::Dispatch {
                run: g.run,
                id: g.id,
            });
            if !g.expanded {
                return;
            }
            for t in &g.tasks {
                out.push(Selection::Node {
                    run: t.row.run,
                    logical: t.row.logical,
                });
                for sub in &t.nested {
                    walk(sub, out);
                }
            }
        }
        let mut out = Vec::new();
        if !self.accounts_only {
            let shown = self.visible(rows);
            for run in &shown.runs {
                if let Some(b) = &run.brain {
                    out.push(Selection::Node {
                        run: b.run,
                        logical: b.logical,
                    });
                }
                for g in run.active.iter().chain(&run.recent) {
                    walk(g, &mut out);
                }
            }
        }
        out.extend(
            rows.accounts
                .iter()
                .map(|r| Selection::Account(r.account.clone())),
        );
        out
    }

    /// The keys `?` and the hints leave out: cancel, when the board may not cancel.
    pub fn hidden(&self) -> Vec<KeyAction> {
        if self.actions {
            Vec::new()
        } else {
            vec![KeyAction::Cancel, KeyAction::Confirm, KeyAction::Decline]
        }
    }

    // ------------------------------------------------------------ keys

    pub fn on_key(&mut self, board: &mut Board, key: KeyEvent) -> Action {
        if let Some(target) = self.confirm.take() {
            return match keys::action(Surface::Board, &key) {
                Some(KeyAction::ClearOrQuit | KeyAction::Leave) => self.stop(),
                Some(KeyAction::Confirm) => {
                    let label = target_label(board, &target);
                    self.notice = Some((
                        format!("cancelling {label}\u{2026}"),
                        Role::Meta,
                        Instant::now() + StdDuration::from_secs(3600),
                    ));
                    Action::Cancel(target)
                }
                _ => Action::None,
            };
        }
        if self.overlay.is_some() {
            return self.on_pager_key(key);
        }
        let Some(action) = keys::action(Surface::Board, &key) else {
            return Action::None;
        };
        match action {
            KeyAction::ClearOrQuit | KeyAction::Leave | KeyAction::Quit => self.stop(),
            KeyAction::Move => {
                self.touched = true;
                self.move_by(board, if key.code == KeyCode::Up { -1 } else { 1 })
            }
            KeyAction::Ends => {
                self.touched = true;
                self.move_end(board, key.code == KeyCode::Char('G'))
            }
            KeyAction::Stuck => {
                self.touched = true;
                self.next_stuck(board)
            }
            KeyAction::Run => {
                self.touched = true;
                self.cycle_run(board, if key.code == KeyCode::BackTab { -1 } else { 1 })
            }
            KeyAction::Fold => self.fold(board, key.code == KeyCode::Left),
            KeyAction::Open => self.open(board),
            KeyAction::AllRuns => {
                board.focus = None;
                Action::None
            }
            KeyAction::Accounts => {
                self.accounts_only = !self.accounts_only;
                self.scroll = 0;
                Action::None
            }
            KeyAction::Raw => match board
                .selected
                .run()
                .or(board.focus)
                .or_else(|| board.runs.first().map(|p| p.run))
            {
                Some(run) => Action::LoadRaw(run),
                None => Action::None,
            },
            KeyAction::Follow => {
                self.follow = !self.follow;
                Action::None
            }
            KeyAction::Keys => {
                self.overlay = Some(Pager {
                    kind: PagerKind::Keys,
                    title: "board".to_owned(),
                    lines: keys::overlay_text(Surface::Board, 0, &self.hidden()),
                    scroll: 0,
                    run: None,
                });
                Action::None
            }
            KeyAction::Cancel => {
                self.ask_cancel(board);
                Action::None
            }
            KeyAction::Back => {
                self.notice = None;
                Action::None
            }
            _ => Action::None,
        }
    }

    fn stop(&mut self) -> Action {
        self.quit = true;
        Action::Quit
    }

    /// `esc` returns to the board; `r` swaps a trace or a dispatch for its run's journal.
    fn on_pager_key(&mut self, key: KeyEvent) -> Action {
        let Some(p) = &mut self.overlay else {
            return Action::None;
        };
        match keys::action(Surface::Pager, &key) {
            Some(KeyAction::ClearOrQuit | KeyAction::Leave | KeyAction::Quit) => {
                return self.stop();
            }
            Some(KeyAction::Back) => self.overlay = None,
            Some(KeyAction::Scroll) if key.code == KeyCode::Up => {
                p.scroll = p.scroll.saturating_sub(1)
            }
            Some(KeyAction::Scroll) => p.scroll = p.scroll.saturating_add(1),
            Some(KeyAction::Page) if key.code == KeyCode::PageUp => {
                p.scroll = p.scroll.saturating_sub(20)
            }
            Some(KeyAction::Page) => p.scroll = p.scroll.saturating_add(20),
            Some(KeyAction::Ends) if key.code == KeyCode::Char('g') => p.scroll = 0,
            Some(KeyAction::Ends) => p.scroll = p.lines.len(),
            Some(KeyAction::Raw) if matches!(p.kind, PagerKind::Trace | PagerKind::Dispatch) => {
                if let Some(run) = p.run {
                    return Action::LoadRaw(run);
                }
            }
            _ => {}
        }
        Action::None
    }

    fn move_by(&mut self, board: &mut Board, delta: isize) -> Action {
        let targets = self.targets(&board.rows());
        if targets.is_empty() {
            board.selected = Selection::None;
            return Action::None;
        }
        let last = targets.len() as isize - 1;
        let next = match targets.iter().position(|t| *t == board.selected) {
            Some(i) => (i as isize + delta).clamp(0, last),
            None if delta < 0 => last,
            None => 0,
        };
        board.selected = targets[next as usize].clone();
        Action::None
    }

    fn move_end(&mut self, board: &mut Board, bottom: bool) -> Action {
        let targets = self.targets(&board.rows());
        let pick = if bottom {
            targets.last()
        } else {
            targets.first()
        };
        if let Some(sel) = pick {
            board.selected = sel.clone();
        }
        Action::None
    }

    /// `!`: the next task that failed, was refused, lost its process or is blocked, and any
    /// folded dispatch hiding a failure, in draw order and wrapping.
    fn next_stuck(&mut self, board: &mut Board) -> Action {
        let rows = board.rows();
        let shown = self.visible(&rows);
        let stuck: Vec<Selection> = self
            .targets(&rows)
            .into_iter()
            .filter(|t| match t {
                Selection::Node { run, logical } => shown.task(*run, *logical).is_some_and(|t| {
                    order::rank(&t.row.state) == 0
                        || matches!(t.row.state, NodeState::Blocked { .. })
                }),
                Selection::Dispatch { run, id } => shown
                    .group(*run, *id)
                    .is_some_and(|g| !g.expanded && g.tally.failed + g.tally.rejected > 0),
                _ => false,
            })
            .collect();
        let all = self.targets(&rows);
        let here = all.iter().position(|t| *t == board.selected);
        let next = stuck
            .iter()
            .find(|s| {
                let at = all.iter().position(|t| t == *s);
                here.is_none_or(|h| at.is_some_and(|a| a > h))
            })
            .or_else(|| stuck.first());
        if let Some(sel) = next {
            board.selected = sel.clone();
        }
        Action::None
    }

    /// `tab` walks the tailed runs and then the merged view, which is what `0` names.
    fn cycle_run(&mut self, board: &mut Board, delta: isize) -> Action {
        if board.runs.len() < 2 {
            return Action::None;
        }
        let stops = board.runs.len() as isize + 1;
        let at = match board.focus {
            None => 0,
            Some(run) => board
                .runs
                .iter()
                .position(|p| p.run == run)
                .map_or(0, |i| i as isize + 1),
        };
        let next = (at + delta).rem_euclid(stops);
        board.focus = match next {
            0 => None,
            i => Some(board.runs[(i - 1) as usize].run),
        };
        board.clamp();
        self.scroll = 0;
        Action::None
    }

    /// `←` folds the selected dispatch, or the one the selected task belongs to, and takes
    /// the cursor to its header; `→` unfolds it.
    fn fold(&mut self, board: &mut Board, collapse: bool) -> Action {
        let rows = board.rows();
        let Some((run, id)) = dispatch_of(&rows, &board.selected) else {
            return Action::None;
        };
        self.folds.insert((run, id), !collapse);
        self.touched = true;
        if collapse {
            board.selected = Selection::Dispatch { run, id };
        }
        Action::None
    }

    fn open(&mut self, board: &Board) -> Action {
        match board.selected.clone() {
            Selection::Node { run, logical } => self.open_trace(board, run, logical),
            Selection::Dispatch { run, id } => self.open_dispatch(board, run, id),
            Selection::Account(_) => {
                self.accounts_only = true;
                self.scroll = 0;
            }
            Selection::None => {}
        }
        Action::None
    }

    /// The selected task's trace, rendered by `trace::*` exactly as `swamp trace` renders it.
    fn open_trace(&mut self, board: &Board, run: RunId, logical: NodeId) {
        let Some(pane) = board.pane(run) else {
            return;
        };
        let node = pane
            .view
            .by_logical
            .get(&logical)
            .and_then(|a| a.last().copied())
            .unwrap_or(logical);
        let text = trace_render(
            &pane.view,
            &TraceOpts {
                node: Some(node),
                events: true,
                ..TraceOpts::default()
            },
        );
        let row = pane.node_row(logical);
        let id = match &row {
            Some(r) if r.brain => "brain".to_owned(),
            Some(r) => r.short(),
            None => logical.short(),
        };
        let mut title = vec![id];
        if let Some(d) = row.as_ref().and_then(|r| r.dispatch) {
            title.push(dispatch_label(
                pane.view
                    .dispatches
                    .get(&d)
                    .and_then(|v| v.record.as_ref().and_then(|r| r.call_seq)),
                d,
            ));
        }
        title.push(format!("run {}", run.short()));
        self.overlay = Some(pager(
            PagerKind::Trace,
            &title.join(order::SEP),
            &text,
            Some(run),
        ));
    }

    /// `swamp dispatch <id>`, the same text, in the pager.
    fn open_dispatch(&mut self, board: &Board, run: RunId, id: DispatchId) {
        let Some(pane) = board.pane(run) else {
            return;
        };
        let text = dispatches::render_detail(&pane.view, id, false, board.now);
        let seq = pane
            .view
            .dispatches
            .get(&id)
            .and_then(|v| v.record.as_ref().and_then(|r| r.call_seq));
        let title = format!(
            "{}{}run {}",
            dispatch_label(seq, id),
            order::SEP,
            run.short()
        );
        self.overlay = Some(pager(PagerKind::Dispatch, &title, &text, Some(run)));
    }

    /// The last `RAW_LINES` journal lines of a run, verbatim but sanitized.
    pub fn load_raw(&mut self, board: &Board, run: RunId) {
        let Some(pane) = board.pane(run) else {
            return;
        };
        let tail = tail_lines(&pane.paths.journal(), RAW_LINES);
        self.overlay = Some(pager(
            PagerKind::Raw,
            &format!("run {}{}last {RAW_LINES} lines", run.short(), order::SEP),
            &tail,
            Some(run),
        ));
    }

    // ------------------------------------------------------------ cancel

    /// `k`: a prompt for a task or a dispatch that has something live, a one-line notice
    /// for everything else.
    fn ask_cancel(&mut self, board: &Board) {
        let say = |app: &mut App, text: String| {
            app.notice = Some((text, Role::Meta, Instant::now() + NOTICE_TTL));
        };
        if !self.actions {
            return say(self, "read-only: ui.board_actions = false".to_owned());
        }
        match board.selected.clone() {
            Selection::Node { run, logical } => {
                let Some(pane) = board.pane(run) else { return };
                if logical == pane.brain {
                    return say(
                        self,
                        format!("the brain stops with swamp cancel {}", run.short()),
                    );
                }
                let Some(row) = pane.node_row(logical) else {
                    return;
                };
                if row.state.is_terminal() {
                    return say(
                        self,
                        format!("{} is already {}", row.short(), fmt::state_word(&row.state)),
                    );
                }
                self.confirm = Some(CancelTarget::Task { run, logical });
            }
            Selection::Dispatch { run, id } => {
                let target = CancelTarget::Dispatch { run, id };
                if live_tasks(board, &target) == 0 {
                    return say(
                        self,
                        format!("{} has no live tasks", target_label(board, &target)),
                    );
                }
                self.confirm = Some(target);
            }
            Selection::Account(_) | Selection::None => {
                say(self, "select a task or dispatch to cancel".to_owned())
            }
        }
    }

    /// What the spawned cancel reported.
    pub fn cancel_done(&mut self, board: &Board, done: CancelDone) {
        let label = target_label(board, &done.target);
        self.notice = Some(match done.result {
            Ok(_) => (
                format!("cancelled {label}"),
                Role::Meta,
                Instant::now() + NOTICE_TTL,
            ),
            Err(e) => (
                format!("cancel {label} failed: {e}"),
                Role::Err,
                Instant::now() + NOTICE_TTL,
            ),
        });
    }

    /// The prompt, a live notice, or the key hints.
    fn bottom(&self, board: &Board, c: &render::Ctx) -> Line<'static> {
        if let Some(target) = &self.confirm {
            return render::bottom(
                &prompt(board, target, c.width as usize),
                Role::Accent,
                true,
                c,
            );
        }
        if let Some((text, role, until)) = &self.notice
            && Instant::now() < *until
        {
            return render::bottom(text, *role, false, c);
        }
        render::hints(c, &self.hidden())
    }

    // ------------------------------------------------------------ draw

    pub fn draw(
        &mut self,
        board: &Board,
        f: &mut Frame,
        theme: &Theme,
        tick: u64,
        max_age: StdDuration,
    ) {
        let area = f.area();
        let lines = self.lines(board, area, theme, tick, max_age);
        f.render_widget(Paragraph::new(lines), area);
    }

    /// The whole pane: the board, or the pager over it.
    pub fn lines(
        &mut self,
        board: &Board,
        area: Rect,
        theme: &Theme,
        tick: u64,
        max_age: StdDuration,
    ) -> Vec<Line<'static>> {
        let c = render::Ctx::new(board, area.width, theme, tick, max_age);
        let height = area.height as usize;
        if self.overlay.is_some() {
            return self.overlay_lines(&c, height);
        }
        let rows = board.rows();
        let bottom = self.bottom(board, &c);
        if self.accounts_only {
            let mut out = render::header(board, &rows, &c);
            out.push(render::rule(&c));
            let body = usage::render(&board.accounts, area.width, theme, max_age);
            let room = height.saturating_sub(out.len() + 1).max(1);
            self.scroll = self.scroll.min(body.len().saturating_sub(room));
            out.extend(body.into_iter().skip(self.scroll).take(room));
            out.push(bottom);
            return out;
        }
        let shown = self.visible(&rows);
        render::compose(
            board,
            &shown,
            &rows,
            &c,
            bottom,
            Some(height),
            &mut self.scroll,
        )
    }

    fn overlay_lines(&mut self, c: &render::Ctx, height: usize) -> Vec<Line<'static>> {
        let width = c.width as usize;
        let Some(p) = &mut self.overlay else {
            return Vec::new();
        };
        let t = c.theme;
        let hide: &[KeyAction] = match p.kind {
            PagerKind::Trace | PagerKind::Dispatch => &[],
            PagerKind::Raw | PagerKind::Keys => &[KeyAction::Raw],
        };
        let hint = keys::hints(Surface::Pager, width, hide);
        let room = height.saturating_sub(4).max(1);
        p.scroll = p.scroll.min(p.lines.len().saturating_sub(room));
        let (word, word_role) = match p.kind {
            PagerKind::Trace => ("trace", Role::Name),
            PagerKind::Dispatch => ("dispatch", Role::Name),
            PagerKind::Raw => (" raw journal ", Role::Code),
            PagerKind::Keys => ("keys", Role::Name),
        };
        let body_role = if p.kind == PagerKind::Raw {
            Role::Meta
        } else {
            Role::Text
        };
        let title = Line::from(vec![
            t.span(word.to_owned(), word_role),
            t.span(
                fmt::truncate(
                    &format!(" {}", p.title),
                    width.saturating_sub(word.chars().count()),
                ),
                Role::Meta,
            ),
        ]);
        let mut out = vec![title, render::rule(c)];
        out.extend(
            p.lines
                .iter()
                .skip(p.scroll)
                .take(room)
                .map(|l| Line::from(t.span(fmt::truncate(l, width), body_role))),
        );
        out.resize(room + 2, Line::default());
        out.push(render::rule(c));
        out.push(Line::from(t.span(fmt::truncate(&hint, width), Role::Meta)));
        out
    }
}

/// `cancel 9g5f04 "rebuild the index"? y / n`, the title cut so the whole line fits.
pub fn prompt(board: &Board, target: &CancelTarget, width: usize) -> String {
    let label = target_label(board, target);
    match target {
        CancelTarget::Task { run, logical } => {
            let title = board
                .pane(*run)
                .and_then(|p| p.node_row(*logical))
                .map(|r| r.title)
                .unwrap_or_default();
            let frame = format!("cancel {label} \"\"? y / n");
            let room = width.saturating_sub(frame.chars().count()).max(1);
            format!("cancel {label} \"{}\"? y / n", fmt::truncate(&title, room))
        }
        CancelTarget::Dispatch { .. } => {
            let n = live_tasks(board, target);
            let noun = if n == 1 { "live task" } else { "live tasks" };
            format!("cancel {label}{}{n} {noun}? y / n", order::SEP)
        }
    }
}

/// `9g5f04`, or `#1 9g5f18` for a dispatch.
pub fn target_label(board: &Board, target: &CancelTarget) -> String {
    match *target {
        CancelTarget::Task { run, logical } => board
            .pane(run)
            .and_then(|p| p.node_row(logical))
            .map_or_else(|| logical.short(), |r| r.short()),
        CancelTarget::Dispatch { run, id } => {
            let seq = board
                .pane(run)
                .and_then(|p| p.view.dispatches.get(&id))
                .and_then(|d| d.record.as_ref().and_then(|r| r.call_seq));
            dispatch_label(seq, id)
        }
    }
}

fn dispatch_label(seq: Option<crate::ids::CallSeq>, id: DispatchId) -> String {
    match seq {
        Some(seq) if id != DispatchId::LEGACY => format!("#{seq} {}", id.short()),
        _ => inspect::short(id),
    }
}

/// The tasks a cancel would reach that have not ended yet.
fn live_tasks(board: &Board, target: &CancelTarget) -> usize {
    let Some(pane) = board.pane(target.run()) else {
        return 0;
    };
    let tasks = match *target {
        CancelTarget::Task { logical, .. } => vec![logical],
        CancelTarget::Dispatch { id, .. } => dispatch_tasks(&pane.view, id),
    };
    tasks
        .iter()
        .filter(|t| pane.view.state_of(**t).is_some_and(|s| !s.is_terminal()))
        .count()
}

/// The dispatch the selection is on, or the one its task belongs to.
fn dispatch_of(rows: &Rows, sel: &Selection) -> Option<(RunId, DispatchId)> {
    match sel {
        Selection::Dispatch { run, id } => Some((*run, *id)),
        Selection::Node { run, logical } => rows
            .groups()
            .into_iter()
            .find(|g| g.run == *run && g.tasks.iter().any(|t| t.row.logical == *logical))
            .map(|g| (g.run, g.id)),
        _ => None,
    }
}

/// The last `want` lines of a file, read backwards from the end. A long-lived run's journal
/// is append-only and unbounded, and this runs inline on the loop that draws and polls: what
/// it costs must depend on `want`, never on how long the run has been going.
pub(crate) fn tail_lines(path: &Utf8Path, want: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = f.seek(SeekFrom::End(0)) else {
        return String::new();
    };
    let mut span = 0u64;
    let mut buf: Vec<u8> = Vec::new();
    while span < len.min(RAW_MAX) {
        span = (span + RAW_CHUNK).min(len).min(RAW_MAX);
        buf.resize(span as usize, 0);
        if f.seek(SeekFrom::Start(len - span)).is_err() || f.read_exact(&mut buf).is_err() {
            return String::new();
        }
        if buf.iter().filter(|b| **b == b'\n').count() > want {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    // A window that starts mid-file opens on a fragment; taking only the last `want` of more
    // than `want` line ends is what drops it.
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(want)..].join("\n")
}

fn pager(kind: PagerKind, title: &str, text: &str, run: Option<RunId>) -> Pager {
    Pager {
        kind,
        title: fmt::sanitize(title),
        // Every line came from a model or the filesystem: §7.6 is what this is for.
        lines: text.lines().map(fmt::sanitize).collect(),
        scroll: 0,
        run,
    }
}

/// A frame only animates while something is running; anything else wakes on the account poll.
fn animating(board: &Board) -> bool {
    board.runs.iter().any(|p| {
        p.view
            .nodes
            .values()
            .any(|n| matches!(n.state, NodeState::Running { .. }))
    })
}

// ---------------------------------------------------------------- json

/// `--json`: the frame's own model, each run's dispatches in the shape `swamp dispatches
/// --json` publishes, and the accounts in the shape `swamp usage --json` publishes.
pub fn json(b: &Board) -> Value {
    let rows = b.rows();
    let s = b.summary(&rows);
    let mut in_flight: Vec<Value> = Vec::new();
    let mut waiting: Vec<Value> = Vec::new();
    let mut recent: Vec<Value> = Vec::new();
    for run in &rows.runs {
        if let Some(brain) = run.brain.as_ref().filter(|n| !n.state.is_terminal()) {
            in_flight.push(node_json(b, brain));
        }
    }
    for t in rows.tasks() {
        let n = &t.row;
        let list = match n.state {
            NodeState::Running { .. } | NodeState::Leased { .. } | NodeState::Orphaned { .. } => {
                &mut in_flight
            }
            NodeState::Queued | NodeState::Blocked { .. } => &mut waiting,
            _ => &mut recent,
        };
        list.push(node_json(b, n));
    }
    let accounts = usage::json(&b.accounts);
    json!({
        "at": stamp(b.now),
        "runs": b.runs.iter().map(|p| json!({
            "run": p.run.to_string(),
            "short": p.run.short(),
            "dir": p.paths.dir,
            "stale": p.stale.is_some(),
            "finished": p.view.finished,
            "dispatches": inspect::list(&p.view, b.now).dispatches,
        })).collect::<Vec<_>>(),
        "in_flight": in_flight,
        "waiting": waiting,
        "recent": recent,
        "accounts": accounts.get("accounts").cloned().unwrap_or(Value::Null),
        "totals": {
            "runs": s.runs,
            "hidden_runs": s.hidden_runs,
            "stale_runs": s.stale_runs,
            "in_flight": in_flight.len(),
            "waiting": waiting.len(),
            "running": s.tally.running,
            "stuck": s.tally.stuck(),
            "queued": s.tally.queued,
            "accounts": s.accounts,
            "cost_usd": s.cost_usd,
            "cost_complete": s.cost_complete,
        },
    })
}

fn node_json(b: &Board, n: &crate::ui::board::model::NodeRow) -> Value {
    let note = b.note_for(n.run, n.logical);
    json!({
        "run": n.run.short(),
        "id": n.id.to_string(),
        "short": n.id.short(),
        "logical": n.logical.to_string(),
        "attempt": n.attempt,
        "brain": n.brain,
        "dispatch": n.dispatch.map(inspect::short),
        "provider": n.provider,
        "account": n.account,
        "tier": n.tier,
        "model": n.model,
        "title": n.title,
        "state": n.state,
        "started_at": n.started_at.map(stamp),
        "ended_at": n.ended_at.map(stamp),
        "elapsed_s": n.elapsed(b.now).map(|d| d.as_secs()),
        "usage": n.usage,
        "cost_usd": n.cost.map(|c| c.usd),
        "stale": n.stale,
        "selection": note.map(|s| json!({
            "account": s.account,
            "policy": s.policy,
            "reason": s.reason.text,
            "score": s.reason.score,
            "excluded": s.excluded,
        })),
    })
}

fn stamp(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

// ---------------------------------------------------------------- loop

/// `~/.swamp/board.pid` while the board owns a tty: chat reads it to decide whether to print
/// its hint. Removed on the way out, and a stale one is harmless because chat checks liveness.
pub(crate) struct BoardPid(Utf8PathBuf);

impl BoardPid {
    pub(crate) fn write(path: &Utf8Path) -> Option<BoardPid> {
        match write_board_pid(path) {
            Ok(()) => Some(BoardPid(path.to_owned())),
            Err(e) => {
                tracing::warn!("board: cannot write {path}: {e:#}");
                None
            }
        }
    }
}

impl Drop for BoardPid {
    fn drop(&mut self) {
        // Only ours: a pid file another board wrote over this one is its business, not ours.
        if paths::read_board_pid(&self.0) == Some(std::process::id() as i32) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

/// Which of the things the loop waits on spoke first.
enum Wake {
    Key(Option<Result<Event, std::io::Error>>),
    Tail(anyhow::Result<bool>),
    Cancelled(CancelDone),
    Tick,
}

/// The full-screen board: alternate screen, `select!` over the tails, the keys and the
/// redraw clock, and a guard that restores the terminal whatever happens.
pub async fn run_tui(
    mut sources: Sources,
    color: bool,
    interval: Option<StdDuration>,
) -> anyhow::Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

    let cfg = sources.cfg.clone();
    let theme = Theme::detect(color, cfg.ui.chat_theme.as_deref());
    let max_age = cfg.quota_max_age();
    let frame = interval.unwrap_or_else(|| {
        StdDuration::from_micros(
            1_000_000 / u64::from(cfg.ui.refresh_hz.unwrap_or(REFRESH_HZ).max(1)),
        )
    });
    let idle = frame.max(IDLE_PERIOD);

    let mut board = sources.board(Instant::now())?;
    let mut app = App::new(cfg.ui.board_actions.unwrap_or(true));
    let grace = cfg.limits.grace_period.unwrap_or(StdDuration::from_secs(5));
    let (tx, mut done) = tokio::sync::mpsc::unbounded_channel::<CancelDone>();
    let _pid = BoardPid::write(&sources.paths.board_pid());

    // The guard first: it installs the panic hook and the restore, so a failure on the way
    // into the alternate screen cannot leave the shell in raw mode.
    let _guard = watch::TerminalGuard::new();
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut keys = crossterm::event::EventStream::new();
    let mut tick = 0u64;

    while !app.quit {
        let at = Instant::now();
        board.now = OffsetDateTime::now_utc();
        sources.sync_runs(&mut board, at)?;
        sources.sync_accounts(&mut board, at)?;
        sources.refresh_liveness(&mut board);
        app.sync(&mut board);
        terminal.draw(|f| app.draw(&board, f, &theme, tick, max_age))?;
        tick += 1;

        let period = if animating(&board) || app.notice.is_some() {
            frame
        } else {
            idle
        };
        let wake = tokio::select! {
            key = keys.next() => Wake::Key(key),
            polled = sources.poll(&mut board) => Wake::Tail(polled),
            Some(d) = done.recv() => Wake::Cancelled(d),
            () = tokio::time::sleep(period) => Wake::Tick,
        };
        match wake {
            Wake::Key(Some(Ok(Event::Key(k)))) if k.kind == KeyEventKind::Press => {
                match app.on_key(&mut board, k) {
                    Action::LoadRaw(run) => app.load_raw(&board, run),
                    Action::Cancel(target) => {
                        if let Some(pane) = board.pane(target.run()) {
                            let tasks = match target {
                                CancelTarget::Task { logical, .. } => vec![logical],
                                CancelTarget::Dispatch { id, .. } => dispatch_tasks(&pane.view, id),
                            };
                            spawn_cancel(pane.paths.clone(), target, tasks, grace, tx.clone());
                        }
                    }
                    Action::None | Action::Quit => {}
                }
            }
            Wake::Key(Some(Err(e))) => return Err(e.into()),
            Wake::Key(None) => break,
            Wake::Tail(polled) => {
                polled?;
            }
            Wake::Cancelled(d) => app.cancel_done(&board, d),
            Wake::Key(_) | Wake::Tick => {}
        }
    }
    Ok(())
}
