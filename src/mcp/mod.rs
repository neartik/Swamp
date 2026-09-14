#![allow(dead_code, unused_variables)]

pub mod bridge;
pub mod jsonrpc;
pub mod server;
pub mod tools;

pub use bridge::run_bridge;
pub use jsonrpc::{Request, Response, RpcError};
pub use tools::{ToolSchema, wrap_untrusted};

use crate::dispatch::Dispatcher;
use crate::journal::JournalHandle;
use crate::journal::paths::RunPaths;
use camino::{Utf8Path, Utf8PathBuf};
use std::sync::Arc;

pub struct McpServer {
    pub socket: Utf8PathBuf,
    pub disp: Arc<Dispatcher>,
}

impl McpServer {
    /// Binds a UDS at paths.socket() with 0600 in a 0700 dir; removes it on drop.
    pub async fn bind(
        paths: &RunPaths,
        disp: Arc<Dispatcher>,
        view: Arc<JournalHandle>,
    ) -> anyhow::Result<(Self, Utf8PathBuf)> {
        todo!("WP6")
    }
    pub fn serve(self) -> tokio::task::JoinHandle<()> {
        todo!("WP6")
    }
    /// Uses std::env::current_exe(), never the bare name `swamp`.
    pub fn mcp_config_json(socket: &Utf8Path) -> String {
        todo!("WP6")
    }
    pub fn codex_config_args(socket: &Utf8Path) -> Vec<String> {
        todo!("WP6")
    }
}
