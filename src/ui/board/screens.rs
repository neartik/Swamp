//! `docs/BOARD.md` §7: the three layout tiers through a `TestBackend`, all offline,
//! `Theme::plain()` for stable bytes. One fixture, `tests_support::p4_journal`, the one chat
//! renders too: two dispatches, a retry, a blocked task and a rejection.

use crate::dispatch::account::Health;
use crate::dispatch::policy::{Scoring, SelectionPolicy};
use crate::ids::{CallSeq, NodeId, RunId};
use crate::journal::paths::RunPaths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{
    AccountId, LimitScope, LimitStatus, LimitWindow, NodeKind, NodeState, Provider,
    RateLimitSnapshot, Tier, Usage,
};
use crate::model::dispatch::{DispatchRecord, TaskRef};
use crate::ui::board::app::{App, PagerKind};
use crate::ui::board::model::{Board, RunPane, Selection, attention};
use crate::ui::board::render;
use crate::ui::board::sources::Tail;
use crate::ui::chat::tests_support as fx;
use crate::ui::chat::theme::Theme;
use crate::ui::usage::AccountRow;
use camino::Utf8PathBuf;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration as StdDuration;
use unicode_width::UnicodeWidthStr;

const MAX_AGE: StdDuration = StdDuration::from_secs(60);

