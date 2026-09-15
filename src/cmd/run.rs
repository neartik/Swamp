use crate::cli::RunArgs;
use crate::cmd::{Ctx, RunSession, parse_duration, task_text, title_of, write_result};
use crate::config::Config;
use crate::dispatch::{NodeCtx, NodeOutcome, NodeRunner, run_node};
use crate::ids::{NodeId, NodeIds, RunId};
use crate::journal::paths::RunPaths;
use crate::model::core::{AccountId, NodeKind, NodeState, Provider, Tier};
use crate::model::failure::Failure;
use crate::model::node::{NodeRecord, WorkResultRef};
use crate::model::result::{IsolationMode, NodeResult, TaskRequest};
use crate::ui::trace::{TraceOpts, render};
use crate::worker::Executor;
use crate::worker::adapter::{LaunchSpec, SessionPlan};
use crate::workspace::{NodeWorktree, WorkspaceManager};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const DETACH_WAIT: Duration = Duration::from_secs(60);

/// One-shot dispatch. The v1 smoke path when --no-brain is set.
pub async fn run(ctx: &Ctx, args: &RunArgs) -> anyhow::Result<i32> {
    let task = task_text(&args.task)?;
    let cfg = Arc::new(overrides(ctx, args)?);
    for w in &cfg.warnings {
        tracing::warn!("{w}");
    }
    let session = RunSession::start(ctx, cfg, RunId::new(), Some(&task)).await?;
    if !ctx.json {
        println!("run {}", session.paths.run);
    }
    if args.no_brain {
        single_node(ctx, session, args, task).await
    } else {
        with_brain(ctx, session, args, task).await
    }
}

/// The smallest runnable thing: one process, one worktree, one node, one diff.
async fn single_node(
    ctx: &Ctx,
    session: RunSession,
    args: &RunArgs,
    task: String,
) -> anyhow::Result<i32> {
    let tier = args
        .tier
        .or(session.cfg.dispatch.default_tier)
        .unwrap_or(Tier::Mid);
    let request = TaskRequest {
        title: title_of(&task),
        prompt: task,
        tier: Some(tier),
        provider: args.provider,
        isolation: args.isolation,
        account: args.account.clone().map(AccountId),
        deps: Vec::new(),
    };
    let order = provider_order(&session.cfg, &request, tier);
    let provider = order.first().copied().unwrap_or(Provider::Anthropic);
    let spec = launch_spec(&session.cfg, &request, tier, provider, &session.paths);

    let cancel = CancellationToken::new();
    let cx = NodeCtx {
        cfg: session.cfg.clone(),
        pool: session.pool.clone(),
        runner: Arc::new(DirectRunner {
            exec: session.exec.clone(),
            workspace: session.workspace.clone(),
        }),
        journal: session.journal.clone(),
        provider_order: order,
        cross_provider: session.cfg.dispatch.cross_provider_failover.unwrap_or(false),
        max_attempts: session.cfg.dispatch.max_attempts.unwrap_or(3).max(1),
        deadline: Instant::now() + session.cfg.node_timeout(tier),
        // No brain, no parent: this node is the run.
        parent: None,
        logical: NodeId::new(),
        cancel: cancel.clone(),
    };

    if args.detach {
        let paths = session.paths.clone();
        tokio::spawn(async move { run_node(&cx, spec, &request).await });
        wait_for_spawn(&paths).await;
        println!("detached; follow with `swamp trace {} --follow`", paths.run);
        return Ok(0);
    }

    let (outcome, interrupted) = supervise(&cx, spec, &request, &cancel).await;
    // The context holds journal and pool handles; the writer task cannot drain until it goes.
    drop(cx);
    let result = node_result(&request, tier, provider, &outcome);
    write_result(&session.paths, &result);

    let state = if interrupted {
        NodeState::Cancelled {
            by: crate::model::core::CancelSource::User,
        }
    } else if outcome.failure.is_none() {
        NodeState::Succeeded
    } else {
        NodeState::Failed {
            failure: outcome.failure.clone().expect("checked above"),
        }
    };
    let cost = result.cost.map(|c| c.usd);
    let usage = result.usage;
    let paths = session.paths.clone();
    session
        .finish(state, outcome.attempts.len() as u32, usage, cost)
        .await?;
    report(ctx, &paths, &result)?;
    Ok(exit_code(outcome.failure.as_ref(), interrupted))
}

