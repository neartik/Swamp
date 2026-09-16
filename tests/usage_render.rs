//! WP-C acceptance: `/usage` and `swamp usage` share one renderer.

mod support;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{TerminalOptions, Viewport};
use std::collections::BTreeMap;
use support::Harness;
use swamp::dispatch::account::{AccountState, Health, QuotaSource};
use swamp::dispatch::policy::DEFAULT_QUOTA_MAX_AGE as MAX_AGE;
use swamp::model::core::{
    AccountId, CostBasis, LimitReached, LimitScope, LimitStatus, LimitWindow, Provider,
    RateLimitSnapshot, Usage,
};
use swamp::ui::chat::theme::{Role, Theme};
use swamp::ui::usage::{self, AccountRow};
use time::OffsetDateTime;

fn screen(lines: &[Line<'static>], width: u16) -> String {
    let height = (lines.len() as u16).max(1);
    let mut term = Terminal::with_options(
        TestBackend::new(width, height),
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .expect("terminal");
    term.draw(|f| f.render_widget(Paragraph::new(lines.to_vec()), f.area()))
        .expect("draw");
    let buffer = term.backend().buffer();
    let w = buffer.area.width as usize;
    let text: String = buffer.content().iter().map(|c| c.symbol()).collect();
    text.chars()
        .collect::<Vec<_>>()
        .chunks(w)
        .map(|row| row.iter().collect::<String>().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

fn row(id: &str, provider: Provider, health: Health) -> AccountRow {
    AccountRow {
        provider: Some(provider),
        account: AccountId(id.to_owned()),
        exec: format!("{id}-cli"),
        in_config: true,
        health,
        inflight: 1,
        max_concurrency: Some(3),
        cooldown_until: None,
        quota: None,
        quota_buckets: BTreeMap::new(),
        quota_observed_at: None,
        quota_source: None,
        window_tokens: Usage::default(),
        window_started_at: None,
        lifetime_tokens: Usage::default(),
        lifetime_nodes: 0,
        cost_usd: 0.0,
        cost_basis: None,
    }
}

fn with_seven_day(mut r: AccountRow, utilization: f64, measured: bool) -> AccountRow {
    r.quota = Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization,
            resets_at: Some(OffsetDateTime::now_utc() + time::Duration::days(3)),
            window_minutes: Some(10080),
            measured,
        }],
        ..RateLimitSnapshot::default()
    });
    r
}

/// Acceptance 1: the documented drop order, and that ACCOUNT, the tightest percentage and
/// WINDOW survive every width.
#[test]
fn the_table_drops_columns_in_the_documented_order() {
    let r = with_seven_day(
        row("claude-main", Provider::Anthropic, Health::Healthy),
        0.64,
        true,
    );
    for width in [100u16, 86, 78, 62] {
        let body = screen(
            &usage::render(std::slice::from_ref(&r), width, &Theme::plain(), MAX_AGE),
            width,
        );
        assert!(body.contains("claude-main"), "{width}: {body}");
        assert!(body.contains("64%"), "{width}: {body}");
        assert!(body.contains("WINDOW"), "{width}: {body}");
    }
    let wide = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(wide.contains("LIFETIME") && wide.contains("COST") && wide.contains("HEALTH"));

    let below_lifetime = screen(
        &usage::render(std::slice::from_ref(&r), 90, &Theme::plain(), MAX_AGE),
        90,
    );
    assert!(!below_lifetime.contains("LIFETIME"), "{below_lifetime}");
    assert!(below_lifetime.contains("COST"), "{below_lifetime}");

    let below_cost = screen(
        &usage::render(std::slice::from_ref(&r), 80, &Theme::plain(), MAX_AGE),
        80,
    );
    assert!(!below_cost.contains("COST"), "{below_cost}");
    assert!(
        below_cost.contains("7D") || below_cost.contains("64%"),
        "{below_cost}"
    );

    let collapsed = screen(
        &usage::render(std::slice::from_ref(&r), 70, &Theme::plain(), MAX_AGE),
        70,
    );
    assert!(!collapsed.contains("7D"), "{collapsed}");
    assert!(collapsed.contains("HEALTH"), "{collapsed}");

    let narrow = screen(
        &usage::render(std::slice::from_ref(&r), 62, &Theme::plain(), MAX_AGE),
        62,
    );
    assert!(!narrow.contains("HEALTH"), "{narrow}");
    assert!(narrow.contains("claude-main"), "{narrow}");
}

/// Acceptance 2: no quota renders `-` and never a bare `0%`; an estimated one gets `~`.
#[test]
fn missing_quota_is_a_dash_and_an_estimate_carries_a_tilde() {
    let bare = row("claude-main", Provider::Anthropic, Health::Healthy);
    let body = screen(&usage::render(&[bare], 100, &Theme::plain(), MAX_AGE), 100);
    assert!(!body.contains("0%"), "{body}");
    assert!(body.contains(" - "), "{body}");

    let est = with_seven_day(
        row("codex-alt", Provider::Openai, Health::Healthy),
        0.02,
        false,
    );
    let body = screen(&usage::render(&[est], 100, &Theme::plain(), MAX_AGE), 100);
    assert!(body.contains("~2%"), "{body}");
    assert!(body.contains("~in"), "{body}");
}

