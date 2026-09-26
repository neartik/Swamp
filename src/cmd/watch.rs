use crate::cli::WatchArgs;
use crate::cmd::Ctx;

/// Live TUI over a run journal; `k` cancels a node after a y / n. Needs no live supervisor.
pub async fn run(ctx: &Ctx, args: &WatchArgs) -> anyhow::Result<i32> {
    if args.board {
        return crate::cmd::board::from_watch(ctx, args).await;
    }
    let paths = ctx.run_paths(args.run.as_deref())?;
    crate::ui::watch::run_tui(paths, ctx.cfg.clone()).await?;
    Ok(0)
}
