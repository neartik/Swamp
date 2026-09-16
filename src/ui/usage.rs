//! `/usage` and `swamp usage`: one renderer shared byte-for-byte by the chat block and the
//! CLI. `render` draws the §3.1 table; `json` builds the §3.3 shape. Neither does any I/O.

use crate::config::schema::AccountCfg;
use crate::dispatch::account::{AccountState, Health, QuotaSource};
use crate::model::core::{
    AccountId, CostBasis, LimitReached, LimitScope, LimitWindow, Provider, RateLimitSnapshot, Usage,
};
use crate::ui::chat::theme::{Role, Theme};
use crate::ui::{fmt, watch};
use ratatui::text::{Line, Span};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration as StdDuration;
use time::OffsetDateTime;
use unicode_width::UnicodeWidthStr;

const ACCOUNT_W: usize = 13;
const HEALTH_W: usize = 9;
const PCT_W: usize = 5;
const RESET_W: usize = 9;
const WINDOW_W: usize = 10;
const LIFETIME_W: usize = 10;
const COST_W: usize = 8;
const FLIGHT_W: usize = 6;

/// One row of the table. `provider: None` marks an account recorded in `accounts.json` but no
/// longer present in this repo's config, listed last under its own heading.
#[derive(Debug, Clone)]
pub struct AccountRow {
    pub provider: Option<Provider>,
    pub account: AccountId,
    pub exec: String,
    pub in_config: bool,
    pub health: Health,
    pub inflight: usize,
    pub max_concurrency: Option<usize>,
    pub cooldown_until: Option<OffsetDateTime>,
    pub quota: Option<RateLimitSnapshot>,
    pub quota_buckets: BTreeMap<String, RateLimitSnapshot>,
    pub quota_observed_at: Option<OffsetDateTime>,
    pub quota_source: Option<QuotaSource>,
    pub window_tokens: Usage,
    pub window_started_at: Option<OffsetDateTime>,
    pub lifetime_tokens: Usage,
    pub lifetime_nodes: u64,
    pub cost_usd: f64,
    pub cost_basis: Option<CostBasis>,
}

/// Builds rows the same way `AccountPool::snapshot()` would: one row per configured account,
/// ordered by account id like the pool's own map, plus one per account the state file
/// remembers but the config no longer names. The ordering lives here so the chat block and
/// the CLI cannot diverge on it.
pub fn rows_from(
    accounts: &[AccountCfg],
    pool: &[(Provider, AccountId, AccountState)],
    stale: &[(AccountId, AccountState)],
) -> Vec<AccountRow> {
    let mut sorted: Vec<&(Provider, AccountId, AccountState)> = pool.iter().collect();
    sorted.sort_by(|a, b| a.1.cmp(&b.1));
    let mut out: Vec<AccountRow> = sorted
        .iter()
        .map(|(provider, id, s)| {
            let cfg = accounts.iter().find(|a| &a.id == id);

            row_of(
                Some(*provider),
                id.clone(),
                cfg.map(|a| a.exec.clone()).unwrap_or_default(),
                cfg.and_then(|a| a.max_concurrency),
                true,
                s,
            )
        })
        .collect();
    out.extend(
        stale
            .iter()
            .map(|(id, s)| row_of(None, id.clone(), String::new(), None, false, s)),
    );
    out
}

fn row_of(
    provider: Option<Provider>,
    account: AccountId,
    exec: String,
    max_concurrency: Option<usize>,
    in_config: bool,
    s: &AccountState,
) -> AccountRow {
    AccountRow {
        provider,
        account,
        exec,
        in_config,
        health: s.health,
        inflight: s.inflight,
        max_concurrency,
        cooldown_until: s.cooldown_until,
        quota: s.quota.clone(),
        quota_buckets: s.quota_buckets.clone(),
        quota_observed_at: s.quota_observed_at,
        quota_source: s.quota_source,
        window_tokens: s.window_tokens,
        window_started_at: s.window_started_at,
        lifetime_tokens: s.lifetime_tokens,
        lifetime_nodes: s.lifetime_nodes,
        cost_usd: s.lifetime_cost_usd,
        cost_basis: s.lifetime_cost_basis,
    }
}

#[derive(Debug, Clone, Copy)]
struct Layout {
    show_health: bool,
    show_lifetime: bool,
    show_cost: bool,
    collapse_resets: bool,
}

