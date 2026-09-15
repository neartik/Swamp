use crate::cli::{CancelArgs, SignalArg};
use crate::cmd::Ctx;
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::model::core::NodeState;
use crate::worker::spawn::{Reaper, terminate};
use std::time::Duration;

/// Cancel a run or a node by killing its process group. Detached workers do not care that
/// the supervisor is gone, so this is the only way to stop them.
pub async fn run(ctx: &Ctx, args: &CancelArgs) -> anyhow::Result<i32> {
    let grace = match args.signal {
        SignalArg::Term => ctx
            .cfg
            .limits
            .grace_period
            .unwrap_or(Duration::from_secs(5)),
        SignalArg::Kill => Duration::ZERO,
    };

    let mut targets: Vec<(RunPaths, RunView)> = Vec::new();
    if args.all {
        for id in ctx.paths.list_runs()? {
            let paths = ctx.paths.run_paths(id);
            if let Ok(view) = RunView::load(&paths.dir, false)
                && !view.finished
            {
                targets.push((paths, view));
            }
        }
    }
    for spec in &args.targets {
        if let Ok(paths) = ctx.run_paths(Some(spec)) {
            let view = ctx.view(&paths, false)?;
            targets.push((paths, view));
            continue;
        }
        let (paths, node) = ctx.find_node(spec)?;
        let mut view = ctx.view(&paths, false)?;
        view.nodes.retain(|id, _| *id == node.id);
        targets.push((paths, view));
    }
    if targets.is_empty() {
        anyhow::bail!("nothing to cancel: name a run or a node, or pass --all");
    }

    let mut killed = 0;
    for (paths, view) in targets {
        for (id, node) in &view.nodes {
            let NodeState::Running { pid, pgid, .. } = node.state else {
                continue;
            };
            // A recycled pid can belong to anything; the pidfile carries the start time.
            if !crate::worker::liveness::is_ours(&paths.pidfile(*id)) {
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
                paths.run
            );
        }
    }
    println!("cancelled {killed} nodes");
    Ok(if killed > 0 { 0 } else { 1 })
}
