use crate::cli::TraceArgs;
use crate::cmd::{Ctx, parse_duration};
use crate::ids::NodeId;
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::ui::trace::{TraceOpts, follow, keep_since, render};
use camino::Utf8Path;
use std::io::Write;

/// Static tree render of a recorded run.
pub async fn run(ctx: &Ctx, args: &TraceArgs) -> anyhow::Result<i32> {
    let paths = ctx.run_paths(args.run.as_deref())?;
    let opts = TraceOpts {
        node: None,
        events: args.events,
        raw: args.raw,
        stderr: args.stderr,
        depth: args.depth,
        failed: args.failed,
        json: ctx.json,
    };

    if args.follow {
        let node = args
            .node
            .as_deref()
            .map(|spec| resolve(ctx, &paths, spec))
            .transpose()?;
        follow(&paths, &TraceOpts { node, ..opts }).await?;
        return Ok(0);
    }

    let mut view = ctx.view(&paths, args.events)?;
    let node = args
        .node
        .as_deref()
        .map(|spec| resolve(ctx, &paths, spec))
        .transpose()?;

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

fn resolve(ctx: &Ctx, paths: &RunPaths, spec: &str) -> anyhow::Result<NodeId> {
    let view = ctx.view(paths, false)?;
    let hit = view
        .nodes
        .keys()
        .find(|id| super::node_matches(**id, spec))
        .copied();
    hit.ok_or_else(|| anyhow::anyhow!("no node matches `{spec}` in run {}", paths.run))
}