fn layout_for(width: u16) -> Layout {
    Layout {
        show_health: width >= 66,
        show_lifetime: width >= 96,
        show_cost: width >= 86,
        collapse_resets: width < 78,
    }
}

/// The §3.1 table: one section per provider, `not in config` listed last, then `observed` and
/// `totals`. No I/O; `rows` is already in memory.
pub fn render(
    rows: &[AccountRow],
    width: u16,
    theme: &Theme,
    max_age: StdDuration,
) -> Vec<Line<'static>> {
    let now = OffsetDateTime::now_utc();
    let layout = layout_for(width);
    // `None` is the `not in config` group and sorts last, not first: BTreeMap would put it
    // ahead of every provider, which is the opposite of what the spec asks for.
    let mut groups: BTreeMap<(bool, Option<Provider>), Vec<&AccountRow>> = BTreeMap::new();
    for r in rows {
        groups
            .entry((r.provider.is_none(), r.provider))
            .or_default()
            .push(r);
    }
    let mut out = Vec::new();
    let mut first = true;
    for ((_, provider), group) in &groups {
        if group.is_empty() {
            continue;
        }
        if !first {
            out.push(Line::from(String::new()));
        }
        first = false;
        let head = match provider {
            Some(p) => p.as_str().to_owned(),
            None => "not in config (drop with swamp accounts reset <id>)".to_owned(),
        };
        out.push(Line::from(
            theme.span(fmt::truncate(&head, width as usize), Role::Name),
        ));
        out.push(header_line(&layout, theme, width));
        for r in group {
            out.extend(account_lines(r, &layout, theme, now, width));
        }
    }
    if !rows.is_empty() {
        out.push(Line::from(String::new()));
        out.extend(observed_lines(rows, width, theme, now, max_age));
        out.extend(totals_lines(rows, width, theme));
    }
    out
}

fn header_line(l: &Layout, theme: &Theme, width: u16) -> Line<'static> {
    let mut cells = Vec::new();
    let account_w = if l.show_health {
        ACCOUNT_W
    } else {
        ACCOUNT_W + 2
    };
    cells.push(left("ACCOUNT", account_w));
    if l.show_health {
        cells.push(left("HEALTH", HEALTH_W));
    }
    if l.collapse_resets {
        cells.push(right("%", PCT_W));
        cells.push(right("RESETS", RESET_W));
    } else {
        cells.push(right("5H", PCT_W));
        cells.push(right("RESETS", RESET_W));
        cells.push(right("7D", PCT_W));
        cells.push(right("RESETS", RESET_W));
    }
    cells.push(right("WINDOW", WINDOW_W));
    if l.show_lifetime {
        cells.push(right("LIFETIME", LIFETIME_W));
    }
    if l.show_cost {
        cells.push(right("COST", COST_W));
    }
    cells.push(right("FLIGHT", FLIGHT_W));
    Line::from(theme.span(
        fmt::truncate(&format!("  {}", cells.join(" ")), width as usize),
        Role::Meta,
    ))
}

