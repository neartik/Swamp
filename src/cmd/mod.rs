pub mod accounts;
pub mod adopt;
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
use crate::model::core::{NodeState, Usage};
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

    /// A node id, a short id, or a unique prefix, searched newest run first.
    pub fn find_node(&self, spec: &str) -> anyhow::Result<(RunPaths, NodeRecord)> {
        let spec = spec.trim();
        anyhow::ensure!(!spec.is_empty(), "empty node specifier");
        for run in self.paths.list_runs()? {
            let rp = self.paths.run_paths(run);
            let Ok(view) = RunView::load(&rp.dir, false) else {
                continue;
            };
            let hit = view.nodes.values().find(|n| node_matches(n.id, spec));
            if let Some(n) = hit {
                return Ok((rp, n.clone()));
            }
        }
        anyhow::bail!("no node matches `{spec}`")
    }

    pub fn out(&self, text: &str) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(text.as_bytes());
        let _ = out.flush();
    }
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