fn screen(lines: Vec<Line<'static>>, width: u16) -> String {
    let height = (lines.len() as u16).max(1);
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    term.draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
        .expect("draw");
    let buffer = term.backend().buffer();
    let width = (buffer.area.width as usize).max(1);
    let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(width)
        .map(|row| row.iter().collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw(b: &Board, width: u16) -> String {
    screen(
        render::frame(b, width, &Theme::plain(), 0, MAX_AGE, &[]),
        width,
    )
}

/// What the loop draws: `App::lines` in a pane tall enough for the whole board.
fn app_screen(app: &mut App, b: &Board, width: u16, height: u16) -> String {
    let lines = app.lines(
        b,
        Rect::new(0, 0, width, height),
        &Theme::plain(),
        0,
        MAX_AGE,
    );
    screen(lines, width)
}

fn fits(text: &str, width: usize) {
    for line in text.lines() {
        assert!(line.width() <= width, "{width}: {line:?}");
    }
}

// ---------------------------------------------------------------- fixture

fn run_b() -> RunId {
    RunId::from_str("01ARZ3NDEKTSV4RRFFQ69G5FBZ").expect("run id")
}

fn pane(run: RunId, lines: &[JournalLine]) -> RunPane {
    let mut pane = RunPane::new(
        RunPaths {
            run,
            dir: Utf8PathBuf::from(format!("/repo/.swamp/runs/{run}")),
            sock_dir: Utf8PathBuf::from("/home/.swamp/sock"),
        },
        Tail::detached("/repo/.swamp/runs/x/journal.jsonl"),
    );
    pane.apply(lines);
    pane
}

fn window(scope: LimitScope, util: f64, resets_in: i64) -> LimitWindow {
    LimitWindow {
        scope,
        utilization: util,
        resets_at: Some(fx::now() + time::Duration::seconds(resets_in)),
        window_minutes: None,
        measured: true,
    }
}

fn quota(five: f64, five_in: i64, seven: f64, seven_in: i64) -> RateLimitSnapshot {
    RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![
            window(LimitScope::FiveHour, five, five_in),
            window(LimitScope::SevenDay, seven, seven_in),
        ],
        ..RateLimitSnapshot::default()
    }
}

fn account(id: &str, provider: Provider, health: Health, max: Option<usize>) -> AccountRow {
    AccountRow {
        provider: Some(provider),
        account: AccountId(id.to_owned()),
        exec: format!("claude-{id}"),
        in_config: true,
        health,
        inflight: 0,
        max_concurrency: max,
        cooldown_until: None,
        quota: None,
        quota_buckets: BTreeMap::new(),
        quota_observed_at: Some(fx::now() - time::Duration::seconds(2)),
        quota_source: None,
        window_tokens: Usage::default(),
        window_started_at: None,
        lifetime_tokens: Usage::default(),
        lifetime_nodes: 0,
        cost_usd: 0.0,
        cost_basis: None,
    }
}

fn tokens(n: u64) -> Usage {
    Usage {
        input_tokens: n,
        ..Usage::default()
    }
}

/// `main` at its two slots, `alt` past `stop_at` on its 5h window, `codex-main` cooling.
fn accounts() -> Vec<AccountRow> {
    let mut main = account("main", Provider::Anthropic, Health::Healthy, Some(2));
    main.quota = Some(quota(0.71, 38 * 60, 0.32, 4 * 86_400 + 2 * 3600));
    main.window_tokens = tokens(812_000);

    let mut alt = account("alt", Provider::Anthropic, Health::Degraded, None);
    alt.quota = Some(quota(0.93, 12 * 60, 0.54, 2 * 86_400 + 7 * 3600));
    alt.window_tokens = tokens(402_000);

    let mut codex = account("codex-main", Provider::Openai, Health::Cooling, Some(2));
    codex.quota = Some(quota(0.99, 64 * 60, 0.81, 3 * 86_400 + 3600));
    codex.cooldown_until = Some(fx::now() + time::Duration::seconds(64 * 60));

    vec![main, alt, codex]
}

fn scoring() -> Scoring {
    Scoring {
        warn_at: 0.80,
        stop_at: 0.90,
        ..Scoring::default()
    }
}

fn board_of(panes: Vec<RunPane>) -> Board {
    let mut b = Board::new(scoring(), SelectionPolicy::default(), fx::now());
    b.runs = panes;
    b.accounts = accounts();
    b.accounts_at = Some(fx::now() - time::Duration::seconds(4));
    b
}

/// The P4 fixture, the cursor where it lands before anyone touches a key.
fn board() -> Board {
    let mut b = board_of(vec![pane(fx::run_id(), &fx::p4_journal())]);
    b.selected = attention(&b.rows()).expect("something needs attention");
    b
}

fn retry() -> Selection {
    Selection::Node {
        run: fx::run_id(),
        logical: fx::p4_task(2),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

// ---------------------------------------------------------------- tiers

/// The 40-column layout is the contract: a side pane is usually narrow. The same fixture at
/// every width a tier starts, ends or sits in the middle of.
#[test]
fn the_board_at_every_tier() {
    let mut b = board();
    assert_eq!(
        b.selected,
        Selection::Node {
            run: fx::run_id(),
            logical: fx::p4_task(3),
        },
        "the blocked task needs attention first"
    );
    let narrow = draw(&b, 40);
    insta::assert_snapshot!("board_40", narrow);
    insta::assert_snapshot!("board_52", draw(&b, 52));

    b.selected = retry();
    let medium = draw(&b, 60);
    insta::assert_snapshot!("board_60", medium);
    insta::assert_snapshot!("board_85", draw(&b, 85));
    let wide = draw(&b, 100);
    insta::assert_snapshot!("board_100", wide);
    insta::assert_snapshot!("board_140", draw(&b, 140));

    for width in [40u16, 52, 60, 85, 100, 140] {
        let text = draw(&b, width);
        fits(&text, width as usize);
        assert!(text.contains("9g5f09\u{b7}2"), "{width}: the retry id");
        assert!(text.contains("observed 4s ago"), "{width}: freshness");
        assert!(text.contains("2 running"), "{width}: the header count");
        assert!(text.contains("1 stuck"), "{width}");
        assert!(text.contains("#1"), "{width}: the dispatch label");
        assert!(text.contains("max_nodes_per_run"), "{width}: the rejection");
    }
    assert!(
        narrow.contains("until 22:54 \u{b7} main at capacity +1"),
        "{narrow}"
    );
    assert!(medium.contains("until 22:54 \u{b7} main at capacity \u{b7} alt quota stop"));
    assert!(wide.contains("main at capacity (2/2) \u{b7} alt quota stop (93%)"));
    assert!(!narrow.contains("9g5f18"), "the short id waits for Medium");
    assert!(medium.contains("#1 9g5f18"));
    assert!(wide.contains("[high] backfill the users index"));
    assert!(wide.contains("\u{2193} 437k"), "the rollup tokens");
}

/// `layout_for` is the only place a width becomes a decision, and a tier is a whole row.
#[test]
fn the_tiers_start_where_the_table_says() {
    assert_eq!(render::layout_for(0), render::NARROW);
    assert_eq!(render::layout_for(59), render::NARROW);
    assert_eq!(render::layout_for(60), render::MEDIUM);
    assert_eq!(render::layout_for(99), render::MEDIUM);
    assert_eq!(render::layout_for(100), render::WIDE);
    assert_eq!(render::layout_for(u16::MAX), render::WIDE);
    for (l, min) in render::LAYOUTS.iter().zip([0u16, 60, 100]) {
        assert_eq!(l.min, min);
    }
}

/// At a tier's narrowest width a task at its deepest indent still keeps `title_min` columns
/// (below 40 the board is best effort), measured on the row the board draws.
#[test]
fn every_tier_keeps_its_title_minimum() {
    let b = board();
    let theme = Theme::plain();
    for (l, width, want) in [
        (render::NARROW, 40u16, 14usize),
        (render::MEDIUM, 60, 16),
        (render::WIDE, 100, 24),
    ] {
        let mut rows = b.rows();
        let task = &mut rows.runs[0].active[0].tasks[0];
        task.level = l.indent_cap;
        task.row.title = "x".repeat(200);
        let c = render::Ctx::new(&b, width, &theme, 0, MAX_AGE);
        let body = render::body(&rows, &Selection::None, &rows, &c);
        let title = body
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|s| s.content.starts_with("xxx"))
            .map(|s| s.content.width())
            .expect("the title span");
        assert_eq!(title, want, "{:?}", l.band);
        assert!(
            title >= l.title_min,
            "{:?}: {title} < {}",
            l.band,
            l.title_min
        );
    }
}

/// A nested dispatch sits under the task that issued it, two columns deeper, `by` its caller.
#[test]
fn a_nested_dispatch_under_its_task() {
    let mut lines = fx::p4_journal();
    let seq = lines.len() as u64 + 10;
    lines.push(JournalLine {
        seq,
        at: fx::at(138),
        run: fx::run_id(),
        node: Some(fx::nid("01")),
        event: JournalEvent::DispatchIssued {
            record: Box::new(DispatchRecord {
                id: fx::did("1k"),
                run: fx::run_id(),
                caller: fx::nid("01"),
                call_seq: Some(CallSeq(3)),
                wait: true,
                max_wait_s: None,
                tasks: vec![
                    TaskRef {
                        logical: fx::nid("1m"),
                        title: "split the handler".into(),
                        tier: Tier::Low,
                        provider: Provider::Anthropic,
                    },
                    TaskRef {
                        logical: fx::nid("1n"),
                        title: "write the tests".into(),
                        tier: Tier::Low,
                        provider: Provider::Anthropic,
                    },
                ],
                at: fx::at(138),
            }),
        },
    });
    let mut b = board_of(vec![pane(fx::run_id(), &lines)]);
    b.selected = Selection::Dispatch {
        run: fx::run_id(),
        id: fx::did("1k"),
    };
    let text = draw(&b, 60);
    insta::assert_snapshot!("board_nested_60", text);
    assert!(text.contains("#3 9g5f1k \u{b7} by 9g5f01"), "{text}");
    assert!(text.contains("swamp dispatch 9g5f1k"), "{text}");
}

// ---------------------------------------------------------------- runs

/// A schema-2 run and a schema-1 run side by side: the header sums both, the older one is a
/// single legacy bucket.
#[test]
fn two_runs_one_of_them_legacy() {
    let mut b = board_of(vec![
        pane(fx::run_id(), &fx::p4_journal()),
        pane(run_b(), &retag(fx::running(), run_b())),
    ]);
    b.selected = retry();
    let mid = draw(&b, 60);
    insta::assert_snapshot!("two_runs_60", mid);
    assert!(
        mid.contains("4 running"),
        "the header counts both runs: {mid}"
    );
    assert!(draw(&b, 100).contains("2 runs \u{b7} 4 running"));
    assert!(mid.contains("legacy"), "{mid}");
    assert!(
        mid.contains("\u{25c6} 9g5fav") && mid.contains("\u{25c6} 9g5fbz"),
        "{mid}"
    );
}

/// The same journal, as another run: every line and every node id moves to `run`.
fn retag(lines: Vec<JournalLine>, run: RunId) -> Vec<JournalLine> {
    lines
        .into_iter()
        .map(|mut l| {
            l.run = run;
            if let JournalEvent::NodeSpawned { node } = &mut l.event {
                node.run_id = run;
                if node.kind == NodeKind::Brain {
                    node.id = NodeId(run.0);
                    node.logical = NodeId(run.0);
                    l.node = Some(node.id);
                } else {
                    node.parent = Some(NodeId(run.0));
                }
            }
            l
        })
        .collect()
}

/// A brain whose pidfile is gone: the run says so and its spinners freeze.
#[test]
fn a_stale_run_freezes_its_glyphs() {
    let mut b = board();
    let alive = |node: NodeId| node != NodeId(fx::run_id().0);
    for pane in &mut b.runs {
        pane.refresh_liveness(&alive, b.now);
    }
    let mid = draw(&b, 60);
    insta::assert_snapshot!("stale_60", mid);
    assert!(mid.contains("1 stale"), "{mid}");
    assert!(mid.contains("? brain"), "the frozen brain glyph: {mid}");

    b.accounts_at = Some(fx::now() - time::Duration::seconds(7505));
    let head = draw(&b, 60).lines().next().expect("header").to_owned();
    assert!(head.starts_with("swamp board  "), "{head}");
    assert!(head.ends_with(" · observed 2h05m ago"), "{head}");
    assert!(head.width() <= 60, "{head}");
}

/// Ten or more in flight and a cost of $10 or more are never cut.
#[test]
fn wide_counts_are_never_cut() {
    let mut b = board();
    b.accounts[0].max_concurrency = Some(12);
    let rows = b.rows();
    let theme = Theme::plain();
    let c = render::Ctx::new(&b, 100, &theme, 0, MAX_AGE);
    let mut rows_10 = rows.clone();
    rows_10.accounts[0].inflight = 10;
    let strip = screen(render::accounts(&rows_10, &Selection::None, &c), 100);
    assert!(strip.contains("main         10/12"), "{strip}");
    assert!(strip.contains("  1/- "), "{strip}");

    let mut rows_cost = rows;
    let task = &mut rows_cost.runs[0].active[0].tasks[0];
    task.row.spend.usd = 12.34;
    task.row.spend.complete = false;
    let body = screen(
        render::body(&rows_cost, &Selection::None, &rows_cost, &c).lines,
        100,
    );
    assert!(body.contains("~$12.34+"), "{body}");
}

// ---------------------------------------------------------------- detail

/// Why the retry landed on `alt`: the terms as recorded, who was passed over and why, and
/// the attempt before it.
#[test]
fn the_detail_shows_the_terms_as_recorded() {
    let mut b = board();
    b.selected = retry();
    let theme = Theme::plain();
    for width in [40u16, 60, 100] {
        let c = render::Ctx::new(&b, width, &theme, 0, MAX_AGE);
        let rows = b.rows();
        let text = screen(render::detail(&b, &rows, &c), width);
        insta::assert_snapshot!(format!("reason_{width}"), text);
        assert!(text.contains("9g5f09\u{b7}2 \u{2192} alt"), "{text}");
        assert!(text.contains("util .93\u{d7}.50"), "{text}");
        assert!(text.contains("main at capacity (2/2)"), "{text}");
        assert!(text.contains("attempt 1 9g5f08 on main"), "{text}");
    }
}

/// An account `excluded` names that `accounts.json` has never heard of, and one the recorded
/// verdict still covers: the live ones say `now`, the recorded one does not.
#[test]
fn every_exclusion_names_the_gate_that_holds_it_back() {
    let mut lines = fx::p4_journal();
    let seq = lines.len() as u64 + 10;
    lines.push(JournalLine {
        seq,
        at: fx::at(40),
        run: fx::run_id(),
        node: Some(fx::p4_task(3)),
        event: JournalEvent::AccountSelected {
            account: AccountId("alt".into()),
            exec: "claude-alt".into(),
            policy: SelectionPolicy::QuotaAware,
            reason: fx::TERMS.to_owned(),
            excluded: vec![
                AccountId("main".into()),
                AccountId("codex-main".into()),
                AccountId("gone".into()),
            ],
        },
    });
    lines.push(JournalLine {
        seq: seq + 1,
        at: fx::at(41),
        run: fx::run_id(),
        node: Some(fx::p4_task(3)),
        event: JournalEvent::NodeStateChanged {
            from: crate::model::dispatch::Phase::Blocked,
            to: NodeState::Leased {
                account: AccountId("alt".into()),
            },
            why: "leased".into(),
        },
    });
    let mut b = board_of(vec![pane(fx::run_id(), &lines)]);
    b.selected = Selection::Node {
        run: fx::run_id(),
        logical: fx::p4_task(3),
    };
    let theme = Theme::plain();
    let c = render::Ctx::new(&b, 100, &theme, 0, MAX_AGE);
    let text = screen(render::detail(&b, &b.rows(), &c), 100);
    insta::assert_snapshot!("reason_excluded_100", text);
    assert!(
        text.contains("passed over: main at capacity (2/2)"),
        "{text}"
    );
    assert!(
        text.contains("codex-main cooling until 23:20 (now)"),
        "{text}"
    );
    assert!(text.contains("not in accounts.json (now)"), "{text}");
}

/// Journals written before WP5 carry `format!("score {sc:.4}")` and nothing else.
#[test]
fn the_legacy_reason_degrades_to_one_line() {
    let mut lines = fx::p4_journal();
    let seq = lines.len() as u64 + 10;
    lines.push(JournalLine {
        seq,
        at: fx::at(33),
        run: fx::run_id(),
        node: Some(fx::nid("09")),
        event: JournalEvent::AccountSelected {
            account: AccountId("alt".into()),
            exec: "claude-alt".into(),
            policy: SelectionPolicy::QuotaAware,
            reason: "score 0.4100".into(),
            excluded: Vec::new(),
        },
    });
    let mut b = board_of(vec![pane(fx::run_id(), &lines)]);
    b.selected = retry();
    let theme = Theme::plain();
    let c = render::Ctx::new(&b, 100, &theme, 0, MAX_AGE);
    let text = screen(render::detail(&b, &b.rows(), &c), 100);
    insta::assert_snapshot!("reason_legacy_100", text);
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("score"))
            .collect::<Vec<_>>(),
        vec!["  9g5f09\u{b7}2 \u{2192} alt   score 0.4100"],
        "{text}"
    );
}