/// Acceptance 3: a cooling account's continuation row, and an auth-broken one's.
#[test]
fn continuation_rows_name_the_cooldown_and_the_broken_account() {
    let mut cooling = row("claude-work", Provider::Anthropic, Health::Cooling);
    cooling.cooldown_until = Some(OffsetDateTime::now_utc() + time::Duration::minutes(40));
    cooling.quota = Some(RateLimitSnapshot {
        reached: Some(LimitReached::RateLimit),
        ..RateLimitSnapshot::default()
    });
    let body = screen(
        &usage::render(&[cooling], 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(body.contains("until"), "{body}");
    assert!(body.contains("rate_limit"), "{body}");

    let broken = row("claude-broke", Provider::Anthropic, Health::AuthBroken);
    let body = screen(
        &usage::render(&[broken], 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(body.contains("auth broken"), "{body}");
    assert!(body.contains("re-auth claude-broke-cli"), "{body}");
}

/// Acceptance 4: `swamp usage --json` matches the §3.3 shape and round-trips.
#[test]
fn json_matches_the_documented_shape_and_round_trips() {
    let h = Harness::new();
    let out = h.swamp(&["usage", "--json"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    assert!(v.get("at").is_some());
    assert!(v["accounts"].is_array());
    assert!(v["totals"]["usage"].is_object());
    assert!(v["totals"]["accounts_without_quota_source"].is_number());
    let acct = &v["accounts"][0];
    assert_eq!(acct["account"], "main");
    for which in ["window", "lifetime"] {
        let tokens = &acct["tokens"][which];
        let billable = tokens["billable"]
            .as_u64()
            .unwrap_or_else(|| panic!("tokens.{which}.billable is documented in USAGE 3.3"));
        assert_eq!(
            billable,
            tokens["input_tokens"].as_u64().unwrap()
                + tokens["cache_write_tokens"].as_u64().unwrap()
                + tokens["output_tokens"].as_u64().unwrap()
        );
    }
    let back: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
    assert_eq!(v, back);
}

/// Acceptance 5: no supervisor needed; a hand-written state file renders, and a missing one
/// with no accounts configured renders an empty table, both exit 0.
#[test]
fn swamp_usage_needs_no_supervisor_and_a_missing_file_is_not_an_error() {
    let h = Harness::new();
    let mut state = swamp::dispatch::persist::StateMap::new();
    state.insert(
        AccountId("main".into()),
        AccountState {
            lifetime_nodes: 3,
            lifetime_cost_usd: 0.42,
            ..Default::default()
        },
    );
    swamp::dispatch::persist::save_state(&h.paths().accounts_state(), &state)
        .expect("writing accounts.json by hand");
    let out = h.swamp(&["usage"]).assert().success();
    let stdout = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(stdout.contains("main"), "{stdout}");

    let empty = Harness::new().with_accounts(0, 0);
    empty.swamp(&["usage"]).assert().success();
}

/// An unreadable state file is the one case `load_state` reports: rendering it as zeros
/// reads as "nothing was spent", and `--probe` would then overwrite it with the probed
/// accounts alone, losing every lifetime counter the file held.
#[test]
fn an_unreadable_state_file_is_reported_and_never_overwritten() {
    let h = Harness::new();
    let path = h.paths().accounts_state();
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the state directory");
    std::fs::write(&path, "{ not json at all").expect("a corrupt state file");

    let out = h.swamp(&["usage"]).assert().failure();
    let stderr = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    assert!(stderr.contains("accounts.json"), "{stderr}");

    h.swamp(&["usage", "--probe"]).assert().failure();
    assert_eq!(
        std::fs::read_to_string(&path).expect("the file survives"),
        "{ not json at all",
        "a probe must not replace a file it could not read"
    );
}

/// Acceptance 6: a stale `quota_observed_at` renders its age in the `err` role.
#[test]
fn a_stale_observed_time_renders_in_the_err_role() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.quota_observed_at = Some(OffsetDateTime::now_utc() - time::Duration::minutes(5));
    r.quota_source = Some(QuotaSource::Telemetry);
    let theme = Theme::plain();
    let lines = usage::render(&[r], 100, &theme, MAX_AGE);
    let err_style = theme.style(Role::Err);
    let found = lines.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content.contains("claude-main") && s.style == err_style)
    });
    assert!(found, "{lines:?}");
}

/// Acceptance 7: a control character in an account id never reaches a real terminal.
#[test]
fn a_control_character_in_an_account_id_is_neutralised() {
    let r = row("evil\u{1b}[2J", Provider::Anthropic, Health::Healthy);
    let lines = usage::render(&[r], 100, &Theme::plain(), MAX_AGE);
    for l in &lines {
        for s in &l.spans {
            assert!(!s.content.contains('\u{1b}'), "{:?}", s.content);
        }
    }
}

/// A seven-day window resets days away, and `fmt::duration` has no day unit: the cell used to
/// come out truncated inside the 9-column budget.
#[test]
fn a_reset_days_away_fits_the_column_with_a_day_unit() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.quota = Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: 0.05,
            resets_at: Some(OffsetDateTime::now_utc() + time::Duration::hours(165)),
            window_minutes: Some(10080),
            measured: true,
        }],
        ..RateLimitSnapshot::default()
    });
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(
        body.contains("in 6d20h") || body.contains("in 6d21h"),
        "{body}"
    );
    assert!(!body.contains('\u{2026}'), "{body}");

    r.quota.as_mut().unwrap().windows[0].measured = false;
    let body = screen(&usage::render(&[r], 100, &Theme::plain(), MAX_AGE), 100);
    assert!(body.contains("~in 6d"), "{body}");
    assert!(!body.contains('\u{2026}'), "{body}");
}

