use crate::cli::{BoardArgs, WatchArgs};
use crate::cmd::Ctx;
use crate::ui::board::sources::{Scope, Sources};
use crate::ui::board::{app, model, render};
use crate::ui::chat::blocks::text_of;
use crate::ui::chat::theme::Theme;
use std::time::{Duration, Instant};

/// The dispatch board of `docs/BOARD.md`. It tails journals and reads `accounts.json`; it
/// never talks to a supervisor, and the one thing it changes is a confirmed cancel.
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
        let mut board = sources.board(Instant::now())?;
        if let Some(sel) = model::attention(&board.rows()) {
            board.selected = sel;
        }
        let width = terminal_width();
        let theme = Theme::detect(ctx.color, ctx.cfg.ui.chat_theme.as_deref());
        let hide = app::hidden_keys(ctx.cfg.ui.board_actions.unwrap_or(true));
        let lines = render::frame(&board, width, &theme, 0, ctx.cfg.quota_max_age(), &hide);
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

/// `$COLUMNS` first, so a script can pick the tier it wants; then the terminal; then Wide.
fn terminal_width() -> u16 {
    width_from(
        std::env::var("COLUMNS").ok().as_deref(),
        crossterm::terminal::size().ok().map(|(w, _)| w),
    )
}

fn width_from(columns: Option<&str>, terminal: Option<u16>) -> u16 {
    columns
        .and_then(|c| c.trim().parse::<u16>().ok())
        .filter(|w| *w > 0)
        .or(terminal)
        .unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::width_from;

    #[test]
    fn columns_wins_over_the_terminal_and_garbage_falls_through() {
        assert_eq!(width_from(Some("40"), Some(200)), 40);
        assert_eq!(width_from(Some("wide"), Some(120)), 120);
        assert_eq!(width_from(Some("0"), None), 100);
        assert_eq!(width_from(None, None), 100);
    }
}
