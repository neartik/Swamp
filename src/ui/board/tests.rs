//! `docs/BOARD.md` §7.5: the board driven through a real `vt100` emulator, plus the keys of
//! §4. The emulator pattern is the one `src/ui/chat/live/tests.rs` already uses: every byte
//! the backend writes is fed to the parser, and the assertions are made on what a user sees.

use crate::dispatch::persist::StateMap;
use crate::dispatch::policy::{Scoring, SelectionPolicy};
use crate::ids::RunId;
use crate::journal::paths::RunPaths;
use crate::journal::record::JournalLine;
use crate::model::core::AccountId;
use crate::ui::actions::CancelTarget;
use crate::ui::board::app::{Action, App, BoardPid, json, tail_lines};
use crate::ui::board::model::{Board, RunPane, Selection};
use crate::ui::board::render;
use crate::ui::board::sources::{Tail, rows_from_state};
use crate::ui::chat::blocks::text_of;
use crate::ui::chat::tests_support as fx;
use crate::ui::chat::theme::Theme;
use crate::ui::keys::{self, Surface};
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

/// The P4 run, and a schema-1 run that holds only a legacy bucket.
fn board() -> Board {
    let mut b = Board::new(Scoring::default(), SelectionPolicy::default(), fx::now());
    b.runs = vec![
        pane(fx::run_id(), &fx::p4_journal()),
        pane(run_b(), &fx::fixture()),
    ];
    b.accounts = rows_from_state(&fx::config(), &StateMap::default());
    b.accounts_at = Some(fx::now());
    b
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn task(n: u8) -> Selection {
    Selection::Node {
        run: fx::run_id(),
        logical: fx::p4_task(n),
    }
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
    let mut app = App::new(true);
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
    b.selected = task(1);
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

/// The cursor walks the brain, each dispatch and its tasks, then the accounts, and stops at
/// the ends.
#[test]
fn the_arrows_walk_the_rows_in_draw_order() {
    let mut b = board();
    let mut app = App::new(true);
    let targets = app.targets(&b.rows());
    assert_eq!(
        targets[0],
        Selection::Node {
            run: fx::run_id(),
            logical: fx::id(0),
        },
        "the brain first"
    );
    assert_eq!(
        targets[1],
        Selection::Dispatch {
            run: fx::run_id(),
            id: fx::did("18"),
        }
    );
    assert_eq!(targets[2], task(1), "then its tasks in rank order");
    assert_eq!(
        targets.last(),
        Some(&Selection::Account(AccountId("main".into())))
    );

    app.on_key(&mut b, key(KeyCode::Down));
    assert_eq!(b.selected, targets[0]);
    assert!(
        app.touched,
        "moving stops the selection following attention"
    );
    app.on_key(&mut b, key(KeyCode::Down));
    assert_eq!(b.selected, targets[1]);
    app.on_key(&mut b, key(KeyCode::Up));
    app.on_key(&mut b, key(KeyCode::Up));
    assert_eq!(b.selected, targets[0], "the first row is the ceiling");
    app.on_key(&mut b, ch('G'));
    assert_eq!(&b.selected, targets.last().expect("a last row"));
    app.on_key(&mut b, key(KeyCode::Down));
    assert_eq!(
        &b.selected,
        targets.last().expect("a last row"),
        "the floor"
    );
}

/// Until the user moves, the cursor sits on whatever needs attention; afterwards it stays.
#[test]
fn the_selection_follows_attention_until_touched() {
    let mut b = board();
    let mut app = App::new(true);
    let failed = Selection::Node {
        run: run_b(),
        logical: fx::id(2),
    };
    app.sync(&mut b);
    assert_eq!(b.selected, failed, "a failure outranks the blocked task");
    b.focus = Some(fx::run_id());
    app.sync(&mut b);
    assert_eq!(
        b.selected,
        task(3),
        "the blocked task, once the failure is out of view"
    );
    app.on_key(&mut b, key(KeyCode::Up));
    app.sync(&mut b);
    assert_eq!(b.selected, task(2), "moved, and kept");
}

/// `←` on a task folds its dispatch and takes the cursor to the header; `→` opens it again.
#[test]
fn folding_a_dispatch_hides_its_tasks_but_not_its_counts() {
    let mut b = board();
    let mut app = App::new(true);
    let before = app.targets(&b.rows()).len();
    b.selected = task(1);
    app.on_key(&mut b, key(KeyCode::Left));
    let header = Selection::Dispatch {
        run: fx::run_id(),
        id: fx::did("18"),
    };
    assert_eq!(b.selected, header);
    assert_eq!(app.targets(&b.rows()).len(), before - 5);
    let shown = app.visible(&b.rows());
    assert!(!shown.runs[0].active[0].expanded);
    assert_eq!(
        shown.tally.running,
        b.rows().tally.running,
        "the header still counts"
    );

    app.on_key(&mut b, key(KeyCode::Right));
    assert_eq!(app.targets(&b.rows()).len(), before);
}

/// `!` walks the stuck rows only, and wraps.
#[test]
fn bang_jumps_to_the_next_stuck_row() {
    let mut b = board();
    let mut app = App::new(true);
    b.selected = task(1);
    app.on_key(&mut b, ch('!'));
    assert_eq!(b.selected, task(3), "the blocked task");
    app.on_key(&mut b, ch('!'));
    assert_eq!(
        b.selected,
        Selection::Dispatch {
            run: fx::run_id(),
            id: fx::did("1c"),
        },
        "the folded dispatch hiding a rejection"
    );
    app.on_key(&mut b, ch('!'));
    assert_eq!(
        b.selected,
        Selection::Node {
            run: run_b(),
            logical: fx::id(2),
        },
        "the failed worker of the other run"
    );
    app.on_key(&mut b, ch('!'));
    assert_eq!(b.selected, task(3), "and round again");
}

/// `enter` opens the trace of the selected task and `esc` puts the board back.
#[test]
fn enter_opens_the_trace_overlay_and_esc_returns() {
    let mut b = board();
    let mut app = App::new(true);
    b.selected = task(1);
    app.on_key(&mut b, key(KeyCode::Enter));
    let overlay = app.overlay.clone().expect("the trace overlay");
    assert!(overlay.title.starts_with("9g5f01"), "{overlay:?}");
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
    assert_eq!(b.selected, task(1));

    app.on_key(&mut b, key(KeyCode::Esc));
    assert!(app.overlay.is_none(), "esc returns to the board");

    app.on_key(&mut b, ch('?'));
    let keys = app.overlay.clone().expect("the keys pager");
    assert!(
        keys.lines
            .iter()
            .any(|l| l.contains("jump to the next stuck row"))
    );
}

/// `tab` cycles the tailed runs, `0` merges them again, and the rows follow the focus.
#[test]
fn tab_cycles_the_runs_and_zero_merges_them() {
    let mut b = board();
    let mut app = App::new(true);
    assert_eq!(b.focus, None);
    app.on_key(&mut b, key(KeyCode::Tab));
    assert_eq!(b.focus, Some(fx::run_id()));
    app.on_key(&mut b, key(KeyCode::Tab));
    assert_eq!(b.focus, Some(run_b()));
    app.on_key(&mut b, key(KeyCode::Tab));
    assert_eq!(b.focus, None, "past the last run is every run");
    app.on_key(&mut b, KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(b.focus, Some(run_b()));
    app.on_key(&mut b, ch('0'));
    assert_eq!(b.focus, None);
}

/// `a` swaps the tree for the `/usage` table, and `q` leaves.
#[test]
fn the_accounts_view_and_quit() {
    let mut b = board();
    let mut app = App::new(true);
    app.on_key(&mut b, ch('a'));
    assert!(app.accounts_only);
    assert!(
        app.targets(&b.rows())
            .iter()
            .all(|t| matches!(t, Selection::Account(_))),
        "accounts only means accounts only"
    );
    let lines = app.lines(&b, Rect::new(0, 0, 100, ROWS), &Theme::plain(), 0, MAX_AGE);
    let text = text_of(&lines).join("\n");
    assert!(text.contains("swamp board"), "{text}");
    assert!(text.contains("main"), "the usage table: {text}");

    app.on_key(&mut b, ch('a'));
    assert!(!app.accounts_only);
    assert_eq!(app.on_key(&mut b, ch('q')), Action::Quit);
    assert!(app.quit);
    assert_eq!(
        App::new(true).on_key(
            &mut b,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        Action::Quit,
        "ctrl+c is a key in raw mode, not a signal"
    );
}

/// The hint line is the shared table's, and `k` leaves it when the board may not cancel.
#[test]
fn the_hints_are_the_key_tables() {
    let b = board();
    let lines = render::frame(&b, 200, &Theme::plain(), 0, MAX_AGE);
    let last = text_of(&lines).pop().expect("a hint line");
    assert_eq!(last, keys::hints(Surface::Board, 200, &[]));

    let mut app = App::new(false);
    let lines = app.lines(&b, Rect::new(0, 0, 200, 40), &Theme::plain(), 0, MAX_AGE);
    let last = text_of(&lines).pop().expect("a hint line");
    assert!(!last.contains("cancel"), "{last}");
}

/// Any pane, however small, draws: the detail gives up lines before the body does, and the
/// selection stays in view while the body scrolls.
#[test]
fn every_pane_size_draws_and_keeps_the_selection_in_view() {
    let mut b = board();
    let ascii = Theme {
        ascii: true,
        ..Theme::plain()
    };
    for theme in [Theme::plain(), ascii] {
        let mut app = App::new(true);
        for width in [0u16, 20, 40, 60, 100, 160] {
            for height in 0u16..30 {
                let lines = app.lines(&b, Rect::new(0, 0, width, height), &theme, 0, MAX_AGE);
                for line in text_of(&lines) {
                    assert!(
                        unicode_width::UnicodeWidthStr::width(line.as_str()) <= width as usize,
                        "{width}x{height}: {line:?}"
                    );
                }
            }
        }
    }
    let mut app = App::new(true);
    app.touched = true;
    b.selected = Selection::Account(AccountId("main".into()));
    app.on_key(&mut b, key(KeyCode::Up));
    let lines = app.lines(&b, Rect::new(0, 0, 60, 20), &Theme::plain(), 0, MAX_AGE);
    let text = text_of(&lines).join("\n");
    assert!(
        text.contains("\u{258c}"),
        "the selected row is on screen: {text}"
    );
}

// ---------------------------------------------------------------- cancel

/// `k` then `y` is exactly one cancel, of what is selected.
#[test]
fn a_confirmed_cancel_emits_one_effect() {
    let mut b = board();
    let mut app = App::new(true);
    b.selected = task(3);
    assert_eq!(app.on_key(&mut b, ch('k')), Action::None);
    assert!(app.confirm.is_some(), "k asks first");
    assert_eq!(
        app.on_key(&mut b, ch('y')),
        Action::Cancel(CancelTarget::Task {
            run: fx::run_id(),
            logical: fx::p4_task(3),
        })
    );
    assert!(app.confirm.is_none());
    assert_eq!(
        app.on_key(&mut b, ch('y')),
        Action::None,
        "one y, one cancel"
    );

    b.selected = Selection::Dispatch {
        run: fx::run_id(),
        id: fx::did("18"),
    };
    app.on_key(&mut b, ch('k'));
    let lines = app.lines(&b, Rect::new(0, 0, 40, 40), &Theme::plain(), 0, MAX_AGE);
    assert_eq!(
        text_of(&lines).pop().as_deref(),
        Some("cancel #1 9g5f18 \u{b7} 4 live tasks? y / n")
    );
    assert_eq!(
        app.on_key(&mut b, ch('y')),
        Action::Cancel(CancelTarget::Dispatch {
            run: fx::run_id(),
            id: fx::did("18"),
        })
    );
}

/// Anything but `y` dismisses the prompt, cancels nothing and is swallowed.
#[test]
fn no_confirm_emits_nothing() {
    for answer in [ch('n'), key(KeyCode::Esc), ch('x'), key(KeyCode::Down)] {
        let mut b = board();
        let mut app = App::new(true);
        b.selected = task(1);
        app.touched = true;
        app.on_key(&mut b, ch('k'));
        assert!(app.confirm.is_some());
        assert_eq!(app.on_key(&mut b, answer), Action::None, "{answer:?}");
        assert!(app.confirm.is_none(), "{answer:?} clears the prompt");
        assert_eq!(b.selected, task(1), "{answer:?} was swallowed");
    }
}

/// The brain, a finished task and a read-only board each get a notice instead of a prompt.
#[test]
fn what_cannot_be_cancelled_says_why() {
    let cases = [
        (
            true,
            Selection::Node {
                run: fx::run_id(),
                logical: fx::id(0),
            },
            "the brain stops with swamp cancel 9g5fav",
        ),
        (true, task(5), "9g5f05 is already ok"),
        (
            true,
            Selection::Dispatch {
                run: fx::run_id(),
                id: fx::did("1c"),
            },
            "#2 9g5f1c has no live tasks",
        ),
        (
            true,
            Selection::Account(AccountId("main".into())),
            "select a task or dispatch to cancel",
        ),
        (false, task(3), "read-only: ui.board_actions = false"),
    ];
    for (actions, sel, want) in cases {
        let mut b = board();
        let mut app = App::new(actions);
        b.selected = sel;
        assert_eq!(app.on_key(&mut b, ch('k')), Action::None, "{want}");
        assert!(app.confirm.is_none(), "{want}: no prompt");
        let lines = app.lines(&b, Rect::new(0, 0, 60, 40), &Theme::plain(), 0, MAX_AGE);
        assert_eq!(text_of(&lines).pop().as_deref(), Some(want));
    }
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

/// `--json` carries the same rows the frame draws, each run's dispatches in the
/// `swamp dispatches --json` shape and the accounts in `/usage`'s.
#[test]
fn the_json_dump_carries_the_frame() {
    let b = board();
    let v = json(&b);
    assert_eq!(v["runs"].as_array().expect("runs").len(), 2);
    assert_eq!(
        v["totals"]["in_flight"],
        serde_json::json!(v["in_flight"].as_array().expect("in flight").len())
    );
    assert_eq!(v["totals"]["stuck"], serde_json::json!(2));
    let dispatches = v["runs"][0]["dispatches"].as_array().expect("dispatches");
    assert_eq!(dispatches.len(), 2);
    assert_eq!(dispatches[0]["short"], serde_json::json!("9g5f18"));
    let running: Vec<&serde_json::Value> = v["in_flight"]
        .as_array()
        .expect("in flight")
        .iter()
        .filter(|n| n["title"] == "add pagination to /users")
        .collect();
    assert_eq!(running.len(), 1, "the other run's copy has finished");
    assert_eq!(running[0]["dispatch"], serde_json::json!("9g5f18"));
    assert!(
        v["accounts"][0]["account"].is_string(),
        "the /usage account shape: {}",
        v["accounts"]
    );
}

/// `r` runs on the same task as the render and the tails, and a long-lived run's journal has
/// no bound: what the read costs has to depend on the 200 lines, not on the file.
#[test]
fn the_raw_view_reads_only_the_tail_of_a_journal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = Utf8PathBuf::from_path_buf(dir.path().join("journal.jsonl")).expect("utf8");
    // Several windows wide, so the read really has to walk backwards to find its 200 lines.
    let pad = "x".repeat(64);
    let body: String = (0..5_000).map(|n| format!("line {n} {pad}\n")).collect();
    std::fs::write(&path, &body).expect("write");
    assert!(body.len() > 4 * 64 * 1024, "wider than one read window");

    let text = tail_lines(&path, 200);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 200);
    assert_eq!(lines[0], format!("line 4800 {pad}"));
    assert_eq!(lines[199], format!("line 4999 {pad}"));
    // No fragment survives the window's leading edge.
    assert!(
        lines.iter().all(|l| l.starts_with("line ")),
        "{:?}",
        lines[0]
    );

    // A file shorter than one window, and one that is not there at all.
    std::fs::write(&path, "a\nb\n").expect("write");
    assert_eq!(tail_lines(&path, 200), "a\nb");
    assert_eq!(tail_lines(&path.with_file_name("gone.jsonl"), 200), "");
}
