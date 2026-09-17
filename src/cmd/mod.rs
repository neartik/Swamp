pub mod accounts;
pub mod adopt;
pub mod board;
pub mod cancel;
pub mod chat;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod gc;
pub mod mcp_bridge;
pub mod replay;
pub mod resume;
pub mod run;
pub mod runs;
pub mod trace;
pub mod usage;
pub mod watch;
pub mod worktrees;

use crate::config::Config;
use crate::dispatch::{AccountPool, Dispatcher};
use crate::ids::{NodeId, RunId};
use crate::journal::fold::RunView;
use crate::journal::paths::{Paths, RunPaths};
use crate::journal::record::{JournalEvent, SCHEMA_VERSION};
use crate::journal::writer::FsyncPolicy;
use crate::journal::{Journal, JournalHandle};
use crate::model::core::{NodeKind, NodeState, Usage};
use crate::model::node::NodeRecord;
use crate::worker::Executor;
use crate::workspace::{Git, WorkspaceManager};
use anyhow::Context;
use camino::Utf8PathBuf;
use serde_json::json;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

/// How long `finish` waits for the journal writer to drain before giving up on it.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Everything every subcommand needs, built once in main.
pub struct Ctx {
    pub cfg: Arc<Config>,
    pub paths: Arc<Paths>,
    pub color: bool,
    pub json: bool,
    /// The invocation's `--config` and `--profile`, so a command that reloads the config
    /// reloads the same layer stack the rest of the process runs on.
    pub config_arg: Option<Utf8PathBuf>,
    pub profile: Option<String>,
}

impl Ctx {
    /// `last`, `-2`, a full id or a unique prefix. No argument means `last`.
    pub fn run_paths(&self, spec: Option<&str>) -> anyhow::Result<RunPaths> {
        let run = self.paths.resolve_run(spec.unwrap_or("last"))?;
        Ok(self.paths.run_paths(run))
    }

    pub fn view(&self, run: &RunPaths, with_events: bool) -> anyhow::Result<RunView> {
        RunView::load(&run.dir, with_events)
            .with_context(|| format!("reading the journal of run {}", run.run))
    }

    /// An attempt id, the logical node id, a short id or a prefix, searched across every run
    /// newest first; `last` and `-N` name a run instead and resolve to that run's node.
    pub fn find_node(&self, spec: &str) -> anyhow::Result<(RunPaths, NodeRecord)> {
        let spec = spec.trim();
        anyhow::ensure!(!spec.is_empty(), "empty node specifier");
        if is_run_alias(spec) {
            let rp = self.run_paths(Some(spec))?;
            let view = self.view(&rp, false)?;
            let node = run_node(&view, &rp)?;
            return Ok((rp, node));
        }
        let exact = NodeId::from_str(spec).is_ok();
        let mut hits: Vec<(RunPaths, NodeRecord)> = Vec::new();
        for run in self.paths.list_runs()? {
            let rp = self.paths.run_paths(run);
            let Ok(view) = RunView::load(&rp.dir, false) else {
                continue;
            };
            // The worktree branch carries the LOGICAL short id and the node directory the
            // attempt's, so both spellings are on screen and both have to resolve.
            let mut logical: Vec<NodeId> = Vec::new();
            for n in view.nodes.values() {
                if node_matches(n.id, spec) {
                    // A full id is unique by construction; only a prefix can collide.
                    if exact {
                        return Ok((rp, n.clone()));
                    }
                    hits.push((rp.clone(), n.clone()));
                } else if node_matches(n.logical, spec) && !logical.contains(&n.logical) {
                    logical.push(n.logical);
                }
            }
            for id in logical {
                if hits.iter().any(|(r, n)| r.run == rp.run && n.logical == id) {
                    continue;
                }
                let Some(node) = attempt_of(&view, id) else {
                    continue;
                };
                if exact {
                    return Ok((rp, node));
                }
                hits.push((rp.clone(), node));
            }
        }
        match hits.len() {
            1 => Ok(hits.remove(0)),
            0 => anyhow::bail!("no node matches `{spec}`"),
            _ => anyhow::bail!("node `{spec}` is ambiguous: {}", candidates(&hits)),
        }
    }

    pub fn out(&self, text: &str) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
}

