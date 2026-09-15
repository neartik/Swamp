use crate::config::Config;
use crate::dispatch::account::{AccountState, Health};
use crate::ids::{NodeId, RunId};
use crate::journal::fold::{RunView, TreeRow};
use crate::journal::paths::RunPaths;
use crate::journal::reader::Tailer;
use crate::journal::record::JournalLine;
use crate::model::core::{AccountId, NodeState};
use crate::model::node::NodeRecord;
use crate::ui::fmt;
use crate::ui::trace;
use camino::Utf8PathBuf;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

const TAIL_LINES: usize = 200;
const TREE_WIDTH: u16 = 46;
const REFRESH_HZ: u16 = 20;

/// What a keypress asks the outer loop to do. The pane state itself stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Pager(Utf8PathBuf),
    Cancel(NodeId),
    LoadRaw(NodeId),
}

/// Everything the TUI draws. Built from the journal alone, so it never needs the
/// supervisor to be alive.
pub struct App {
    pub run: RunId,
    pub view: RunView,
    pub rows: Vec<TreeRow>,
    pub selected: usize,
    pub raw: bool,
    pub accounts_pane: bool,
    pub quit: bool,
    pub raw_lines: Vec<String>,
    pub tail_lines: usize,
    pub tree_width: u16,
    /// Set by the TUI; without it a dead node would render as running forever.
    paths: Option<RunPaths>,
}

impl App {
    pub fn new(run: RunId) -> Self {
        let mut view = RunView::default();
        view.with_events = true;
        App {
            run,
            view,
            rows: Vec::new(),
            selected: 0,
            raw: false,
            accounts_pane: false,
            quit: false,
            raw_lines: Vec::new(),
            tail_lines: TAIL_LINES,
            tree_width: TREE_WIDTH,
            paths: None,
        }
    }

    pub fn from_lines(run: RunId, lines: &[JournalLine]) -> Self {
        let mut app = App::new(run);
        app.apply(lines);
        app
    }