/// Ctrl-C cancels the node, which kills its process group, and the loop still returns so the
/// journal gets its `RunFinished`.
async fn supervise(
    cx: &NodeCtx,
    spec: LaunchSpec,
    request: &TaskRequest,
    cancel: &CancellationToken,
) -> (NodeOutcome, bool) {
    let node = run_node(cx, spec, request);
    tokio::pin!(node);
    let mut interrupted = false;
    loop {
        tokio::select! {
            outcome = &mut node => return (outcome, interrupted),
            signal = tokio::signal::ctrl_c(), if !interrupted => {
                if signal.is_ok() {
                    interrupted = true;
                    eprintln!("interrupted: stopping workers");
                    cancel.cancel();
                }
            }
        }
    }
}

async fn with_brain(
    ctx: &Ctx,
    session: RunSession,
    args: &RunArgs,
    task: String,
) -> anyhow::Result<i32> {
    use crate::mcp::McpServer;
    use std::collections::HashSet;

    let cfg = session.cfg.clone();
    let provider = cfg.brain.provider.unwrap_or(Provider::Anthropic);
    let dispatcher = session.dispatcher();
    let (server, socket) = McpServer::bind(
        &session.paths,
        dispatcher.clone(),
        Arc::new(session.journal.clone()),
    )
    .await?;
    let serving = server.serve();

    let deadline = Instant::now() + cfg.node_timeout(cfg.brain.tier.unwrap_or(Tier::High));
    let lease = session
        .pool
        .acquire(provider, &HashSet::new(), deadline)
        .await
        .map_err(|e| anyhow::anyhow!("no account for the brain: {e:?}"))?;
    let mut brain = crate::brain::build(
        &cfg,
        lease,
        &session.paths,
        &socket,
        session.journal.clone(),
        None,
    )?;
    brain.start().await?;
    brain.send(&task).await?;
    let code = crate::ui::chat::drain_turn(&mut brain, ctx).await;
    brain.shutdown().await?;
    serving.abort();
    drop(dispatcher);

    let view = ctx.view(&session.paths, false)?;
    let totals = view.totals();
    let failed = totals.failed > 0;
    let paths = session.paths.clone();
    session
        .finish(
            if failed {
                NodeState::Failed {
                    failure: Failure::WorkerError {
                        subtype: "node_failed".into(),
                        detail: format!("{} of {} nodes failed", totals.failed, totals.nodes),
                    },
                }
            } else {
                NodeState::Succeeded
            },
            totals.nodes,
            totals.usage,
            Some(totals.cost_usd),
        )
        .await?;
    let view = ctx.view(&paths, false)?;
    ctx.out(&render(
        &view,
        &TraceOpts {
            json: ctx.json,
            ..TraceOpts::default()
        },
    ));
    let _ = args;
    Ok(if code != 0 {
        code
    } else if failed {
        4
    } else {
        0
    })
}

fn report(ctx: &Ctx, paths: &RunPaths, result: &NodeResult) -> anyhow::Result<()> {
    if ctx.json {
        ctx.out(&format!("{}\n", serde_json::to_string_pretty(result)?));
        return Ok(());
    }
    let view = ctx.view(paths, false)?;
    ctx.out(&render(&view, &TraceOpts::default()));
    Ok(())
}

fn exit_code(failure: Option<&Failure>, interrupted: bool) -> i32 {
    if interrupted {
        return 6;
    }
    match failure {
        None => 0,
        Some(Failure::NoCapacity { .. }) => 3,
        Some(Failure::BudgetExceeded { .. }) => 7,
        Some(_) => 4,
    }
}

