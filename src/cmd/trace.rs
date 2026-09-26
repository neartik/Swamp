use crate::cli::{GroupBy, TraceArgs};
use crate::cmd::{Ctx, parse_duration};
use crate::ids::{DispatchId, NodeId};
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::ui::trace::{TraceOpts, follow, keep_since, render};
use camino::Utf8Path;
use std::io::Write;

/// Static tree render of a recorded run.
pub async fn run(ctx: &Ctx, args: &TraceArgs) -> anyhow::Result<i32> {
    let (paths, node, dispatch) = match args.dispatch.as_deref() {
        Some(spec) => {
            let (paths, id) = dispatch_in(ctx, args.run.as_deref(), spec)?;
            (paths, None, Some(id))
        }
        None => {
            let (paths, node) = locate(ctx, args)?;
            (paths, node, None)
        }
    };
    let opts = TraceOpts {
        node: None,
        events: args.events,
        raw: args.raw,
        stderr: args.stderr,
        depth: args.depth,
        failed: args.failed,
        json: ctx.json,
        dispatch,
        by_dispatch: args.group_by == Some(GroupBy::Dispatch),
    };

    if args.follow {
        follow(&paths, &TraceOpts { node, ..opts }).await?;
        return Ok(0);
    }

    let mut view = ctx.view(&paths, args.events)?;

    if args.raw || args.stderr {
        let node = node
            .or_else(|| only_node(&view))
            .ok_or_else(|| anyhow::anyhow!("--raw and --stderr need --node <id>"))?;
        let path = if args.raw {
            paths.stream(node)
        } else {
            paths.stderr(node)
        };
        return dump(&path).map(|()| 0);
    }

    if let Some(since) = &args.since {
        let cutoff = time::OffsetDateTime::now_utc() - parse_duration(since)?;
        keep_since(&mut view, cutoff);
    }
    let pidfiles = paths.clone();
    view.mark_orphans(&move |id| crate::worker::liveness::is_ours(&pidfiles.pidfile(id)));
    ctx.out(&render(&view, &TraceOpts { node, ..opts }));
    Ok(0)
}

/// Verbatim: the raw stream is the evidence, so it is never re-encoded.
fn dump(path: &Utf8Path) -> anyhow::Result<()> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let mut out = std::io::stdout().lock();
    out.write_all(&bytes)?;
    out.flush()?;
    Ok(())
}

fn only_node(view: &RunView) -> Option<NodeId> {
    match view.nodes.len() {
        1 => view.nodes.keys().next().copied(),
        _ => None,
    }
}

/// Within the named run, or across every run when none is named.
fn dispatch_in(ctx: &Ctx, run: Option<&str>, spec: &str) -> anyhow::Result<(RunPaths, DispatchId)> {
    let Some(run) = run else {
        return ctx.find_dispatch(spec);
    };
    let paths = ctx.run_paths(Some(run))?;
    let view = ctx.view(&paths, false)?;
    match crate::journal::inspect::match_dispatches(&view, spec)[..] {
        [one] => Ok((paths, one)),
        [] => anyhow::bail!("no dispatch matches `{spec}` in run {}", paths.run),
        _ => anyhow::bail!("dispatch `{spec}` is ambiguous in run {}", paths.run),
    }
}

/// The run that actually holds the node: without an explicit run, `--node <id>` searches
/// every run rather than only the one `last` points at.
fn locate(ctx: &Ctx, args: &TraceArgs) -> anyhow::Result<(RunPaths, Option<NodeId>)> {
    let paths = ctx.run_paths(args.run.as_deref())?;
    let Some(spec) = args.node.as_deref() else {
        return Ok((paths, None));
    };
    match resolve(ctx, &paths, spec) {
        Ok(id) => Ok((paths, Some(id))),
        Err(e) if args.run.is_some() => Err(e),
        Err(_) => {
            let (rp, node) = ctx.find_node(spec)?;
            Ok((rp, Some(node.id)))
        }
    }
}

fn resolve(ctx: &Ctx, paths: &RunPaths, spec: &str) -> anyhow::Result<NodeId> {
    let view = ctx.view(paths, false)?;
    // A logical short id lands on its finished attempt: --raw and --stderr name a directory.
    let hit = view
        .nodes
        .values()
        .find(|n| n.id.matches(spec))
        .map(|n| n.id)
        .or_else(|| {
            view.nodes
                .values()
                .find(|n| n.logical.matches(spec))
                .and_then(|n| super::attempt_of(&view, n.logical))
                .map(|n| n.id)
        });
    hit.ok_or_else(|| anyhow::anyhow!("no node matches `{spec}` in run {}", paths.run))
}