    pub fn apply(&mut self, lines: &[JournalLine]) {
        for l in lines {
            self.view.apply(l);
        }
        if let Some(paths) = self.paths.clone() {
            self.view
                .mark_orphans(&|id| crate::worker::liveness::is_ours(&paths.pidfile(id)));
        }
        self.rows = self.view.tree();
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    pub fn selected_row(&self) -> Option<&TreeRow> {
        self.rows.get(self.selected)
    }

    pub fn selected_node(&self) -> Option<&NodeRecord> {
        let row = self.selected_row()?;
        row.attempts
            .iter()
            .rev()
            .find_map(|a| self.view.nodes.get(a))
    }

    /// Per-account utilization, health and cooldown, in a stable order.
    pub fn gauges(&self) -> Vec<(AccountId, f64, Health, Option<OffsetDateTime>)> {
        self.view
            .accounts
            .iter()
            .map(|(id, s): (&AccountId, &AccountState)| {
                let util = s.quota.as_ref().map_or(0.0, |q| q.worst_utilization());
                (id.clone(), util, s.health, s.cooldown_until)
            })
            .collect()
    }

    /// The raw pane is titled from the selection, so moving the selection must move its body.
    fn reload_raw(&mut self) -> Action {
        self.raw_lines.clear();
        match (self.raw, self.selected_node()) {
            (true, Some(n)) => Action::LoadRaw(n.id),
            _ => Action::None,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true;
                Action::Quit
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                self.quit = true;
                Action::Quit
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.reload_raw()
            }
            KeyCode::Down => {
                let last = self.rows.len().saturating_sub(1);
                self.selected = (self.selected + 1).min(last);
                self.reload_raw()
            }
            KeyCode::Char('r') => {
                self.raw = !self.raw;
                match (self.raw, self.selected_node()) {
                    (true, Some(n)) => Action::LoadRaw(n.id),
                    _ => Action::None,
                }
            }
            KeyCode::Char('a') => {
                self.accounts_pane = !self.accounts_pane;
                Action::None
            }
            KeyCode::Char('d') => match self.selected_node().and_then(|n| n.work.clone()) {
                Some(w) => Action::Pager(w.patch),
                None => Action::None,
            },
            KeyCode::Char('k') => match self.selected_node() {
                Some(n) => Action::Cancel(n.id),
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    pub fn draw(&self, f: &mut Frame) {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(self.footer_height())])
            .split(f.area());
        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(self.tree_width), Constraint::Min(10)])
            .split(outer[0]);
        self.draw_tree(f, panes[0]);
        self.draw_node(f, panes[1]);
        self.draw_footer(f, outer[1]);
    }

    fn footer_height(&self) -> u16 {
        let accounts = self.view.accounts.len() as u16;
        if self.accounts_pane {
            (accounts + 2).max(3)
        } else {
            3
        }
    }

    fn draw_tree(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let indent = "  ".repeat(r.depth as usize);
                let text = format!(
                    "{indent}{} {} {}",
                    fmt::glyph(&r.state),
                    fmt::truncate(&r.title, 28),
                    if r.attempts.len() > 1 {
                        format!("x{}", r.attempts.len())
                    } else {
                        String::new()
                    }
                );
                let style = if i == self.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(state_color(&r.state))
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect();
        let title = format!(" run {} ", self.run.short());
        f.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn draw_node(&self, f: &mut Frame, area: Rect) {
        let title = match self.selected_node() {
            Some(n) => format!(
                " {} {} {} ",
                fmt::truncate(&n.title, 30),
                n.provider,
                n.model.as_deref().unwrap_or("-")
            ),
            None => " node ".to_owned(),
        };
        let body = if self.raw {
            self.raw_lines
                .iter()
                .rev()
                .take(self.tail_lines)
                .rev()
                .map(|l| Line::from(fmt::truncate(l, area.width.saturating_sub(2) as usize)))
                .collect::<Vec<_>>()
        } else {
            self.event_lines(area.width.saturating_sub(2) as usize)
        };
        f.render_widget(
            Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn event_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(row) = self.selected_row() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(n) = self.selected_node() {
            out.push(Line::from(format!(
                "{}  {}  {}  {}",
                fmt::state_word(&n.state),
                n.duration()
                    .map(fmt::duration)
                    .unwrap_or_else(|| "-".to_owned()),
                fmt::tokens(n.usage.billable()),
                fmt::cost(n.cost),
            )));
        }
        for id in &row.attempts {
            for ev in self.view.events.get(id).into_iter().flatten() {
                out.push(Line::from(fmt::truncate(&trace::event_text(ev), width)));
            }
        }
        out.into_iter().rev().take(self.tail_lines).rev().collect()
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let now = OffsetDateTime::now_utc();
        let mut lines: Vec<Line> = Vec::new();
        let totals = self.view.totals();
        lines.push(Line::from(format!(
            "nodes {}  failed {}  tok {}  cost {}  [up/down] select  r raw  d diff  k cancel  a accounts  q quit",
            totals.nodes,
            totals.failed,
            fmt::tokens(totals.usage.billable()),
            if totals.cost_complete {
                format!("~${:.2}", totals.cost_usd)
            } else {
                format!("~${:.2}+", totals.cost_usd)
            },
        )));
        for (id, util, health, cooldown) in self.gauges() {
            let bar = gauge_bar(util);
            let cd = cooldown
                .filter(|t| *t > now)
                .map(|t| {
                    let left: std::time::Duration =
                        (t - now).try_into().unwrap_or(std::time::Duration::ZERO);
                    format!(" in {}", fmt::duration(left))
                })
                .unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!(
                    "{:<10} {bar} {:>3}%  {}{cd}",
                    id.0,
                    (util * 100.0).round() as i64,
                    health_word(health)
                ),
                Style::default().fg(health_color(health)),
            )));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::TOP)),
            area,
        );
    }
}

pub fn gauge_bar(util: f64) -> String {
    let filled = (util.clamp(0.0, 1.0) * 10.0).round() as usize;
    format!("[{}{}]", "#".repeat(filled), "-".repeat(10 - filled))
}

pub fn health_word(h: Health) -> &'static str {
    match h {
        Health::Healthy => "healthy",
        Health::Degraded => "degraded",
        Health::Cooling => "cooling",
        Health::AuthBroken => "auth-broken",
        Health::Disabled => "disabled",
    }
}

