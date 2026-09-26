use crate::cli::{CancelArgs, SignalArg};
use crate::cmd::Ctx;
use crate::dispatch::cancel::{Outcome, Sink, Stop, cancel_node};
use crate::ids::{DispatchId, NodeId};
use crate::journal::fold::RunView;
use crate::journal::inspect;
use crate::journal::paths::RunPaths;
use crate::model::core::{CancelSource, NodeKind, NodeState};
use crate::ui::fmt;
use crate::worker::spawn::{Reaper, terminate};
use std::time::Duration;

/// What one TARGET named: a whole run, or some of its tasks.
struct Target {
    paths: RunPaths,
    view: RunView,
    tasks: Vec<NodeId>,
    /// A whole run also stops what is not a task, the brain included.
    whole: bool,
}

/// Cancel a run, a node or a dispatch. Detached workers do not care that the supervisor is
/// gone, so killing their process groups is the only way to stop them; every task is also
/// journaled as cancelled, so a live supervisor does not retry it.
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
                targets.push(whole_run(paths, view));
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
            let outcome = cancel_node(
                &t.paths,
                &t.view,
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
            cancelled += stop_the_rest(&t, grace).await?;
        }
    }
    println!("cancelled {cancelled} nodes");
    Ok(if cancelled > 0 { 0 } else { 1 })
}

fn whole_run(paths: RunPaths, view: RunView) -> Target {
    let tasks = view.tasks.keys().copied().collect();
    Target {
        paths,
        view,
        tasks,
        whole: true,
    }
}

/// A run spec first, then nodes and dispatches together: a prefix that names one of each is
/// ambiguous, exactly as two nodes would be.
fn resolve(ctx: &Ctx, spec: &str) -> anyhow::Result<Target> {
    let spec = spec.trim();
    let prefixed = spec.starts_with("dsp_") || spec.starts_with("nd_");
    if !prefixed && let Ok(paths) = ctx.run_paths(Some(spec)) {
        let view = ctx.view(&paths, false)?;
        return Ok(whole_run(paths, view));
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
        ([(paths, node)], []) => tasks(ctx, paths.clone(), vec![node.logical]),
        ([], [(paths, id)]) => dispatch(ctx, paths.clone(), *id),
        ([], []) => match unstarted(ctx, spec)? {
            Some((paths, logical)) => tasks(ctx, paths, vec![logical]),
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

fn tasks(ctx: &Ctx, paths: RunPaths, tasks: Vec<NodeId>) -> anyhow::Result<Target> {
    let view = ctx.view(&paths, false)?;
    Ok(Target {
        paths,
        view,
        tasks,
        whole: false,
    })
}

fn dispatch(ctx: &Ctx, paths: RunPaths, id: DispatchId) -> anyhow::Result<Target> {
    let view = ctx.view(&paths, false)?;
    let tasks = view
        .dispatches
        .get(&id)
        .map(|d| d.tasks.clone())
        .unwrap_or_default();
    Ok(Target {
        paths,
        view,
        tasks,
        whole: false,
    })
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
            hits.push((paths.clone(), id));
        }
    }
    match hits.len() {
        0 => Ok(None),
        1 => Ok(hits.pop()),
        _ => anyhow::bail!("node `{spec}` is ambiguous"),
    }
}

/// What a whole run holds besides its tasks: the brain, or a schema-1 node nobody tracked.
async fn stop_the_rest(t: &Target, grace: Duration) -> anyhow::Result<usize> {
    let mut killed = 0;
    for (id, node) in &t.view.nodes {
        let NodeState::Running { pid, pgid, .. } = node.state else {
            continue;
        };
        if node.kind != NodeKind::Brain && t.tasks.contains(&node.logical) {
            continue;
        }
        // A recycled pid can belong to anything; the pidfile carries the start time.
        if !crate::worker::liveness::is_ours(&t.paths.pidfile(*id)) {
            if crate::worker::liveness::running(pid) {
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