/// `dispatch.quota_max_age` is the threshold for the table too, not a hardcoded 60s.
#[test]
fn the_observed_age_honours_the_configured_quota_max_age() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.quota_observed_at = Some(OffsetDateTime::now_utc() - time::Duration::minutes(5));
    r.quota_source = Some(QuotaSource::Telemetry);
    let theme = Theme::plain();
    let err_style = theme.style(Role::Err);
    let painted = |max_age| {
        usage::render(std::slice::from_ref(&r), 100, &theme, max_age)
            .iter()
            .any(|l| {
                l.spans
                    .iter()
                    .any(|s| s.content.contains("claude-main") && s.style == err_style)
            })
    };
    assert!(painted(std::time::Duration::from_secs(60)));
    assert!(
        !painted(std::time::Duration::from_secs(600)),
        "a 5m reading is fresh when the user allows 10m"
    );
}

/// `not in config` is listed last, under every real provider, matching `swamp accounts`.
#[test]
fn the_not_in_config_section_is_listed_last() {
    let mut stale = row("codex-gone", Provider::Openai, Health::Healthy);
    stale.provider = None;
    stale.in_config = false;
    let rows = vec![
        row("claude-main", Provider::Anthropic, Health::Healthy),
        row("codex-main", Provider::Openai, Health::Healthy),
        stale,
    ];
    let body = screen(&usage::render(&rows, 100, &Theme::plain(), MAX_AGE), 100);
    let at = |needle: &str| {
        body.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {body}"))
    };
    assert!(at("anthropic") < at("not in config"), "{body}");
    assert!(at("openai") < at("not in config"), "{body}");
}

/// Two accounts without a source own `their utilization`, not `its`.
#[test]
fn the_totals_footnote_agrees_with_itself_in_the_plural() {
    let rows = vec![
        row("claude-main", Provider::Anthropic, Health::Healthy),
        row("claude-alt", Provider::Anthropic, Health::Healthy),
    ];
    let body = screen(&usage::render(&rows, 100, &Theme::plain(), MAX_AGE), 100);
    assert!(
        body.contains("2 accounts have no quota source; their utilization is estimated"),
        "{body}"
    );
}

/// `Estimated` is Swamp's own arithmetic, not a provider reading. `doctor` already counts it
/// as no source; the footnote used to exclude the very rows it describes.
#[test]
fn an_estimated_quota_source_counts_as_no_quota_source() {
    let mut r = row("codex-main", Provider::Openai, Health::Healthy);
    r.quota_source = Some(QuotaSource::Estimated);
    let r = with_seven_day(r, 0.42, false);
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(
        body.contains("1 account has no quota source; its utilization is estimated"),
        "{body}"
    );
    assert_eq!(
        usage::json(&[r])["totals"]["accounts_without_quota_source"],
        1
    );
}

/// A consumer reading both `swamp usage --json` and `accounts.json` must not meet two
/// spellings of one enum value.
#[test]
fn the_json_quota_source_is_the_spelling_serde_writes() {
    let mut r = row("codex-main", Provider::Openai, Health::Healthy);
    r.quota_source = Some(QuotaSource::AppServer);
    let r = with_seven_day(r, 0.10, true);
    let source = &usage::json(std::slice::from_ref(&r))["accounts"][0]["quota"]["source"];
    assert_eq!(
        source,
        &serde_json::to_value(QuotaSource::AppServer).expect("serde value")
    );
    assert_eq!(source, "app_server");
}

/// A `[pricing]` multiplication is not what the provider told us, and the JSON is the one
/// surface that drops the `~`.
#[test]
fn an_estimated_cost_is_never_labelled_reported() {
    let mut r = row("codex-main", Provider::Openai, Health::Healthy);
    r.cost_usd = 1.2;
    r.cost_basis = Some(CostBasis::Estimated);
    assert_eq!(
        usage::json(std::slice::from_ref(&r))["accounts"][0]["cost_basis"],
        "estimated"
    );

    // A state file written before the basis was recorded must not promise provider truth.
    r.cost_basis = None;
    assert_eq!(usage::json(&[r])["accounts"][0]["cost_basis"], "estimated");
}

