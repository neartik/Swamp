use crate::cli::{DispatchArgs, DispatchesArgs};
use crate::cmd::Ctx;
use crate::ui::dispatches::{ListOpts, follow, render_detail, render_list};
use time::OffsetDateTime;

/// `swamp dispatches [RUN]`: one row per dispatch of a run.
pub async fn list(ctx: &Ctx, args: &DispatchesArgs) -> anyhow::Result<i32> {
    let paths = ctx.run_paths(args.run.as_deref())?;
    let opts = ListOpts {
        failed: args.failed,
        json: args.json || ctx.json,
    };
    if args.follow {
        follow(&paths, opts).await?;
        return Ok(0);
    }
    let mut view = ctx.view(&paths, false)?;
    view.mark_orphans(&|id| crate::worker::liveness::is_ours(&paths.pidfile(id)));
    ctx.out(&render_list(&view, opts, OffsetDateTime::now_utc()));
    Ok(0)
}

/// `swamp dispatch <ID>`: the task tree of one dispatch, from whichever run holds it.
pub async fn show(ctx: &Ctx, args: &DispatchArgs) -> anyhow::Result<i32> {
    let (paths, id) = ctx.find_dispatch(&args.id)?;
    let mut view = ctx.view(&paths, false)?;
    view.mark_orphans(&|id| crate::worker::liveness::is_ours(&paths.pidfile(id)));
    let json = args.json || ctx.json;
    ctx.out(&render_detail(&view, id, json, OffsetDateTime::now_utc()));
    Ok(0)
}