// ---------------------------------------------------------------- cancel and pagers

/// The prompt replaces the hints, in the pane's own width.
#[test]
fn the_cancel_prompt_replaces_the_hints() {
    let mut b = board();
    let mut app = App::new(true);
    app.touched = true;
    app.on_key(&mut b, key(KeyCode::Char('k')));
    let height = render::frame(&b, 40, &Theme::plain(), 0, MAX_AGE, &[]).len() as u16;
    let text = app_screen(&mut app, &b, 40, height);
    assert_eq!(text, {
        let mut frame = draw(&b, 40);
        let at = frame.rfind('\n').expect("a hint line");
        frame.truncate(at + 1);
        frame + "cancel 9g5f04 \"rebuild the index\"? y / n"
    });
    insta::assert_snapshot!("board_confirm_40", text);
    let last = text.lines().last().expect("a bottom line");
    assert_eq!(last, "cancel 9g5f04 \"rebuild the index\"? y / n");
}

/// Trace, dispatch and raw pagers each say what they are, and their hints differ.
#[test]
fn every_pager_names_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
    let lines = fx::p4_journal();
    let body: String = lines
        .iter()
        .take(3)
        .map(|l| format!("{}\n", serde_json::to_string(l).expect("line")))
        .collect();
    std::fs::write(root.join("journal.jsonl"), body).expect("journal");
    let mut run = pane(fx::run_id(), &lines);
    run.paths.dir = root;
    let mut b = board_of(vec![run]);

    let mut app = App::new(true);
    app.touched = true;
    b.selected = retry();
    app.on_key(&mut b, key(KeyCode::Enter));
    assert_eq!(app.overlay.as_ref().map(|p| p.kind), Some(PagerKind::Trace));
    let trace = app_screen(&mut app, &b, 60, 16);
    insta::assert_snapshot!("pager_trace_60", trace);
    assert!(trace.starts_with("trace 9g5f09\u{b7}2 \u{b7} #1 9g5f18 \u{b7} run 9g5fav"));
    assert!(trace.ends_with(
        "esc back \u{b7} \u{2191}\u{2193} scroll \u{b7} r raw \u{b7} g G ends \u{b7} q quit"
    ));

    app.on_key(&mut b, key(KeyCode::Esc));
    b.selected = Selection::Dispatch {
        run: fx::run_id(),
        id: fx::did("18"),
    };
    app.on_key(&mut b, key(KeyCode::Enter));
    assert_eq!(
        app.overlay.as_ref().map(|p| p.kind),
        Some(PagerKind::Dispatch)
    );
    let dispatch = app_screen(&mut app, &b, 60, 16);
    insta::assert_snapshot!("pager_dispatch_60", dispatch);
    assert!(dispatch.starts_with("dispatch #1 9g5f18 \u{b7} run 9g5fav"));

    let action = app.on_key(&mut b, key(KeyCode::Char('r')));
    let crate::ui::board::app::Action::LoadRaw(run) = action else {
        panic!("r in a dispatch pager asks for the raw journal: {action:?}");
    };
    app.load_raw(&b, run);
    let raw = app_screen(&mut app, &b, 60, 10);
    insta::assert_snapshot!("pager_raw_60", raw);
    assert!(raw.starts_with(" raw journal  run 9g5fav \u{b7} last 200 lines"));
    assert!(
        !raw.contains("r raw"),
        "the raw pager has no raw key: {raw}"
    );
}

