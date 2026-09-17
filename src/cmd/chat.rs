use crate::cli::ChatArgs;
use crate::cmd::{Ctx, RunSession};
use crate::config::Config;
use crate::ids::RunId;
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
