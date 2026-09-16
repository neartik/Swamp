use crate::cli::{AccountsArgs, AccountsCmd};
use crate::cmd::{Ctx, parse_duration};
use crate::dispatch::account::{AccountState, Health};
use crate::dispatch::persist::{self, StateMap};
use crate::dispatch::policy::Scoring;
use crate::dispatch::pool;
use crate::model::core::{AccountId, LimitScope, LimitWindow, Provider};
use serde_json::json;
use time::OffsetDateTime;

/// Column widths every row is padded and clipped to: an over-long id or exec used to push
/// every later column out of line.
const ACCOUNT_W: usize = 13;
const EXEC_W: usize = 14;

/// Account health and rotation state. Cross-run and cross-repo: the state file is the
/// authority, not this process.
pub async fn run(ctx: &Ctx, args: &AccountsArgs) -> anyhow::Result<i32> {
    let path = ctx.paths.accounts_state();
    match args.command.as_ref().unwrap_or(&AccountsCmd::List) {
        AccountsCmd::List => list(ctx),
        AccountsCmd::Check { account } => check(ctx, account.as_deref()).await,
        AccountsCmd::Cooldown { id, duration } => {
            let d = parse_duration(duration)?;
            edit(ctx, &path, id, |s| {
                s.cooldown_until = Some(OffsetDateTime::now_utc() + d);
                s.health = Health::Cooling;
            })?;
            println!("account {id} cooling for {duration}");
            Ok(0)
        }
        AccountsCmd::Clear { id } => {
            let warn_at = Scoring::from_config(&ctx.cfg).warn_at;
            edit(ctx, &path, id, |s| {
                s.clear_gates();
                s.health = pool::health_from_quota(s, warn_at);
            })?;
            println!("account {id} cleared");
            Ok(0)
        }
        AccountsCmd::Enable { id } => {
            edit(ctx, &path, id, |s| s.health = Health::Healthy)?;
            println!("account {id} enabled");
            Ok(0)
        }
        AccountsCmd::Disable { id } => {
            edit(ctx, &path, id, |s| s.health = Health::Disabled)?;
            println!("account {id} disabled");
            Ok(0)
        }
        AccountsCmd::Reset { id } => reset(&path, id.as_deref()),
    }
}

fn list(ctx: &Ctx) -> anyhow::Result<i32> {
    let state = persist::load_state(&ctx.paths.accounts_state())?;
    let now = OffsetDateTime::now_utc();

    if ctx.json {
        let mut rows: Vec<_> = ctx
            .cfg
            .accounts
            .iter()
            .map(|a| {
                let s = state.get(&a.id).cloned().unwrap_or_default();
                json!({
                    "provider": a.provider,
                    "account": a.id,
                    "exec": a.exec,
                    "health": crate::ui::watch::shown_health(s.health, s.cooldown_until, now),
                    "inflight": s.inflight,
                    "max_concurrency": a.max_concurrency,
                    "five_hour": window(&s, LimitScope::FiveHour, now).map(|w| w.utilization),
                    "seven_day": window(&s, LimitScope::SevenDay, now).map(|w| w.utilization),
                    "cooldown_until": s.cooldown_until.map(|t| t.to_string()),
                    "nodes": s.lifetime_nodes,
                    "cost_usd": s.lifetime_cost_usd,
                    "in_config": true,
                })
            })
            .collect();
        for (id, s) in stale(ctx, &state) {
            rows.push(json!({
                "account": id,
                "health": crate::ui::watch::shown_health(s.health, s.cooldown_until, now),
                "consecutive_infra_failures": s.consecutive_infra_failures,
                "cooldown_until": s.cooldown_until.map(|t| t.to_string()),
                "nodes": s.lifetime_nodes,
                "cost_usd": s.lifetime_cost_usd,
                "in_config": false,
            }));
        }
        ctx.out(&format!("{}\n", serde_json::to_string_pretty(&rows)?));
        return Ok(0);
    }

    let mut text = format!(
        "{:<10} {:<13} {:<14} {:<10} {:<9} {:<6} {:<6} {:<10} {:<6} {}\n",
        "PROVIDER", "ACCOUNT", "EXEC", "HEALTH", "INFLIGHT", "5H", "7D", "COOLDOWN", "NODES", "$"
    );
    for a in &ctx.cfg.accounts {
        let s = state.get(&a.id).cloned().unwrap_or_default();
        let cap = a
            .max_concurrency
            .map(|c| c.to_string())
            .unwrap_or_else(|| "-".to_owned());
        text.push_str(&format!(
            "{:<10} {:<13} {:<14} {:<10} {:<9} {:<6} {:<6} {:<10} {:<6} ~{:.2}{}\n",
            // `Display for Provider` ignores the formatter's width, so pad the &str instead.
            a.provider.as_str(),
            crate::ui::fmt::truncate(&a.id.0, ACCOUNT_W),
            crate::ui::fmt::truncate(&a.exec, EXEC_W),
            crate::ui::watch::health_word(crate::ui::watch::shown_health(
                s.health,
                s.cooldown_until,
                now,
            )),
            format!("{}/{cap}", s.inflight),
            util(window(&s, LimitScope::FiveHour, now)),
            util(window(&s, LimitScope::SevenDay, now)),
            cooldown(&s, now),
            s.lifetime_nodes,
            s.lifetime_cost_usd,
            if a.provider == Provider::Openai {
                " est"
            } else {
                ""
            },
        ));
    }
    if ctx.cfg.accounts.is_empty() {
        text.push_str("no accounts configured\n");
    }
    let stale = stale(ctx, &state);
    if !stale.is_empty() {
        text.push_str("\nnot in config (state kept; drop with `swamp accounts reset <id>`):\n");
        for (id, s) in stale {
            text.push_str(&format!(
                "{:<10} {:<13} {:<14} {:<10} {:<9} {:<6} {:<6} {:<10} {:<6} ~{:.2}\n",
                "-",
                crate::ui::fmt::truncate(&id.0, ACCOUNT_W),
                "-",
                crate::ui::watch::health_word(crate::ui::watch::shown_health(
                    s.health,
                    s.cooldown_until,
                    now,
                )),
                "-",
                util(window(&s, LimitScope::FiveHour, now)),
                util(window(&s, LimitScope::SevenDay, now)),
                cooldown(&s, now),
                s.lifetime_nodes,
                s.lifetime_cost_usd,
            ));
        }
    }
    ctx.out(&text);
    Ok(0)
}