// ---------------------------------------------------------------- safety

/// A title is worker output: an escape sequence in it must not repaint the pane.
#[test]
fn control_characters_never_reach_the_pane() {
    let evil = "\u{1b}[2Jswamp: run succeeded\u{7}";
    let lines: Vec<JournalLine> = fx::p4_journal()
        .into_iter()
        .map(|mut l| {
            if let JournalEvent::NodeSpawned { node } = &mut l.event
                && node.id == fx::nid("01")
            {
                node.title = evil.to_owned();
            }
            l
        })
        .collect();
    let mut b = board_of(vec![pane(fx::run_id(), &lines)]);
    b.selected = retry();
    for width in [40u16, 60, 100] {
        let text = draw(&b, width);
        assert!(!text.contains('\u{1b}'), "{width}: {text:?}");
        assert!(!text.contains('\u{7}'), "{width}: {text:?}");
        assert!(text.contains("swamp: run"), "{width}: {text}");
        fits(&text, width as usize);
    }
    insta::assert_snapshot!("control_characters_60", draw(&b, 60));
}

/// A pane the user dragged to nothing, and a board with nothing in it yet, still draw.
#[test]
fn every_width_draws_inside_its_pane() {
    let full = board();
    let empty = Board::new(Scoring::default(), SelectionPolicy::default(), fx::now());
    for b in [&full, &empty] {
        for width in 0u16..=160 {
            fits(&draw(b, width), width as usize);
        }
    }
}

