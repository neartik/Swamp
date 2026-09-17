//! `docs/BOARD.md` §7.5: the board driven through a real `vt100` emulator, plus the keys of
//! §4. The emulator pattern is the one `src/ui/chat/live/tests.rs` already uses: every byte
//! the backend writes is fed to the parser, and the assertions are made on what a user sees.

use crate::dispatch::persist::StateMap;
use crate::dispatch::policy::{Scoring, SelectionPolicy};
use crate::ids::RunId;
use crate::journal::paths::RunPaths;
use crate::journal::record::JournalLine;
use crate::model::core::AccountId;
use crate::ui::board::app::{Action, App, BoardPid, json};
use crate::ui::board::model::{Board, RunPane, Selection};
use crate::ui::board::sources::{Tail, rows_from_state};
use crate::ui::chat::tests_support as fx;
use crate::ui::chat::theme::Theme;
use camino::Utf8PathBuf;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;
use std::str::FromStr;
use std::time::Duration as StdDuration;

const MAX_AGE: StdDuration = StdDuration::from_secs(60);
const ROWS: u16 = 24;

// ---------------------------------------------------------------- fixtures

fn run_b() -> RunId {
    RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FBZ").expect("run id")
}

fn paths(run: RunId) -> RunPaths {
    RunPaths {
        run,
        dir: Utf8PathBuf::from(format!("/repo/.swamp/runs/{run}")),
        sock_dir: Utf8PathBuf::from("/home/.swamp/sock"),
    }
}

fn pane(run: RunId, lines: &[JournalLine]) -> RunPane {
    let mut pane = RunPane::new(
        paths(run),
        Tail::detached("/repo/.swamp/runs/x/journal.jsonl"),
    );
    pane.apply(lines);
    pane
}

