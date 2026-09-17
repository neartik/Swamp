use crate::cli::{BoardArgs, WatchArgs};
use crate::cmd::Ctx;
use crate::ui::board::sources::{Scope, Sources};
use crate::ui::board::{app, render};
use crate::ui::chat::blocks::text_of;
use crate::ui::chat::theme::Theme;
use std::time::{Duration, Instant};

/// The read-only dispatch board of `docs/BOARD.md`. It tails journals and reads
/// `accounts.json`; it never talks to a supervisor, so it is safe to leave open forever.
pub async fn run(ctx: &Ctx, args: &BoardArgs) -> anyhow::Result<i32> {
    let scope = match (&args.run, args.all) {
        (Some(spec), _) => Scope::Pinned(ctx.paths.resolve_run(spec)?),
        (None, true) => Scope::All,
        (None, false) => Scope::Repo,
    };
    let mut sources = Sources::new(ctx.paths.clone(), ctx.cfg.clone(), scope);

    if args.json || ctx.json {
        let board = sources.board(Instant::now())?;
        ctx.out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&app::json(&board))?
        ));
        return Ok(0);
    }

    if args.once {
        let board = sources.board(Instant::now())?;
        let width = terminal_width();
        let theme = Theme::detect(ctx.color, ctx.cfg.ui.chat_theme.as_deref());
        let lines = render::frame(&board, width, &theme, 0, ctx.cfg.quota_max_age());
        ctx.out(&format!("{}\n", text_of(&lines).join("\n")));
        return Ok(0);
    }

    app::run_tui(sources, ctx.color, args.interval.map(Duration::from_millis)).await?;
    Ok(0)
}

/// `swamp watch --board`: the spelling people guess, forwarded whole.
pub async fn from_watch(ctx: &Ctx, args: &WatchArgs) -> anyhow::Result<i32> {
    run(
        ctx,
        &BoardArgs {
            run: args.run.clone(),
            ..BoardArgs::default()
        },
    )
    .await
}

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100)
}