fn health_color(h: Health) -> Color {
    match h {
        Health::Healthy => Color::Green,
        Health::Degraded => Color::Yellow,
        Health::Cooling => Color::Magenta,
        Health::AuthBroken | Health::Disabled => Color::Red,
    }
}

fn state_color(s: &NodeState) -> Color {
    match s {
        NodeState::Succeeded => Color::Green,
        NodeState::Failed { .. } => Color::Red,
        NodeState::Running { .. } => Color::Cyan,
        NodeState::Cancelled { .. } | NodeState::Orphaned { .. } => Color::Yellow,
        _ => Color::Gray,
    }
}

// ---------------------------------------------------------------- terminal

type Restore = fn() -> std::io::Result<()>;

/// Drop restores the tty. A panic hook installed alongside does the same, so neither a
/// crash nor a `?` on the way out leaves a wrecked terminal.
pub struct TerminalGuard {
    restore: Restore,
}

impl Default for TerminalGuard {
    fn default() -> Self {
        TerminalGuard::new()
    }
}

impl TerminalGuard {
    pub fn new() -> Self {
        install_panic_hook(restore_terminal);
        TerminalGuard {
            restore: restore_terminal,
        }
    }

    /// The chat UI restores an inline viewport, which must not leave an alternate screen.
    pub fn with(restore: Restore) -> Self {
        install_panic_hook(restore);
        TerminalGuard { restore }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = (self.restore)();
    }
}

pub fn restore_terminal() -> std::io::Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode};
    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

pub fn install_panic_hook(restore: Restore) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore();
        previous(info);
    }));
}

/// Live TUI: tree pane, node pane, account footer. Read-only.
pub async fn run_tui(paths: RunPaths, cfg: Arc<Config>) -> anyhow::Result<()> {
    use crossterm::execute;
    use crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};

    let mut app = App::new(paths.run);
    app.tail_lines = cfg.ui.tail_lines.unwrap_or(TAIL_LINES).max(1);
    app.tree_width = cfg.ui.tree_width.unwrap_or(TREE_WIDTH).max(12);
    app.paths = Some(paths.clone());
    let redraw = Duration::from_micros(
        1_000_000 / u64::from(cfg.ui.refresh_hz.unwrap_or(REFRESH_HZ).max(1)),
    );
    let mut tailer = Tailer::open(&paths.journal())?;

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard::new();
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    let mut keys = crossterm::event::EventStream::new();

    loop {
        terminal.draw(|f| app.draw(f))?;
        // ui.refresh_hz is a floor on the time between redraws, not a polling clock.
        tokio::time::sleep(redraw).await;
        let action = tokio::select! {
            key = keys.next() => match key {
                Some(Ok(Event::Key(k))) if k.kind == crossterm::event::KeyEventKind::Press => app.on_key(k),
                Some(Err(e)) => return Err(e.into()),
                None => Action::Quit,
                _ => Action::None,
            },
            lines = tailer.poll() => {
                app.apply(&lines?);
                Action::None
            }
        };
        match action {
            Action::Quit => break,
            Action::LoadRaw(node) => {
                app.raw_lines = read_tail(&paths.stream(node), app.tail_lines);
            }
            Action::Pager(patch) => {
                restore_terminal()?;
                pager(&patch).await?;
                enable_raw_mode()?;
                execute!(std::io::stdout(), EnterAlternateScreen)?;
                terminal.clear()?;
            }
            Action::Cancel(node) => cancel_node(&app, node).await,
            Action::None => {}
        }
    }
    Ok(())
}

fn read_tail(path: &camino::Utf8Path, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![format!("no raw stream at {path}")];
    };
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .rev()
        .take(n)
        .rev()
        .map(|l| (*l).to_owned())
        .collect()
}