/// CLI flags are the last config layer, applied here so everything below sees one Config.
fn overrides(ctx: &Ctx, args: &RunArgs) -> anyhow::Result<Config> {
    let mut cfg = (*ctx.cfg).clone();
    if let Some(n) = args.workers {
        cfg.limits.max_parallel = Some(n.max(1));
    }
    if let Some(t) = &args.timeout {
        cfg.limits.worker_timeout = Some(parse_duration(t)?);
    }
    if let Some(b) = args.budget {
        cfg.limits.run_budget_usd = Some(b);
        cfg.limits.node_budget_usd = Some(b);
    }
    if let Some(n) = args.max_attempts {
        cfg.dispatch.max_attempts = Some(n.max(1));
    }
    if let Some(i) = args.isolation {
        cfg.workspace.isolation = Some(i);
    }
    if let Some(base) = &args.base {
        cfg.workspace.base = Some(base.clone());
    }
    if args.include_dirty {
        cfg.workspace.include_dirty = Some(true);
    }
    if let Some(id) = &args.account {
        let id = AccountId(id.clone());
        anyhow::ensure!(
            cfg.accounts.iter().any(|a| a.id == id),
            "no account `{}` in the configuration",
            id.0
        );
        // Pinning an account is also opting out of failover: nothing else is left to rotate to.
        cfg.accounts.retain(|a| a.id == id);
        cfg.warnings
            .push(format!("--account {} pins the run: failover is off", id.0));
    }
    Ok(cfg)
}

fn provider_order(cfg: &Config, task: &TaskRequest, tier: Tier) -> Vec<Provider> {
    match task.provider {
        Some(p) => {
            let mut order = vec![p];
            order.extend(cfg.provider_order(tier).into_iter().filter(|q| *q != p));
            order
        }
        None => {
            let order = cfg.provider_order(tier);
            if order.is_empty() {
                vec![Provider::Anthropic]
            } else {
                order
            }
        }
    }
}

fn launch_spec(
    cfg: &Config,
    task: &TaskRequest,
    tier: Tier,
    provider: Provider,
    paths: &RunPaths,
) -> LaunchSpec {
    let worker = cfg
        .providers
        .get(&provider)
        .map(|p| p.worker.clone())
        .unwrap_or_default();
    let isolation = task
        .isolation
        .or(cfg.workspace.isolation)
        .unwrap_or(IsolationMode::Worktree);
    let mut extra_args = worker.args.clone();
    if isolation == IsolationMode::ReadOnly {
        extra_args.extend(worker.readonly_args.clone());
    }
    LaunchSpec {
        node: NodeIds {
            id: NodeId::new(),
            session_uuid: uuid::Uuid::new_v4(),
        },
        provider,
        exec: String::new(),
        env: Default::default(),
        model: String::new(),
        tier,
        cwd: paths.dir.clone(),
        isolation,
        session: SessionPlan::New { preassigned: None },
        kind: NodeKind::Worker,
        permission_mode: worker.permission_mode.clone().unwrap_or_default(),
        sandbox: worker.sandbox.clone().unwrap_or_default(),
        budget_usd: cfg.node_budget_usd(tier),
        append_system_prompt: None,
        allow_tools: Vec::new(),
        deny_tools: Vec::new(),
        mcp: None,
        last_message_path: paths.dir.join("last-message.txt"),
        extra_args,
        attempt: 1,
    }
}

