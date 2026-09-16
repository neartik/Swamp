use crate::config::Config;
use crate::dispatch::account::QuotaSource;
use crate::journal::paths::Paths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{LimitScope, Provider, Tier};
use crate::model::failure::{Detector, Failure};
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use std::time::Duration;

/// Above this share of unparsed stream lines the adapters have drifted from the CLIs.
const UNPARSED_MAX: f64 = 0.02;
/// Above this share of pattern-matched classifications the structured signals are gone.
const PATTERN_MAX: f64 = 0.25;
const RECENT_RUNS: usize = 20;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
const PROBE_TURN_TIMEOUT: Duration = Duration::from_secs(90);
/// One token out, and nothing that could edit anything.
const PROBE_PROMPT: &str = "Reply with the single word: pong";
/// Directories a fresh worktree will not inherit unless `workspace.link` names them.
const HEAVY_DIRS: [&str; 5] = ["target", "node_modules", ".venv", "vendor", "build"];
const HEAVY_BYTES: u64 = 1 << 30;

pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Error,
}

impl Level {
    pub fn label(&self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Note => "note",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }
}

impl Check {
    fn new(name: impl Into<String>, level: Level, detail: impl Into<String>) -> Check {
        Check {
            name: name.into(),
            level,
            detail: detail.into(),
        }
    }
}

pub async fn checks(cfg: &Config, paths: &Paths, probe: bool, schema: bool) -> Vec<Check> {
    let mut out = Vec::new();
    environment(paths, &mut out).await;
    accounts(cfg, paths, probe, &mut out).await;
    quota(cfg, paths, &mut out);
    tiers(cfg, &mut out);
    unsafe_args(cfg, &mut out);
    permission_modes(cfg, &mut out);
    workspace(cfg, paths, &mut out);
    config_sources(cfg, &mut out);
    if schema {
        out.push(schema_drift(paths));
    }
    out
}

async fn environment(paths: &Paths, out: &mut Vec<Check>) {
    match tokio::process::Command::new("git")
        .arg("--version")
        .output()
        .await
    {
        Ok(o) if o.status.success() => out.push(Check::new(
            "environment/git",
            Level::Ok,
            String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )),
        _ => out.push(Check::new(
            "environment/git",
            Level::Error,
            "git is not on PATH; worktree isolation needs it",
        )),
    }

    let head = run_git(&paths.repo, &["rev-parse", "--short", "HEAD"]).await;
    let dirty = run_git(&paths.repo, &["status", "--porcelain"]).await;
    match (head, dirty) {
        (Some(head), Some(status)) => out.push(Check::new(
            "environment/repo",
            Level::Ok,
            format!(
                "{} HEAD {head}, {}",
                paths.repo,
                if status.trim().is_empty() {
                    "clean"
                } else {
                    "dirty"
                }
            ),
        )),
        _ => out.push(Check::new(
            "environment/repo",
            Level::Error,
            format!("{} is not a usable git repository", paths.repo),
        )),
    }

    out.push(match writable(&paths.dot_swamp) {
        Ok(()) => {
            let excluded = git_excludes_swamp(&paths.repo);
            if excluded {
                Check::new(
                    "environment/.swamp",
                    Level::Ok,
                    format!("{} writable, listed in .git/info/exclude", paths.dot_swamp),
                )
            } else {
                Check::new(
                    "environment/.swamp",
                    Level::Warn,
                    format!(
                        "{} is not listed in .git/info/exclude; run any swamp command from the \
                         repo root to add it",
                        paths.dot_swamp
                    ),
                )
            }
        }
        Err(e) => Check::new(
            "environment/.swamp",
            Level::Error,
            format!("{} is not writable: {e}", paths.dot_swamp),
        ),
    });

    out.push(match writable(&paths.home_swamp) {
        Ok(()) => Check::new(
            "environment/state",
            Level::Ok,
            format!("{} writable", paths.home_swamp),
        ),
        Err(e) => Check::new(
            "environment/state",
            Level::Error,
            format!("{} is not writable: {e}", paths.home_swamp),
        ),
    });

    out.push(match std::env::current_exe() {
        Ok(p) => Check::new(
            "environment/swamp",
            Level::Ok,
            format!(
                "{} (the MCP bridge is spawned by absolute path)",
                p.display()
            ),
        ),
        Err(e) => Check::new(
            "environment/swamp",
            Level::Error,
            format!("cannot resolve the swamp executable: {e}"),
        ),
    });

    out.push(match std::env::var("SWAMP_DEPTH") {
        Err(_) => Check::new("environment/depth", Level::Ok, "not inside a worker"),
        Ok(d) => Check::new(
            "environment/depth",
            Level::Warn,
            format!("SWAMP_DEPTH={d}: this shell is inside a worker; nested dispatch is capped"),
        ),
    });
}

