use crate::cli::{CancelArgs, SignalArg};
use crate::cmd::Ctx;
use crate::dispatch::cancel::{Outcome, Sink, Stop, cancel_node, dispatch_tasks};
use crate::ids::{DispatchId, NodeId};
use crate::journal::fold::RunView;
use crate::journal::inspect;
use crate::journal::paths::RunPaths;
use crate::model::core::{CancelSource, NodeKind, NodeState};
use crate::ui::fmt;
use crate::worker::liveness;
use crate::worker::spawn::{Reaper, terminate};
use camino::Utf8Path;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::time::{Duration, Instant};

/// What one TARGET named: a whole run, or some of its tasks.
struct Target {
    paths: RunPaths,
    tasks: Vec<NodeId>,
    /// A whole run also stops what is not a task, the brain included.
    whole: bool,
}

/// Cancel a run, a node or a dispatch: kill its process groups and journal each task cancelled.
pub async fn run(ctx: &Ctx, args: &CancelArgs) -> anyhow::Result<i32> {
    let grace = match args.signal {
        SignalArg::Term => ctx
            .cfg
            .limits
            .grace_period
            .unwrap_or(Duration::from_secs(5)),
        SignalArg::Kill => Duration::ZERO,
    };

    let mut targets: Vec<Target> = Vec::new();
    if args.all {
        for id in ctx.paths.list_runs()? {
            let paths = ctx.paths.run_paths(id);
            if let Ok(view) = RunView::load(&paths.dir, false)
                && !view.finished
            {
                targets.push(whole_run(paths, &view));
            }
        }
    }
    for spec in &args.targets {
        targets.push(resolve(ctx, spec)?);
    }
    if targets.is_empty() {
        anyhow::bail!("nothing to cancel: name a run, a node or a dispatch, or pass --all");
    }

    let mut cancelled = 0;
    for t in targets {
        for logical in &t.tasks {
            // An earlier task's grace period gives this one time to settle on its own.
            let view = ctx.view(&t.paths, false)?;
            let outcome = cancel_node(
                &t.paths,
                &view,
                Sink::Shared,
                *logical,
                CancelSource::User,
                Stop::Kill { grace },
            )
            .await?;
            match outcome {
                Outcome::Cancelled { pgid, .. } => {
                    cancelled += 1;
                    let group = pgid.map(|g| format!(" (pgid {g})")).unwrap_or_default();
                    println!(
                        "cancelled node {}{group} in run {}",
                        logical.short(),
                        t.paths.run
                    );
                }
                Outcome::Killed { phase, pgid } => {
                    cancelled += 1;
                    println!(
                        "stopped node {} (pgid {pgid}), already {}, in run {}",
                        logical.short(),
                        fmt::phase_word(phase),
                        t.paths.run
                    );
                }
                Outcome::Ended(phase) if !t.whole => {
                    println!(
                        "node {} already {}",
                        logical.short(),
                        fmt::phase_word(phase)
                    );
                }
                Outcome::Ended(_) => {}
            }
        }
        if t.whole {
            let view = ctx.view(&t.paths, false)?;
            cancelled += stop_the_rest(&t, &view, grace).await?;
        }
    }
    println!("cancelled {cancelled} nodes");
    Ok(if cancelled > 0 { 0 } else { 1 })
}

fn whole_run(paths: RunPaths, view: &RunView) -> Target {
    Target {
        paths,
        tasks: view.tasks.keys().copied().collect(),
        whole: true,
    }
}

