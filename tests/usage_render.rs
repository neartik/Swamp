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
use swamp::model::core::{
    AccountId, LimitReached, LimitScope, LimitStatus, LimitWindow, Provider, RateLimitSnapshot,
    Usage,
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
            &usage::render(std::slice::from_ref(&r), width, &Theme::plain()),
            width,
        );
        assert!(body.contains("claude-main"), "{width}: {body}");
        assert!(body.contains("64%"), "{width}: {body}");
        assert!(body.contains("WINDOW"), "{width}: {body}");
    }
    let wide = screen(
        &usage::render(std::slice::from_ref(&r), 100, &Theme::plain()),
        100,
    );
    assert!(wide.contains("LIFETIME") && wide.contains("COST") && wide.contains("HEALTH"));

    let below_lifetime = screen(
        &usage::render(std::slice::from_ref(&r), 90, &Theme::plain()),
        90,
    );
    assert!(!below_lifetime.contains("LIFETIME"), "{below_lifetime}");
    assert!(below_lifetime.contains("COST"), "{below_lifetime}");

    let below_cost = screen(
        &usage::render(std::slice::from_ref(&r), 80, &Theme::plain()),
        80,
    );
    assert!(!below_cost.contains("COST"), "{below_cost}");
    assert!(
        below_cost.contains("7D") || below_cost.contains("64%"),
        "{below_cost}"
    );

    let collapsed = screen(
        &usage::render(std::slice::from_ref(&r), 70, &Theme::plain()),
        70,
    );
    assert!(!collapsed.contains("7D"), "{collapsed}");
    assert!(collapsed.contains("HEALTH"), "{collapsed}");

    let narrow = screen(
        &usage::render(std::slice::from_ref(&r), 62, &Theme::plain()),
        62,
    );
    assert!(!narrow.contains("HEALTH"), "{narrow}");
    assert!(narrow.contains("claude-main"), "{narrow}");
}

/// Acceptance 2: no quota renders `-` and never a bare `0%`; an estimated one gets `~`.
#[test]
fn missing_quota_is_a_dash_and_an_estimate_carries_a_tilde() {
    let bare = row("claude-main", Provider::Anthropic, Health::Healthy);
    let body = screen(&usage::render(&[bare], 100, &Theme::plain()), 100);
    assert!(!body.contains("0%"), "{body}");
    assert!(body.contains(" - "), "{body}");

    let est = with_seven_day(
        row("codex-alt", Provider::Openai, Health::Healthy),
        0.02,
        false,
    );
    let body = screen(&usage::render(&[est], 100, &Theme::plain()), 100);
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
    let body = screen(&usage::render(&[cooling], 100, &Theme::plain()), 100);
    assert!(body.contains("until"), "{body}");
    assert!(body.contains("rate_limit"), "{body}");

    let broken = row("claude-broke", Provider::Anthropic, Health::AuthBroken);
    let body = screen(&usage::render(&[broken], 100, &Theme::plain()), 100);
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
    let tokens = &acct["tokens"]["lifetime"];
    let billable = tokens["input_tokens"].as_u64().unwrap()
        + tokens["cache_write_tokens"].as_u64().unwrap()
        + tokens["output_tokens"].as_u64().unwrap();
    assert_eq!(billable, 0);
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

/// Acceptance 6: a stale `quota_observed_at` renders its age in the `err` role.
#[test]
fn a_stale_observed_time_renders_in_the_err_role() {
    let mut r = row("claude-main", Provider::Anthropic, Health::Healthy);
    r.quota_observed_at = Some(OffsetDateTime::now_utc() - time::Duration::minutes(5));
    r.quota_source = Some(QuotaSource::Telemetry);
    let theme = Theme::plain();
    let lines = usage::render(&[r], 100, &theme);
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
    let lines = usage::render(&[r], 100, &Theme::plain());
    for l in &lines {
        for s in &l.spans {
            assert!(!s.content.contains('\u{1b}'), "{:?}", s.content);
        }
    }
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
    let cli_text = screen(&usage::render(&rows, width, &Theme::plain()), width);

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
