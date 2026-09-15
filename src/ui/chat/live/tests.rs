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
        let vt = Vt(parser);
        let term = Inline::new(vt.clone(), COLS, ROWS, 4).expect("inline");
        Host {
            term,
            vt,
            cols: COLS,
            rows: ROWS,
        }
    }

    /// One turn of the chat loop: size the live area, then draw it.
    fn frame(&mut self, live: &[&str]) {
        self.frame_at(live, (0, 0));
    }

    /// The same, with the caret somewhere other than the area's first row.
    fn frame_at(&mut self, live: &[&str], caret: (u16, u16)) {
        self.term.set_height(live.len() as u16).expect("set_height");
        self.term.draw(lines(live), caret).expect("draw");
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

    /// The window changing shape: the host reshapes its rows first, then the chat loop repairs
    /// the live area from the cursor it left on it.
    fn resize(&mut self, cols: u16, rows: u16, live: &[&str]) {
        self.reshape(cols, rows);
        self.term.reflow(cols, rows).expect("reflow");
        self.frame(live);
    }

    /// A real host bottom-anchors its rows: growing pulls them back out of scrollback and
    /// everything on screen moves *down*, shrinking pushes the top ones into scrollback and
    /// everything moves up, and a row the new width cannot hold is split in two. The cursor
    /// rides along with the row it is on. vt100 does none of that - it resizes the grid in
    /// place - so the screen is rebuilt here from the whole history.
    fn reshape(&mut self, cols: u16, rows: u16) {
        let history = self.history();
        let (cursor_row, cursor_col) = {
            let parser = self.vt.0.borrow();
            parser.screen().cursor_position()
        };
        let at = self.depth() + cursor_row as usize;
        let used = history
            .iter()
            .rposition(|r| !r.is_empty())
            .map_or(0, |i| i + 1)
            .max(at + 1);
        let mut out: Vec<String> = Vec::new();
        let mut cursor_at = 0;
        for (i, row) in history[..used].iter().enumerate() {
            if i == at {
                cursor_at = out.len() + cursor_col as usize / cols as usize;
            }
            let chars: Vec<char> = row.chars().collect();
            if chars.is_empty() {
                out.push(String::new());
            } else {
                out.extend(chars.chunks(cols as usize).map(|c| c.iter().collect()));
            }
        }
        let mut parser = vt100::Parser::new(rows, cols, 500);
        for (i, row) in out.iter().enumerate() {
            if i > 0 {
                parser.process(b"\r\n");
            }
            parser.process(row.as_bytes());
        }
        let y = cursor_at - out.len().saturating_sub(rows as usize).min(cursor_at);
        let col = cursor_col as usize % cols as usize;
        parser.process(format!("\x1b[{};{}H", y + 1, col + 1).as_bytes());
        *self.vt.0.borrow_mut() = parser;
        self.cols = cols;
        self.rows = rows;
    }

    fn screen(&self) -> Vec<String> {
        let mut parser = self.vt.0.borrow_mut();
        parser.set_scrollback(0);
        rows(&parser, self.cols)
    }

    fn depth(&self) -> usize {
        let mut parser = self.vt.0.borrow_mut();
        parser.set_scrollback(usize::MAX);
        let depth = parser.screen().scrollback();
        parser.set_scrollback(0);
        depth
    }

    /// Scrollback first, then the visible screen: everything the terminal still holds.
    fn history(&self) -> Vec<String> {
        let depth = self.depth();
        let mut parser = self.vt.0.borrow_mut();
        let mut out = Vec::new();
        // vt100 shows one screenful at a time, so the scrollback is read one row per offset.
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

/// The idle layout with rules that reach the last column, and a status row that does too.
fn wide_idle(cols: u16) -> Vec<String> {
    let rule = "-".repeat(cols as usize);
    let status = format!(
        "{:<pad$}{}",
        "status",
        "run 01ARZ3",
        pad = cols as usize - 10
    );
    vec![rule.clone(), "> ".to_owned(), rule, status]
}

/// The slash popup over that layout: ten entries, each around half the width of a rule.
fn popup(cols: u16) -> Vec<String> {
    let mut rows: Vec<String> = (0..10)
        .map(|i| format!("  /cmd{i}   what it does"))
        .collect();
    rows.extend(wide_idle(cols));
    rows
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
    // The six rows the screen loses go into scrollback, not into the bin.
    let mut wanted: Vec<String> = welcome().iter().map(|r| (*r).to_owned()).collect();
    wanted.retain(|r| !r.is_empty());
    wanted.extend(filler(19));
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

#[test]
fn a_grow_that_pulls_rows_back_from_scrollback_keeps_them() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    // A full screen: the live area is on the last rows and everything else is in scrollback.
    host.commit(&refs(&filler(30)), &idle());
    host.resize(COLS, 32, &idle());

    let screen = host.screen();
    assert_no_duplicate_live(&screen, &idle());
    assert_no_hole(&screen, "----");
    // The eight rows the host hands back are committed ones, and the area does not sit on them.
    assert_in_order(&host.history(), &refs(&filler(30)));
    assert_eq!(screen[31], "status");
    assert_eq!(screen[27], "row 29");
}

#[test]
fn a_width_shrink_that_splits_the_rules_leaves_no_stale_rows() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    // Full-width rules, and the caret on the input row under the first of them: at 24 columns
    // the host splits each rule in two and the area grows rows the chat never drew.
    let rule = "-".repeat(COLS as usize);
    let live = vec![rule.as_str(), "> hi", rule.as_str(), "status"];
    host.frame_at(&live, (1, 4));
    host.resize(
        24,
        ROWS,
        &[
            "------------------------",
            "> hi",
            "------------------------",
            "st",
        ],
    );

    let screen = host.screen();
    let seen = screen.iter().filter(|r| r.starts_with("---")).count();
    assert_eq!(seen, 2, "{seen} rule rows in:\n{}", screen.join("\n"));
    assert_in_order(&host.history(), &refs(&filler(19)));
}

#[test]
fn a_width_shrink_under_a_popup_that_grew_the_area_keeps_committed_rows() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    // Full-width rules, then `/`: the area triples and the rows the rules held are popup entries
    // half as wide. A row measured at the width it used to have is three screen rows too tall.
    host.frame_at(&refs(&wide_idle(COLS)), (1, 2));
    host.frame_at(&refs(&popup(COLS)), (11, 3));
    let narrow = popup(24);
    host.resize(24, ROWS, &refs(&narrow));

    let mut wanted: Vec<String> = welcome().iter().map(|r| (*r).to_owned()).collect();
    wanted.retain(|r| !r.is_empty());
    wanted.extend(filler(19));
    assert_in_order(&host.history(), &refs(&wanted));
    let screen = host.screen();
    assert_no_duplicate_live(&screen, &refs(&narrow));
    let rules = screen.iter().filter(|r| r.starts_with("---")).count();
    assert_eq!(rules, 2, "{rules} rule rows in:\n{}", screen.join("\n"));
}