/// The account checks, including the one that matters: two names, one subscription.
async fn accounts(cfg: &Config, paths: &Paths, probe: bool, out: &mut Vec<Check>) {
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
fn quota(cfg: &Config, paths: &Paths, out: &mut Vec<Check>) {
    let state = crate::dispatch::persist::load_state(&paths.accounts_state()).unwrap_or_default();
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
fn tiers(cfg: &Config, out: &mut Vec<Check>) {
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

fn unsafe_args(cfg: &Config, out: &mut Vec<Check>) {
    let acked = cfg.limits.unsafe_ack == Some(true);
    for (p, provider) in &cfg.providers {
        let (_, refused) = crate::worker::adapter::gate_unsafe_args(&provider.worker.args, acked);
        if !refused.is_empty() {
            out.push(Check::new(
                format!("providers/{p}/args"),
                Level::Error,
                format!(
                    "providers.{p}.worker.args carries {} without limits.unsafe_ack = true; \
                     the configuration is refused until it is set",
                    refused.join(" ")
                ),
            ));
        }
    }
}

/// Swamp always launches with `--permission-prompts none`, and under it these modes deny
/// every Bash call that is not explicitly allowed: the worker cannot run the tests it was
/// sent to run. Allowing Bash by name is what makes them safe, and `acceptEdits` plus an
/// allowed Bash is the recommended pair, because `auto` denies the file writes as well.
const BASH_DENYING_MODES: [&str; 4] = ["acceptEdits", "plan", "manual", "dontAsk"];

fn permission_modes(cfg: &Config, out: &mut Vec<Check>) {
    let Some(provider) = cfg.providers.get(&Provider::Anthropic) else {
        return;
    };
    let worker = &provider.worker;
    if denies_bash(
        worker.permission_mode.as_deref(),
        &worker.allow_tools,
        &worker.args,
    ) {
        out.push(Check::new(
            "providers/anthropic/permission_mode",
            Level::Warn,
            warning("providers.anthropic.worker", &worker.permission_mode),
        ));
    }
    if denies_bash(
        cfg.brain.permission_mode.as_deref(),
        &cfg.brain.allow_tools,
        &[],
    ) {
        out.push(Check::new(
            "brain/permission_mode",
            Level::Warn,
            warning("brain", &cfg.brain.permission_mode),
        ));
    }
}

fn warning(key: &str, mode: &Option<String>) -> String {
    let mode = mode.as_deref().unwrap_or("");
    format!(
        "{key}.permission_mode = \"{mode}\" denies every Bash call under \
         --permission-prompts none and Bash is not allowed, so it cannot run tests, a build \
         or git; add \"Bash\" to {key}.allow_tools"
    )
}

/// A mode from the list denies Bash unless the tool is allowed by name, either through
/// `allow_tools` or through a raw `--allowedTools` in `worker.args`.
fn denies_bash(mode: Option<&str>, allow: &[String], args: &[String]) -> bool {
    let mode = mode.unwrap_or("");
    if !BASH_DENYING_MODES
        .iter()
        .any(|m| m.eq_ignore_ascii_case(mode))
    {
        return false;
    }
    !allow.iter().any(|t| is_bash(t)) && !allowed_in_args(args)
}

fn is_bash(tool: &str) -> bool {
    tool == "Bash" || tool.starts_with("Bash(")
}

fn allowed_in_args(args: &[String]) -> bool {
    let Some(at) = args
        .iter()
        .position(|a| a == "--allowedTools" || a == "--allowed-tools")
    else {
        return false;
    };
    args[at + 1..]
        .iter()
        .take_while(|a| !a.starts_with("--"))
        .any(|t| is_bash(t))
}

fn workspace(cfg: &Config, paths: &Paths, out: &mut Vec<Check>) {
    if !cfg.workspace.link.is_empty() {
        return;
    }
    for name in HEAVY_DIRS {
        let dir = paths.repo.join(name);
        if !dir.is_dir() {
            continue;
        }
        let bytes = dir_size(&dir, 20_000);
        if bytes >= HEAVY_BYTES {
            out.push(Check::new(
                "workspace/link",
                Level::Warn,
                format!(
                    "[workspace] link is empty but ./{name} is {:.1} GiB; fresh worktrees will \
                     rebuild from scratch. Consider link = [\"{name}\"]",
                    bytes as f64 / (1u64 << 30) as f64
                ),
            ));
        }
    }
}

fn config_sources(cfg: &Config, out: &mut Vec<Check>) {
    let level = if cfg.sources.is_empty() {
        Level::Note
    } else {
        Level::Ok
    };
    // SWAMP_CONFIG_DIR and XDG_CONFIG_HOME move the user layer: without the resolved path
    // here, a file written to ~/.config/swamp is simply never mentioned again.
    let detail = if cfg.sources.is_empty() {
        match crate::config::load::user_config_path() {
            Some(p) => format!(
                "built-in defaults only; no config file was found (looked for {p} and <repo>/.swamp/config.toml)"
            ),
            None => "built-in defaults only; no config file was found".to_owned(),
        }
    } else {
        cfg.sources
            .iter()
            .map(Utf8PathBuf::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push(Check::new("config/sources", level, detail));
    for w in &cfg.warnings {
        out.push(Check::new("config/warning", Level::Warn, w.clone()));
    }
}

/// `--schema`: how often the classifier fell back to regexes, and how many raw lines the
/// adapters could not read. Both are the early warning that a vendor changed wording.
fn schema_drift(paths: &Paths) -> Check {
    let mut events = 0u64;
    let mut unparsed = 0u64;
    let mut detections = 0u64;
    let mut pattern = 0u64;
    let runs = paths.list_runs().unwrap_or_default();
    for run in runs.into_iter().take(RECENT_RUNS) {
        let journal = paths.run_paths(run).journal();
        let Ok(text) = std::fs::read_to_string(&journal) else {
            continue;
        };
        for line in text.lines() {
            let Ok(l) = serde_json::from_str::<JournalLine>(line) else {
                continue;
            };
            match &l.event {
                JournalEvent::NodeEvent { .. } => events += 1,
                JournalEvent::NodeFinished {
                    state,
                    unparsed_lines,
                    ..
                } => {
                    unparsed += u64::from(*unparsed_lines);
                    if let crate::model::core::NodeState::Failed { failure } = state {
                        count_detector(failure, &mut detections, &mut pattern);
                    }
                }
                JournalEvent::NodeRetry { reason, .. } => {
                    count_detector(reason, &mut detections, &mut pattern)
                }
                _ => {}
            }
        }
    }

    let total = events + unparsed;
    if total == 0 {
        return Check::new(
            "protocol/schema",
            Level::Note,
            "no recorded runs to measure adapter drift against",
        );
    }
    let unparsed_ratio = unparsed as f64 / total as f64;
    let pattern_ratio = if detections == 0 {
        0.0
    } else {
        pattern as f64 / detections as f64
    };
    let detail = format!(
        "{unparsed} of {total} stream lines unparsed ({:.1}%), {pattern} of {detections} \
         classifications used the regex fallback ({:.1}%)",
        unparsed_ratio * 100.0,
        pattern_ratio * 100.0
    );
    if unparsed_ratio > UNPARSED_MAX || pattern_ratio > PATTERN_MAX {
        return Check::new(
            "protocol/schema",
            Level::Error,
            format!(
                "{detail}: the adapters have drifted from the CLIs. Re-record docs/ref fixtures \
                 and fix the adapter, then `swamp replay --reparse` the affected runs"
            ),
        );
    }
    Check::new("protocol/schema", Level::Ok, detail)
}

fn count_detector(f: &Failure, detections: &mut u64, pattern: &mut u64) {
    let detected_by = match f {
        Failure::RateLimited { detected_by, .. } | Failure::AuthExpired { detected_by, .. } => {
            Some(detected_by)
        }
        _ => None,
    };
    if let Some(d) = detected_by {
        *detections += 1;
        if *d == Detector::Pattern {
            *pattern += 1;
        }
    }
}

/// What one `--reap` removed, counted per place so the report can name both.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    /// Sockets, pidfiles and worktrees belonging to runs of this repo.
    pub runs: u32,
    /// Sockets left in the machine-wide socket directory by any repo.
    pub sockets: u32,
}

/// Removes stale worktrees, sockets and pidfiles.
pub async fn reap(paths: &Paths) -> anyhow::Result<Reaped> {
    let mut removed = 0u32;
    for run in paths.list_runs().unwrap_or_default() {
        let rp = paths.run_paths(run);
        let view = match crate::journal::fold::RunView::load(&rp.dir, false) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let live = view.nodes.values().any(|n| {
            matches!(n.state, crate::model::core::NodeState::Running { .. })
                && crate::worker::liveness::is_ours(&rp.pidfile(n.id))
        });
        if live {
            continue;
        }
        for node in view.nodes.keys() {
            let pidfile = rp.pidfile(*node);
            if pidfile.is_file() && !crate::worker::liveness::is_ours(&pidfile) {
                std::fs::remove_file(&pidfile).ok();
                removed += 1;
            }
        }
        let socket = rp.socket();
        if socket.exists() {
            std::fs::remove_file(&socket).ok();
            removed += 1;
        }
    }
    if let Ok(git) = crate::workspace::git::Git::discover(&paths.repo).await {
        removed += crate::workspace::worktree::prune(&git).await.unwrap_or(0);
    }
    Ok(Reaped {
        runs: removed,
        sockets: reap_sockets(&paths.sock_dir()),
    })
}

/// A run whose repository is gone leaves its socket behind here, so the directory is swept
/// on its own: anything that still accepts a connection is live and is never touched.
fn reap_sockets(dir: &Utf8Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut removed = 0u32;
    for entry in entries.flatten() {
        let Ok(path) = Utf8PathBuf::from_path_buf(entry.path()) else {
            continue;
        };
        if path.extension() != Some("sock") || !is_dead_socket(&path) {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn is_dead_socket(path: &Utf8Path) -> bool {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => false,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
        ),
    }
}

// ---------------------------------------------------------------- helpers

fn env_note(env: &BTreeMap<String, String>) -> String {
    if env.is_empty() {
        return String::new();
    }
    let keys: Vec<&str> = env.keys().map(String::as_str).collect();
    format!(" env {}", keys.join(","))
}

async fn run_git(dir: &Utf8Path, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn writable(dir: &Utf8Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".swamp-doctor-probe");
    std::fs::write(&probe, b"ok")?;
    std::fs::remove_file(&probe)
}

fn git_excludes_swamp(repo: &Utf8Path) -> bool {
    let exclude = repo.join(".git").join("info").join("exclude");
    let Ok(text) = std::fs::read_to_string(exclude) else {
        return false;
    };
    text.lines()
        .any(|l| matches!(l.trim(), "/.swamp/" | "/.swamp" | ".swamp/" | ".swamp"))
}

/// Bounded on purpose: doctor must stay fast on a repo with a huge build directory.
fn dir_size(dir: &Utf8Path, budget: usize) -> u64 {
    let mut stack = vec![dir.to_path_buf()];
    let mut bytes = 0u64;
    let mut seen = 0usize;
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > budget {
                return bytes;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if let Ok(p) = Utf8PathBuf::from_path_buf(entry.path()) {
                    stack.push(p);
                }
            } else if meta.is_file() {
                bytes += meta.len();
            }
        }
    }
    bytes
}