/// The board's bar is `watch::gauge_bar` with the board's own glyphs: same rounding, so a
/// percentage can never read one cell apart between `swamp watch` and `swamp board`.
#[test]
fn the_bar_fills_exactly_like_watch() {
    let theme = Theme::plain();
    for pct in 0..=100 {
        let util = pct as f64 / 100.0;
        let ours = render::gauge(util, 10, &theme);
        let theirs = crate::ui::watch::gauge_bar(util);
        assert_eq!(
            ours.chars().filter(|c| *c == '\u{2587}').count(),
            theirs.chars().filter(|c| *c == '#').count(),
            "{pct}%: {ours} vs {theirs}"
        );
    }
    assert_eq!(
        render::gauge(
            0.5,
            5,
            &Theme {
                ascii: true,
                ..Theme::plain()
            }
        ),
        "###--"
    );
}

/// Every glyph the board invents on top of the theme table is one column wide, or a bar and
/// the cell beside it disagree about where they end.
#[test]
fn every_glyph_is_one_column() {
    for g in [
        "\u{2587}", "\u{2591}", "\u{21bb}", "\u{25c6}", "\u{25d0}", "\u{2192}", "\u{25be}",
        "\u{25b8}", "\u{23f8}", "\u{2297}", "\u{258c}",
    ] {
        assert_eq!(g.width(), 1, "{g:?} is not one column");
    }
}