fn account_lines(
    r: &AccountRow,
    l: &Layout,
    theme: &Theme,
    now: OffsetDateTime,
    width: u16,
) -> Vec<Line<'static>> {
    let shown = shown_health(r, now);
    let role = health_role(shown);
    let word = health_cell(shown);
    let glyph = health_glyph(shown);
    let mut cells = Vec::new();
    if l.show_health {
        cells.push(left(&r.account.0, ACCOUNT_W));
        cells.push(left(word, HEALTH_W));
    } else {
        let name = format!("{glyph} {}", r.account.0);
        cells.push(left(&name, ACCOUNT_W + 2));
    }
    let mut collapsed = None;
    if l.collapse_resets {
        let w = r.quota.as_ref().and_then(|q| q.tightest_at(now));
        collapsed = w;
        cells.push(right(&pct_cell(w), PCT_W));
        cells.push(right(&reset_cell(w, now), RESET_W));
    } else {
        let w5 = window_for(&r.quota, LimitScope::FiveHour, now);
        let w7 = window_for(&r.quota, LimitScope::SevenDay, now);
        cells.push(right(&pct_cell(w5), PCT_W));
        cells.push(right(&reset_cell(w5, now), RESET_W));
        cells.push(right(&pct_cell(w7), PCT_W));
        cells.push(right(&reset_cell(w7, now), RESET_W));
    }
    cells.push(right(&fmt::tokens(r.window_tokens.billable()), WINDOW_W));
    if l.show_lifetime {
        cells.push(right(
            &fmt::tokens(r.lifetime_tokens.billable()),
            LIFETIME_W,
        ));
    }
    if l.show_cost {
        cells.push(right(&cost_cell(r.cost_usd), COST_W));
    }
    let cap = r
        .max_concurrency
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".to_owned());
    cells.push(right(&format!("{}/{cap}", r.inflight), FLIGHT_W));

    let mut out = vec![Line::from(theme.span(
        fmt::truncate(&format!("  {}", cells.join(" ")), width as usize),
        role,
    ))];
    let indent = " ".repeat(2 + ACCOUNT_W + 1);
    let mut push = |text: String| {
        out.push(Line::from(theme.span(
            fmt::truncate(&format!("{indent}{text}"), width as usize),
            Role::Meta,
        )));
    };
    if let Some(cont) = status_continuation(r, now) {
        push(cont);
    }
    for w in extra_windows(&r.quota, now, collapsed) {
        push(format!(
            "{} {} \u{b7} resets {}",
            window_label(w),
            pct_cell(Some(w)),
            reset_cell(Some(w), now)
        ));
    }
    out
}

/// The health word the row shows, shared with `swamp accounts` so the two cannot disagree.
fn shown_health(r: &AccountRow, now: OffsetDateTime) -> Health {
    crate::ui::watch::shown_health(r.health, r.cooldown_until, now)
}

/// The gates `block_reason` applies that `Health` cannot express: the account is refused by
/// the provider and no timer brings it back.
fn parked_reason(r: &AccountRow) -> Option<&'static str> {
    let q = r.quota.as_ref()?;
    match q.reached {
        Some(LimitReached::CreditsDepleted) => return Some("credits_depleted"),
        Some(LimitReached::SpendControl) => return Some("spend_control"),
        _ => {}
    }
    (q.ordinary_usage_allowed == Some(false)).then_some("ordinary usage refused")
}

fn status_continuation(r: &AccountRow, now: OffsetDateTime) -> Option<String> {
    let health = shown_health(r, now);
    let refused = parked_reason(r)
        .map(|w| format!("{w} \u{b7} "))
        .unwrap_or_default();
    // The hard gates first, exactly as `pool::block_reason` orders them: an account the
    // provider refuses must never advertise an `until HH:MM` that brings nobody back.
    if health != Health::AuthBroken
        && let Some(w) = parked_reason(r)
    {
        return Some(format!("{w} \u{b7} no timer clears this"));
    }
    match health {
        Health::AuthBroken if r.exec.is_empty() => {
            return Some(format!(
                "auth broken \u{b7} {refused}drop with swamp accounts reset <id>"
            ));
        }
        Health::AuthBroken => {
            return Some(format!(
                "auth broken \u{b7} {refused}re-auth {}",
                fmt::sanitize(&r.exec)
            ));
        }
        Health::Cooling => {
            let until = r.cooldown_until?;
            let reason = r.quota.as_ref().and_then(|q| q.reached).map(reached_word);
            // A cooldown that crosses midnight is a bare HH:MM the user reads as the past.
            return Some(match reason {
                Some(w) => format!("until {} \u{b7} {w}", fmt::clock_day(until, now)),
                None => format!("until {}", fmt::clock_day(until, now)),
            });
        }
        _ => {}
    }
    None
}

fn reached_word(r: LimitReached) -> &'static str {
    match r {
        LimitReached::RateLimit => "rate_limit",
        LimitReached::CreditsDepleted => "credits_depleted",
        LimitReached::SpendControl => "spend_control",
    }
}