/// Two runs: one still going, one that also carries the finished nodes of `recent`.
fn board() -> Board {
    let mut b = Board::new(Scoring::default(), SelectionPolicy::default(), fx::now());
    b.runs = vec![
        pane(fx::run_id(), &fx::running()),
        pane(run_b(), &fx::fixture()),
    ];
    b.accounts = rows_from_state(&fx::config(), &StateMap::default());
    b.accounts_at = Some(fx::now());
    b
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

// ---------------------------------------------------------------- §7.5

#[derive(Clone)]
struct Vt(Rc<RefCell<vt100::Parser>>);

impl Write for Vt {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().process(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Vt {
    fn new(cols: u16) -> Vt {
        Vt(Rc::new(RefCell::new(vt100::Parser::new(ROWS, cols, 200))))
    }

    fn set_size(&self, cols: u16) {
        self.0.borrow_mut().set_size(ROWS, cols);
    }

    fn rows(&self, cols: u16) -> Vec<String> {
        self.0.borrow().screen().rows(0, cols).collect()
    }

    fn alternate(&self) -> bool {
        self.0.borrow().screen().alternate_screen()
    }
}

/// One board on screen, no torn row: the header appears exactly once, every row fits the
/// pane, and nothing of the 100-column frame survives the shrink to 40.
#[test]
fn the_alternate_screen_holds_exactly_one_board_across_a_resize() {
    use crossterm::execute;
    use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

    let mut vt = Vt::new(100);
    execute!(vt, EnterAlternateScreen).expect("alternate screen");
    assert!(vt.alternate(), "the board owns the alternate screen");

    let theme = Theme::plain();
    let mut b = board();
    let mut app = App::new();
    let mut term = Terminal::with_options(
        CrosstermBackend::new(vt.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, 100, ROWS)),
        },
    )
    .expect("terminal");

    for tick in 0..3u64 {
        term.draw(|f| app.draw(&b, f, &theme, tick, MAX_AGE))
            .expect("draw");
    }
    assert_one_board(&vt, 100);

    for cols in [40u16, 100] {
        vt.set_size(cols);
        term.resize(Rect::new(0, 0, cols, ROWS)).expect("resize");
        term.draw(|f| app.draw(&b, f, &theme, 3, MAX_AGE))
            .expect("draw");
        assert_one_board(&vt, cols);
    }

    // The overlay is a full-pane view: it replaces the board rather than drawing over it.
    b.selected = Selection::Node {
        run: fx::run_id(),
        logical: fx::id(1),
    };
    app.on_key(&mut b, key(KeyCode::Enter));
    term.draw(|f| app.draw(&b, f, &theme, 4, MAX_AGE))
        .expect("draw");
    let screen = vt.rows(100).join("\n");
    assert!(screen.contains("trace 9g5f01"), "{screen}");
    assert_eq!(screen.matches("swamp board").count(), 0, "{screen}");

    execute!(vt, LeaveAlternateScreen).expect("restore");
    assert!(!vt.alternate(), "the guard gives the tty back");
}

fn assert_one_board(vt: &Vt, cols: u16) {
    let rows = vt.rows(cols);
    let screen = rows.join("\n");
    assert_eq!(
        screen.matches("swamp board").count(),
        1,
        "{cols}: one board, not several\n{screen}"
    );
    assert_eq!(rows.len(), ROWS as usize, "{cols}: the pane is full height");
    for row in &rows {
        assert!(
            unicode_width::UnicodeWidthStr::width(row.as_str()) <= cols as usize,
            "{cols}: torn row {row:?}"
        );
    }
    assert!(
        rows.iter().any(|r| r.contains("q quit")),
        "{cols}: the hints survive\n{screen}"
    );
}

// ---------------------------------------------------------------- §4

/// The cursor walks accounts, nodes, waiting and recent in draw order, and stops at the ends.
#[test]
fn the_arrows_walk_the_rows_in_draw_order() {
    let mut b = board();
    let mut app = App::new();
    let targets = app.targets(&b.rows());
    assert!(targets.len() > 2, "{targets:?}");
    assert_eq!(targets[0], Selection::Account(AccountId("main".into())));

    app.on_key(&mut b, key(KeyCode::Down));
    assert_eq!(b.selected, targets[0]);
    app.on_key(&mut b, key(KeyCode::Down));
    assert_eq!(b.selected, targets[1]);
    app.on_key(&mut b, key(KeyCode::Up));
    app.on_key(&mut b, key(KeyCode::Up));
    assert_eq!(b.selected, targets[0], "the first row is the ceiling");

    for _ in 0..targets.len() + 4 {
        app.on_key(&mut b, key(KeyCode::Down));
    }
    assert_eq!(
        b.selected,
        *targets.last().expect("a last row"),
        "the last row is the floor"
    );
}

/// `←` folds an account's nodes away and takes the cursor with it; `→` brings them back.
#[test]
fn collapsing_an_account_hides_its_nodes_but_not_its_count() {
    let mut b = board();
    let mut app = App::new();
    let main = AccountId("main".into());
    let rows = b.rows();
    let before = app.targets(&rows).len();

    b.selected = Selection::Account(main.clone());
    app.on_key(&mut b, key(KeyCode::Left));
    assert!(app.collapsed.contains(&main));
    assert!(app.targets(&b.rows()).len() < before);
    let shown = app.visible(&rows);
    assert!(
        shown.providers[0].accounts[0].nodes.is_empty(),
        "the nodes are folded away"
    );
    assert!(
        !rows.providers[0].accounts[0].nodes.is_empty(),
        "the header still counts them"
    );

    app.on_key(&mut b, key(KeyCode::Right));
    assert!(!app.collapsed.contains(&main));
    assert_eq!(app.targets(&b.rows()).len(), before);
}

/// `enter` opens the trace of the selected node and `esc` puts the board back.
#[test]
fn enter_opens_the_trace_overlay_and_esc_returns() {
    let mut b = board();
    let mut app = App::new();
    b.selected = Selection::Node {
        run: fx::run_id(),
        logical: fx::id(1),
    };
    app.on_key(&mut b, key(KeyCode::Enter));
    let overlay = app.overlay.clone().expect("the trace overlay");
    assert!(overlay.title.starts_with("trace 9g5f01"), "{overlay:?}");
    assert!(
        overlay
            .lines
            .iter()
            .any(|l| l.contains("add pagination to /users")),
        "{overlay:?}"
    );

    // Scrolling stays inside the overlay; the board's own selection never moves under it.
    app.on_key(&mut b, key(KeyCode::Down));
    assert_eq!(app.overlay.as_ref().expect("still open").scroll, 1);
    assert_eq!(
        b.selected,
        Selection::Node {
            run: fx::run_id(),
            logical: fx::id(1)
        }
    );

    app.on_key(&mut b, key(KeyCode::Esc));
    assert!(app.overlay.is_none(), "esc returns to the board");
}

/// `tab` cycles the tailed runs, `0` merges them again, and the rows follow the focus.
#[test]
fn tab_cycles_the_runs_and_zero_merges_them() {
    let mut b = board();
    let mut app = App::new();
    assert_eq!(b.focus, None);
    app.on_key(&mut b, key(KeyCode::Tab));
    assert_eq!(b.focus, Some(fx::run_id()));
    app.on_key(&mut b, key(KeyCode::Tab));
    assert_eq!(b.focus, Some(run_b()));
    app.on_key(&mut b, key(KeyCode::Tab));
    assert_eq!(b.focus, None, "past the last run is every run");

    b.focus = Some(run_b());
    let focused = b.rows();
    b.focus = None;
    assert!(
        b.rows().recent.len() >= focused.recent.len(),
        "merged shows at least what one run does"
    );
    app.on_key(&mut b, key(KeyCode::Char('0')));
    assert_eq!(b.focus, None);
}

/// `a` swaps the tree for the `/usage` table, and `q` leaves.
#[test]
fn the_accounts_view_and_quit() {
    let mut b = board();
    let mut app = App::new();
    app.on_key(&mut b, key(KeyCode::Char('a')));
    assert!(app.accounts_only);
    assert!(
        app.targets(&b.rows())
            .iter()
            .all(|t| matches!(t, Selection::Account(_))),
        "accounts only means accounts only"
    );
    let lines = app.lines(&b, Rect::new(0, 0, 100, ROWS), &Theme::plain(), 0, MAX_AGE);
    let text = crate::ui::chat::blocks::text_of(&lines).join("\n");
    assert!(text.contains("swamp board"), "{text}");
    assert!(text.contains("main"), "the usage table: {text}");

    app.on_key(&mut b, key(KeyCode::Char('a')));
    assert!(!app.accounts_only);
    assert_eq!(app.on_key(&mut b, key(KeyCode::Char('q'))), Action::Quit);
    assert!(app.quit);
    assert_eq!(
        App::new().on_key(
            &mut b,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        Action::Quit,
        "ctrl+c is a key in raw mode, not a signal"
    );
}

/// §5: the pid file is what tells `swamp chat` a board is already attached.
#[test]
fn the_board_pid_is_written_on_start_and_removed_on_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("board.pid")).expect("utf8");
    {
        let _pid = BoardPid::write(&path).expect("written");
        assert!(crate::journal::paths::board_is_alive(&path));
    }
    assert!(!path.exists(), "the guard removes it on the way out");
}

/// `--json` carries the same rows the frame draws, and the accounts in `/usage`'s shape.
#[test]
fn the_json_dump_carries_the_frame() {
    let b = board();
    let v = json(&b);
    assert_eq!(v["runs"].as_array().expect("runs").len(), 2);
    assert_eq!(
        v["totals"]["in_flight"],
        serde_json::json!(v["in_flight"].as_array().expect("in flight").len())
    );
    let titles: Vec<&str> = v["in_flight"]
        .as_array()
        .expect("in flight")
        .iter()
        .filter_map(|n| n["title"].as_str())
        .collect();
    assert!(titles.contains(&"add pagination to /users"), "{titles:?}");
    assert!(
        v["accounts"][0]["account"].is_string(),
        "the /usage account shape: {}",
        v["accounts"]
    );
}
