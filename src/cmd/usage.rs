use crate::cli::UsageArgs;
use crate::cmd::Ctx;
use crate::config::resolve::expand_env;
use crate::dispatch::account::{AccountState, QuotaSource};
use crate::dispatch::persist;
use crate::dispatch::policy::Scoring;
use crate::dispatch::pool;
use crate::model::core::{AccountId, Provider, RateLimitSnapshot};
use crate::ui::chat::theme::Theme;
use crate::ui::usage;
use crate::worker::codex_quota;
use std::collections::BTreeMap;
use time::OffsetDateTime;

/// Reads `~/.swamp/accounts.json` under the `fs4` lock and renders the same table `/usage`
/// does. Never needs a running supervisor; a missing file renders an empty table, not an
/// error. An unreadable one IS an error: rendering zeros would read as "nothing was spent",
/// and `--probe` would then rewrite the file with only the accounts it probed.
pub async fn run(ctx: &Ctx, args: &UsageArgs) -> anyhow::Result<i32> {
    let path = ctx.paths.accounts_state();
    let mut state = persist::load_state(&path)?;

    if args.probe {
        let probed = probe_all(ctx).await;
        if !probed.is_empty() {
            let now = OffsetDateTime::now_utc();
            let warn_at = Scoring::from_config(&ctx.cfg).warn_at;
            let ids: Vec<AccountId> = probed.iter().map(|(id, ..)| id.clone()).collect();
            state = persist::update_state(&path, &ids, |id, entry| {
                let Some((_, buckets, snap)) = probed.iter().find(|(p, ..)| p == id) else {
                    return;
                };
                let was_gated = pool::hard_gated(entry);
                entry.apply_buckets(buckets);
                entry.apply_quota(snap.clone(), QuotaSource::AppServer, now);
                // The pool re-derives health on every reading it ingests; a probe that skipped
                // it would print a health word the snapshot under it contradicts.
                pool::health_after_quota(entry, was_gated, warn_at);
                entry.updated_at = Some(now);
            })
            .unwrap_or(state);
        }
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
    let lines = usage::render(&rows, width, &theme, ctx.cfg.quota_max_age());
    let text = crate::ui::chat::blocks::text_of(&lines).join("\n");
    ctx.out(&format!("{text}\n"));
    Ok(0)
}

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100)
}

/// One account's reading: every bucket it reported, plus the one it bills against.
type Probed = (
    AccountId,
    BTreeMap<String, RateLimitSnapshot>,
    RateLimitSnapshot,
);

/// One `account/rateLimits/read` per openai account, each capped at
/// `codex_quota::PROBE_TIMEOUT`; a timeout leaves the cached row exactly as it was, never an
/// error. Anthropic has no out-of-band source: its telemetry only arrives inside a worker
/// stream, so it is not probed here. Returns the readings rather than writing them, so the
/// caller can apply them to the file's own entries instead of to a pre-probe copy.
async fn probe_all(ctx: &Ctx) -> Vec<Probed> {
    let mut out = Vec::new();
    // `providers.openai.quota_source` is not a dispatch-only setting: "none" and "rollout"
    // both mean "do not spawn an app-server", whoever is asking.
    if !ctx.cfg.probes_app_server(Provider::Openai) {
        return out;
    }
    for a in ctx
        .cfg
        .accounts
        .iter()
        .filter(|a| a.provider == Provider::Openai)
    {
        let env = expand_env(&a.env);
        let model = codex_quota::quota_model(&ctx.cfg, &a.id);
        match tokio::time::timeout(
            codex_quota::PROBE_TIMEOUT,
            codex_quota::read_rate_limits(&a.exec, &env),
        )
        .await
        {
            Ok(Ok(read)) => {
                if let Some(snap) = read.select(a.limit_id.as_deref(), model.as_deref()) {
                    out.push((a.id.clone(), read.buckets.clone(), snap));
                }
            }
            Ok(Err(e)) => tracing::debug!("probing {} for quota: {e:#}", a.id.0),
            Err(_) => tracing::debug!("probing {} for quota timed out", a.id.0),
        }
    }
    out
}
