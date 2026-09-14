pub mod accounts;
pub mod adopt;
pub mod cancel;
pub mod chat;
pub mod config;
pub mod diff;
pub mod doctor;
pub mod gc;
pub mod mcp_bridge;
pub mod replay;
pub mod resume;
pub mod run;
pub mod runs;
pub mod trace;
pub mod watch;
pub mod worktrees;

use crate::config::Config;
use crate::journal::paths::Paths;
use std::sync::Arc;

/// Everything every subcommand needs, built once in main.
pub struct Ctx {
    pub cfg: Arc<Config>,
    pub paths: Arc<Paths>,
    pub color: bool,
    pub json: bool,
}
