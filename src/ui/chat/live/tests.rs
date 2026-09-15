//! `Inline` drives a real terminal, so the tests give it one: every byte it writes is fed to a
//! `vt100` emulator with scrollback, and the assertions are made on what a user would see and
//! be able to scroll back to.

use super::*;
use std::cell::RefCell;
use std::rc::Rc;

const COLS: u16 = 40;
const ROWS: u16 = 24;

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

struct Host {
    term: Inline<Vt>,
    vt: Vt,
    cols: u16,
    rows: u16,
}

impl Host {
    /// A host terminal that already has `prelude` on it, with the chat started under it.
    fn new(prelude: &[&str]) -> Host {
        let parser = Rc::new(RefCell::new(vt100::Parser::new(ROWS, COLS, 500)));
        for line in prelude {
            parser
                .borrow_mut()
                .process(format!("{line}\r\n").as_bytes());
        }
        let row = parser.borrow().screen().cursor_position().0;
        let vt = Vt(parser);
        let term = Inline::new(vt.clone(), COLS, ROWS, 4, row).expect("inline");
        Host {
            term,
            vt,
            cols: COLS,
            rows: ROWS,
        }
    }

    /// One turn of the chat loop: size the live area, then draw it.
    fn frame(&mut self, live: &[&str]) {
        self.term.set_height(live.len() as u16).expect("set_height");
        self.term.draw(lines(live), (0, 0)).expect("draw");
    }

    /// A block leaving the live area: the height is set from the post-commit state first, the
    /// way `chat::interactive` does it.
    fn commit(&mut self, block: &[&str], live_after: &[&str]) {
        self.term
            .set_height(live_after.len() as u16)
            .expect("set_height");
        self.term.commit(lines(block)).expect("commit");
        self.term.draw(lines(live_after), (0, 0)).expect("draw");
    }

    /// The window changing shape: the emulator reflows first, then the chat loop is told.
    fn resize(&mut self, cols: u16, rows: u16, live: &[&str]) {
        self.vt.0.borrow_mut().set_size(rows, cols);
        self.cols = cols;
        self.rows = rows;
        self.term.set_size(cols, rows).expect("set_size");
        self.frame(live);
    }

    fn screen(&self) -> Vec<String> {
        let mut parser = self.vt.0.borrow_mut();
        parser.set_scrollback(0);
        rows(&parser, self.cols)
    }

    /// Scrollback first, then the visible screen: everything the terminal still holds.
    fn history(&self) -> Vec<String> {
        let mut parser = self.vt.0.borrow_mut();
        // vt100 cannot show more than one screenful of scrollback at a time.
        parser.set_scrollback(self.rows as usize);
        let depth = parser.screen().scrollback();
        let mut out = Vec::new();
        for n in (1..=depth).rev() {
            parser.set_scrollback(n);
            out.push(rows(&parser, self.cols).swap_remove(0));
        }
        parser.set_scrollback(0);
        out.extend(rows(&parser, self.cols));
        out
    }
}

fn rows(parser: &vt100::Parser, cols: u16) -> Vec<String> {
    parser
        .screen()
        .rows(0, cols)
        .map(|r| r.trim_end().to_owned())
        .collect()
}

fn lines(text: &[&str]) -> Vec<Line<'static>> {
    text.iter().map(|t| Line::from((*t).to_owned())).collect()
}

/// Every one of `wanted`, in that order, with nothing of it lost.
fn assert_in_order(history: &[String], wanted: &[&str]) {
    let mut at = 0;
    for want in wanted {
        match history[at..].iter().position(|row| row == want) {
            Some(i) => at += i + 1,
            None => panic!(
                "`{want}` missing after row {at} in:\n{}",
                history.join("\n")
            ),
        }
    }
}

/// No committed block is ever padded apart from the next by more than one blank row.
fn assert_no_blank_runs(history: &[String], first: &str, last: &str) {
    let from = history.iter().position(|r| r == first).expect("first row");
    let to = history.iter().rposition(|r| r == last).expect("last row");
    let mut run = 0;
    for row in &history[from..=to] {
        run = if row.is_empty() { run + 1 } else { 0 };
        assert!(
            run <= 1,
            "blank run of {run} rows in:\n{}",
            history.join("\n")
        );
    }
}

/// The live area hangs off the committed tail: one blank row between them at the very most.
fn assert_no_hole(screen: &[String], first_live: &str) {
    let live = screen
        .iter()
        .position(|r| r == first_live)
        .expect("live area");
    let tail = screen[..live]
        .iter()
        .rposition(|r| !r.is_empty())
        .expect("committed tail");
    assert!(
        live - tail <= 2,
        "{} blank rows above the live area:\n{}",
        live - tail - 1,
        screen.join("\n")
    );
}

/// Exactly one live area on screen, whatever the terminal has just done to its rows.
fn assert_no_duplicate_live(screen: &[String], live: &[&str]) {
    let seen = screen
        .windows(live.len())
        .filter(|w| w.iter().zip(live).all(|(row, want)| row == want.trim_end()))
        .count();
    assert_eq!(seen, 1, "{seen} live areas in:\n{}", screen.join("\n"));
}

fn welcome() -> Vec<&'static str> {
    vec![
        "* swamp",
        "  cwd /repo",
        "  brain anthropic/main",
        "  workers 2 accounts",
        "  run 01ARZ3",
        "",
    ]
}

fn idle() -> Vec<&'static str> {
    vec!["----", "> ", "----", "status"]
}