/// `last` and `-2` name a run, never a node: no short id is ever spelled that way.
fn is_run_alias(spec: &str) -> bool {
    spec.eq_ignore_ascii_case("last")
        || spec
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// What `swamp diff last` means: the run's only node, or the last one that finished.
fn run_node(view: &RunView, rp: &RunPaths) -> anyhow::Result<NodeRecord> {
    let mut pool: Vec<&NodeRecord> = view
        .nodes
        .values()
        .filter(|n| n.kind != NodeKind::Brain)
        .collect();
    if pool.is_empty() {
        pool = view.nodes.values().collect();
    }
    anyhow::ensure!(!pool.is_empty(), "run {} recorded no nodes", rp.run);
    if let [only] = pool[..] {
        return Ok(only.clone());
    }
    let finished = pool
        .iter()
        .filter(|n| n.ended_at.is_some())
        .max_by_key(|n| (n.ended_at, n.id));
    finished.map(|n| (*n).clone()).ok_or_else(|| {
        let listed: Vec<(RunPaths, NodeRecord)> =
            pool.iter().map(|n| (rp.clone(), (*n).clone())).collect();
        anyhow::anyhow!(
            "run {} has no finished node; name one: {}",
            rp.run,
            candidates(&listed)
        )
    })
}

/// Which attempt a logical node id resolves to: the latest one that finished, else the latest.
fn attempt_of(view: &RunView, logical: NodeId) -> Option<NodeRecord> {
    let chain: Vec<&NodeRecord> = view
        .by_logical
        .get(&logical)?
        .iter()
        .filter_map(|a| view.nodes.get(a))
        .collect();
    chain
        .iter()
        .filter(|n| n.ended_at.is_some())
        .max_by_key(|n| (n.ended_at, n.id))
        .or_else(|| chain.last())
        .map(|n| (*n).clone())
}

/// `short (run, title)` per hit, for an error the user can act on.
fn candidates(hits: &[(RunPaths, NodeRecord)]) -> String {
    hits.iter()
        .take(8)
        .map(|(rp, n)| {
            format!(
                "{} (run {}, {})",
                n.id.short(),
                rp.run.short(),
                crate::ui::fmt::truncate(&n.title, 40)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn node_matches(id: NodeId, spec: &str) -> bool {
    if let Ok(exact) = NodeId::from_str(spec) {
        return exact == id;
    }
    let needle = spec.trim_start_matches("nd_").to_ascii_lowercase();
    let full = id.0.to_string().to_ascii_lowercase();
    !needle.is_empty() && (full.starts_with(&needle) || id.short() == needle)
}

/// "25m" on the command line; config uses the same spelling through humantime.
pub fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    humantime_serde::re::humantime::parse_duration(s.trim())
        .with_context(|| format!("invalid duration `{s}`; try 25m, 2h, 90s"))
}

/// `-` reads stdin, `@file` reads a file, everything else is the task itself.
pub fn task_text(parts: &[String]) -> anyhow::Result<String> {
    use std::io::Read;
    anyhow::ensure!(!parts.is_empty(), "no task given");
    if parts.len() == 1 {
        let one = parts[0].trim();
        if one == "-" {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            return Ok(buf.trim().to_owned());
        }
        if let Some(path) = one.strip_prefix('@') {
            return std::fs::read_to_string(path)
                .map(|t| t.trim().to_owned())
                .with_context(|| format!("reading the task from {path}"));
        }
    }
    let text = parts.join(" ").trim().to_owned();
    anyhow::ensure!(!text.is_empty(), "the task is empty");
    Ok(text)
}

/// `SWAMP_DEPTH` is exported into every worker env. A worker that shells out to `swamp`
/// must not start a whole tree of its own, so the refusal lives here, not only in the export.
pub fn guard_depth(cfg: &Config) -> anyhow::Result<u32> {
    const DEFAULT_MAX_DEPTH: u32 = 2;
    let depth: u32 = std::env::var("SWAMP_DEPTH")
        .ok()
        .and_then(|d| d.parse().ok())
        .unwrap_or(0);
    let max = cfg.limits.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    anyhow::ensure!(
        depth < max,
        "refusing to nest: SWAMP_DEPTH={depth} is already at limits.max_depth = {max}"
    );
    Ok(depth)
}

/// Resolves on SIGINT or SIGTERM. Workers run in their own process groups, so a terminal
/// Ctrl-C never reaches them: the shutdown path has to cancel them itself.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let Ok(mut term) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// The first line, clipped: a node title is a label, not the prompt.
pub fn title_of(task: &str) -> String {
    let first = task.lines().find(|l| !l.trim().is_empty()).unwrap_or(task);
    crate::ui::fmt::truncate(first.trim(), 60)
}

/// One run's shared machinery: journal, git, pool, executor and worktrees.
pub struct RunSession {
    pub cfg: Arc<Config>,
    pub paths: RunPaths,
    pub journal: JournalHandle,
    pub git: Git,
    pub pool: Arc<AccountPool>,
    pub exec: Arc<Executor>,
    pub workspace: Arc<WorkspaceManager>,
    writer: tokio::task::JoinHandle<()>,
}

impl RunSession {
    pub async fn start(
        ctx: &Ctx,
        cfg: Arc<Config>,
        run: RunId,
        task: Option<&str>,
    ) -> anyhow::Result<RunSession> {
        let paths = ctx.paths.run_paths(run);
        tokio::fs::create_dir_all(&paths.dir).await?;
        ctx.paths.ensure_git_excluded().ok();

        let policy = cfg
            .journal
            .fsync
            .as_deref()
            .unwrap_or("barrier")
            .parse::<FsyncPolicy>()
            .unwrap_or_default();
        let (journal, writer) = Journal::open(paths.clone(), policy, &cfg.journal.redact).await?;
        paths.link_last().ok();

        let git = Git::discover(&ctx.paths.repo).await?;
        let head = git.head().await.ok();
        let argv: Vec<String> = std::env::args().collect();
        journal
            .emit_durable(
                None,
                JournalEvent::RunStarted {
                    swamp_version: crate::VERSION.to_owned(),
                    schema: SCHEMA_VERSION,
                    argv: argv.clone(),
                    cwd: ctx.paths.repo.clone(),
                    repo: Some(ctx.paths.repo.clone()),
                    base: head.clone(),
                    config_sha256: cfg.sha256(),
                    task: task.map(str::to_owned),
                },
            )
            .await?;
        write_header(&paths, ctx, &cfg, &argv, head.as_deref(), task)?;

        let workspace =
            WorkspaceManager::new(git.clone(), ctx.paths.clone(), cfg.clone(), journal.clone())
                .await?;
        let pool = AccountPool::new(cfg.clone(), ctx.paths.accounts_state(), journal.clone())?;
        let exec = Arc::new(Executor::new(journal.clone(), cfg.clone()));
        Ok(RunSession {
            cfg,
            paths,
            journal,
            git,
            pool,
            exec,
            workspace,
            writer,
        })
    }

    pub fn dispatcher(&self) -> Arc<Dispatcher> {
        Dispatcher::new(
            self.cfg.clone(),
            self.pool.clone(),
            self.exec.clone(),
            self.workspace.clone(),
            self.journal.clone(),
        )
    }

    /// The absence of `RunFinished` is what marks a run interrupted, so this is durable and
    /// the writer task is drained before the process exits.
    pub async fn finish(
        self,
        state: NodeState,
        nodes: u32,
        usage: Usage,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<()> {
        self.journal
            .emit_durable(
                None,
                JournalEvent::RunFinished {
                    state,
                    nodes,
                    usage,
                    cost_usd,
                },
            )
            .await?;
        let RunSession {
            journal, writer, ..
        } = self;
        drop(journal);
        // The writer task ends when the last handle drops; a stray clone must not hang a
        // command, and RunFinished is already on disk.
        let _ = tokio::time::timeout(DRAIN_TIMEOUT, writer).await;
        Ok(())
    }
}

fn write_header(
    paths: &RunPaths,
    ctx: &Ctx,
    cfg: &Config,
    argv: &[String],
    head: Option<&str>,
    task: Option<&str>,
) -> anyhow::Result<()> {
    let header = json!({
        "run": paths.run.to_string(),
        "swamp_version": crate::VERSION,
        "schema": SCHEMA_VERSION,
        "cwd": ctx.paths.repo,
        "repo": ctx.paths.repo,
        "head": head,
        "config_sha256": cfg.sha256(),
        "argv": argv,
        "task": task,
        "socket": paths.socket(),
    });
    std::fs::write(
        paths.dir.join("run.json"),
        serde_json::to_vec_pretty(&header)?,
    )?;
    Ok(())
}

/// Mirrors the workspace manager's layout: `<workspace.root>/<repo-name>-<hash8>`.
pub fn worktree_root(ctx: &Ctx) -> Utf8PathBuf {
    match &ctx.cfg.workspace.root {
        Some(root) => {
            let base = Utf8PathBuf::from(shellexpand::tilde(root.as_str()).into_owned());
            let name = ctx.paths.repo.file_name().unwrap_or("repo");
            base.join(format!("{name}-{}", hash8(ctx.paths.repo.as_str())))
        }
        None => ctx.paths.worktree_root(),
    }
}

fn hash8(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where a node's `NodeResult` is kept, so a lost journal can still be rebuilt.
pub fn write_result(paths: &RunPaths, result: &crate::model::result::NodeResult) {
    let path: Utf8PathBuf = paths.result(result.node);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_vec_pretty(result) {
        let _ = std::fs::write(&path, body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SWAMP_DEPTH` was exported into every worker env and then never compared against
    /// anything: a worker that ran `swamp` started an unbounded second tree.
    #[test]
    fn swamp_depth_is_refused_at_the_limit() {
        let mut cfg = Config {
            version: 1,
            limits: Default::default(),
            brain: Default::default(),
            dispatch: Default::default(),
            cooldown: Default::default(),
            workspace: Default::default(),
            journal: Default::default(),
            providers: Default::default(),
            accounts: Vec::new(),
            tiers: Default::default(),
            failure: Default::default(),
            pricing: Default::default(),
            ui: Default::default(),
            profiles: Default::default(),
            sources: Vec::new(),
            warnings: Vec::new(),
        };
        cfg.limits.max_depth = Some(2);
        let restore = std::env::var("SWAMP_DEPTH").ok();
        unsafe { std::env::set_var("SWAMP_DEPTH", "1") };
        assert_eq!(guard_depth(&cfg).expect("under the limit"), 1);
        unsafe { std::env::set_var("SWAMP_DEPTH", "2") };
        let e = guard_depth(&cfg).expect_err("at the limit");
        assert!(e.to_string().contains("max_depth"), "{e}");
        match restore {
            Some(v) => unsafe { std::env::set_var("SWAMP_DEPTH", v) },
            None => unsafe { std::env::remove_var("SWAMP_DEPTH") },
        }
    }
}
