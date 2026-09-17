//! `docs/BOARD.md` §7.1-7.4 and §7.6: the three layouts through a `TestBackend`, all offline,
//! `Theme::plain()` for stable bytes.

use crate::dispatch::account::Health;
use crate::dispatch::policy::{Scoring, SelectionPolicy};
use crate::ids::{NodeId, RunId};
use crate::journal::paths::RunPaths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{
    AccountId, Cost, CostBasis, LimitScope, LimitStatus, LimitWindow, NodeKind, NodeState,
    Provider, RateLimitSnapshot, Tier, Usage, WorkspaceRef,
};
use crate::model::failure::Failure;
use crate::model::node::NodeRecord;
use crate::ui::board::model::{Board, RunPane, Selection};
use crate::ui::board::render;
use crate::ui::board::sources::Tail;
use crate::ui::chat::tests_support as fx;
use crate::ui::chat::theme::Theme;
use crate::ui::usage::AccountRow;
use camino::Utf8PathBuf;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::Duration as StdDuration;

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
    screen(render::frame(b, width, &Theme::plain(), 0, MAX_AGE), width)
}

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

fn usage(billable: u64) -> Usage {
    Usage {
        input_tokens: billable,
        ..Usage::default()
    }
}

fn cost(usd: f64) -> Option<Cost> {
    Some(Cost {
        usd,
        basis: CostBasis::Reported,
    })
}

struct Spawn {
    id: NodeId,
    run: RunId,
    parent: Option<NodeId>,
    logical: NodeId,
    attempt: u32,
    kind: NodeKind,
    title: String,
    account: Option<&'static str>,
    tier: Tier,
    model: Option<&'static str>,
    state: NodeState,
    started: Option<i64>,
    ended: Option<i64>,
    tokens: u64,
    usd: Option<f64>,
}

impl Spawn {
    fn new(run: RunId, id: NodeId, title: &str, state: NodeState) -> Spawn {
        Spawn {
            id,
            run,
            parent: Some(NodeId(run.0)),
            logical: id,
            attempt: 1,
            kind: NodeKind::Worker,
            title: title.to_owned(),
            account: Some("main"),
            tier: Tier::Mid,
            model: Some("claude-sonnet-4-5-20250929"),
            state,
            started: Some(10),
            ended: None,
            tokens: 118_000,
            usd: Some(0.08),
        }
    }

    fn line(self, seq: u64) -> JournalLine {
        let node = NodeRecord {
            id: self.id,
            run_id: self.run,
            parent: self.parent,
            logical: self.logical,
            attempt: self.attempt,
            retry_of: None,
            kind: self.kind,
            title: self.title,
            prompt_path: Utf8PathBuf::from("prompt.md"),
            prompt_sha256: String::new(),
            provider: Provider::Anthropic,
            account: self.account.map(|a| AccountId(a.to_owned())),
            exec: self.account.map(|a| format!("claude-{a}")),
            argv: Vec::new(),
            model: self.model.map(|m| m.to_owned()),
            tier: self.tier,
            workspace: WorkspaceRef::ReadOnly {
                path: Utf8PathBuf::from("/repo"),
            },
            session: None,
            state: self.state,
            created_at: fx::at(2),
            started_at: self.started.map(fx::at),
            ended_at: self.ended.map(fx::at),
            usage: usage(self.tokens),
            cost: self.usd.and_then(cost),
            exit: None,
            files: Vec::new(),
            work: None,
            summary: None,
            stream_offset: 0,
            unparsed_lines: 0,
        };
        JournalLine {
            seq,
            at: fx::at(seq as i64),
            run: self.run,
            node: Some(self.id),
            event: JournalEvent::NodeSpawned {
                node: Box::new(node),
            },
        }
    }
}

fn running(since: i64) -> NodeState {
    NodeState::Running {
        pid: 10,
        pgid: 10,
        since: fx::at(since),
    }
}

/// `brain::build` records the title "brain"; the board is what turns it into a verb.
fn brain(run: RunId) -> Spawn {
    let mut s = Spawn::new(run, NodeId(run.0), "brain", running(10));
    s.parent = None;
    s.kind = NodeKind::Brain;
    s.tier = Tier::High;
    s.model = Some("claude-opus-4-1-20250805");
    s.tokens = 214_000;
    s.usd = Some(0.09);
    s
}

