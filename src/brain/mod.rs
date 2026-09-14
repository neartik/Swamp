#![allow(dead_code, unused_variables)]

pub mod claude;
pub mod codex;
pub mod prompt;

pub use prompt::system_prompt;

use crate::config::Config;
use crate::dispatch::Lease;
use crate::journal::JournalHandle;
use crate::journal::paths::RunPaths;
use crate::model::core::{Cost, SessionHandle, Usage};
use async_trait::async_trait;
use camino::Utf8Path;
use tokio::sync::mpsc;

pub enum BrainEvent {
    Ready { session: String, model: String },
    Text { delta: String },
    Thinking { delta: String },
    ToolCall { name: String, preview: String },
    ToolDone { name: String, ok: bool },
    TurnDone { usage: Usage, cost: Option<Cost> },
    Fatal { message: String },
}

#[async_trait]
pub trait Brain: Send {
    async fn start(&mut self) -> anyhow::Result<()>;
    async fn send(&mut self, text: &str) -> anyhow::Result<()>;
    fn events(&mut self) -> &mut mpsc::Receiver<BrainEvent>;
    async fn interrupt(&mut self) -> anyhow::Result<()>;
    async fn shutdown(self: Box<Self>) -> anyhow::Result<()>;
    fn session(&self) -> Option<&SessionHandle>;
}

pub fn build(
    cfg: &Config,
    lease: Lease,
    paths: &RunPaths,
    socket: &Utf8Path,
    journal: JournalHandle,
    resume: Option<SessionHandle>,
) -> anyhow::Result<Box<dyn Brain>> {
    todo!("WP6")
}
