//! WP6: the keys, the overlays and the loop of `docs/BOARD.md` §4-5.
//!
//! The pane state is pure and testable: `on_key` takes a `Board` and returns what the outer
//! loop has to do, and `lines` turns the two of them into a frame. Only `run_tui` touches a
//! terminal, and it enters the alternate screen behind `watch::TerminalGuard`, so neither a
//! panic nor a ctrl+c leaves a wrecked tty.

use crate::ids::RunId;
use crate::journal::paths::{self, write_board_pid};
use crate::model::core::AccountId;
use crate::ui::board::model::{Board, NodeRow, Rows, Section, Selection};
use crate::ui::board::render;
use crate::ui::board::sources::Sources;
use crate::ui::chat::theme::{Role, Theme};
use crate::ui::trace::{TraceOpts, render as trace_render};
use crate::ui::{fmt, usage, watch};
use camino::{Utf8Path, Utf8PathBuf};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use serde_json::{Value, json};
use std::collections::BTreeSet;
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

// ---------------------------------------------------------------- state

/// What a keypress asks the outer loop to do. Everything that needs no file is already done
/// by the time this is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    LoadRaw(RunId),
}

/// A full-pane scroll view over text: a node's trace, or a run's raw journal tail.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pager {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
}

/// Everything the board draws that is not the board itself.
#[derive(Debug, Default)]
pub struct App {
    /// Accounts whose node list is folded away.
    pub collapsed: BTreeSet<AccountId>,
    /// `a`: the `/usage` table instead of the tree.
    pub accounts_only: bool,
    /// `f`: pin the selection to the newest in-flight node.
    pub follow: bool,
    pub scroll: usize,
    pub overlay: Option<Pager>,
    pub quit: bool,
}

impl App {
    pub fn new() -> App {
        App::default()
    }

    /// Per frame: drop a selection the journals no longer carry, then re-pin it if `follow`.
    pub fn sync(&mut self, board: &mut Board) {
        board.clamp();
        if !self.follow {
            return;
        }
        if let Some(sel) = newest_in_flight(&board.rows()) {
            board.selected = sel;
        }
    }

    /// The rows a frame draws: `rows` minus the nodes of every collapsed account. The header
    /// keeps counting the unfiltered ones, so folding a list never changes `4 in flight`.
    pub fn visible(&self, rows: &Rows) -> Rows {
        let mut out = rows.clone();
        for group in &mut out.providers {
            for account in &mut group.accounts {
                if self.collapsed.contains(&account.row.account) {
                    account.nodes.clear();
                }
            }
        }
        out
    }

    /// Every row the cursor can land on, in draw order.
    pub fn targets(&self, rows: &Rows) -> Vec<Selection> {
        let mut out = Vec::new();
        for group in &rows.providers {
            for account in &group.accounts {
                out.push(Selection::Account(account.row.account.clone()));
                if self.accounts_only || self.collapsed.contains(&account.row.account) {
                    continue;
                }
                out.extend(account.nodes.iter().map(node_target));
            }
        }
        if self.accounts_only {
            return out;
        }
        out.extend(rows.orphan_nodes.iter().map(node_target));
        out.extend(rows.waiting.iter().map(node_target));
        out.extend(rows.recent.iter().map(node_target));
        out
    }

    // ------------------------------------------------------------ keys