/// Run A: a brain and three workers still going, one of them on its second attempt.
fn journal_a() -> Vec<JournalLine> {
    let run = fx::run_id();
    let mut retry = Spawn::new(run, fx::id(9), "backfill the users index", running(32));
    retry.started = Some(32);
    retry.logical = fx::id(3);
    retry.attempt = 2;
    retry.account = Some("alt");
    retry.tier = Tier::High;
    retry.model = Some("claude-opus-4-1-20250805");
    retry.tokens = 223_000;
    retry.usd = Some(0.21);

    let mut second = Spawn::new(run, fx::id(2), "rename the fixtures", running(138));
    second.started = Some(138);
    second.tier = Tier::Low;
    second.model = Some("claude-haiku-4-5-20251001");
    second.tokens = 61_000;
    second.usd = Some(0.01);

    let mut first = Spawn::new(
        run,
        fx::id(3),
        "backfill the users index",
        NodeState::Queued,
    );
    first.logical = fx::id(3);
    first.account = Some("alt");

    vec![
        brain(run).line(1),
        Spawn::new(run, fx::id(1), "add pagination to /users", running(10)).line(2),
        second.line(3),
        first.line(4),
        retry.line(5),
        JournalLine {
            seq: 6,
            at: fx::at(6),
            run,
            node: Some(fx::id(9)),
            event: JournalEvent::AccountSelected {
                account: AccountId("alt".into()),
                exec: "claude-alt".into(),
                policy: SelectionPolicy::QuotaAware,
                reason: TERMS.to_owned(),
                excluded: vec![AccountId("main".into())],
            },
        },
    ]
}

const TERMS: &str = "score .41 = util .93\u{d7}.50 + load .33\u{d7}.30 + share .12\u{d7}.15 \
                     \u{2212} weight .00 \u{2212} idle .02";

/// Run B: every anthropic account at its limit, and the three nodes that already landed.
fn journal_b() -> Vec<JournalLine> {
    let run = run_b();
    let mut blocked = Spawn::new(run, fx::id(4), "rebuild the index", NodeState::Queued);
    blocked.account = None;
    blocked.model = None;
    blocked.started = None;
    blocked.tokens = 0;
    blocked.usd = None;

    let mut ok = Spawn::new(run, fx::id(5), "add the /users route", NodeState::Succeeded);
    ok.tier = Tier::Low;
    ok.ended = Some(140);
    ok.tokens = 96_000;
    ok.usd = Some(0.04);

    let mut failed = Spawn::new(
        run,
        fx::id(6),
        "seed script",
        NodeState::Failed {
            failure: Failure::PermissionDenied {
                denials: 2,
                tools: vec!["Bash".into()],
            },
        },
    );
    failed.ended = Some(136);
    failed.tokens = 12_000;
    failed.usd = Some(0.01);

    let mut cancelled = Spawn::new(
        run,
        fx::id(7),
        "probe the migration",
        NodeState::Cancelled {
            by: crate::model::core::CancelSource::User,
        },
    );
    cancelled.account = Some("alt");
    cancelled.tier = Tier::Low;
    cancelled.model = Some("claude-haiku-4-5-20251001");
    cancelled.ended = Some(190);
    cancelled.tokens = 2_000;
    cancelled.usd = Some(0.0);

    vec![
        brain(run).line(1),
        blocked.line(2),
        JournalLine {
            seq: 3,
            at: fx::at(3),
            run,
            node: Some(fx::id(4)),
            event: JournalEvent::NodeBlocked {
                until: fx::at(200 + 38 * 60),
                why: "main cooling until 14:45".into(),
            },
        },
        ok.line(4),
        failed.line(5),
        cancelled.line(6),
    ]
}