fn test_config() -> swamp::config::Config {
    let schema: swamp::config::Schema = toml::from_str(
        r#"
version = 1
[providers.anthropic]
models = { high = "claude-opus-4-20250514", mid = "claude-sonnet-4-20250514", low = "claude-haiku-4-20250514" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "claude-main"
max_concurrency = 3
"#,
    )
    .expect("fixture config parses");
    let layers = vec![
        swamp::config::load::default_layer(),
        swamp::config::load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = swamp::config::resolve::from_schema(swamp::config::load::merge(layers));
    swamp::config::validate::validate(&mut cfg).expect("fixture config is valid");
    cfg
}

fn key(code: crossterm::event::KeyCode) -> swamp::ui::chat::app::Msg {
    swamp::ui::chat::app::Msg::Key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ))
}

/// Acceptance 8: `/usage` committed in chat is byte-identical to `swamp usage` at the same
/// width. A row with no quota keeps every field wall-clock-independent.
#[test]
fn chat_and_cli_render_the_same_bytes() {
    let cfg = test_config();
    let pool = vec![(
        Provider::Anthropic,
        AccountId("main".into()),
        AccountState {
            inflight: 1,
            lifetime_nodes: 5,
            lifetime_cost_usd: 1.5,
            ..Default::default()
        },
    )];
    let rows = usage::rows_from(&cfg.accounts, &pool, &[]);
    let width = 100u16;
    let cli_text = screen(
        &usage::render(&rows, width, &Theme::plain(), MAX_AGE),
        width,
    );

    let mut app = swamp::ui::chat::app::App::new(
        swamp::RunId::new(),
        Theme::plain(),
        swamp::ui::chat::blocks::WelcomeInfo::default(),
        swamp::ui::chat::input::History::load(None, 10),
        &cfg,
    );
    app.width = width;
    app.pool = pool;
    let mut chat_lines: Vec<Line<'static>> = Vec::new();
    for c in "/usage".chars() {
        app.reduce(key(crossterm::event::KeyCode::Char(c)));
    }
    // Effect 0 is the echoed `> /usage` bar; the table itself follows.
    for effect in app
        .reduce(key(crossterm::event::KeyCode::Enter))
        .into_iter()
        .skip(1)
    {
        if let swamp::ui::chat::app::Effect::Commit(body) = effect {
            chat_lines.extend(body);
        }
    }
    let chat_text = screen(&chat_lines, width);
    assert_eq!(chat_text, cli_text);
}

/// The chat block and the CLI must still agree once `accounts.json` holds an account the
/// config no longer names: `/usage` has to see those entries too.
#[test]
fn chat_lists_the_accounts_that_are_no_longer_in_config() {
    let cfg = test_config();
    let pool = vec![(
        Provider::Anthropic,
        AccountId("main".into()),
        AccountState::default(),
    )];
    let stale = vec![(
        AccountId("codex-gone".into()),
        AccountState {
            lifetime_nodes: 2,
            ..AccountState::default()
        },
    )];
    let width = 100u16;
    let rows = usage::rows_from(&cfg.accounts, &pool, &stale);
    let cli_text = screen(
        &usage::render(&rows, width, &Theme::plain(), MAX_AGE),
        width,
    );
    assert!(cli_text.contains("not in config"), "{cli_text}");

    let mut app = chat_app(&cfg, width);
    app.pool = pool;
    app.set_stale_accounts(stale);
    assert_eq!(usage_commit(&mut app, width), cli_text);
}

/// USAGE 3.2: a stale OpenAI reading kicks one background probe instead of committing a
/// table nothing will ever refresh.
#[test]
fn a_stale_openai_account_kicks_one_background_probe() {
    let cfg = openai_config();
    let mut app = chat_app(&cfg, 100);
    app.pool = vec![(
        Provider::Openai,
        AccountId("codex-main".into()),
        AccountState::default(),
    )];
    let effects = usage_effects(&mut app);
    let probed: Vec<AccountId> = effects
        .iter()
        .filter_map(|e| match e {
            swamp::ui::chat::app::Effect::ProbeQuota(ids) => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(probed, vec![AccountId("codex-main".into())]);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, swamp::ui::chat::app::Effect::Commit(_))),
        "the table stays live until the reading lands"
    );

    // A fresh reading lands: the block commits once, and no second probe is asked for.
    app.pool = vec![(
        Provider::Openai,
        AccountId("codex-main".into()),
        AccountState {
            quota_observed_at: Some(OffsetDateTime::now_utc()),
            quota_source: Some(QuotaSource::AppServer),
            ..AccountState::default()
        },
    )];
    app.set_stale_accounts(Vec::new());
    app.now = OffsetDateTime::now_utc();
    let after = app.reduce(swamp::ui::chat::app::Msg::Tick);
    assert!(
        after
            .iter()
            .any(|e| matches!(e, swamp::ui::chat::app::Effect::Commit(_))),
        "the landed reading commits the block"
    );
}

fn chat_app(cfg: &swamp::config::Config, width: u16) -> swamp::ui::chat::app::App {
    let mut app = swamp::ui::chat::app::App::new(
        swamp::RunId::new(),
        Theme::plain(),
        swamp::ui::chat::blocks::WelcomeInfo::default(),
        swamp::ui::chat::input::History::load(None, 10),
        cfg,
    );
    app.width = width;
    app
}