    pub fn on_key(&mut self, board: &mut Board, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c' | 'd') if ctrl => self.stop(),
            KeyCode::Char('q') => self.stop(),
            _ if self.overlay.is_some() => self.on_overlay_key(key),
            KeyCode::Up => self.move_by(board, -1),
            KeyCode::Down => self.move_by(board, 1),
            KeyCode::Left => self.fold(board, true),
            KeyCode::Right => self.fold(board, false),
            KeyCode::Enter => self.open_trace(board),
            KeyCode::Tab => self.cycle_run(board, 1),
            KeyCode::BackTab => self.cycle_run(board, -1),
            KeyCode::Char('0') => {
                board.focus = None;
                Action::None
            }
            KeyCode::Char('a') => {
                self.accounts_only = !self.accounts_only;
                self.scroll = 0;
                Action::None
            }
            KeyCode::Char('r') => match board.selected.run().or(board.focus).or_else(|| {
                let first = board.runs.first()?;
                Some(first.run)
            }) {
                Some(run) => Action::LoadRaw(run),
                None => Action::None,
            },
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                Action::None
            }
            KeyCode::Char('g') => {
                self.scroll = 0;
                self.move_end(board, false)
            }
            KeyCode::Char('G') => {
                self.scroll = usize::MAX;
                self.move_end(board, true)
            }
            _ => Action::None,
        }
    }

    fn stop(&mut self) -> Action {
        self.quit = true;
        Action::Quit
    }

    /// `esc` returns to the board; everything else scrolls.
    fn on_overlay_key(&mut self, key: KeyEvent) -> Action {
        let Some(p) = &mut self.overlay else {
            return Action::None;
        };
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Up => p.scroll = p.scroll.saturating_sub(1),
            KeyCode::Down => p.scroll = p.scroll.saturating_add(1),
            KeyCode::PageUp => p.scroll = p.scroll.saturating_sub(20),
            KeyCode::PageDown => p.scroll = p.scroll.saturating_add(20),
            KeyCode::Char('g') | KeyCode::Home => p.scroll = 0,
            KeyCode::Char('G') | KeyCode::End => p.scroll = p.lines.len(),
            _ => {}
        }
        Action::None
    }

    fn move_by(&mut self, board: &mut Board, delta: isize) -> Action {
        let rows = board.rows();
        let targets = self.targets(&rows);
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
        let rows = board.rows();
        let targets = self.targets(&rows);
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

    /// `←` folds the selected account's node list away, `→` brings it back. A node lands the
    /// cursor on the account that owns it, so the row the fold hid is never the selected one.
    fn fold(&mut self, board: &mut Board, collapse: bool) -> Action {
        let rows = board.rows();
        let Some(id) = account_of(&rows, &board.selected) else {
            return Action::None;
        };
        if collapse {
            self.collapsed.insert(id.clone());
            board.selected = Selection::Account(id);
        } else {
            self.collapsed.remove(&id);
        }
        Action::None
    }

    /// The selected node's trace, rendered by `trace::*` exactly as `swamp trace` renders it.
    fn open_trace(&mut self, board: &Board) -> Action {
        let Selection::Node { run, logical } = board.selected.clone() else {
            return Action::None;
        };
        let Some(pane) = board.pane(run) else {
            return Action::None;
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
        self.overlay = Some(pager(
            &format!("trace {} \u{b7} run {}", node.short(), run.short()),
            &text,
        ));
        Action::None
    }

    /// The last `RAW_LINES` journal lines of a run, verbatim but sanitized.
    pub fn load_raw(&mut self, board: &Board, run: RunId) {
        let Some(pane) = board.pane(run) else {
            return;
        };
        let tail = tail_lines(&pane.paths.journal(), RAW_LINES);
        self.overlay = Some(pager(&format!("raw \u{b7} run {}", run.short()), &tail));
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

    /// Header and rule on top, key hints at the bottom, whatever is between them scrolled.
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
        let head = vec![render::header(board, &rows, &c), render::rule(&c)];
        let foot = render::footer(board, &rows, &c);
        let body = if self.accounts_only {
            usage::render(&board.accounts, area.width, theme, max_age)
        } else {
            render::body(&self.visible(&rows), &c)
        };
        let room = height.saturating_sub(head.len() + foot.len()).max(1);
        self.scroll = self.scroll.min(body.len().saturating_sub(room));
        let mut out = head;
        out.extend(body.into_iter().skip(self.scroll).take(room));
        out.extend(foot);
        out
    }

    fn overlay_lines(&mut self, c: &render::Ctx, height: usize) -> Vec<Line<'static>> {
        let width = c.l.width as usize;
        let hint = "esc back \u{b7} \u{2191}\u{2193} scroll \u{b7} q quit";
        let Some(p) = &mut self.overlay else {
            return Vec::new();
        };
        let room = height.saturating_sub(3).max(1);
        p.scroll = p.scroll.min(p.lines.len().saturating_sub(room));
        let mut out = vec![
            Line::from(c.theme.span(fmt::truncate(&p.title, width), Role::Accent)),
            render::rule(c),
        ];
        out.extend(
            p.lines
                .iter()
                .skip(p.scroll)
                .take(room)
                .map(|l| Line::from(c.theme.span(fmt::truncate(l, width), Role::Text))),
        );
        out.push(render::rule(c));
        out.push(Line::from(
            c.theme.span(fmt::truncate(hint, width), Role::Meta),
        ));
        out
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

fn pager(title: &str, text: &str) -> Pager {
    Pager {
        title: fmt::sanitize(title),
        // Every line came from a model or the filesystem: §7.6 is what this is for.
        lines: text.lines().map(fmt::sanitize).collect(),
        scroll: 0,
    }
}

fn node_target(n: &NodeRow) -> Selection {
    Selection::Node {
        run: n.run,
        logical: n.logical,
    }
}

/// Which account owns the selected row, so `←` can fold the list the cursor sits in.
fn account_of(rows: &Rows, selected: &Selection) -> Option<AccountId> {
    match selected {
        Selection::Account(id) => Some(id.clone()),
        Selection::Node { run, logical } => rows
            .providers
            .iter()
            .flat_map(|g| &g.accounts)
            .find(|a| {
                a.nodes
                    .iter()
                    .any(|n| n.run == *run && n.logical == *logical)
            })
            .map(|a| a.row.account.clone()),
        Selection::None => None,
    }
}

/// What `follow` pins to: the in-flight node that started last.
fn newest_in_flight(rows: &Rows) -> Option<Selection> {
    rows.providers
        .iter()
        .flat_map(|g| &g.accounts)
        .flat_map(|a| &a.nodes)
        .chain(rows.orphan_nodes.iter())
        .max_by_key(|n| (n.started_at, n.id))
        .map(node_target)
}

/// A frame only animates while something is running; anything else wakes on the account poll.
fn animating(board: &Board) -> bool {
    board.runs.iter().any(|p| {
        p.view
            .nodes
            .values()
            .any(|n| matches!(n.state, crate::model::core::NodeState::Running { .. }))
    })
}

// ---------------------------------------------------------------- json

/// `--json`: the frame's own model, with the accounts in the shape `swamp usage --json`
/// already publishes so the two cannot drift.
pub fn json(b: &Board) -> Value {
    let rows = b.rows();
    let s = b.summary(&rows);
    let in_flight: Vec<Value> = rows
        .providers
        .iter()
        .flat_map(|g| &g.accounts)
        .flat_map(|a| &a.nodes)
        .chain(rows.orphan_nodes.iter())
        .map(|n| node_json(b, n))
        .collect();
    let accounts = usage::json(&b.accounts);
    json!({
        "at": stamp(b.now),
        "runs": b.runs.iter().map(|p| json!({
            "run": p.run.to_string(),
            "short": p.run.short(),
            "dir": p.paths.dir,
            "stale": p.stale.is_some(),
            "finished": p.view.finished,
        })).collect::<Vec<_>>(),
        "in_flight": in_flight,
        "waiting": rows.waiting.iter().map(|n| node_json(b, n)).collect::<Vec<_>>(),
        "recent": rows.recent.iter().map(|n| node_json(b, n)).collect::<Vec<_>>(),
        "accounts": accounts.get("accounts").cloned().unwrap_or(Value::Null),
        "totals": {
            "runs": s.runs,
            "hidden_runs": s.hidden_runs,
            "stale_runs": s.stale_runs,
            "in_flight": s.in_flight,
            "waiting": s.waiting,
            "accounts": s.accounts,
            "cost_usd": s.cost_usd,
            "cost_complete": s.cost_complete,
        },
    })
}

fn node_json(b: &Board, n: &NodeRow) -> Value {
    let note = b.note_for(n.run, n.logical);
    json!({
        "run": n.run.short(),
        "id": n.id.to_string(),
        "short": n.id.short(),
        "logical": n.logical.to_string(),
        "attempt": n.attempt,
        "brain": n.brain,
        "section": section_word(n.section()),
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

fn section_word(s: Section) -> &'static str {
    match s {
        Section::InFlight => "in_flight",
        Section::Waiting => "waiting",
        Section::Recent => "recent",
    }
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

/// Which of the three things the loop waits on spoke first.
enum Wake {
    Key(Option<Result<Event, std::io::Error>>),
    Tail(anyhow::Result<bool>),
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
    let mut app = App::new();
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

        let period = if animating(&board) { frame } else { idle };
        let wake = tokio::select! {
            key = keys.next() => Wake::Key(key),
            polled = sources.poll(&mut board) => Wake::Tail(polled),
            () = tokio::time::sleep(period) => Wake::Tick,
        };
        match wake {
            Wake::Key(Some(Ok(Event::Key(k)))) if k.kind == KeyEventKind::Press => {
                if let Action::LoadRaw(run) = app.on_key(&mut board, k) {
                    app.load_raw(&board, run);
                }
            }
            Wake::Key(Some(Err(e))) => return Err(e.into()),
            Wake::Key(None) => break,
            Wake::Tail(polled) => {
                polled?;
            }
            Wake::Key(_) | Wake::Tick => {}
        }
    }
    Ok(())
}