fn pane(run: RunId, lines: &[JournalLine]) -> RunPane {
    let mut pane = RunPane::new(
        paths(run),
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

fn accounts() -> Vec<AccountRow> {
    let mut main = account("main", Provider::Anthropic, Health::Healthy, Some(4));
    main.quota = Some(quota(0.71, 38 * 60, 0.32, 4 * 86_400 + 2 * 3600));
    main.window_tokens = usage(812_000);

    let mut alt = account("alt", Provider::Anthropic, Health::Degraded, None);
    alt.quota = Some(quota(0.93, 12 * 60, 0.54, 2 * 86_400 + 7 * 3600));
    alt.window_tokens = usage(402_000);

    let mut codex = account("codex-main", Provider::Openai, Health::Cooling, Some(2));
    codex.quota = Some(quota(0.99, 64 * 60, 0.81, 3 * 86_400 + 3600));
    codex.cooldown_until = Some(fx::now() + time::Duration::seconds(64 * 60));

    vec![main, alt, codex]
}

fn board() -> Board {
    let mut b = Board::new(Scoring::default(), SelectionPolicy::default(), fx::now());
    b.runs = vec![
        pane(fx::run_id(), &journal_a()),
        pane(run_b(), &journal_b()),
    ];
    b.accounts = accounts();
    b.accounts_at = Some(fx::now() - time::Duration::milliseconds(400));
    b.selected = Selection::Node {
        run: fx::run_id(),
        logical: fx::id(3),
    };
    b
}

// ---------------------------------------------------------------- §7.1

/// The 40-column layout is the contract: a side pane is usually narrow.
#[test]
fn the_three_widths() {
    let b = board();
    let narrow = draw(&b, 40);
    let mid = draw(&b, 60);
    let wide = draw(&b, 100);
    insta::assert_snapshot!("board_40", narrow);
    insta::assert_snapshot!("board_60", mid);
    insta::assert_snapshot!("board_100", wide);

    for (text, width) in [(&narrow, 40usize), (&mid, 60), (&wide, 100)] {
        for line in text.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= width,
                "{width}: {line:?}"
            );
        }
        // Glyph, id and title never drop, at any width.
        assert!(text.contains("9g5f09\u{b7}2"), "{width}: the retry id");
        assert!(
            text.contains("backfill the users index"),
            "{width}: a title"
        );
        assert!(text.contains("\u{2587}"), "{width}: a gauge bar");
    }

    // Drop order, widest first: cost, model, run, tier, tokens.
    assert!(wide.contains("~$0.21") && wide.contains("opus-4-1") && wide.contains("9g5fbz"));
    assert!(!mid.contains("~$0.21"), "cost drops first");
    assert!(!mid.contains("opus-4-1"), "then the model");
    assert!(
        mid.contains("high") && mid.contains("\u{2193} 223k"),
        "{mid}"
    );
    assert!(!narrow.contains("~$0.21") && !narrow.contains("opus-4-1"));
    assert!(narrow.contains("high") && narrow.contains("\u{2193} 223k"));
    // The title moves to its own line rather than disappearing.
    assert!(
        narrow.contains("\n    backfill the users index"),
        "{narrow}"
    );
    // Bars halve under 52 columns.
    assert!(
        narrow.contains("5h \u{2587}\u{2587}\u{2587}\u{2587}\u{2591}  71%"),
        "{narrow}"
    );
    assert!(
        mid.contains("5h \u{2587}\u{2587}\u{2587}\u{2587}\u{2587}\u{2587}\u{2587}\u{2591}\u{2591}\u{2591}  71%"),
        "{mid}"
    );
}

/// `layout_for` is the only place a width becomes a decision.
#[test]
fn the_column_drop_order_is_monotonic() {
    let mut last = render::layout_for(20);
    for width in 20u16..=140 {
        let l = render::layout_for(width);
        assert!(
            !(last.show_cost && !l.show_cost),
            "cost drops first at {width}"
        );
        assert!(!(last.show_model && !l.show_model), "then model at {width}");
        assert!(!(last.show_run && !l.show_run), "then run at {width}");
        assert!(!(last.show_tier && !l.show_tier), "then tier at {width}");
        assert!(
            !(last.show_tokens && !l.show_tokens),
            "then tokens at {width}"
        );
        last = l;
    }
    assert!(render::layout_for(100).show_cost);
    assert!(!render::layout_for(60).show_model);
    assert!(render::layout_for(51).title_own_line);
    assert_eq!(render::layout_for(51).bar, 5);
    assert_eq!(render::layout_for(52).bar, 10);
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
        render::gauge(0.5, 5, &theme),
        "\u{2587}\u{2587}\u{2587}\u{2591}\u{2591}"
    );
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