fn usage_effects(app: &mut swamp::ui::chat::app::App) -> Vec<swamp::ui::chat::app::Effect> {
    for c in "/usage".chars() {
        app.reduce(key(crossterm::event::KeyCode::Char(c)));
    }
    // Effect 0 is the echoed `> /usage` bar; the table itself follows.
    app.reduce(key(crossterm::event::KeyCode::Enter))
        .into_iter()
        .skip(1)
        .collect()
}

fn usage_commit(app: &mut swamp::ui::chat::app::App, width: u16) -> String {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for effect in usage_effects(app) {
        if let swamp::ui::chat::app::Effect::Commit(body) = effect {
            lines.extend(body);
        }
    }
    screen(&lines, width)
}

fn openai_config() -> swamp::config::Config {
    let schema: swamp::config::Schema = toml::from_str(
        r#"
version = 1
[providers.openai]
models = { high = "gpt-5-codex", mid = "gpt-5-codex", low = "gpt-5-codex" }
[[accounts]]
id = "codex-main"
provider = "openai"
exec = "codex-main"
"#,
    )
    .expect("fixture config parses");
    let layers = vec![
        swamp::config::load::default_layer(),
        swamp::config::load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = swamp::config::resolve::from_schema(swamp::config::load::merge(layers));
    swamp::config::validate::validate(&mut cfg).expect("fixture config is valid");
    cfg
}

/// The chat block iterates the pool's `BTreeMap` and the CLI its config declaration order.
/// One renderer means one row order, so the ordering lives in `rows_from`.
#[test]
fn both_surfaces_order_the_rows_the_same_way() {
    let cfg = two_account_config();
    // Declaration order, as `swamp usage` builds it.
    let declared = vec![
        (
            Provider::Anthropic,
            AccountId("zeta".into()),
            AccountState::default(),
        ),
        (
            Provider::Anthropic,
            AccountId("alpha".into()),
            AccountState::default(),
        ),
    ];
    let mut by_id = declared.clone();
    by_id.reverse();
    let names = |pool: &[(Provider, AccountId, AccountState)]| -> Vec<String> {
        usage::rows_from(&cfg.accounts, pool, &[])
            .iter()
            .map(|r| r.account.0.clone())
            .collect()
    };
    assert_eq!(
        names(&declared),
        vec!["alpha".to_owned(), "zeta".to_owned()]
    );
    assert_eq!(names(&declared), names(&by_id));
}

/// An estimated reset between ten hours and a day out is `~in 23h59m` on the old format: one
/// column too many for RESETS, which then truncates it to an ellipsis.
#[test]
fn an_estimated_reset_under_a_day_fits_the_column() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.quota = Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::SevenDay,
            utilization: 0.4,
            resets_at: Some(
                OffsetDateTime::now_utc() + time::Duration::hours(23) + time::Duration::minutes(59),
            ),
            window_minutes: Some(10080),
            measured: false,
        }],
        ..RateLimitSnapshot::default()
    });
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(!body.contains('\u{2026}'), "{body}");
    assert!(body.contains("~in 23h"), "{body}");
}

/// Nothing on the reading side resets `health` when a cooldown expires, so `swamp usage`
/// with no supervisor running used to print "cooling" with no `until` row, indefinitely,
/// while `score` happily dispatched to the account.
#[test]
fn an_expired_cooldown_is_not_rendered_as_cooling() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Cooling);
    r.cooldown_until = Some(OffsetDateTime::now_utc() - time::Duration::hours(1));
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(!body.contains("cooling"), "{body}");
    assert!(body.contains("healthy"), "{body}");

    // At 62 columns the HEALTH word is a glyph; it must not stay the cooling one either.
    let narrow = screen(&usage::render(&[r], 62, &Theme::plain(), MAX_AGE), 62);
    assert!(narrow.contains("+ claude-main"), "{narrow}");
}

/// §3.1: a provider-refused account says so on its continuation row, under the same health
/// word every other surface prints. USAGE 2.2 makes that word `AuthBroken`, so the table has
/// no word of its own to invent.
#[test]
fn a_provider_parked_account_says_so() {
    let mut r = with_seven_day(
        row("codex-main", Provider::Openai, Health::AuthBroken),
        0.32,
        true,
    );
    let q = r.quota.as_mut().expect("quota");
    q.reached = Some(LimitReached::CreditsDepleted);
    q.ordinary_usage_allowed = Some(false);
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(body.contains("no-auth"), "{body}");
    assert!(body.contains("credits_depleted"), "{body}");
    assert!(!body.contains("parked"), "{body}");
    assert!(!body.contains("healthy"), "{body}");

    // A state file written before 2.2 can still carry the refusal without the word.
    r.health = Health::Healthy;
    let stale = screen(&usage::render(&[r], 100, &Theme::plain(), MAX_AGE), 100);
    assert!(stale.contains("credits_depleted"), "{stale}");
}

/// The estimated variant of a sub-hour reset was one column too wide for RESETS and came out
/// ellipsized, exactly in the last hour before the window rolls.
#[test]
fn an_estimated_reset_in_minutes_fits_the_column() {
    for minutes in [10i64, 24, 59] {
        let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
        r.quota = Some(RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![LimitWindow {
                scope: LimitScope::FiveHour,
                utilization: 0.8,
                resets_at: Some(
                    OffsetDateTime::now_utc()
                        + time::Duration::minutes(minutes)
                        + time::Duration::seconds(59),
                ),
                window_minutes: Some(300),
                measured: false,
            }],
            ..RateLimitSnapshot::default()
        });
        let body = screen(&usage::render(&[r], 100, &Theme::plain(), MAX_AGE), 100);
        assert!(!body.contains('\u{2026}'), "{minutes}m: {body}");
        assert!(
            body.contains(&format!("~in {minutes}m")),
            "{minutes}m: {body}"
        );
    }
}

