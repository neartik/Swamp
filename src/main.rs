use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use std::sync::Arc;
use swamp::cli::{ChatArgs, Cli, Command};
use swamp::cmd::{self, Ctx};
use swamp::config::Config;
use swamp::error::exit_code;
use swamp::journal::paths::Paths;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let root = cli
        .cd
        .clone()
        .unwrap_or_else(|| Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap());
    let _log = init_tracing(&root, cli.verbose, cli.quiet);

    match dispatch(&cli, &root).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("swamp: {e:#}");
            std::process::exit(exit_code(&e));
        }
    }
}

/// Logs go to .swamp/swamp.log. The journal is a separate, structured artifact.
fn init_tracing(
    root: &Utf8Path,
    verbose: u8,
    quiet: bool,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt};

    let dir = root.join(".swamp");
    std::fs::create_dir_all(&dir).ok()?;
    let level = match (quiet, verbose) {
        (true, _) => "error",
        (_, 0) => "warn",
        (_, 1) => "info",
        (_, 2) => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("SWAMP_LOG").unwrap_or_else(|_| EnvFilter::new(level));
    let appender = tracing_appender::rolling::never(dir.as_std_path(), "swamp.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .init();
    Some(guard)
}

fn completions(args: &swamp::cli::CompletionsArgs) -> Result<i32> {
    use clap::CommandFactory;
    clap_complete::generate(
        args.shell,
        &mut Cli::command(),
        "swamp",
        &mut std::io::stdout(),
    );
    Ok(0)
}

async fn dispatch(cli: &Cli, root: &Utf8Path) -> Result<i32> {
    if let Some(Command::McpBridge(args)) = &cli.command {
        return swamp::mcp::run_bridge(&args.socket).await.map(|()| 0);
    }

    // Paths first: config discovery must use the same canonical git root, or `<repo>/.swamp/
    // config.toml` is invisible from every subdirectory.
    let paths = Paths::discover(root)?;
    let cfg = Config::load(&paths.repo, cli.config.as_deref(), cli.profile.as_deref())?;
    let ctx = Ctx {
        cfg: Arc::new(cfg),
        paths: Arc::new(paths),
        color: !cli.no_color,
        json: cli.json,
        config_arg: cli.config.clone(),
        profile: cli.profile.clone(),
    };

    let default_chat = ChatArgs::default();
    match &cli.command {
        None => cmd::chat::run(&ctx, &default_chat).await,
        Some(Command::Chat(a)) => cmd::chat::run(&ctx, a).await,
        Some(Command::Run(a)) => cmd::run::run(&ctx, a).await,
        Some(Command::Trace(a)) => cmd::trace::run(&ctx, a).await,
        Some(Command::Watch(a)) => cmd::watch::run(&ctx, a).await,
        Some(Command::Board(a)) => cmd::board::run(&ctx, a).await,
        Some(Command::Runs(a)) => cmd::runs::run(&ctx, a).await,
        Some(Command::Resume(a)) => cmd::resume::run(&ctx, a).await,
        Some(Command::Accounts(a)) => cmd::accounts::run(&ctx, a).await,
        Some(Command::Usage(a)) => cmd::usage::run(&ctx, a).await,
        Some(Command::Diff(a)) => cmd::diff::run(&ctx, a).await,
        Some(Command::Adopt(a)) => cmd::adopt::run(&ctx, a).await,
        Some(Command::Worktrees(a)) => cmd::worktrees::run(&ctx, a).await,
        Some(Command::Cancel(a)) => cmd::cancel::run(&ctx, a).await,
        Some(Command::Gc(a)) => cmd::gc::run(&ctx, a).await,
        Some(Command::Replay(a)) => cmd::replay::run(&ctx, a).await,
        Some(Command::Doctor(a)) => cmd::doctor::run(&ctx, a).await,
        Some(Command::Config(a)) => cmd::config::run(&ctx, a).await,
        Some(Command::Completions(a)) => completions(a),
        Some(Command::McpBridge(_)) => unreachable!("handled above"),
    }
}