// ---------------------------------------------------------------- §7.2

/// Two runs, one exhausted account: the wait is visible, and so is why.
#[test]
fn two_runs_one_exhausted_account() {
    let b = board();
    let mid = draw(&b, 60);
    insta::assert_snapshot!("two_runs_60", mid);

    assert!(mid.contains("2 runs"), "the header sums both runs");
    assert!(mid.contains("waiting 1"), "{mid}");
    assert!(mid.contains("rebuild the index"), "{mid}");
    assert!(
        mid.contains("every anthropic account is at its limit"),
        "{mid}"
    );
    assert!(mid.contains("earliest reset 22:54 (in 38m)"), "{mid}");
    assert!(mid.contains("cooling until 23:20"), "{mid}");
    assert!(mid.contains("recent 3"), "{mid}");
    // In flight counts every tailed run: two brains and three workers.
    assert!(mid.contains("5 in flight"), "{mid}");
    // §3.2: the waiting row's last cell names the wait rather than a dash.
    assert!(
        mid.contains("rebuild the index            3m18s blocked"),
        "{mid}"
    );
    // §3.4: the footer says why the pool could not use the account it passed over.
    assert!(mid.contains("main ineligible (at 4/4)"), "{mid}");
}

// ---------------------------------------------------------------- §7.3

