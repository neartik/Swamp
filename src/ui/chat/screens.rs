//! Every screen state in `docs/UI.md` §3, drawn through a `TestBackend` and snapshotted.

use crate::brain::BrainEvent;
use crate::model::core::{Cost, CostBasis, Usage};
use crate::ui::chat::app::{App, Effect, Msg};
use crate::ui::chat::tests_support as fx;
use crate::ui::chat::theme::{Palette, Theme};
use crate::ui::chat::{render, workers};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{TerminalOptions, Viewport};

fn screen(lines: Vec<Line<'static>>, width: u16) -> String {
    let height = (lines.len() as u16).max(1);
    let mut term = Terminal::with_options(
        TestBackend::new(width, height),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .expect("terminal");
    term.draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
        .expect("draw");
    let buffer = term.backend().buffer();
    let width = buffer.area.width as usize;
    let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|row| row.iter().collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn live(app: &mut App, width: u16) -> String {
    let previous = app.width;
    app.set_width(width);
    let out = screen(render::live(app), width);
    app.set_width(previous);
    out
}

/// Everything the reducer committed, as one block of text.
fn committed(effects: Vec<Effect>, width: u16) -> String {
    let mut lines = Vec::new();
    for effect in effects {
        if let Effect::Commit(body) = effect {
            lines.extend(body);
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    screen(lines, width)
}

fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.reduce(key(KeyCode::Char(c)));
    }
}

fn text(delta: &str) -> Msg {
    Msg::Brain(BrainEvent::Text {
        delta: delta.to_owned(),
    })
}

fn turn_done() -> Msg {
    Msg::Brain(BrainEvent::TurnDone {
        usage: Usage {
            input_tokens: 1_200,
            output_tokens: 340,
            ..Usage::default()
        },
        cost: Some(Cost {
            usd: 0.06,
            basis: CostBasis::Reported,
        }),
    })
}

const PROSE: &str = "I'll read the current handler first, then dispatch two workers.\n\n\
The endpoint lives in `api/users.rs` and returns the full table today. Two independent \
pieces of work fall out of that:\n\n\
1. the handler change, cursor based, no schema change\n\
2. the index backfill, which must not run in the same worktree\n\n\
Reading the handler now";

// ---------------------------------------------------------------- 3.1

#[test]
fn welcome_and_idle() {
    let mut app = fx::app(100);
    let welcome_100 = screen(app.take_welcome(), 100);
    insta::assert_snapshot!("welcome_100", welcome_100);
    insta::assert_snapshot!("idle_100", live(&mut app, 100));
    insta::assert_snapshot!("idle_62", live(&mut app, 62));

    let mut app62 = fx::app(62);
    let welcome_62 = screen(app62.take_welcome(), 62);
    insta::assert_snapshot!("welcome_62", welcome_62);

    // WP-A acceptance: the welcome box no longer names a global cap or a budget, at any width.
    for text in [&welcome_100, &welcome_62] {
        assert!(!text.contains("parallel"), "{text}");
        assert!(!text.contains("budget"), "{text}");
    }
}

// ---------------------------------------------------------------- 3.2

#[test]
fn brain_streaming_text() {
    let mut app = fx::app(100);
    app.take_welcome();
    typed(&mut app, "split the pagination work across two workers");
    let bar = app.reduce(key(KeyCode::Enter));
    insta::assert_snapshot!("user_bar_100", committed(bar, 100));
    let scrollback = app.reduce(text(PROSE));
    insta::assert_snapshot!("assistant_committed_100", committed(scrollback, 100));
    insta::assert_snapshot!("streaming_live_100", live(&mut app, 100));
    insta::assert_snapshot!("streaming_live_62", live(&mut app, 62));
}

// ---------------------------------------------------------------- 3.3

#[test]
fn tool_call_running_then_done() {
    let mut app = fx::app(100);
    app.take_welcome();
    app.reduce(Msg::Brain(BrainEvent::ToolCall {
        id: "t1".into(),
        name: "Read".into(),
        preview: "src/api/users.rs".into(),
    }));
    insta::assert_snapshot!("tool_running_100", live(&mut app, 100));

    let done = app.reduce(Msg::Brain(BrainEvent::ToolDone {
        id: "t1".into(),
        name: "Read".into(),
        ok: true,
        detail: Some(
            "running 3 tests\ntest parse_codex::final_event ... ok\n\
             test parse_codex::rate_limit ... ok\n"
                .to_owned()
                + &(0..14)
                    .map(|i| format!("test extra_{i} ... ok\n"))
                    .collect::<String>(),
        ),
    }));
    insta::assert_snapshot!("tool_done_collapsed_100", committed(done, 100));

    app.reduce(Msg::Brain(BrainEvent::ToolCall {
        id: "t2".into(),
        name: "Bash".into(),
        preview: "cargo build --release".into(),
    }));
    let failed = app.reduce(Msg::Brain(BrainEvent::ToolDone {
        id: "t2".into(),
        name: "Bash".into(),
        ok: false,
        detail: Some(
            "error[E0308]: mismatched types\n  --> src/worker/codex.rs:212:17\n\
             error: could not compile `swamp` (lib) due to 2 previous errors\n\
             note: one\nnote: two\n"
                .to_owned(),
        ),
    }));
    insta::assert_snapshot!("tool_failed_100", committed(failed, 100));
}

// ---------------------------------------------------------------- 3.4 and 3.5

fn dispatched(width: u16) -> App {
    let mut app = fx::app(width);
    app.take_welcome();
    app.reduce(Msg::Brain(BrainEvent::ToolCall {
        id: "d1".into(),
        name: "mcp__swamp__swamp_dispatch".into(),
        preview: r#"{"tasks":[{"title":"a"},{"title":"b"}]}"#.into(),
    }));
    app.reduce(Msg::Journal(fx::running()));
    app
}

#[test]
fn a_live_worker_board() {
    let mut app = dispatched(100);
    insta::assert_snapshot!("board_live_100", live(&mut app, 100));
    insta::assert_snapshot!("board_live_62", live(&mut app, 62));
}

#[test]
fn a_finished_board_commits_with_its_detail_lines() {
    let mut app = dispatched(100);
    app.reduce(Msg::Brain(BrainEvent::ToolDone {
        id: "d1".into(),
        name: "swamp_dispatch".into(),
        ok: true,
        detail: None,
    }));
    let committed_lines = app.reduce(Msg::Journal(fx::fixture()));
    insta::assert_snapshot!("board_committed_100", committed(committed_lines, 100));
    assert!(
        app.blocks.is_empty(),
        "a settled board leaves the live area"
    );
}

// ---------------------------------------------------------------- 3.6

#[test]
fn errors_and_interrupts() {
    let mut app = fx::app(100);
    app.take_welcome();
    typed(&mut app, "go");
    app.reduce(key(KeyCode::Enter));
    let fatal = app.reduce(Msg::Brain(BrainEvent::Fatal {
        message: "rate_limited (five_hour, telemetry) resets 14:20".into(),
    }));
    insta::assert_snapshot!("brain_failed_100", committed(fatal, 100));

    let mut app = dispatched(100);
    typed(&mut app, "go");
    app.reduce(key(KeyCode::Enter));
    let interrupt = app.reduce(key(KeyCode::Esc));
    insta::assert_snapshot!("interrupt_notice_100", committed(interrupt, 100));
    let cancelled = app.note_cancelled(2);
    insta::assert_snapshot!("cancelled_notice_100", committed(cancelled, 100));
}

/// USAGE 4.7: a blocked node is a wait, not a failure, and the wait is visible.
#[test]
fn every_account_at_its_limit_commits_a_notice() {
    let mut app = dispatched(100);
    let blocked = app.reduce(Msg::Journal(fx::blocked()));
    insta::assert_snapshot!("blocked_notice_100", committed(blocked, 100));
    assert!(
        app.blocks
            .iter()
            .all(|b| !matches!(b, crate::ui::chat::blocks::Block::Notice { .. })),
        "the notice commits to scrollback, it does not hold the live area"
    );
}

// ---------------------------------------------------------------- 3.7 and 3.8

#[test]
fn the_slash_popup_and_the_shortcut_overlay() {
    let mut app = fx::app(100);
    app.take_welcome();
    typed(&mut app, "/t");
    insta::assert_snapshot!("popup_100", live(&mut app, 100));

    let mut app = fx::app(100);
    app.take_welcome();
    app.reduce(key(KeyCode::Char('?')));
    insta::assert_snapshot!("overlay_100", live(&mut app, 100));
}

/// The centre segment was laid out from byte lengths while the right block used char
/// counts, so the multi-byte popup hint pushed `dispatch mid` several columns off centre.
#[test]
fn the_status_centre_is_placed_by_display_width() {
    let centre = "\u{23f5}\u{23f5} dispatch mid";
    for hint in [false, true] {
        let mut app = fx::app(100);
        app.take_welcome();
        if hint {
            typed(&mut app, "/t");
            assert!(app.popup.is_some(), "the hint needs the popup open");
        }
        let line = screen(vec![app.status_line()], 100);
        let cols = unicode_width::UnicodeWidthStr::width;
        let i = line
            .find(centre)
            .unwrap_or_else(|| panic!("no centre segment in {line}"));
        let head = &line[..i];
        let tail = &line[i + centre.len()..];
        let left_gap = cols(head) - cols(head.trim_end());
        let right_gap = cols(tail) - cols(tail.trim_start());
        assert!(
            left_gap.abs_diff(right_gap) <= 2,
            "centre sits {left_gap} from the left and {right_gap} from the right: {line}"
        );
    }
}

#[test]
fn the_welcome_box_carries_the_run_id_so_chat_drops_the_one_shot_header() {
    let mut app = fx::app(100);
    let welcome = screen(app.take_welcome(), 100);
    assert!(welcome.contains("run: 9g5fav"), "{welcome}");
}

#[test]
fn enter_runs_a_command_that_is_typed_out_and_completes_a_partial_one() {
    let mut app = fx::app(100);
    app.take_welcome();
    typed(&mut app, "/status");
    let out = app.reduce(key(KeyCode::Enter));
    assert!(
        app.editor.is_empty(),
        "the command ran, it did not complete"
    );
    assert!(app.popup.is_none());
    assert!(!committed(out, 100).is_empty(), "the run tree committed");

    let mut app = fx::app(100);
    app.take_welcome();
    typed(&mut app, "/t");
    assert!(
        app.reduce(key(KeyCode::Enter)).is_empty(),
        "three candidates"
    );
    assert_eq!(app.editor.text(), "/trace");
    assert!(app.popup.is_none());
}

#[test]
fn slash_help_and_status_commit_like_model_output() {
    let mut app = fx::app(100);
    app.take_welcome();
    typed(&mut app, "/help");
    let help = app.reduce(key(KeyCode::Enter));
    insta::assert_snapshot!("slash_help_100", committed(help, 100));

    app.reduce(Msg::Journal(fx::fixture()));
    typed(&mut app, "/status");
    let status = app.reduce(key(KeyCode::Enter));
    insta::assert_snapshot!("slash_status_100", committed(status, 100));
}

// ---------------------------------------------------------------- colour

#[test]
fn the_true_colour_theme_paints_the_documented_rgb() {
    use ratatui::style::Color;
    let mut app = fx::app(100);
    app.theme = Theme {
        palette: Palette::TrueColor,
        ascii: false,
    };
    app.take_welcome();
    let lines = app.reduce(text("hello\n"));
    let Some(Effect::Commit(body)) = lines.into_iter().next() else {
        panic!("the first assistant line commits");
    };
    assert_eq!(body[0].spans[0].content, "● ");
    assert_eq!(body[0].spans[0].style.fg, Some(Color::Rgb(215, 119, 87)));

    let mut app = dispatched(100);
    app.reduce(Msg::Journal(fx::fixture()));
    app.theme = Theme {
        palette: Palette::TrueColor,
        ascii: false,
    };
    let rendered = render::live(&app);
    let ok = rendered
        .iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| s.content.as_ref() == "✔")
        .expect("a finished worker glyph");
    assert_eq!(ok.style.fg, Some(Color::Rgb(87, 170, 120)));
}

// ---------------------------------------------------------------- safety

#[test]
fn a_forged_escape_sequence_never_reaches_the_viewport() {
    let evil = "\u{1b}[2J\u{1b}[Hswamp: run succeeded";
    let mut app = fx::app(100);
    app.take_welcome();
    let out = committed(app.reduce(text(&format!("{evil}\n"))), 100);
    assert!(!out.contains('\u{1b}'), "{out}");
    assert!(out.contains("swamp: run succeeded"));

    let done = committed(
        app.reduce(Msg::Brain(BrainEvent::ToolDone {
            id: "t9".into(),
            name: "Bash".into(),
            ok: true,
            detail: Some(evil.to_owned()),
        })),
        100,
    );
    assert!(!done.contains('\u{1b}'), "{done}");

    let mut lines = fx::running();
    if let crate::journal::record::JournalEvent::NodeSpawned { node } = &mut lines[2].event {
        node.title = format!("{evil} title");
    }
    let mut app = dispatched(100);
    app.reduce(Msg::Journal(lines));
    let board = live(&mut app, 100);
    assert!(!board.contains('\u{1b}'), "{board}");
}

// ---------------------------------------------------------------- reducer

#[test]
fn ctrl_c_clears_once_and_quits_twice() {
    let mut app = fx::app(100);
    typed(&mut app, "half a thought");
    assert!(app.reduce(ctrl('c')).is_empty());
    assert!(app.editor.is_empty(), "the first ctrl+c clears the buffer");
    assert!(app.reduce(ctrl('c')).is_empty(), "the second arms the quit");
    let quit = app.reduce(ctrl('c'));
    assert!(
        quit.iter().any(|e| matches!(e, Effect::Quit(0))),
        "the third leaves"
    );
}

#[test]
fn esc_interrupts_the_turn_and_esc_esc_cancels_the_workers() {
    let mut app = dispatched(100);
    typed(&mut app, "go");
    app.reduce(key(KeyCode::Enter));
    let first = app.reduce(key(KeyCode::Esc));
    assert!(first.iter().any(|e| matches!(e, Effect::Interrupt)));
    let second = app.reduce(key(KeyCode::Esc));
    assert!(second.iter().any(|e| matches!(e, Effect::CancelAll)));
}

#[test]
fn a_submit_during_a_turn_is_queued_and_flushed_on_turn_done() {
    let mut app = fx::app(100);
    typed(&mut app, "first");
    let sent = app.reduce(key(KeyCode::Enter));
    assert!(
        sent.iter()
            .any(|e| matches!(e, Effect::Send(t) if t == "first"))
    );
    typed(&mut app, "second");
    let queued = app.reduce(key(KeyCode::Enter));
    assert!(!queued.iter().any(|e| matches!(e, Effect::Send(_))));
    assert_eq!(app.pending_send.as_deref(), Some("second"));
    let flushed = app.reduce(turn_done());
    assert!(
        flushed
            .iter()
            .any(|e| matches!(e, Effect::Send(t) if t == "second"))
    );
}

#[test]
fn tab_completes_and_the_history_round_trips() {
    let mut app = fx::app(100);
    typed(&mut app, "/tr");
    app.reduce(key(KeyCode::Tab));
    assert_eq!(app.editor.text(), "/trace", "one candidate, typed in full");

    let mut app = fx::app(100);
    typed(&mut app, "/t");
    app.reduce(key(KeyCode::Tab));
    assert_eq!(
        app.editor.text(),
        "/trace",
        "/tier, /trace and /thinking share only what is typed, so tab takes the selection"
    );

    let mut app = fx::app(100);
    typed(&mut app, "first");
    app.reduce(key(KeyCode::Enter));
    typed(&mut app, "draft");
    app.reduce(key(KeyCode::Up));
    assert_eq!(app.editor.text(), "first");
    app.reduce(key(KeyCode::Down));
    assert_eq!(app.editor.text(), "draft", "the stash comes back");
}

#[test]
fn alt_enter_makes_a_second_line_and_a_trailing_backslash_does_too() {
    let mut app = fx::app(100);
    typed(&mut app, "one");
    app.reduce(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
    typed(&mut app, "two");
    assert_eq!(app.editor.text(), "one\ntwo");
    assert!(app.reduce(key(KeyCode::Enter)).len() >= 2, "now it submits");

    let mut app = fx::app(100);
    typed(&mut app, "one\\");
    assert!(app.reduce(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.editor.text(), "one\n");
}

#[test]
fn two_dispatches_never_steal_each_others_nodes() {
    let mut app = dispatched(100);
    app.reduce(Msg::Brain(BrainEvent::ToolDone {
        id: "d1".into(),
        name: "swamp_dispatch".into(),
        ok: true,
        detail: None,
    }));
    app.reduce(Msg::Brain(BrainEvent::ToolCall {
        id: "d2".into(),
        name: "swamp_dispatch".into(),
        preview: r#"{"tasks":[{"title":"c"}]}"#.into(),
    }));
    // The first batch is closed, so a node that appears now belongs to the second.
    app.reduce(Msg::Journal(fx::third()));
    let batches: Vec<usize> = app
        .blocks
        .iter()
        .filter_map(|b| match b {
            crate::ui::chat::blocks::Block::Dispatch(batch) => Some(batch.owned.len()),
            _ => None,
        })
        .collect();
    assert_eq!(batches, vec![2, 1], "{batches:?}");
    assert_eq!(workers::MAX_ROWS, 8);
}
