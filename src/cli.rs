use crate::model::core::{Provider, Tier};
use crate::model::result::IsolationMode;
use crate::workspace::adopt::MergeStrategy;
use camino::Utf8PathBuf;
use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "swamp",
    version,
    about = "Parallel agent orchestration over CLI subscriptions",
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Project root (default: cwd, walked up to the git root)
    #[arg(short = 'C', long = "cd", value_name = "DIR", global = true)]
    pub cd: Option<Utf8PathBuf>,
    /// Extra config layer, highest priority
    #[arg(long, value_name = "FILE", global = true)]
    pub config: Option<Utf8PathBuf>,
    /// Named profile from [profiles.<name>]
    #[arg(long, value_name = "NAME", global = true)]
    pub profile: Option<String>,
    /// Machine-readable output where applicable
    #[arg(long, global = true)]
    pub json: bool,
    /// -v info, -vv debug, -vvv trace; logs to .swamp/swamp.log
    #[arg(short = 'v', long, action = ArgAction::Count, global = true)]
    pub verbose: u8,
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,
    #[arg(long, global = true)]
    pub no_color: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Interactive session with the brain
    Chat(ChatArgs),
    /// One-shot, non-interactive
    Run(RunArgs),
    /// Render a run tree
    Trace(TraceArgs),
    /// Live TUI, read-only, attachable from another terminal
    Watch(WatchArgs),
    /// List runs, newest first
    Runs(RunsArgs),
    /// Recover an interrupted run
    Resume(ResumeArgs),
    /// Account health and rotation state
    Accounts(AccountsArgs),
    /// Show a worker's diff
    Diff(DiffArgs),
    /// Land a worker's work in the user's tree
    Adopt(AdoptArgs),
    /// Inspect and clean worker worktrees
    Worktrees(WorktreesArgs),
    /// Cancel a run or a node
    Cancel(CancelArgs),
    /// Delete old runs and worktrees
    Gc(GcArgs),
    /// Re-render, or re-derive, a recorded run
    Replay(ReplayArgs),
    /// Health checks
    Doctor(DoctorArgs),
    /// Inspect the effective configuration
    Config(ConfigArgs),
    /// Print a shell completion script
    Completions(CompletionsArgs),
    /// stdio <-> UDS pump, spawned by the brain CLI
    #[command(hide = true)]
    McpBridge(McpBridgeArgs),
}

#[derive(Debug, Default, Args)]
pub struct ChatArgs {
    /// Override [brain].provider
    #[arg(long, value_name = "PROVIDER")]
    pub brain: Option<Provider>,
    /// Pin the brain to one account
    #[arg(long, value_name = "ID")]
    pub account: Option<String>,
    /// Brain tier
    #[arg(long, value_name = "TIER")]
    pub tier: Option<Tier>,
    /// Relaunch the brain with --resume plus a state preamble
    #[arg(long, value_name = "RUN")]
    pub resume: Option<String>,
    /// Override limits.max_parallel
    #[arg(long, value_name = "N")]
    pub workers: Option<usize>,
    #[arg(long, value_name = "USD")]
    pub budget: Option<f64>,
    /// Boot a real brain whose dispatch tools journal and return a fake success
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// The task. `-` reads stdin; `@file` reads a file
    #[arg(value_name = "TASK", num_args = 1..)]
    pub task: Vec<String>,
    /// Dispatch TASK to a single worker, with no brain in the loop
    #[arg(long)]
    pub no_brain: bool,
    #[arg(long, value_name = "TIER")]
    pub tier: Option<Tier>,
    #[arg(long, value_name = "PROVIDER")]
    pub provider: Option<Provider>,
    /// Pins the account and disables failover
    #[arg(long, value_name = "ID")]
    pub account: Option<String>,
    #[arg(long, value_name = "N")]
    pub workers: Option<usize>,
    #[arg(long, value_name = "MODE")]
    pub isolation: Option<IsolationMode>,
    /// Branch workers from this ref instead of HEAD
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
    /// Base from `git stash create` so uncommitted work is visible
    #[arg(long)]
    pub include_dirty: bool,
    #[arg(long, value_name = "DUR")]
    pub timeout: Option<String>,
    #[arg(long, value_name = "USD")]
    pub budget: Option<f64>,
    #[arg(long, value_name = "N")]
    pub max_attempts: Option<u32>,
    #[arg(long, conflicts_with = "detach")]
    pub wait: bool,
    #[arg(long)]
    pub detach: bool,
}

#[derive(Debug, Args)]
pub struct TraceArgs {
    /// A full id, a unique prefix, `last`, or `-2`
    #[arg(value_name = "RUN")]
    pub run: Option<String>,
    #[arg(long, value_name = "ID")]
    pub node: Option<String>,
    #[arg(long)]
    pub events: bool,
    #[arg(long)]
    pub raw: bool,
    #[arg(long)]
    pub stderr: bool,
    #[arg(long)]
    pub follow: bool,
    #[arg(long, value_name = "N")]
    pub depth: Option<u32>,
    #[arg(long)]
    pub failed: bool,
    #[arg(long, value_name = "DUR")]
    pub since: Option<String>,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    #[arg(value_name = "RUN")]
    pub run: Option<String>,
}

