use crate::cli::ChatArgs;
use crate::cmd::{Ctx, RunSession};
use crate::config::Config;
use crate::ids::{NodeId, RunId};
use crate::mcp::McpServer;
use crate::model::core::{AccountId, NodeState, Provider, Tier};
use std::sync::Arc;
use tokio::time::Instant;

/// Interactive brain session. The brain is just another account in the same pool.
pub async fn run(ctx: &Ctx, args: &ChatArgs) -> anyhow::Result<i32> {
    let cfg = Arc::new(overrides(ctx, args)?);
    let depth = crate::cmd::guard_depth(&cfg)?;
    let resume = match args.resume.as_deref() {
        Some(spec) => Some(ctx.paths.resolve_run(spec)?),
        None => None,
    };
    let session = RunSession::start(ctx, cfg.clone(), RunId::new(), None).await?;
    ctx.paths
        .register_run(session.paths.run, &session.paths.dir)
        .ok();
    // After the run exists, never before: the board is pinned to this session, and its id is
    // only minted by `RunSession::start`.
    if args.board {
        open_board_pane(&cfg, session.paths.run);
    }
    // The interactive UI prints the run id in its welcome box; the header would leak above it.
    if !crate::ui::chat::interactive_stdout() {
        println!("run {}", session.paths.run);
    }

    let provider = cfg.brain.provider.unwrap_or(Provider::Anthropic);
    let dispatcher = session.dispatcher();
    dispatcher.set_base_depth(depth);
    let (server, socket) = McpServer::bind(
        &session.paths,
        dispatcher.clone(),
        Arc::new(session.journal.clone()),
    )
    .await?;
    let serving = server.serve();

    let tier = cfg.brain.tier.unwrap_or(Tier::High);
    let lease = session
        .pool
        .acquire_brain(
            provider,
            cfg.brain.account.as_ref(),
            Instant::now() + cfg.node_timeout(tier),
            Some(NodeId(session.paths.run.0)),
        )
        .await
        .map_err(|e| anyhow::anyhow!("no account for the brain: {e:?}"))?;
    let handle = resume.and_then(|run| previous_session(ctx, run));
    let brain = crate::brain::build(
        &cfg,
        lease,
        &session.paths,
        &socket,
        session.journal.clone(),
        handle,
        crate::brain::BrainMode::Interactive,
    )?;

    let code = crate::ui::chat::repl(brain, dispatcher, ctx).await?;
    serving.abort();

    let view = ctx.view(&session.paths, false)?;
    let totals = view.totals();
    let run = session.paths.run;
    session
        .finish(
            NodeState::Succeeded,
            totals.nodes,
            totals.usage,
            Some(totals.cost_usd),
        )
        .await?;
    ctx.paths.deregister_run(run).ok();
    Ok(code)
}

/// A resume handle is only valid together with the account that minted it, so it is taken
/// from the journal rather than reconstructed.
fn previous_session(ctx: &Ctx, run: RunId) -> Option<crate::model::core::SessionHandle> {
    let paths = ctx.paths.run_paths(run);
    let view = ctx.view(&paths, false).ok()?;
    view.nodes
        .values()
        .rfind(|n| n.kind == crate::model::core::NodeKind::Brain)
        .and_then(|n| n.session.clone())
}

/// `ui.board_width` default, per docs/BOARD.md §5.
const BOARD_WIDTH: u16 = 46;

/// docs/BOARD.md §5: `--board` splits a pane for `swamp board` when tmux is available, and
/// otherwise just names the command, since chat must never fail over a pane it cannot open.
fn open_board_pane(cfg: &Config, run: RunId) {
    if std::env::var_os("TMUX").is_none() {
        eprintln!("--board only works inside tmux; run `swamp board --run {run}` in another pane");
        return;
    }
    let argv = board_split_argv(cfg.ui.board_width.unwrap_or(BOARD_WIDTH), run);
    if let Err(e) = std::process::Command::new("tmux").args(&argv).status() {
        eprintln!("--board: could not start `swamp board` in a tmux split: {e}");
    }
}

/// Split the width `ui.board_width` asks for, and pin the board to the run chat just started
/// so another live run in the repo cannot pull it away.
fn board_split_argv(width: u16, run: RunId) -> Vec<String> {
    [
        "split-window",
        "-h",
        "-l",
        &width.max(1).to_string(),
        "-d",
        "swamp",
        "board",
        "--run",
        &run.to_string(),
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect()
}

fn overrides(ctx: &Ctx, args: &ChatArgs) -> anyhow::Result<Config> {
    let mut cfg = (*ctx.cfg).clone();
    if let Some(p) = args.brain {
        cfg.brain.provider = Some(p);
    }
    if let Some(t) = args.tier {
        cfg.brain.tier = Some(t);
    }
    if let Some(id) = &args.account {
        let id = AccountId(id.clone());
        anyhow::ensure!(
            cfg.account(&id).is_some(),
            "no account `{}` in the configuration",
            id.0
        );
        cfg.brain.account = Some(id);
    }
    if args.dry_run {
        cfg.warnings
            .push("--dry-run: dispatch tools journal and return a fake success".into());
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/BOARD.md §5: the split is sized by `ui.board_width` and pinned to the run chat
    /// just started, or a board opened next to a busy repo shows somebody else's run.
    #[test]
    fn the_board_split_is_sized_and_pinned_to_this_run() {
        let run = RunId::new();
        let argv = board_split_argv(60, run);
        assert_eq!(
            argv,
            vec![
                "split-window",
                "-h",
                "-l",
                "60",
                "-d",
                "swamp",
                "board",
                "--run",
                &run.to_string(),
            ]
        );
        assert_eq!(board_split_argv(BOARD_WIDTH, run)[3], "46");
        assert_eq!(
            board_split_argv(0, run)[3],
            "1",
            "tmux rejects a zero split"
        );
    }
}