/// Rows the two named columns have no room for: `Minute`, or an `Unknown` window that still
/// carries `window_minutes`. `shown` is the window the collapsed layout already put in the
/// percentage cell, which must not be stated twice.
fn extra_windows<'a>(
    q: &'a Option<RateLimitSnapshot>,
    now: OffsetDateTime,
    shown: Option<&LimitWindow>,
) -> Vec<&'a LimitWindow> {
    q.as_ref()
        .map(|q| {
            q.windows
                .iter()
                .filter(|w| w.is_current(now))
                .filter(|w| !shown.is_some_and(|s| std::ptr::eq(s, *w)))
                .filter(|w| {
                    w.scope == LimitScope::Minute
                        || (w.scope == LimitScope::Unknown && w.window_minutes.is_some())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn window_label(w: &LimitWindow) -> String {
    match w.scope {
        LimitScope::Minute => "minute".to_owned(),
        _ => format!("{}m window", w.window_minutes.unwrap_or(0)),
    }
}

/// A window whose reset has passed measures an allowance that has already rolled: dispatch
/// ignores it, so the table must not report its percentage either.
fn window_for(
    q: &Option<RateLimitSnapshot>,
    scope: LimitScope,
    now: OffsetDateTime,
) -> Option<&LimitWindow> {
    q.as_ref()?
        .windows
        .iter()
        .find(|w| w.scope == scope && w.is_current(now))
}

/// A `~` prefix marks an estimated number; a measured one never carries it.
fn pct_cell(w: Option<&LimitWindow>) -> String {
    match w {
        None => "-".to_owned(),
        Some(w) => {
            let p = (w.utilization * 100.0).round() as i64;
            if w.measured {
                format!("{p}%")
            } else {
                format!("~{p}%")
            }
        }
    }
}

fn reset_cell(w: Option<&LimitWindow>, now: OffsetDateTime) -> String {
    let Some(w) = w else {
        return "-".to_owned();
    };
    let Some(at) = w.resets_at else {
        return "-".to_owned();
    };
    let secs = (at - now).whole_seconds().max(0) as u64;
    let s = format!("in {}", fmt::until(StdDuration::from_secs(secs)));
    if w.measured { s } else { format!("~{s}") }
}

/// `-`, never `$0.00`: an account that has spent nothing looks the same as one with no cost
/// data at all, and both mean "nothing to show here". A four-figure lifetime is abbreviated
/// like the token columns, or the eight-column cell would cut the digits that carry it.
fn cost_cell(usd: f64) -> String {
    if usd <= 0.0 {
        return "-".to_owned();
    }
    if usd < 1000.0 {
        return format!("~${usd:.2}");
    }
    format!("~${}", fmt::tokens(usd.round() as u64))
}

fn health_role(h: Health) -> Role {
    match h {
        Health::Healthy => Role::Ok,
        Health::Degraded => Role::Accent,
        Health::Cooling => Role::Meta,
        Health::AuthBroken | Health::Disabled => Role::Err,
    }
}

/// `watch::health_word` inside the documented nine-column HEALTH budget: `auth-broken` is eleven
/// columns and would be clipped mid-word, so the cell names the action instead.
fn health_cell(h: Health) -> &'static str {
    match h {
        Health::AuthBroken => "no-auth",
        h => watch::health_word(h),
    }
}

/// A one-column stand-in for the HEALTH word once the terminal is too narrow to keep it.
fn health_glyph(h: Health) -> &'static str {
    match h {
        Health::Healthy => "+",
        Health::Degraded => "!",
        Health::Cooling => "~",
        Health::AuthBroken => "x",
        Health::Disabled => "-",
    }
}

fn left(s: &str, w: usize) -> String {
    fmt::pad(s, w)
}

fn right(s: &str, w: usize) -> String {
    let c = fmt::truncate(s, w);
    format!("{}{c}", " ".repeat(w.saturating_sub(c.width())))
}

fn observed_lines(
    rows: &[AccountRow],
    width: u16,
    theme: &Theme,
    now: OffsetDateTime,
    max_age: StdDuration,
) -> Vec<Line<'static>> {
    let entries: Vec<(String, bool)> = rows
        .iter()
        .filter(|r| r.in_config)
        .map(|r| {
            let id = fmt::sanitize(&r.account.0);
            match (r.quota_observed_at, r.quota_source) {
                (Some(at), Some(src)) => {
                    let secs = (now - at).whole_seconds().max(0) as u64;
                    let stale = StdDuration::from_secs(secs) > max_age;
                    (
                        format!(
                            "{id} {} ago {}",
                            fmt::until(StdDuration::from_secs(secs)),
                            src.as_str()
                        ),
                        stale,
                    )
                }
                _ => (format!("{id} estimated"), false),
            }
        })
        .collect();
    if entries.is_empty() {
        return Vec::new();
    }
    let lead = "observed  ";
    let indent = " ".repeat(lead.width());
    // Every other row is width-bounded, and chat commits fixed-width lines to scrollback.
    let cap = (width as usize).saturating_sub(indent.width());
    let mut out = Vec::new();
    let mut spans: Vec<Span<'static>> = vec![theme.span(lead.to_owned(), Role::Meta)];
    let mut used = lead.width();
    for (i, (text, stale)) in entries.iter().enumerate() {
        let text = fmt::truncate(text, cap);
        let sep = if i == 0 { "" } else { " \u{b7} " };
        let role = if *stale { Role::Err } else { Role::Meta };
        if used + sep.width() + text.width() > width as usize && used > indent.width() {
            out.push(Line::from(std::mem::take(&mut spans)));
            spans = vec![theme.span(indent.clone(), Role::Meta)];
            used = indent.width();
            spans.push(theme.span(text.clone(), role));
            used += text.width();
            continue;
        }
        if !sep.is_empty() {
            spans.push(theme.span(sep.to_owned(), Role::Meta));
            used += sep.width();
        }
        spans.push(theme.span(text.clone(), role));
        used += text.width();
    }
    out.push(Line::from(spans));
    out
}

/// Whether the row's percentages came from the provider. `Estimated` is Swamp's own
/// arithmetic, so it counts as no source at all, exactly as `doctor` counts it.
fn has_measured_source(r: &AccountRow) -> bool {
    r.quota_source.is_some_and(|s| s.is_measured())
}

/// The footer sheds with the table: at 62 columns a fixed-width totals line wraps into three
/// ragged ones in the CLI and is cut mid-number in chat.
fn totals_lines(rows: &[AccountRow], width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut usage = Usage::default();
    let mut cost = 0.0;
    let mut missing = 0usize;
    for r in rows.iter().filter(|r| r.in_config) {
        usage.absorb(&r.lifetime_tokens);
        cost += r.cost_usd;
        if !has_measured_source(r) {
            missing += 1;
        }
    }
    let mut parts = vec![
        format!("in {}", fmt::tokens(usage.input_tokens)),
        format!("out {}", fmt::tokens(usage.output_tokens)),
        format!("cache-read {}", fmt::tokens(usage.cached_input_tokens)),
        format!("cache-write {}", fmt::tokens(usage.cache_write_tokens)),
    ];
    let mut cost_part = Some(format!("~${cost:.2}"));
    let compose = |parts: &[String], cost: &Option<String>| {
        let mut s = format!("totals    {}", parts.join("  "));
        if let Some(c) = cost {
            s.push_str(&format!("  \u{b7}  {c}"));
        }
        s
    };
    let mut line = compose(&parts, &cost_part);
    while line.width() > width as usize {
        if parts.len() > 2 {
            parts.pop();
        } else if cost_part.is_some() {
            cost_part = None;
        } else {
            line = fmt::truncate(&line, width as usize);
            break;
        }
        line = compose(&parts, &cost_part);
    }
    let mut out = vec![Line::from(theme.span(line, Role::Meta))];
    if missing > 0 {
        let clause = if missing == 1 {
            "account has no quota source; its utilization is estimated"
        } else {
            "accounts have no quota source; their utilization is estimated"
        };
        let note = format!("          {missing} {clause}");
        out.push(Line::from(
            theme.span(fmt::truncate(&note, width as usize), Role::Meta),
        ));
    }
    out
}

/// The §3.3 JSON shape, shared by `swamp usage --json` and `/usage --json`.
pub fn json(rows: &[AccountRow]) -> Value {
    let now = OffsetDateTime::now_utc();
    let accounts: Vec<Value> = rows.iter().map(|r| account_json(r, now)).collect();

    let mut usage = Usage::default();
    let mut cost = 0.0;
    let mut missing = 0usize;
    let mut cost_complete = true;
    for r in rows.iter().filter(|r| r.in_config) {
        usage.absorb(&r.lifetime_tokens);
        cost += r.cost_usd;
        if !has_measured_source(r) {
            missing += 1;
        }
        if r.provider == Some(Provider::Openai) && r.cost_usd == 0.0 && r.lifetime_nodes > 0 {
            cost_complete = false;
        }
    }
    json!({
        "at": now.format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        "accounts": accounts,
        "totals": {
            "usage": usage,
            "cost_usd": cost,
            "cost_complete": cost_complete,
            "accounts_without_quota_source": missing,
        },
    })
}

/// A window whose reset has passed measures an allowance that already rolled: the table and
/// `swamp accounts --json` drop it, so the JSON view must not quote it as current either.
fn current_only(q: &RateLimitSnapshot, now: OffsetDateTime) -> RateLimitSnapshot {
    let mut q = q.clone();
    q.windows.retain(|w| w.is_current(now));
    q
}

fn account_json(r: &AccountRow, now: OffsetDateTime) -> Value {
    let quota = r.quota.as_ref().map(|q| {
        let mut v = serde_json::to_value(current_only(q, now)).unwrap_or(Value::Null);
        if let Some(obj) = v.as_object_mut() {
            // The serde spelling, so the journal, accounts.json and this agree.
            obj.insert("source".to_owned(), json!(r.quota_source));
            obj.insert(
                "observed_at".to_owned(),
                json!(r.quota_observed_at.and_then(rfc3339)),
            );
            obj.insert(
                "age_s".to_owned(),
                json!(
                    r.quota_observed_at
                        .map(|at| (now - at).whole_seconds().max(0))
                ),
            );
        }
        v
    });
    // A pricing-table multiplication is not provider truth: a state file written before the
    // basis was recorded falls back to what the provider can report at all.
    let cost_basis = (r.cost_usd > 0.0).then(|| {
        r.cost_basis.unwrap_or(match r.provider {
            Some(Provider::Openai) => CostBasis::Estimated,
            _ => CostBasis::Reported,
        })
    });
    json!({
        "provider": r.provider.map(|p| p.as_str()),
        "account": r.account.0,
        "exec": r.exec,
        "health": shown_health(r, now),
        "in_config": r.in_config,
        "inflight": r.inflight,
        "max_concurrency": r.max_concurrency,
        "cooldown_until": r.cooldown_until.and_then(rfc3339),
        "quota": quota,
        "quota_buckets": r.quota_buckets
            .iter()
            .map(|(id, q)| (id.clone(), current_only(q, now)))
            .collect::<BTreeMap<_, _>>(),
        "tokens": {
            "window": tokens_json(&r.window_tokens),
            "window_started_at": r.window_started_at.and_then(rfc3339),
            "lifetime": tokens_json(&r.lifetime_tokens),
        },
        "nodes": r.lifetime_nodes,
        "cost_usd": r.cost_usd,
        "cost_basis": cost_basis,
    })
}

/// `billable` is documented as part of the shape, and `Usage` only computes it.
fn tokens_json(u: &Usage) -> Value {
    let mut v = serde_json::to_value(u).unwrap_or(Value::Null);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("billable".to_owned(), json!(u.billable()));
    }
    v
}

