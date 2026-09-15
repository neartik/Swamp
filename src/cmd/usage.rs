use crate::cli::UsageArgs;
use crate::cmd::Ctx;
use crate::config::resolve::expand_env;
use crate::dispatch::account::{AccountState, QuotaSource};
use crate::dispatch::persist::{self, StateMap};
use crate::model::core::{AccountId, Provider};
use crate::ui::chat::theme::Theme;
use crate::ui::usage;
use crate::worker::codex_quota;
use time::OffsetDateTime;

/// Reads `~/.swamp/accounts.json` under the `fs4` lock and renders the same table `/usage`
/// does. Never needs a running supervisor; a missing file renders an empty table, not an error.
pub async fn run(ctx: &Ctx, args: &UsageArgs) -> anyhow::Result<i32> {
    let path = ctx.paths.accounts_state();
    let mut state = persist::load_state(&path).unwrap_or_default();

    if args.probe {
        probe_all(ctx, &mut state).await;
        state = persist::merge_state(&path, &state).unwrap_or(state);
    }

    let pool: Vec<(Provider, AccountId, AccountState)> = ctx
        .cfg
        .accounts
        .iter()
        .map(|a| {
            (
                a.provider,
                a.id.clone(),
                state.get(&a.id).cloned().unwrap_or_default(),
            )
        })
        .collect();
    let stale: Vec<(AccountId, AccountState)> = state
        .iter()
        .filter(|(id, _)| ctx.cfg.account(id).is_none())
        .map(|(id, s)| (id.clone(), s.clone()))
        .collect();
    let rows = usage::rows_from(&ctx.cfg.accounts, &pool, &stale);

    if ctx.json || args.json {
        ctx.out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&usage::json(&rows))?
        ));
        return Ok(0);
    }

    let width = terminal_width();
    let theme = Theme::detect(ctx.color, None);
    let lines = usage::render(&rows, width, &theme);
    let text = crate::ui::chat::blocks::text_of(&lines).join("\n");
    ctx.out(&format!("{text}\n"));
    Ok(0)
}

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100)
}

/// One `account/rateLimits/read` per openai account, each capped at
/// `codex_quota::PROBE_TIMEOUT`; a timeout leaves the cached row exactly as it was, never an
/// error. Anthropic has no out-of-band source: its telemetry only arrives inside a worker
/// stream, so it is not probed here.
async fn probe_all(ctx: &Ctx, state: &mut StateMap) {
    for a in ctx
        .cfg
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Openai)
    {
        let env = expand_env(&a.env);
        match tokio::time::timeout(
            codex_quota::PROBE_TIMEOUT,
            codex_quota::read_rate_limits(&a.exec, &env),
        )
        .await
        {
            Ok(Ok(read)) => {
                if let Some(snap) = read.select(a.limit_id.as_deref(), None) {
                    let now = OffsetDateTime::now_utc();
                    let entry = state.entry(a.id.clone()).or_default();
                    entry.apply_quota(snap, QuotaSource::AppServer, now);
                    entry.updated_at = Some(now);
                }
            }
            Ok(Err(e)) => tracing::debug!("probing {} for quota: {e:#}", a.id.0),
            Err(_) => tracing::debug!("probing {} for quota timed out", a.id.0),
        }
    }
}
