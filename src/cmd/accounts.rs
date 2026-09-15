use crate::cli::{AccountsArgs, AccountsCmd};
use crate::cmd::{Ctx, parse_duration};
use crate::dispatch::account::{AccountState, Health};
use crate::dispatch::persist::{self, StateMap};
use crate::model::core::{AccountId, LimitScope, Provider};
use serde_json::json;
use time::OffsetDateTime;

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
            edit(ctx, &path, id, |s| {
                s.cooldown_until = None;
                s.consecutive_infra_failures = 0;
                s.health = Health::Healthy;
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
        AccountsCmd::Reset => {
            persist::save_state(&path, &StateMap::new())?;
            println!("account state reset");
            Ok(0)
        }
    }
}

fn list(ctx: &Ctx) -> anyhow::Result<i32> {
    let state = persist::load_state(&ctx.paths.accounts_state())?;
    let now = OffsetDateTime::now_utc();

    if ctx.json {
        let rows: Vec<_> = ctx
            .cfg
            .accounts
            .iter()
            .map(|a| {
                let s = state.get(&a.id).cloned().unwrap_or_default();
                json!({
                    "provider": a.provider,
                    "account": a.id,
                    "exec": a.exec,
                    "health": s.health,
                    "inflight": s.inflight,
                    "max_concurrency": a.max_concurrency,
                    "five_hour": window(&s, LimitScope::FiveHour),
                    "seven_day": window(&s, LimitScope::SevenDay),
                    "cooldown_until": s.cooldown_until.map(|t| t.to_string()),
                    "nodes": s.lifetime_nodes,
                    "cost_usd": s.lifetime_cost_usd,
                })
            })
            .collect();
        ctx.out(&format!("{}\n", serde_json::to_string_pretty(&rows)?));
        return Ok(0);
    }

    let mut text = format!(
        "{:<10} {:<8} {:<14} {:<10} {:<9} {:<6} {:<6} {:<10} {:<6} {}\n",
        "PROVIDER", "ACCOUNT", "EXEC", "HEALTH", "INFLIGHT", "5H", "7D", "COOLDOWN", "NODES", "$"
    );
    for a in &ctx.cfg.accounts {
        let s = state.get(&a.id).cloned().unwrap_or_default();
        text.push_str(&format!(
            "{:<10} {:<8} {:<14} {:<10} {:<9} {:<6} {:<6} {:<10} {:<6} ~{:.2}{}\n",
            a.provider,
            a.id.0,
            a.exec,
            crate::ui::watch::health_word(s.health),
            format!("{}/{}", s.inflight, a.max_concurrency.unwrap_or(2)),
            util(window(&s, LimitScope::FiveHour)),
            util(window(&s, LimitScope::SevenDay)),
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
    ctx.out(&text);
    Ok(0)
}

async fn check(ctx: &Ctx, only: Option<&str>) -> anyhow::Result<i32> {
    let mut failed = 0;
    for a in &ctx.cfg.accounts {
        if only.is_some_and(|id| id != a.id.0) {
            continue;
        }
        let check = crate::doctor::probe_account(&a.exec, &a.env, a.id.0.clone()).await;
        if check.level == crate::doctor::Level::Error {
            failed += 1;
        }
        println!("{:<6} {:<10} {}", check.level.label(), a.id.0, check.detail);
    }
    Ok(if failed > 0 { 1 } else { 0 })
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
    f(state.entry(id).or_default());
    persist::save_state(path, &state)
}

/// Blank, never zero: `codex exec --json` reports no quota telemetry at all.
fn window(s: &AccountState, scope: LimitScope) -> Option<f64> {
    s.quota
        .as_ref()?
        .windows
        .iter()
        .find(|w| w.scope == scope)
        .map(|w| w.utilization)
}

fn util(v: Option<f64>) -> String {
    v.map(|u| format!("{u:.2}")).unwrap_or_else(|| "-".to_owned())
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
