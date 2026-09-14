#![allow(dead_code, unused_variables)]

use crate::config::Config;
use crate::journal::paths::RunPaths;
use std::sync::Arc;

/// Live TUI: tree pane, node pane, account footer. Read-only.
pub async fn run_tui(paths: RunPaths, cfg: Arc<Config>) -> anyhow::Result<()> {
    todo!("WP7")
}