/// A run first, then nodes and dispatches together: a prefix naming one of each is ambiguous.
fn resolve(ctx: &Ctx, spec: &str) -> anyhow::Result<Target> {
    let spec = spec.trim();
    let prefixed = spec.starts_with("dsp_") || spec.starts_with("nd_");
    if !prefixed && let Ok(paths) = ctx.run_paths(Some(spec)) {
        let view = ctx.view(&paths, false)?;
        return Ok(whole_run(paths, &view));
    }
    let nodes = if spec.starts_with("dsp_") {
        Vec::new()
    } else {
        ctx.node_hits(spec)?
    };
    let dispatches = if spec.starts_with("nd_") {
        Vec::new()
    } else {
        ctx.dispatch_hits(spec)?
    };
    match (&nodes[..], &dispatches[..]) {
        ([(paths, node)], []) if node.kind == NodeKind::Brain => anyhow::bail!(
            "node {} is the brain of run {}: cancel the run instead",
            node.id.short(),
            paths.run
        ),
        ([(paths, node)], []) => Ok(tasks(paths.clone(), vec![node.logical])),
        ([], [(paths, id)]) => dispatch(ctx, paths.clone(), *id),
        ([], []) => match unstarted(ctx, spec)? {
            Some((paths, logical)) => Ok(tasks(paths, vec![logical])),
            None => anyhow::bail!("no run, node or dispatch matches `{spec}`"),
        },
        _ if dispatches.is_empty() => {
            anyhow::bail!("node `{spec}` is ambiguous: {}", super::candidates(&nodes))
        }
        _ if nodes.is_empty() => anyhow::bail!(
            "dispatch `{spec}` is ambiguous: {}",
            ctx.dispatch_candidates(&dispatches)
        ),
        _ => anyhow::bail!(
            "`{spec}` is ambiguous: nodes {}; dispatches {}",
            super::candidates(&nodes),
            ctx.dispatch_candidates(&dispatches)
        ),
    }
}

fn tasks(paths: RunPaths, tasks: Vec<NodeId>) -> Target {
    Target {
        paths,
        tasks,
        whole: false,
    }
}

fn dispatch(ctx: &Ctx, paths: RunPaths, id: DispatchId) -> anyhow::Result<Target> {
    let view = ctx.view(&paths, false)?;
    let list = dispatch_tasks(&view, id);
    Ok(tasks(paths, list))
}

/// A task still waiting for its first attempt has no node to find, only a logical id.
fn unstarted(ctx: &Ctx, spec: &str) -> anyhow::Result<Option<(RunPaths, NodeId)>> {
    let mut hits = Vec::new();
    for run in ctx.paths.list_runs()? {
        let paths = ctx.paths.run_paths(run);
        let Ok(view) = RunView::load(&paths.dir, false) else {
            continue;
        };
        for id in inspect::match_tasks(&view, spec) {
            let title = view
                .tasks
                .get(&id)
                .map(|t| t.title.clone())
                .unwrap_or_default();
            hits.push((paths.clone(), id, title));
        }
    }
    match hits.len() {
        0 => Ok(None),
        1 => Ok(hits.pop().map(|(paths, id, _)| (paths, id))),
        _ => anyhow::bail!(
            "node `{spec}` is ambiguous: {}",
            hits.iter()
                .take(8)
                .map(|(rp, id, title)| format!(
                    "{} (run {}, {})",
                    id.short(),
                    rp.run.short(),
                    fmt::truncate(title, 40)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// What a whole run holds besides its tasks: the brain, or a schema-1 node nobody tracked.
async fn stop_the_rest(t: &Target, view: &RunView, grace: Duration) -> anyhow::Result<usize> {
    let mut killed = 0;
    for (id, node) in &view.nodes {
        if node.kind == NodeKind::Brain {
            if node.state.is_terminal() {
                continue;
            }
            // The brain pidfile names its supervisor; the journaled pgid is a past turn's child.
            let pidfile = t.paths.pidfile(*id);
            let Some(pid) = liveness::owner(&pidfile) else {
                continue;
            };
            stop_supervisor(&pidfile, pid, grace).await;
            killed += 1;
            println!(
                "stopped the brain of run {} (supervisor pid {pid})",
                t.paths.run
            );
            continue;
        }
        let NodeState::Running { pid, pgid, .. } = node.state else {
            continue;
        };
        if t.tasks.contains(&node.logical) {
            continue;
        }
        // A recycled pid can belong to anything; the pidfile carries the start time.
        if !liveness::is_ours(&t.paths.pidfile(*id)) {
            if liveness::running(pid) {
                println!(
                    "skipping node {}: pid {pid} is not ours any more",
                    id.short()
                );
            }
            continue;
        }
        terminate(pgid, grace, Reaper::Here).await?;
        killed += 1;
        println!(
            "cancelled node {} (pgid {pgid}) in run {}",
            id.short(),
            t.paths.run
        );
    }
    Ok(killed)
}

/// SIGTERM lets the supervisor cancel its own work and exit; SIGKILL once the grace runs out.
async fn stop_supervisor(pidfile: &Utf8Path, pid: i32, grace: Duration) {
    let target = Pid::from_raw(pid);
    let gone = || liveness::owner(pidfile) != Some(pid);
    if !grace.is_zero() {
        let _ = kill(target, Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if gone() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    if !gone() {
        let _ = kill(target, Signal::SIGKILL);
    }
}