fn node_result(
    task: &TaskRequest,
    tier: Tier,
    provider: Provider,
    outcome: &NodeOutcome,
) -> NodeResult {
    let last = outcome.attempts.last();
    let work: Option<WorkResultRef> = last.and_then(|r| r.work.clone());
    let run = outcome.outcome.as_ref();
    NodeResult {
        node: last.map_or(outcome.logical, |r| r.id),
        title: task.title.clone(),
        ok: outcome.failure.is_none(),
        state: match &outcome.failure {
            None => "succeeded",
            Some(_) => "failed",
        },
        tier: last.map_or(tier, |r| r.tier),
        provider: last.map_or(provider, |r| r.provider),
        account: last.and_then(|r| r.account.clone()),
        model: last.and_then(|r| r.model.clone()),
        attempts: outcome.attempts.len() as u32,
        summary: run.and_then(|o| o.summary.clone()),
        files: run.map(|o| o.files.clone()).unwrap_or_default(),
        branch: work.as_ref().map(|w| w.branch.clone()),
        patch: work.as_ref().map(|w| w.patch.clone()),
        insertions: work.as_ref().map_or(0, |w| w.insertions),
        deletions: work.as_ref().map_or(0, |w| w.deletions),
        usage: run.map(|o| o.usage).unwrap_or_default(),
        cost: run.and_then(|o| o.cost),
        duration_ms: last
            .and_then(NodeRecord::duration)
            .map_or(0, |d| d.as_millis() as u64),
        failure: outcome.failure.clone(),
        permission_denials: run.map_or(0, |o| o.permission_denials),
    }
}