/// The table downgrades an elapsed cooldown to `healthy`; `--json` on the same bytes must
/// not keep calling it `cooling`, or a script gates dispatch on a word the table denies.
#[test]
fn json_reports_the_health_the_table_shows() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Cooling);
    r.cooldown_until = Some(OffsetDateTime::now_utc() - time::Duration::hours(2));
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(body.contains("healthy"), "{body}");
    let v = usage::json(std::slice::from_ref(&r));
    assert_eq!(v["accounts"][0]["health"], "healthy");

    // A cooldown still running is still cooling on both surfaces.
    r.cooldown_until = Some(OffsetDateTime::now_utc() + time::Duration::hours(2));
    assert_eq!(usage::json(&[r])["accounts"][0]["health"], "cooling");
}

/// `lifetime_cost_usd` spans every run and repo, so four figures are ordinary. `~$12345.67`
/// is nine columns in an eight-column cell, and truncation ate the digits that carry it.
#[test]
fn a_four_figure_cost_keeps_its_magnitude() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.cost_usd = 12_345.67;
    let body = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(!body.contains('\u{2026}'), "{body}");
    assert!(body.contains("~$12.3k"), "{body}");
}

/// Every other row is width-bounded; the `observed` line emitted its first entry whatever
/// its length, so one long account id pushed it past the table it sits under.
#[test]
fn the_observed_line_fits_a_narrow_table() {
    let mut r = row(
        "anthropic-team-billing-account-primary",
        Provider::Anthropic,
        Health::Healthy,
    );
    r.quota_observed_at = Some(OffsetDateTime::now_utc() - time::Duration::seconds(12));
    r.quota_source = Some(QuotaSource::Telemetry);
    for width in [62u16, 80, 100] {
        let lines = usage::render(std::slice::from_ref(&r), width, &Theme::plain(), MAX_AGE);
        for line in &lines {
            assert!(
                line.width() <= width as usize,
                "{width} columns: {:?} is {} wide",
                line,
                line.width()
            );
        }
    }
}

/// §3.1 documents a drop order down to 62 columns; the footer has to shed with the table
/// instead of soft-wrapping into three ragged lines under it.
#[test]
fn the_totals_row_never_overflows_a_narrow_terminal() {
    let mut rows = vec![
        with_seven_day(
            row("claude-main", Provider::Anthropic, Health::Healthy),
            0.2,
            true,
        ),
        with_seven_day(
            row("claude-alt", Provider::Anthropic, Health::Healthy),
            0.3,
            true,
        ),
    ];
    for r in &mut rows {
        r.lifetime_tokens = Usage {
            input_tokens: 2_100_000,
            output_tokens: 96_400,
            cached_input_tokens: 18_200_000,
            cache_write_tokens: 441_000,
            ..Usage::default()
        };
        r.cost_usd = 2.65;
    }
    for width in [62u16, 66, 78, 100] {
        let lines = usage::render(&rows, width, &Theme::plain(), MAX_AGE);
        for line in &lines {
            assert!(
                line.width() <= width as usize,
                "{width} columns: {:?} is {} wide",
                line,
                line.width()
            );
        }
    }
}

/// A `not in config` row has no executable to name, so the re-auth instruction would name
/// nothing at all.
#[test]
fn an_auth_broken_row_with_no_executable_names_something_else() {
    let stale = vec![(
        AccountId("codex-old".into()),
        AccountState {
            health: Health::AuthBroken,
            ..AccountState::default()
        },
    )];
    let rows = usage::rows_from(&[], &[], &stale);
    let body = screen(&usage::render(&rows, 100, &Theme::plain(), MAX_AGE), 100);
    assert!(!body.contains("re-auth"), "{body}");
    assert!(body.contains("auth broken"), "{body}");
}

/// USAGE 4.8: `providers.openai.quota_source = "none"` forbids the app-server, and `/usage`
/// is not exempt from it.
#[test]
fn quota_source_none_kicks_no_probe_from_chat() {
    let cfg = no_probe_config();
    let mut app = chat_app(&cfg, 100);
    app.pool = vec![(
        Provider::Openai,
        AccountId("codex-main".into()),
        AccountState::default(),
    )];
    let effects = usage_effects(&mut app);
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, swamp::ui::chat::app::Effect::ProbeQuota(_))),
        "the user asked Swamp not to spawn an app-server"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, swamp::ui::chat::app::Effect::Commit(_))),
        "the table commits with what it already has"
    );
    assert!(!cfg.probes_app_server(Provider::Openai));
    assert!(!no_probe_config_rollout().probes_app_server(Provider::Openai));
}