/// A brain whose pidfile is gone: the run says so and its spinners freeze.
#[test]
fn a_stale_run_freezes_its_glyphs() {
    let mut b = board();
    let alive = |node: NodeId| node != NodeId(run_b().0);
    for pane in &mut b.runs {
        pane.refresh_liveness(&alive, b.now);
    }
    let mid = draw(&b, 60);
    insta::assert_snapshot!("stale_60", mid);

    assert!(mid.contains("1 stale"), "{mid}");
    assert!(mid.contains("? 9g5fbz"), "the frozen brain glyph: {mid}");
    assert!(
        b.runs[0].stale.is_none() && b.runs[1].stale == Some(b.now),
        "only the dead run is stale"
    );
    // The live run keeps its spinner.
    assert!(mid.contains("\u{25c6} 9g5fav"), "{mid}");
    // §2.3: the marker sits on the account carrying the dead run's node, not only in the
    // header's total, or with two tailed runs nothing says which account is the stale one.
    let block = |head: &str| {
        mid.lines()
            .skip_while(|l| !l.contains(head))
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(block("\u{25cf} main").contains("stale"), "{mid}");
    assert!(
        !block("\u{25d0} alt").contains("stale"),
        "alt only runs the live run: {mid}"
    );
}

// ---------------------------------------------------------------- §7.4

#[test]
fn the_selection_reason_shows_the_terms_as_recorded() {
    let b = board();
    let theme = Theme::plain();
    for width in [40u16, 60, 100] {
        let c = render::Ctx::new(&b, width, &theme, 0, MAX_AGE);
        let text = screen(render::footer(&b, &b.rows(), &c), width);
        insta::assert_snapshot!(format!("reason_{width}"), text);
        assert!(text.contains("9g5f09 \u{2192} alt"), "{text}");
        assert!(text.contains("util .93\u{d7}.50"), "{text}");
        assert!(text.contains("main ineligible (at 4/4)"), "{text}");
    }
}

/// §3.4: every account `excluded` names, with the gate that holds it back right now.
#[test]
fn every_exclusion_names_the_gate_that_holds_it_back() {
    let mut lines = journal_a();
    lines.push(JournalLine {
        seq: 7,
        at: fx::at(7),
        run: fx::run_id(),
        node: Some(fx::id(9)),
        event: JournalEvent::AccountSelected {
            account: AccountId("alt".into()),
            exec: "claude-alt".into(),
            policy: SelectionPolicy::QuotaAware,
            reason: TERMS.to_owned(),
            excluded: vec![
                AccountId("main".into()),
                AccountId("codex-main".into()),
                AccountId("gone".into()),
            ],
        },
    });
    let mut b = board();
    b.runs[0] = pane(fx::run_id(), &lines);

    let theme = Theme::plain();
    let c = render::Ctx::new(&b, 100, &theme, 0, MAX_AGE);
    let text = screen(render::footer(&b, &b.rows(), &c), 100);
    insta::assert_snapshot!("reason_excluded_100", text);
    assert!(text.contains("main ineligible (at 4/4)"), "{text}");
    assert!(
        text.contains("codex-main ineligible (cooldown until 23:20)"),
        "{text}"
    );
    assert!(text.contains("gone excluded"), "{text}");
}

/// A recorded exclusion the live gate no longer agrees with is labelled `now`, per §3.4.
#[test]
fn an_exclusion_the_pool_would_not_make_today_says_now() {
    let mut b = board();
    for r in &mut b.accounts {
        if r.account.0 == "main" {
            r.max_concurrency = Some(8);
        }
    }
    let theme = Theme::plain();
    let c = render::Ctx::new(&b, 100, &theme, 0, MAX_AGE);
    let text = screen(render::footer(&b, &b.rows(), &c), 100);
    assert!(text.contains("main eligible now"), "{text}");
}

/// Journals written before WP5 carry `format!("score {sc:.4}")` and nothing else.
#[test]
fn the_legacy_reason_degrades_to_one_line() {
    let mut lines = journal_a();
    lines.push(JournalLine {
        seq: 7,
        at: fx::at(7),
        run: fx::run_id(),
        node: Some(fx::id(9)),
        event: JournalEvent::AccountSelected {
            account: AccountId("alt".into()),
            exec: "claude-alt".into(),
            policy: SelectionPolicy::QuotaAware,
            reason: "score 0.4100".into(),
            excluded: Vec::new(),
        },
    });
    let mut b = board();
    b.runs[0] = pane(fx::run_id(), &lines);

    let theme = Theme::plain();
    let c = render::Ctx::new(&b, 100, &theme, 0, MAX_AGE);
    let text = screen(render::footer(&b, &b.rows(), &c), 100);
    insta::assert_snapshot!("reason_legacy_100", text);
    assert_eq!(
        text.lines()
            .filter(|l| l.contains("score"))
            .collect::<Vec<_>>(),
        vec!["9g5f09 \u{2192} alt   score 0.4100"],
        "{text}"
    );
}

// ---------------------------------------------------------------- §7.6

/// A title is worker output: an escape sequence in it must not repaint the pane.
#[test]
fn control_characters_never_reach_the_pane() {
    let run = fx::run_id();
    let evil = "\u{1b}[2Jswamp: run succeeded\u{7}";
    let mut lines = journal_a();
    lines.push(Spawn::new(run, fx::id(1), evil, running(10)).line(7));

    let mut b = board();
    b.runs[0] = pane(run, &lines);
    for width in [40u16, 60, 100] {
        let text = draw(&b, width);
        assert!(!text.contains('\u{1b}'), "{width}: {text:?}");
        assert!(!text.contains('\u{7}'), "{width}: {text:?}");
        assert!(text.contains("swamp: run succeeded"), "{width}: {text}");
        for line in text.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= width as usize,
                "{width}: {line:?}"
            );
        }
    }
    insta::assert_snapshot!("control_characters_60", draw(&b, 60));
}

/// A pane the user dragged to nothing, and a board with nothing in it yet, still draw.
#[test]
fn every_width_draws_inside_its_pane() {
    let full = board();
    let empty = Board::new(Scoring::default(), SelectionPolicy::default(), fx::now());
    for b in [&full, &empty] {
        for width in 0u16..=120 {
            for line in draw(b, width).lines() {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= width as usize,
                    "{width}: {line:?}"
                );
            }
        }
    }
}

/// Every glyph the board invents on top of the theme table is one column wide, or a bar and
/// the cell beside it disagree about where they end.
#[test]
fn every_glyph_is_one_column() {
    for g in [
        "\u{2587}", "\u{2591}", "\u{21bb}", "\u{25c6}", "\u{25d0}", "\u{2192}",
    ] {
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(g),
            1,
            "{g:?} is not one column"
        );
    }
}