/// Entries for accounts this config does not name. The file is machine-wide, so another
/// repo may still own them: they are listed, never dropped on their own.
fn stale(ctx: &Ctx, state: &StateMap) -> Vec<(AccountId, AccountState)> {
    state
        .iter()
        .filter(|(id, _)| ctx.cfg.account(id).is_none())
        .map(|(id, s)| (id.clone(), s.clone()))
        .collect()
}

async fn check(ctx: &Ctx, only: Option<&str>) -> anyhow::Result<i32> {
    let mut failed = 0;
    for a in &ctx.cfg.accounts {
        if only.is_some_and(|id| id != a.id.0) {
            continue;
        }
        let check =
            crate::doctor::probe_account(&ctx.cfg, a, &ctx.paths.repo, a.id.0.clone()).await;
        if check.level == crate::doctor::Level::Error {
            failed += 1;
        }
        println!("{:<6} {:<10} {}", check.level.label(), a.id.0, check.detail);
    }
    Ok(if failed > 0 { 1 } else { 0 })
}

/// With an id, drops that one entry: this is how an account that was renamed out of the
/// config leaves the machine-wide state file, which nothing else is allowed to do silently.
fn reset(path: &camino::Utf8Path, id: Option<&str>) -> anyhow::Result<i32> {
    let Some(id) = id else {
        persist::save_state(path, &StateMap::new())?;
        println!("account state reset");
        return Ok(0);
    };
    let mut state = persist::load_state(path)?;
    anyhow::ensure!(
        state.remove(&AccountId(id.to_owned())).is_some(),
        "no recorded state for account `{id}`"
    );
    persist::save_state(path, &state)?;
    println!("dropped the recorded state of account {id}");
    Ok(0)
}

fn edit(
    ctx: &Ctx,
    path: &camino::Utf8Path,
    id: &str,
    f: impl FnOnce(&mut AccountState),
) -> anyhow::Result<()> {
    let id = AccountId(id.to_owned());
    anyhow::ensure!(
        ctx.cfg.account(&id).is_some(),
        "no account `{}` in the configuration",
        id.0
    );
    let mut state = persist::load_state(path)?;
    let entry = state.entry(id).or_default();
    f(entry);
    // Every cross-process hand-off is keyed on `updated_at`: without the bump a running
    // supervisor neither adopts the edit nor yields to it, and overwrites it on its next flush.
    entry.updated_at = Some(OffsetDateTime::now_utc());
    persist::merge_state(path, &state).map(|_| ())
}

/// Blank, never zero: `codex exec --json` reports no quota telemetry at all. A window whose
/// reset has passed measures an allowance that has already rolled, so it is not a window.
fn window(s: &AccountState, scope: LimitScope, now: OffsetDateTime) -> Option<&LimitWindow> {
    s.quota
        .as_ref()?
        .windows
        .iter()
        .find(|w| w.scope == scope && w.is_current(now))
}

/// A `~` prefix marks an estimated number, the way `/usage` renders it.
fn util(w: Option<&LimitWindow>) -> String {
    match w {
        None => "-".to_owned(),
        Some(w) if w.measured => format!("{:.2}", w.utilization),
        Some(w) => format!("~{:.2}", w.utilization),
    }
}

fn cooldown(s: &AccountState, now: OffsetDateTime) -> String {
    match s.cooldown_until {
        Some(t) if t > now => {
            let left: std::time::Duration = (t - now).try_into().unwrap_or_default();
            format!("in {}", crate::ui::fmt::duration(left))
        }
        _ => "-".to_owned(),
    }
}