async fn pager(patch: &camino::Utf8Path) -> anyhow::Result<()> {
    let pager = std::env::var("PAGER").unwrap_or_else(|_| "less".to_owned());
    let mut parts = shell_words::split(&pager).unwrap_or_else(|_| vec![pager.clone()]);
    if parts.is_empty() {
        parts.push("less".to_owned());
    }
    let status = tokio::process::Command::new(&parts[0])
        .args(&parts[1..])
        .arg(patch.as_str())
        .status()
        .await?;
    tracing::debug!("pager exited with {status}");
    Ok(())
}

/// `k` cancels one node, never the run: the TUI is an observer.
async fn cancel_node(app: &App, node: NodeId) {
    if let Some(NodeState::Running { pgid, .. }) = app.view.nodes.get(&node).map(|n| &n.state) {
        let _ = crate::worker::spawn::terminate(
            *pgid,
            std::time::Duration::from_secs(5),
            crate::worker::spawn::Reaper::Here,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::record::{JournalEvent, SCHEMA_VERSION};
    use crate::model::core::{
        LimitScope, LimitStatus, LimitWindow, NodeKind, Provider, RateLimitSnapshot, Tier, Usage,
        WorkspaceRef,
    };
    use crate::model::node::NodeRecord;
    use ratatui::backend::TestBackend;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static RESTORES: AtomicUsize = AtomicUsize::new(0);

    fn counting_restore() -> std::io::Result<()> {
        RESTORES.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn id(n: u8) -> NodeId {
        NodeId::from_str(&format!("01ARZ3NDEKTSV4RRFFQ69G5F{:02}", n)).expect("node id")
    }

    fn run_id() -> RunId {
        RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("run id")
    }

    fn at(o: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + o).expect("time")
    }

    fn line(seq: u64, node: Option<NodeId>, event: JournalEvent) -> JournalLine {
        JournalLine {
            seq,
            at: at(seq as i64),
            run: run_id(),
            node,
            event,
        }
    }

    fn record(id: NodeId, title: &str, state: NodeState) -> NodeRecord {
        NodeRecord {
            id,
            run_id: run_id(),
            parent: None,
            logical: id,
            attempt: 1,
            retry_of: None,
            kind: NodeKind::Worker,
            title: title.to_owned(),
            prompt_path: Utf8PathBuf::from("prompt.md"),
            prompt_sha256: String::new(),
            provider: Provider::Anthropic,
            account: Some(AccountId("main".into())),
            exec: Some("claude-main".into()),
            argv: Vec::new(),
            model: Some("model-mid".into()),
            tier: Tier::Mid,
            workspace: WorkspaceRef::ReadOnly {
                path: Utf8PathBuf::from("/repo"),
            },
            session: None,
            state,
            created_at: at(0),
            started_at: Some(at(1)),
            ended_at: Some(at(61)),
            usage: Usage {
                input_tokens: 1_000,
                output_tokens: 200,
                ..Usage::default()
            },
            cost: None,
            exit: None,
            files: Vec::new(),
            work: None,
            summary: None,
            stream_offset: 0,
            unparsed_lines: 0,
        }
    }

    fn journal() -> Vec<JournalLine> {
        vec![
            line(
                1,
                None,
                JournalEvent::RunStarted {
                    swamp_version: "0.1.0".into(),
                    schema: SCHEMA_VERSION,
                    argv: vec!["swamp".into()],
                    cwd: Utf8PathBuf::from("/repo"),
                    repo: Some(Utf8PathBuf::from("/repo")),
                    base: Some("9f3c1ad".into()),
                    config_sha256: "abc".into(),
                    task: Some("do the thing".into()),
                },
            ),
            line(
                2,
                Some(id(1)),
                JournalEvent::NodeSpawned {
                    node: Box::new(record(id(1), "migrate user model", NodeState::Succeeded)),
                },
            ),
            line(
                3,
                Some(id(2)),
                JournalEvent::NodeSpawned {
                    node: Box::new(record(id(2), "update changelog", NodeState::Queued)),
                },
            ),
            line(
                4,
                None,
                JournalEvent::AccountHealth {
                    account: AccountId("main".into()),
                    health: Health::Degraded,
                    cooldown_until: None,
                    quota: Some(RateLimitSnapshot {
                        status: LimitStatus::Warning,
                        windows: vec![LimitWindow {
                            scope: LimitScope::SevenDay,
                            utilization: 0.64,
                            resets_at: None,
                        }],
                        resets_at: None,
                    }),
                },
            ),
        ]
    }

    /// The raw pane is titled from the selection. Moving the selection without reloading
    /// labelled one node and showed another's bytes.
    #[test]
    fn moving_the_selection_reloads_the_raw_pane() {
        let mut app = App::from_lines(run_id(), &journal());
        assert_eq!(app.on_key(key(KeyCode::Down)), Action::None, "raw is off");

        app.selected = 0;
        assert_eq!(app.on_key(key(KeyCode::Char('r'))), Action::LoadRaw(id(1)));
        app.raw_lines = vec!["node one".to_owned()];
        assert_eq!(app.on_key(key(KeyCode::Down)), Action::LoadRaw(id(2)));
        assert!(app.raw_lines.is_empty(), "stale bytes under a new title");
    }

    /// `swamp trace` marks a node whose process is gone as orphaned; the live view must not
    /// keep rendering it as running.
    #[test]
    fn a_dead_node_is_orphaned_in_the_live_view_too() {
        let mut lines = journal();
        lines.push(line(
            5,
            Some(id(2)),
            JournalEvent::ProcessStarted {
                pid: 424_242,
                pgid: 424_242,
                argv: vec!["claude".into()],
                env_overrides: Default::default(),
                cwd: Utf8PathBuf::from("/repo"),
            },
        ));
        let mut app = App::new(run_id());
        // No pidfile exists for this node, so liveness reports it as gone.
        app.paths = Some(crate::journal::paths::RunPaths {
            run: run_id(),
            dir: Utf8PathBuf::from("/nonexistent/run"),
            sock_dir: Utf8PathBuf::from("/nonexistent/sock"),
        });
        app.apply(&lines);
        assert!(matches!(
            app.view.nodes[&id(2)].state,
            NodeState::Orphaned { .. }
        ));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn app_state_comes_from_the_journal_alone() {
        let app = App::from_lines(run_id(), &journal());
        assert_eq!(app.rows.len(), 2);
        assert_eq!(app.rows[0].title, "migrate user model");
        assert_eq!(
            app.selected_node().map(|n| n.title.clone()).unwrap(),
            "migrate user model"
        );

        let gauges = app.gauges();
        assert_eq!(gauges.len(), 1);
        assert_eq!(gauges[0].0, AccountId("main".into()));
        assert!((gauges[0].1 - 0.64).abs() < 1e-9);
        assert_eq!(gauges[0].2, Health::Degraded);
        assert_eq!(gauge_bar(0.64), "[######----]");
    }

    #[test]
    fn selection_moves_and_stays_in_range() {
        let mut app = App::from_lines(run_id(), &journal());
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        app.on_key(down);
        assert_eq!(app.selected, 1);
        app.on_key(down);
        assert_eq!(app.selected, 1, "the last row is the floor");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.selected, 0);
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
            Action::Quit
        );
        assert!(app.quit);
    }

    #[test]
    fn the_tree_pane_and_the_account_gauges_render() {
        let app = App::from_lines(run_id(), &journal());
        let mut terminal = ratatui::Terminal::new(TestBackend::new(100, 14)).expect("terminal");
        terminal.draw(|f| app.draw(f)).expect("draw");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("migrate user model"), "{text}");
        assert!(text.contains("update changelog"), "{text}");
        assert!(text.contains("main"), "{text}");
        assert!(text.contains("64%"), "{text}");
        assert!(text.contains(&run_id().short()), "{text}");
    }

    #[test]
    fn the_guard_and_the_panic_hook_both_restore_the_terminal() {
        let before = RESTORES.load(Ordering::SeqCst);
        drop(TerminalGuard::with(counting_restore));
        assert_eq!(RESTORES.load(Ordering::SeqCst), before + 1);

        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_hook(counting_restore);
        let caught = std::panic::catch_unwind(|| panic!("tty check"));
        std::panic::set_hook(previous);
        assert!(caught.is_err());
        assert_eq!(RESTORES.load(Ordering::SeqCst), before + 2);
    }
}