fn board() -> Vec<&'static str> {
    vec![
        "* workers",
        "  node 1",
        "  node 2",
        "  node 3",
        "  node 4",
        "  node 5",
        "",
        "----",
        "> ",
        "----",
        "status",
    ]
}

/// `n` committed rows, enough of them to push the live area onto the last rows of the screen.
fn filler(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("row {i}")).collect()
}

fn refs(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

#[test]
fn growing_the_live_area_keeps_committed_rows() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&["> prompt", ""], &idle());
    // The board arrives a second later and the live area nearly triples.
    host.frame(&board());

    let history = host.history();
    let mut wanted = welcome();
    wanted.retain(|r| !r.is_empty());
    wanted.push("> prompt");
    assert_in_order(&history, &wanted);
}

#[test]
fn a_notice_committed_under_a_tall_board_survives() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.frame(&board());
    // `esc esc` while the board is up: the notice is committed, the board stays live.
    host.commit(&["/ cancelled 3 nodes", ""], &board());
    host.frame(&board());
    host.frame(&idle());

    let history = host.history();
    assert_in_order(&history, &["* swamp", "/ cancelled 3 nodes"]);
    let seen = history
        .iter()
        .filter(|r| *r == "/ cancelled 3 nodes")
        .count();
    assert_eq!(seen, 1, "notice duplicated in:\n{}", history.join("\n"));
}

#[test]
fn shrinking_the_live_area_scrolls_no_blank_rows() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.frame(&board());
    // The batch finishes: the board is committed and the live area collapses to the input.
    let done: Vec<&str> = board()[..7].to_vec();
    host.commit(&done, &idle());

    let history = host.history();
    assert_no_blank_runs(&history, "* swamp", "  node 5");
    // The input bar follows the committed board with at most one blank row between.
    assert_no_hole(&host.screen(), "----");
}

#[test]
fn a_shrink_with_no_commit_behind_it_leaves_no_hole() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    // `/` opens the command overlay and Backspace closes it again: nothing is committed.
    host.frame(&board());
    host.frame(&idle());

    let screen = host.screen();
    assert_no_hole(&screen, "----");
    assert_no_duplicate_live(&screen, &idle());
    assert_in_order(&host.history(), &["* swamp", "row 18"]);
}

#[test]
fn grow_shrink_commit_cycles_lose_nothing() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    let mut wanted: Vec<String> = welcome()
        .into_iter()
        .filter(|r| !r.is_empty())
        .map(str::to_owned)
        .collect();
    for turn in 0..3 {
        let bar = format!("> turn {turn}");
        host.commit(&[bar.as_str(), ""], &idle());
        host.frame(&board());
        let answer = format!("* answer {turn}");
        let mut live = vec![answer.as_str()];
        live.extend(board());
        host.frame(&live);
        host.commit(&[answer.as_str()], &board());
        host.commit(&board()[..7], &idle());
        wanted.push(bar);
        wanted.push(answer);
        wanted.push("  node 5".to_owned());
    }

    let history = host.history();
    let wanted: Vec<&str> = wanted.iter().map(String::as_str).collect();
    assert_in_order(&history, &wanted);
    assert_no_blank_runs(&history, "> turn 2", "  node 5");
}

#[test]
fn the_live_area_follows_the_committed_tail() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.frame(&board());
    assert_no_hole(&host.screen(), "* workers");
    // Once the screen is full the area is the last rows of it, and stays there.
    host.commit(&refs(&filler(30)), &idle());
    let screen = host.screen();
    assert_eq!(screen[ROWS as usize - 1], "status");
    assert_eq!(screen[ROWS as usize - 4], "----");
    host.frame(&board());
    let screen = host.screen();
    assert_eq!(screen[ROWS as usize - 1], "status");
}

#[test]
fn shrinking_the_terminal_keeps_the_committed_rows_it_still_has() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    // The live area is on the last four rows; the rows just above it are committed.
    host.resize(COLS, 18, &idle());

    let history = host.history();
    // `row 17` and `row 18` are the two rows the emulator itself drops when it loses six rows:
    // everything the terminal still holds has to survive the redraw.
    let mut wanted: Vec<String> = welcome().iter().map(|r| (*r).to_owned()).collect();
    wanted.retain(|r| !r.is_empty());
    wanted.extend(filler(17));
    assert_in_order(&history, &refs(&wanted));
    let screen = host.screen();
    assert_no_duplicate_live(&screen, &idle());
    assert_eq!(screen[17], "status");
}

#[test]
fn growing_the_terminal_leaves_one_live_area() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.resize(COLS, 32, &idle());

    let screen = host.screen();
    assert_no_duplicate_live(&screen, &idle());
    assert_no_hole(&screen, "----");
    assert_in_order(&host.history(), &refs(&filler(19)));
}

#[test]
fn a_width_change_leaves_no_stale_live_rows() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    let live = vec!["----------", "> ", "----------", "status"];
    host.frame(&live);
    host.resize(28, ROWS, &live);
    host.resize(COLS, ROWS, &live);

    let screen = host.screen();
    assert_no_duplicate_live(&screen, &live);
    assert_no_hole(&screen, "----------");
    assert_in_order(&host.history(), &refs(&filler(19)));
}

#[test]
fn a_resize_under_a_tall_board_keeps_the_board_live() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.frame(&board());
    host.resize(30, 20, &board());
    host.commit(&board()[..7], &idle());

    let history = host.history();
    assert_in_order(&history, &["* workers", "  node 5"]);
    let screen = host.screen();
    assert_no_duplicate_live(&screen, &idle());
    assert_no_hole(&screen, "----");
}