#[derive(Debug, Args)]
pub struct RunsArgs {
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub interrupted: bool,
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    #[arg(value_name = "RUN")]
    pub run: Option<String>,
    /// Print the recovery plan and exit, spending nothing
    #[arg(long)]
    pub plan: bool,
    #[arg(long, value_name = "NODE")]
    pub only: Vec<String>,
    #[arg(long)]
    pub rerun_failed: bool,
    #[arg(long)]
    pub no_brain: bool,
}

#[derive(Debug, Args)]
pub struct AccountsArgs {
    #[command(subcommand)]
    pub command: Option<AccountsCmd>,
}

#[derive(Debug, Subcommand)]
pub enum AccountsCmd {
    /// Health, inflight, 5h/7d, cooldown, nodes, spend
    List,
    /// Run `<exec> --version` plus a one-token probe per account
    Check {
        #[arg(long, value_name = "ID")]
        account: Option<String>,
    },
    Cooldown {
        #[arg(value_name = "ID")]
        id: String,
        #[arg(value_name = "DUR")]
        duration: String,
    },
    Clear {
        #[arg(value_name = "ID")]
        id: String,
    },
    Enable {
        #[arg(value_name = "ID")]
        id: String,
    },
    Disable {
        #[arg(value_name = "ID")]
        id: String,
    },
    Reset,
}

#[derive(Debug, Args)]
pub struct DiffArgs {
    #[arg(value_name = "NODE")]
    pub node: String,
    #[arg(long)]
    pub stat: bool,
    #[arg(long)]
    pub name_only: bool,
    #[arg(long)]
    pub patch: bool,
}

#[derive(Debug, Args)]
pub struct AdoptArgs {
    #[arg(value_name = "NODE", required = true)]
    pub nodes: Vec<String>,
    #[arg(long, value_name = "STRATEGY", default_value = "apply")]
    pub strategy: MergeStrategy,
    #[arg(long, value_name = "BRANCH")]
    pub into: Option<String>,
    #[arg(long)]
    pub dry_run: bool,
    /// Adopt into a dirty tree
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct WorktreesArgs {
    #[command(subcommand)]
    pub command: Option<WorktreesCmd>,
}

#[derive(Debug, Subcommand)]
pub enum WorktreesCmd {
    Ls,
    Prune,
    Open {
        #[arg(value_name = "NODE")]
        node: String,
    },
}

#[derive(Debug, Args)]
pub struct CancelArgs {
    #[arg(value_name = "TARGET")]
    pub targets: Vec<String>,
    #[arg(long)]
    pub all: bool,
    #[arg(long, value_name = "SIGNAL", default_value = "term")]
    pub signal: SignalArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SignalArg {
    Term,
    Kill,
}

#[derive(Debug, Args)]
pub struct GcArgs {
    #[arg(long, value_name = "DUR")]
    pub older_than: Option<String>,
    #[arg(long, value_name = "N")]
    pub keep: Option<u32>,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ReplayArgs {
    #[arg(value_name = "RUN")]
    pub run: String,
    /// Re-derive the journal from the retained raw streams with the current adapters
    #[arg(long)]
    pub reparse: bool,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Send a one-token prompt through each configured account
    #[arg(long)]
    pub probe: bool,
    #[arg(long)]
    pub fix: bool,
    /// Report the pattern-fallback and unparsed-line rates across recent runs
    #[arg(long)]
    pub schema: bool,
    /// Remove stale worktrees, sockets and pidfiles
    #[arg(long)]
    pub reap: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCmd,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCmd {
    Show {
        /// Print the merged result with the origin of every key
        #[arg(long)]
        effective: bool,
    },
    Path,
    Validate,
    Init,
}

#[derive(Debug, Args)]
pub struct CompletionsArgs {
    #[arg(value_name = "SHELL")]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Args)]
pub struct McpBridgeArgs {
    #[arg(long, value_name = "PATH")]
    pub socket: Utf8PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `trailing_var_arg` swallowed every flag written after the task, so the README's own
    /// first-run line silently ran the brain at the default tier.
    #[test]
    fn flags_after_the_task_are_still_flags() {
        let cli = Cli::try_parse_from([
            "swamp",
            "run",
            "do a thing",
            "--no-brain",
            "--tier",
            "mid",
            "--wait",
        ])
        .expect("parses");
        let Some(Command::Run(a)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(a.task, vec!["do a thing".to_owned()]);
        assert!(a.no_brain);
        assert!(a.wait);
        assert_eq!(a.tier, Some(Tier::Mid));
    }

    /// A task made of several words still joins, and `--` still forces literal text.
    #[test]
    fn a_multi_word_task_and_an_escaped_one_both_survive() {
        let cli = Cli::try_parse_from(["swamp", "run", "fix", "the", "parser"]).expect("parses");
        let Some(Command::Run(a)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(a.task, vec!["fix", "the", "parser"]);

        let cli = Cli::try_parse_from(["swamp", "run", "--", "--no-brain"]).expect("parses");
        let Some(Command::Run(a)) = cli.command else {
            panic!("expected run");
        };
        assert_eq!(a.task, vec!["--no-brain".to_owned()]);
        assert!(!a.no_brain);
    }
}
