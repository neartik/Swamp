use super::{Check, Level};
use crate::config::Config;
use crate::dispatch::account::QuotaSource;
use crate::journal::paths::Paths;
use crate::model::core::{LimitScope, Provider, Tier};
use crate::model::failure::Failure;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use std::time::Duration;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_TURN_TIMEOUT: Duration = Duration::from_secs(90);
/// One token out, and nothing that could edit anything.
const PROBE_PROMPT: &str = "Reply with the single word: pong";

/// The account checks, including the one that matters: two names, one subscription.
pub(super) async fn accounts(cfg: &Config, paths: &Paths, probe: bool, out: &mut Vec<Check>) {
    if cfg.accounts.is_empty() {
        out.push(Check::new(
            "accounts",
            Level::Error,
            "no [[accounts]] configured; add one wrapper executable per subscription",
        ));
        return;
    }

    let mut identities: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for account in &cfg.accounts {
        let resolved = which::which(&account.exec).ok();
        let name = format!("accounts/{}", account.id.0);
        match &resolved {
            Some(path) => out.push(Check::new(
                name,
                Level::Ok,
                format!(
                    "{} -> {} ({}){}",
                    account.exec,
                    path.display(),
                    account.provider,
                    env_note(&account.env),
                ),
            )),
            None => out.push(Check::new(
                name,
                Level::Error,
                format!(
                    "executable `{}` for account `{}` is not on PATH; create a wrapper script \
                     that execs the real CLI with this subscription's config dir",
                    account.exec, account.id.0
                ),
            )),
        }
        let binary = resolved
            .and_then(|p| std::fs::canonicalize(&p).ok().or(Some(p)))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| account.exec.clone());
        let env: Vec<String> = account
            .env
            .iter()
            .map(|(k, v)| format!("{k}={}", shellexpand::tilde(v)))
            .collect();
        identities
            .entry(format!("{binary}|{}", env.join(",")))
            .or_default()
            .push(account.id.0.clone());
    }

    for (identity, ids) in &identities {
        if ids.len() > 1 {
            out.push(Check::new(
                "accounts/collision",
                Level::Error,
                format!(
                    "accounts {} resolve to the same binary with the same effective config dir \
                     ({}): they are ONE subscription, so dispatch would double-spend one quota \
                     and failover between them is a silent no-op. Give each account a wrapper \
                     that sets its own config dir.",
                    ids.join(" and "),
                    identity.replace('|', " env ")
                ),
            ));
        }
    }

    if probe {
        for account in &cfg.accounts {
            let name = format!("accounts/{}/probe", account.id.0);
            out.push(probe_account(cfg, account, &paths.repo, name).await);
        }
    }
}

/// One line per account: whether dispatch has quota telemetry to balance load on, or is
/// reduced to token share alone. Read from the last persisted snapshot, no network.
pub(super) fn quota(cfg: &Config, paths: &Paths, out: &mut Vec<Check>) {
    // An empty map would read as "no quota source" on every account and hide the real fault.
    let state = match crate::dispatch::persist::load_state(&paths.accounts_state()) {
        Ok(state) => state,
        Err(e) => {
            out.push(Check::new(
                "accounts/state",
                Level::Error,
                format!(
                    "{} is unreadable: {e}; recover with `swamp accounts reset`",
                    paths.accounts_state()
                ),
            ));
            return;
        }
    };
    let now = time::OffsetDateTime::now_utc();
    for account in &cfg.accounts {
        let name = format!("providers/{}/quota", account.id.0);
        let entry = state.get(&account.id);
        let source = entry.and_then(|s| s.quota_source);
        // An estimate is not a source: dispatch is balancing that account on token share.
        let (level, head) = match source {
            Some(QuotaSource::Telemetry) => (Level::Ok, "quota telemetry live"),
            Some(QuotaSource::Rollout) => (Level::Note, "quota via rollout"),
            Some(QuotaSource::AppServer) => (Level::Note, "quota via app-server"),
            Some(QuotaSource::Estimated) | None => (
                Level::Warn,
                "no quota source; tokens only, utilization is estimated",
            ),
        };
        let mut detail = head.to_owned();
        if let Some(snapshot) = entry.and_then(|s| s.quota.as_ref()) {
            // A window whose reset has passed measures an allowance that already rolled:
            // dispatch ignores it, so doctor must not quote it as evidence either.
            let window = |scope| {
                snapshot
                    .windows
                    .iter()
                    .find(|w| w.scope == scope && w.is_current(now))
                    .map(|w| {
                        let tilde = if w.measured { "" } else { "~" };
                        format!("{tilde}{:.0}%", w.utilization * 100.0)
                    })
            };
            if let Some(u) = window(LimitScope::FiveHour) {
                detail.push_str(&format!("  5h {u}"));
            }
            if let Some(u) = window(LimitScope::SevenDay) {
                detail.push_str(&format!("  7d {u}"));
            }
        }
        if let Some(at) = entry.and_then(|s| s.quota_observed_at) {
            let secs = (now - at).whole_seconds().max(0) as u64;
            detail.push_str(&format!(
                "  observed {} ago",
                crate::ui::fmt::until(Duration::from_secs(secs))
            ));
        }
        out.push(Check::new(name, level, detail));
    }
}

