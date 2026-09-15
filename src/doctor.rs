use crate::config::Config;
use crate::journal::paths::Paths;
use crate::journal::record::{JournalEvent, JournalLine};
use crate::model::core::{Provider, Tier};
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
    tiers(cfg, &mut out);
    unsafe_args(cfg, &mut out);
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
            format!("{} (the MCP bridge is spawned by absolute path)", p.display()),
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
            out.push(probe_account(&account.exec, &account.env, name).await);
        }
    }
    let _ = paths;
}

pub async fn probe_account(
    exec: &str,
    env: &BTreeMap<String, String>,
    name: String,
) -> Check {
    let mut cmd = tokio::process::Command::new(exec);
    cmd.arg("--version");
    for (k, v) in env {
        cmd.env(k, shellexpand::tilde(v).into_owned());
    }
    match tokio::time::timeout(PROBE_TIMEOUT, cmd.output()).await {
        Ok(Ok(o)) if o.status.success() => Check::new(
            name,
            Level::Ok,
            String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        ),
        Ok(Ok(o)) => Check::new(
            name,
            Level::Error,
            format!(
                "`{exec} --version` exited {}: {}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim()
            ),
        ),
        Ok(Err(e)) => Check::new(name, Level::Error, format!("`{exec} --version` failed: {e}")),
        Err(_) => Check::new(
            name,
            Level::Error,
            format!("`{exec} --version` timed out after {PROBE_TIMEOUT:?}"),
        ),
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
        let (_, refused) =
            crate::worker::adapter::gate_unsafe_args(&provider.worker.args, acked);
        if !refused.is_empty() {
            out.push(Check::new(
                format!("providers/{p}/args"),
                Level::Error,
                format!(
                    "providers.{p}.worker.args carries {} without limits.unsafe_ack = true; \
                     the flag is dropped at launch",
                    refused.join(" ")
                ),
            ));
        }
    }
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
    let detail = if cfg.sources.is_empty() {
        "built-in defaults only; no config file was found".to_owned()
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

/// Removes stale worktrees, sockets and pidfiles.
pub async fn reap(paths: &Paths) -> anyhow::Result<u32> {
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
    Ok(removed)
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