fn from_toml(text: &str) -> swamp::config::Config {
    let schema: swamp::config::Schema = toml::from_str(text).expect("fixture config parses");
    let layers = vec![
        swamp::config::load::default_layer(),
        swamp::config::load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = swamp::config::resolve::from_schema(swamp::config::load::merge(layers));
    swamp::config::validate::validate(&mut cfg).expect("fixture config is valid");
    cfg
}

fn two_account_config() -> swamp::config::Config {
    from_toml(
        r#"
version = 1
[providers.anthropic]
models = { high = "opus", mid = "sonnet", low = "haiku" }
[[accounts]]
id = "zeta"
provider = "anthropic"
exec = "claude-zeta"
[[accounts]]
id = "alpha"
provider = "anthropic"
exec = "claude-alpha"
"#,
    )
}

fn no_probe_config() -> swamp::config::Config {
    from_toml(
        r#"
version = 1
[providers.openai]
quota_source = "none"
models = { high = "gpt-5-codex", mid = "gpt-5-codex", low = "gpt-5-codex" }
[[accounts]]
id = "codex-main"
provider = "openai"
exec = "codex-main"
"#,
    )
}

fn no_probe_config_rollout() -> swamp::config::Config {
    from_toml(
        r#"
version = 1
[providers.openai]
quota_source = "rollout"
models = { high = "gpt-5-codex", mid = "gpt-5-codex", low = "gpt-5-codex" }
[[accounts]]
id = "codex-main"
provider = "openai"
exec = "codex-main"
"#,
    )
}

/// §3.1: dispatch scores only the windows that still describe the present, so the table and
/// the gauges must not keep reporting an allowance that has already rolled.
#[test]
fn a_rolled_window_is_not_reported_as_utilization() {
    let now = OffsetDateTime::now_utc();
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.quota = Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![LimitWindow {
            scope: LimitScope::FiveHour,
            utilization: 0.96,
            resets_at: Some(now - time::Duration::hours(3)),
            window_minutes: Some(300),
            measured: true,
        }],
        ..RateLimitSnapshot::default()
    });
    for width in [100u16, 70] {
        let body = screen(
            &usage::render(std::slice::from_ref(&r), width, &Theme::plain(), MAX_AGE),
            width,
        );
        assert!(!body.contains("96%"), "{width}: {body}");
        assert!(!body.contains("in 0s"), "{width}: {body}");
    }
}

/// §3: the two surfaces render the same bytes, so every row has to respect the table width.
/// A continuation row is a row: unbounded, the CLI wraps it and chat cuts it dead.
#[test]
fn continuation_rows_stay_inside_the_table_width() {
    let mut r = with_seven_day(
        row("claude-main", Provider::Anthropic, Health::AuthBroken),
        0.10,
        true,
    );
    r.exec = "/opt/tooling/pnpm/global/5/node_modules/.bin/claude-main-wrapper".to_owned();
    let width = 100u16;
    let lines = usage::render(std::slice::from_ref(&r), width, &Theme::plain(), MAX_AGE);
    for line in swamp::ui::chat::blocks::text_of(&lines) {
        assert!(
            line.chars().count() <= width as usize,
            "{} columns: {line}",
            line.chars().count()
        );
    }
    let body = screen(&lines, width);
    assert!(body.contains("auth broken"), "{body}");
}

/// UI 3.7: `/usage --json` is output a caller copies out and parses. Clipping it to the
/// viewport cut it mid-token and the committed block stopped being JSON.
#[test]
fn usage_json_in_chat_is_never_clipped_to_the_viewport() {
    let cfg = long_exec_config();
    let width = 60u16;
    let mut app = chat_app(&cfg, width);
    app.pool = vec![(
        Provider::Anthropic,
        AccountId("main".into()),
        AccountState::default(),
    )];
    for c in "/usage --json".chars() {
        app.reduce(key(crossterm::event::KeyCode::Char(c)));
    }
    let mut body: Vec<String> = Vec::new();
    for effect in app
        .reduce(key(crossterm::event::KeyCode::Enter))
        .into_iter()
        .skip(1)
    {
        if let swamp::ui::chat::app::Effect::Commit(lines) = effect {
            body.extend(swamp::ui::chat::blocks::text_of(&lines));
        }
    }
    let text = body.join("\n");
    assert!(text.contains("```json"), "{text}");
    let json = text
        .trim()
        .trim_start_matches("```json")
        .trim_end_matches("```");
    let v: serde_json::Value = serde_json::from_str(json).expect("the committed block parses");
    assert_eq!(
        v["accounts"][0]["exec"].as_str(),
        Some("/opt/tooling/pnpm/global/5/node_modules/.bin/claude-main")
    );
}

fn long_exec_config() -> swamp::config::Config {
    let schema: swamp::config::Schema = toml::from_str(
        r#"
version = 1
[providers.anthropic]
models = { high = "claude-opus-4-20250514", mid = "claude-sonnet-4-20250514", low = "claude-haiku-4-20250514" }
[[accounts]]
id = "main"
provider = "anthropic"
exec = "/opt/tooling/pnpm/global/5/node_modules/.bin/claude-main"
"#,
    )
    .expect("fixture config parses");
    let layers = vec![
        swamp::config::load::default_layer(),
        swamp::config::load::Layer {
            origin: "test".into(),
            schema,
        },
    ];
    let mut cfg = swamp::config::resolve::from_schema(swamp::config::load::merge(layers));
    swamp::config::validate::validate(&mut cfg).expect("fixture config is valid");
    cfg
}