pub fn rfc3339(t: OffsetDateTime) -> Option<String> {
    t.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::policy::DEFAULT_QUOTA_MAX_AGE as MAX_AGE;
    use crate::model::core::{LimitStatus, Usage};

    fn row(id: &str, health: Health) -> AccountRow {
        AccountRow {
            provider: Some(Provider::Anthropic),
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

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        crate::ui::chat::blocks::text_of(lines)
    }

    #[test]
    fn a_row_with_no_quota_never_prints_a_bare_zero_percent() {
        let rows = vec![row("claude-main", Health::Healthy)];
        let lines = text(&render(&rows, 100, &Theme::plain(), MAX_AGE));
        let body = lines.join("\n");
        assert!(body.contains("claude-main"));
        assert!(!body.contains("0%"), "{body}");
    }

    #[test]
    fn an_estimated_window_carries_a_leading_tilde() {
        let mut r = row("codex-alt", Health::Healthy);
        r.provider = Some(Provider::Openai);
        r.quota = Some(RateLimitSnapshot {
            status: LimitStatus::Allowed,
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.02,
                resets_at: Some(OffsetDateTime::now_utc() + time::Duration::days(6)),
                window_minutes: Some(10080),
                measured: false,
            }],
            ..RateLimitSnapshot::default()
        });
        let lines = text(&render(&[r], 100, &Theme::plain(), MAX_AGE));
        let body = lines.join("\n");
        assert!(body.contains("~2%"), "{body}");
        assert!(body.contains("~in"), "{body}");
    }

    #[test]
    fn a_cooling_account_gets_a_continuation_row() {
        let mut r = row("claude-work", Health::Cooling);
        r.cooldown_until = Some(OffsetDateTime::now_utc() + time::Duration::minutes(30));
        r.quota = Some(RateLimitSnapshot {
            reached: Some(LimitReached::RateLimit),
            ..RateLimitSnapshot::default()
        });
        let lines = text(&render(&[r], 100, &Theme::plain(), MAX_AGE));
        let body = lines.join("\n");
        assert!(body.contains("until"), "{body}");
        assert!(body.contains("rate_limit"), "{body}");
    }

    #[test]
    fn an_auth_broken_account_points_at_its_exec() {
        let r = row("claude-broke", Health::AuthBroken);
        let lines = text(&render(&[r], 100, &Theme::plain(), MAX_AGE));
        let body = lines.join("\n");
        assert!(body.contains("auth broken"), "{body}");
        assert!(body.contains("re-auth claude-broke-cli"), "{body}");
    }

    #[test]
    fn control_characters_in_an_account_id_never_reach_the_page() {
        let r = row("evil\u{1b}[2J", Health::Healthy);
        let lines = render(&[r], 100, &Theme::plain(), MAX_AGE);
        for l in &lines {
            for s in &l.spans {
                assert!(!s.content.contains('\u{1b}'), "{:?}", s.content);
            }
        }
    }

    #[test]
    fn json_round_trips_and_billable_matches_its_own_definition() {
        let mut r = row("claude-main", Health::Healthy);
        r.lifetime_tokens = Usage {
            input_tokens: 10,
            cached_input_tokens: 2,
            cache_write_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 0,
        };
        r.cost_usd = 1.5;
        let v = json(std::slice::from_ref(&r));
        let text = serde_json::to_string(&v).unwrap();
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v, back);
        let billable = back["accounts"][0]["tokens"]["lifetime"]["input_tokens"]
            .as_u64()
            .unwrap()
            + back["accounts"][0]["tokens"]["lifetime"]["cache_write_tokens"]
                .as_u64()
                .unwrap()
            + back["accounts"][0]["tokens"]["lifetime"]["output_tokens"]
                .as_u64()
                .unwrap();
        assert_eq!(billable, r.lifetime_tokens.billable());
    }

    #[test]
    fn width_drop_order_keeps_account_percentage_and_window() {
        let mut r = row("claude-main", Health::Healthy);
        r.quota = Some(RateLimitSnapshot {
            windows: vec![LimitWindow {
                scope: LimitScope::SevenDay,
                utilization: 0.64,
                resets_at: Some(OffsetDateTime::now_utc() + time::Duration::days(3)),
                window_minutes: Some(10080),
                measured: true,
            }],
            ..RateLimitSnapshot::default()
        });
        for width in [100u16, 86, 78, 62] {
            let body = text(&render(
                std::slice::from_ref(&r),
                width,
                &Theme::plain(),
                MAX_AGE,
            ))
            .join("\n");
            assert!(body.contains("claude-main"), "{width}: {body}");
            assert!(body.contains("64%"), "{width}: {body}");
        }
        let wide = text(&render(
            std::slice::from_ref(&r),
            100,
            &Theme::plain(),
            MAX_AGE,
        ))
        .join("\n");
        assert!(wide.contains("LIFETIME") && wide.contains("COST"));
        let narrow = text(&render(
            std::slice::from_ref(&r),
            62,
            &Theme::plain(),
            MAX_AGE,
        ))
        .join("\n");
        assert!(!narrow.contains("LIFETIME") && !narrow.contains("COST"));
        assert!(!narrow.contains("HEALTH"));
    }

    #[test]
    fn every_health_word_fits_the_health_column() {
        for h in [
            Health::Healthy,
            Health::Degraded,
            Health::Cooling,
            Health::AuthBroken,
            Health::Disabled,
        ] {
            assert!(
                health_cell(h).width() <= HEALTH_W,
                "{} overflows HEALTH_W",
                health_cell(h)
            );
        }
        let body = text(&render(
            &[row("claude-broke", Health::AuthBroken)],
            100,
            &Theme::plain(),
            MAX_AGE,
        ))
        .join("\n");
        assert!(body.contains("claude-broke  no-auth"), "{body}");
        assert!(!body.contains('\u{2026}'), "{body}");
    }
}