/// `--detach` returns once the worker is on disk: it is already in its own process group,
/// so it outlives this process and `swamp resume` adopts it.
async fn wait_for_spawn(paths: &RunPaths) {
    let deadline = Instant::now() + DETACH_WAIT;
    let nodes = paths.dir.join("nodes");
    loop {
        if let Ok(entries) = std::fs::read_dir(&nodes) {
            let started = entries
                .flatten()
                .any(|e| e.path().join("pid").is_file());
            if started {
                return;
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The production runner, wired here so `--no-brain` needs no dispatcher and no brain.
struct DirectRunner {
    exec: Arc<Executor>,
    workspace: Arc<WorkspaceManager>,
}

#[async_trait]
impl NodeRunner for DirectRunner {
    async fn workspace(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree> {
        self.workspace.create(logical, attempt).await
    }
    async fn run(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<crate::worker::RunOutcome> {
        self.exec.run(spec, timeout, cancel).await
    }
    async fn finalize(
        &self,
        wt: &NodeWorktree,
        title: &str,
        tier: Tier,
    ) -> anyhow::Result<Option<WorkResultRef>> {
        self.workspace.finalize(wt, title, tier).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RunArgs;
    use crate::config::{load, resolve};
    use crate::journal::paths::Paths;
    use camino::Utf8PathBuf;

    fn git(dir: &Utf8PathBuf, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.name=swamp tests",
                "-c",
                "user.email=tests@swamp.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A worker that emits the recorded sample stream and edits one file in its worktree.
    fn fake_cli(dir: &Utf8PathBuf) -> Utf8PathBuf {
        let stream = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("docs/ref/claude-stream-sample.jsonl");
        let path = dir.join("fake-worker");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat > /dev/null\nprintf 'worker was here\\n' >> fixed.txt\ncat '{stream}'\n"
            ),
        )
        .expect("write the fake worker");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        path
    }

    fn args(task: &str) -> RunArgs {
        RunArgs {
            task: vec![task.to_owned()],
            no_brain: true,
            tier: Some(Tier::Mid),
            provider: None,
            account: None,
            workers: None,
            isolation: None,
            base: None,
            include_dirty: false,
            timeout: None,
            budget: None,
            max_attempts: None,
            wait: true,
            detach: false,
        }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: Utf8PathBuf,
        ctx: Ctx,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(tmp.path()).expect("canonicalize");
        let root = Utf8PathBuf::from_path_buf(root).expect("utf8");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir");
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("README.md"), "swamp\n").expect("README");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-q", "-m", "initial"]);

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin dir");
        let exec = fake_cli(&bin);
        let schema = toml::from_str(&format!(
            r#"
[workspace]
root = "{root}/worktrees"

[providers.anthropic]
models = {{ high = "tier-high", mid = "tier-mid", low = "tier-low" }}

[[accounts]]
id = "main"
provider = "anthropic"
exec = "{exec}"
"#
        ))
        .expect("config parses");
        let cfg = resolve::from_schema(load::merge(vec![
            load::default_layer(),
            load::Layer {
                origin: "test".into(),
                schema,
            },
        ]));
        let paths = Paths {
            repo: repo.clone(),
            dot_swamp: repo.join(".swamp"),
            home_swamp: root.join("home").join(".swamp"),
        };
        let ctx = Ctx {
            cfg: Arc::new(cfg),
            paths: Arc::new(paths),
            color: false,
            json: false,
        };
        Fixture {
            _tmp: tmp,
            repo,
            ctx,
        }
    }

    /// The smallest runnable thing, end to end: one node, one worktree, one patch, exit 0.
    #[tokio::test]
    async fn the_no_brain_smoke_path_runs_one_node_and_captures_one_patch() {
        let f = fixture();
        let code = run(&f.ctx, &args("fix the flaky test in tests/api.rs"))
            .await
            .expect("the run completes");
        assert_eq!(code, 0, "a successful worker exits 0");

        let runs = f.ctx.paths.list_runs().expect("runs");
        assert_eq!(runs.len(), 1, "one run");
        let paths = f.ctx.paths.run_paths(runs[0]);
        let journal = std::fs::read_to_string(paths.journal()).expect("journal");
        assert!(journal.contains("\"ev\":\"run_started\""), "{journal}");
        assert!(journal.contains("\"ev\":\"run_finished\""), "{journal}");

        let view = f.ctx.view(&paths, false).expect("fold");
        assert_eq!(view.nodes.len(), 1, "exactly one node");
        let node = view.nodes.values().next().expect("the node");
        assert_eq!(node.state, crate::model::core::NodeState::Succeeded);
        assert_eq!(node.model.as_deref(), Some("tier-mid"));
        assert_eq!(node.account.as_ref().map(|a| a.0.as_str()), Some("main"));
        assert_eq!(view.tree().len(), 1, "the node is the root of the tree");

        let prompt = std::fs::read_to_string(paths.prompt(node.id)).expect("prompt.md");
        assert_eq!(prompt, "fix the flaky test in tests/api.rs");
        let stream = std::fs::read(paths.stream(node.id)).expect("stream.jsonl");
        let fixture = std::fs::read(
            Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("docs/ref/claude-stream-sample.jsonl"),
        )
        .expect("fixture");
        assert_eq!(stream, fixture, "the raw stream is kept verbatim");

        let work = node.work.as_ref().expect("a work result");
        assert!(!work.empty, "the worker changed a file");
        let patch = std::fs::read_to_string(&work.patch).expect("patch.diff");
        assert!(patch.contains("fixed.txt"), "{patch}");
        assert!(work.branch.starts_with("swamp/"), "branch {}", work.branch);

        // The user's checkout is untouched: the work lives in a worktree outside the repo.
        let status = std::process::Command::new("git")
            .current_dir(&f.repo)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        let dirt = String::from_utf8_lossy(&status.stdout);
        assert!(
            dirt.lines().all(|l| l.contains(".swamp")),
            "the checkout must not carry worker edits: {dirt}"
        );
        assert!(node.workspace.path().is_dir(), "the worktree exists");
        assert!(!node.workspace.path().starts_with(&f.repo));
    }

    /// A failing task is never an infrastructure failure: exit 4, and no rotation.
    #[tokio::test]
    async fn a_failing_worker_exits_four() {
        let f = fixture();
        let exec = f.ctx.cfg.accounts[0].exec.clone();
        std::fs::write(&exec, "#!/bin/sh\ncat > /dev/null\necho boom >&2\nexit 3\n")
            .expect("rewrite the fake worker");
        // One attempt: a crash retries the same account, and the backoff is not the subject.
        let mut args = args("break everything");
        args.max_attempts = Some(1);
        let code = run(&f.ctx, &args).await.expect("run");
        assert_eq!(code, 4, "a task failure is exit 4");
        let runs = f.ctx.paths.list_runs().expect("runs");
        let view = f
            .ctx
            .view(&f.ctx.paths.run_paths(runs[0]), false)
            .expect("fold");
        assert!(view.finished, "RunFinished is journaled even on failure");
        assert!(
            view.nodes
                .values()
                .all(|n| matches!(n.state, crate::model::core::NodeState::Failed { .. })),
            "the node failed"
        );
    }
}