#[test]
fn a_width_shrink_too_small_to_wrap_anything_keeps_committed_rows() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.frame_at(&refs(&wide_idle(COLS)), (1, 2));
    host.frame_at(&refs(&popup(COLS)), (11, 3));
    // Four columns narrower: nothing committed is wide enough for the host to split.
    let narrow = popup(36);
    host.resize(36, ROWS, &refs(&narrow));

    let mut wanted: Vec<String> = welcome().iter().map(|r| (*r).to_owned()).collect();
    wanted.retain(|r| !r.is_empty());
    wanted.extend(filler(19));
    assert_in_order(&host.history(), &refs(&wanted));
    assert_no_duplicate_live(&host.screen(), &refs(&narrow));
}

#[test]
fn a_width_change_leaves_the_area_under_the_committed_tail() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.frame_at(&refs(&wide_idle(COLS)), (1, 2));
    let narrow = wide_idle(24);
    host.resize(24, ROWS, &refs(&narrow));

    let screen = host.screen();
    assert_no_hole(&screen, &narrow[0]);
    assert_no_duplicate_live(&screen, &refs(&narrow));
    assert_in_order(&host.history(), &refs(&filler(19)));
}

#[test]
fn a_commit_after_a_width_change_pushes_no_blank_row_into_scrollback() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.frame_at(&refs(&wide_idle(COLS)), (1, 2));
    let narrow = wide_idle(24);
    host.resize(24, ROWS, &refs(&narrow));
    host.commit(
        &["* answer", "  first", "  second", "  third", ""],
        &refs(&narrow),
    );

    let history = host.history();
    assert_in_order(&history, &["row 18", "* answer", "  third"]);
    assert_no_blank_runs(&history, "row 18", "  third");
    assert_no_hole(&host.screen(), &narrow[0]);
}

#[test]
fn two_width_changes_then_a_commit_leave_no_hole() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(19)), &idle());
    host.frame_at(&refs(&wide_idle(COLS)), (1, 2));
    // Two reflows with nothing committed between them: the second may not inherit a hole from
    // the first, on screen or in scrollback.
    let narrow = wide_idle(28);
    host.resize(28, ROWS, &refs(&narrow));
    assert_no_hole(&host.screen(), &narrow[0]);
    let wide = wide_idle(COLS);
    host.resize(COLS, ROWS, &refs(&wide));
    assert_no_hole(&host.screen(), &wide[0]);
    host.commit(&["* answer", "  first", "  second", ""], &refs(&wide));

    let history = host.history();
    assert_in_order(&history, &["row 18", "* answer", "  second"]);
    assert_no_blank_runs(&history, "row 18", "  second");
    assert_no_hole(&host.screen(), &wide[0]);
}

#[test]
fn a_burst_of_height_changes_loses_nothing() {
    let mut host = Host::new(&["$ swamp chat"]);
    host.commit(&welcome(), &idle());
    host.commit(&refs(&filler(10)), &idle());
    host.frame(&board());
    // Grow, shrink, grow, shrink, grow, with no commit to repair anything in between.
    for rows in [30u16, 20, 34, 22, 28] {
        host.resize(COLS, rows, &board());
        assert_no_duplicate_live(&host.screen(), &board());
    }
    host.commit(&board()[..7], &idle());

    let history = host.history();
    assert_in_order(&history, &["* swamp", "row 9", "* workers", "  node 5"]);
    assert_no_blank_runs(&history, "row 9", "  node 5");
    assert_no_hole(&host.screen(), "----");
}

#[test]
fn a_startup_with_the_cursor_mid_screen_leaves_no_blank_band() {
    let prelude: Vec<String> = (0..10).map(|i| format!("$ line {i}")).collect();
    let mut host = Host::new(&refs(&prelude));
    host.commit(&welcome(), &idle());

    let screen = host.screen();
    assert_eq!(screen[9], "$ line 9");
    assert_eq!(screen[10], "* swamp");
    assert_no_hole(&screen, "----");
    assert_no_blank_runs(&host.history(), "$ line 0", "  run 01ARZ3");
}