/// `--version` needs no credentials, so it proves nothing about the subscription. This sends
/// one real one-token turn down the adapter's own argv and classifies what comes back.
pub async fn probe_account(
    cfg: &Config,
    account: &crate::config::AccountCfg,
    cwd: &Utf8Path,
    name: String,
) -> Check {
    let exec = &account.exec;
    if let Some(problem) = version_check(exec, &account.env).await {
        return Check::new(name, Level::Error, problem);
    }
    let model = match cfg.model_for(account.provider, Tier::Low, Some(&account.id)) {
        Ok(m) => m,
        Err(e) => return Check::new(name, Level::Error, e.to_string()),
    };
    match probe_turn(cfg, account, cwd, &model).await {
        Ok(None) => Check::new(name, Level::Ok, format!("{exec} answered on {model}")),
        Ok(Some(f)) => Check::new(name, Level::Error, format!("{exec} on {model}: {f:?}")),
        Err(e) => Check::new(name, Level::Error, format!("{exec} on {model}: {e:#}")),
    }
}

async fn version_check(exec: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let mut cmd = tokio::process::Command::new(exec);
    cmd.arg("--version");
    for (k, v) in env {
        cmd.env(k, shellexpand::tilde(v).into_owned());
    }
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) if o.status.success() => None,
        Ok(Ok(o)) => Some(format!(
            "`{exec} --version` exited {}: {}",
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Ok(Err(e)) => Some(format!("`{exec} --version` failed: {e}")),
        Err(_) => Some(format!(
            "`{exec} --version` timed out after {PROBE_TIMEOUT:?}"
        )),
    }
}

async fn probe_turn(
    cfg: &Config,
    account: &crate::config::AccountCfg,
    cwd: &Utf8Path,
    model: &str,
) -> anyhow::Result<Option<Failure>> {
    use crate::worker::adapter::{ExitContext, ParseState, adapter_for};
    use tokio::io::AsyncWriteExt;

    let adapter = adapter_for(account.provider);
    let spec = probe_spec(cfg, account, cwd, model);
    let argv = adapter.build_argv(&spec)?;
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd.as_std_path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(PROBE_PROMPT.as_bytes()).await?;
    }
    let out = tokio::time::timeout(PROBE_TURN_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("no answer after {PROBE_TURN_TIMEOUT:?}"))??;

    let mut st = ParseState::default();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        adapter.parse_line(line, &mut st);
    }
    st.stderr_tail = String::from_utf8_lossy(&out.stderr)
        .lines()
        .map(str::to_owned)
        .collect();
    let patterns = cfg.failure_patterns(account.provider)?;
    Ok(adapter.classify(&ExitContext {
        exit: Some(crate::model::node::ExitInfo {
            code: out.status.code(),
            signal: None,
            duration_ms: 0,
        }),
        state: &st,
        patterns: &patterns,
        deadline_hit: false,
    }))
}

fn probe_spec(
    cfg: &Config,
    account: &crate::config::AccountCfg,
    cwd: &Utf8Path,
    model: &str,
) -> crate::worker::adapter::LaunchSpec {
    let worker = cfg
        .providers
        .get(&account.provider)
        .map(|p| p.worker.clone())
        .unwrap_or_default();
    crate::worker::adapter::LaunchSpec {
        node: crate::ids::NodeIds {
            id: crate::ids::NodeId::new(),
            session_uuid: uuid::Uuid::new_v4(),
        },
        provider: account.provider,
        exec: account.exec.clone(),
        env: crate::config::resolve::expand_env(&account.env),
        model: model.to_owned(),
        tier: Tier::Low,
        cwd: cwd.to_path_buf(),
        isolation: crate::model::result::IsolationMode::ReadOnly,
        session: crate::worker::adapter::SessionPlan::New { preassigned: None },
        kind: crate::model::core::NodeKind::Worker,
        permission_mode: worker.permission_mode.clone().unwrap_or_default(),
        sandbox: worker.sandbox.clone().unwrap_or_default(),
        append_system_prompt: None,
        allow_tools: worker.allow_tools.clone(),
        deny_tools: worker.deny_tools.clone(),
        mcp: None,
        last_message_path: Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .unwrap_or_else(|_| Utf8PathBuf::from("/tmp"))
            .join(format!("swamp-probe-{}.txt", account.id.0)),
        extra_args: worker.args_for(crate::model::result::IsolationMode::ReadOnly),
        extra: Default::default(),
        partial_messages: false,
        attempt: 1,
    }
}

/// The full resolved (provider, tier) -> model matrix, loud on any hole.
pub(super) fn tiers(cfg: &Config, out: &mut Vec<Check>) {
    let providers: Vec<Provider> = cfg.providers.keys().copied().collect();
    for p in providers {
        if cfg.accounts_for(p).is_empty() {
            continue;
        }
        let mut row = Vec::new();
        for t in [Tier::High, Tier::Mid, Tier::Low] {
            match cfg.model_for(p, t, None) {
                Ok(m) => row.push(format!("{t}={m}")),
                Err(e) => out.push(Check::new(
                    format!("providers/{p}/{t}"),
                    Level::Error,
                    format!("{e}"),
                )),
            }
        }
        if !row.is_empty() {
            out.push(Check::new(
                format!("providers/{p}"),
                Level::Ok,
                format!("tiers: {}", row.join("  ")),
            ));
        }
    }
}

fn env_note(env: &BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return String::new();
    }
    let keys: Vec<&str> = env.keys().map(String::as_str).collect();
    format!(" env {}", keys.join(","))
}