/// The P4 journal with `n` reads by the brain before its first dispatch.
fn with_brain_reads(n: usize) -> Vec<JournalLine> {
    use crate::model::event::WorkerEvent;
    let mut lines = fx::p4_journal();
    let first = lines
        .iter()
        .position(|l| matches!(l.event, JournalEvent::DispatchIssued { .. }))
        .expect("the fixture dispatches");
    let template = lines[first].clone();
    let reads = (0..n).map(|i| JournalLine {
        node: Some(fx::id(0)),
        event: JournalEvent::NodeEvent {
            offset: 0,
            event: WorkerEvent::ToolCall {
                id: format!("toolu_{i}"),
                name: "Read".into(),
                summary: String::new(),
            },
        },
        ..template.clone()
    });
    lines.splice(first..first, reads.collect::<Vec<_>>());
    for (i, l) in lines.iter_mut().enumerate() {
        l.seq = i as u64;
    }
    lines
}

/// Within the budget the delegation cell is the first header cell to go; past it, it outlasts
/// the queued count and the cost. Only the 60-column row, where freshness and the stuck count
/// leave no room, goes without it.
#[test]
fn the_delegation_cell_warns_once_the_budget_is_spent() {
    let mut b = board_of(vec![pane(fx::run_id(), &with_brain_reads(11))]);
    let header = |b: &Board, width: u16| {
        draw(b, width)
            .lines()
            .take(2)
            .collect::<Vec<_>>()
            .join("\n")
    };
    for width in [40u16, 52, 60, 85, 100, 140] {
        fits(&draw(&b, width), width as usize);
        assert!(header(&b, width).contains("1 stuck"), "{width}");
        let warned = header(&b, width).contains("brain 11/8 over");
        assert_eq!(warned, width != 60, "{width}: {}", header(&b, width));
    }
    assert!(header(&b, 140).contains("brain 11/8 over (21%)"));
    assert!(!header(&b, 40).contains("~$"), "{}", header(&b, 40));

    b.read_budget = 16;
    assert!(!header(&b, 60).contains("brain"), "{}", header(&b, 60));
    assert!(
        header(&b, 140).contains("brain 11/16 ("),
        "{}",
        header(&b, 140)
    );
}

/// An account that never reported a window has one `-` per window, not an empty bar and a
/// reset countdown to nothing.
#[test]
fn an_account_without_quota_leaves_its_bars_blank() {
    let mut b = board();
    b.accounts = vec![account(
        "fresh",
        Provider::Anthropic,
        Health::Healthy,
        Some(2),
    )];
    for width in [100u16, 140] {
        let text = draw(&b, width);
        fits(&text, width as usize);
        let line = text
            .lines()
            .find(|l| l.contains("fresh"))
            .expect("the account line");
        assert!(!line.contains('\u{21bb}'), "{width}: {line}");
        assert_eq!(line.matches(" - ").count(), 2, "{width}: {line}");
    }
}