/// The header and the account row were the only lines the renderer never bounded, so a narrow
/// terminal soft-wrapped each of them and the table read as two interleaved tables.
#[test]
fn no_rendered_line_is_wider_than_the_terminal() {
    let rows = vec![
        with_seven_day(
            row("claude-main", Provider::Anthropic, Health::Healthy),
            0.64,
            true,
        ),
        row("codex-main", Provider::Openai, Health::Degraded),
    ];
    for width in [30u16, 40, 50, 62, 80, 100] {
        let lines = usage::render(&rows, width, &Theme::plain(), MAX_AGE);
        for line in swamp::ui::chat::blocks::text_of(&lines) {
            assert!(
                line.chars().count() <= width as usize,
                "at width {width} a line is {} columns: {line:?}",
                line.chars().count()
            );
        }
    }
}

/// USAGE 3.2: `swamp accounts` and `swamp usage` share one health word, so a `Cooling` entry
/// whose timer has elapsed cannot read `cooling` on one surface and `healthy` on the other.
#[test]
fn an_elapsed_cooldown_reads_healthy_on_every_surface() {
    let now = OffsetDateTime::now_utc();
    let mut cooling = row("claude-alt", Provider::Anthropic, Health::Cooling);
    cooling.cooldown_until = Some(now - time::Duration::hours(1));

    let body = screen(
        &usage::render(
            std::slice::from_ref(&cooling),
            100,
            &Theme::plain(),
            MAX_AGE,
        ),
        100,
    );
    assert!(body.contains("healthy"), "{body}");
    assert_eq!(
        swamp::ui::watch::health_word(swamp::ui::watch::shown_health(
            cooling.health,
            cooling.cooldown_until,
            now
        )),
        "healthy",
        "`swamp accounts` reads the same normalisation"
    );
    assert_eq!(
        swamp::ui::watch::health_word(swamp::ui::watch::shown_health(
            Health::Cooling,
            Some(now + time::Duration::minutes(5)),
            now
        )),
        "cooling",
        "a live timer still cools"
    );
}

/// A cooldown routinely crosses midnight UTC: `cooldown.max` defaults to six hours and a
/// provider reset is adopted verbatim. A bare HH:MM then reads as a time in the past.
#[test]
fn a_cooldown_that_crosses_midnight_names_its_day() {
    let now = OffsetDateTime::now_utc();
    let until = now + time::Duration::hours(26);
    let mut cooling = row("claude-main", Provider::Anthropic, Health::Cooling);
    cooling.cooldown_until = Some(until);
    cooling.quota = Some(RateLimitSnapshot {
        reached: Some(LimitReached::RateLimit),
        ..RateLimitSnapshot::default()
    });

    let body = screen(
        &usage::render(
            std::slice::from_ref(&cooling),
            100,
            &Theme::plain(),
            MAX_AGE,
        ),
        100,
    );
    let expected = swamp::ui::fmt::clock_day(until, now);
    assert!(
        expected.contains(" on "),
        "the fixture crosses a day: {expected}"
    );
    assert!(body.contains(&format!("until {expected}")), "{body}");

    let soon = now + time::Duration::minutes(5);
    let mut near = row("claude-alt", Provider::Anthropic, Health::Cooling);
    near.cooldown_until = Some(soon);
    let body = screen(
        &usage::render(std::slice::from_ref(&near), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(
        body.contains(&format!("until {}", swamp::ui::fmt::clock_day(soon, now))),
        "a reset later today stays a bare clock: {body}"
    );
}

/// Below 78 columns the single percentage cell is the tightest window, which can be the
/// minute one; the continuation rows must not then state it a second time.
#[test]
fn the_collapsed_cell_and_the_continuation_rows_never_repeat_a_window() {
    let now = OffsetDateTime::now_utc();
    let mut r = row("claude-main", Provider::Anthropic, Health::Degraded);
    r.quota = Some(RateLimitSnapshot {
        status: LimitStatus::Allowed,
        windows: vec![
            LimitWindow {
                scope: LimitScope::Minute,
                utilization: 0.95,
                resets_at: Some(now + time::Duration::seconds(38)),
                window_minutes: Some(1),
                measured: true,
            },
            LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.10,
                resets_at: Some(now + time::Duration::days(3)),
                window_minutes: Some(10080),
                measured: true,
            },
        ],
        ..RateLimitSnapshot::default()
    });

    let collapsed = screen(
        &usage::render(std::slice::from_ref(&r), 70, &Theme::plain(), MAX_AGE),
        70,
    );
    assert_eq!(
        collapsed.matches("95%").count(),
        1,
        "the minute window is the collapsed cell, not a row as well: {collapsed}"
    );
    assert!(!collapsed.contains("minute"), "{collapsed}");

    let wide = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain(), MAX_AGE),
        100,
    );
    assert!(
        wide.contains("minute 95%"),
        "the named columns have no minute, so the row survives: {wide}"
    );
}
