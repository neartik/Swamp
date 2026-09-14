#![allow(dead_code, unused_variables)]

pub mod adapter;
pub mod classify;
pub mod claude;
pub mod codex;
pub mod follow;
pub mod liveness;
pub mod spawn;

pub use adapter::{
    BrainTransport, Capability, ExitContext, LaunchSpec, McpAttach, ParseOutput, ParseState,
    ProviderAdapter, SessionPlan, adapter_for,
};
pub use spawn::{Detached, NodeIo};

use crate::config::Config;
use crate::journal::JournalHandle;
use crate::model::core::{Cost, FileChange, Provider, RateLimitSnapshot, SessionHandle, Usage};
use crate::model::failure::Failure;
use crate::model::node::ExitInfo;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct RunOutcome {
    pub failure: Option<Failure>,
    pub exit: Option<ExitInfo>,
    pub session: Option<SessionHandle>,
    pub usage: Usage,
    pub cost: Option<Cost>,
    pub summary: Option<String>,
    pub files: Vec<FileChange>,
    pub rate_limit: Option<RateLimitSnapshot>,
    pub stream_offset: u64,
    pub unparsed_lines: u32,
    pub permission_denials: u32,
}

pub struct Executor {
    pub journal: JournalHandle,
    pub cfg: Arc<Config>,
}

impl Executor {
    pub fn new(journal: JournalHandle, cfg: Arc<Config>) -> Self {
        todo!("WP3")
    }
    pub fn adapter(&self, p: Provider) -> Arc<dyn ProviderAdapter> {
        todo!("WP3")
    }
    /// Writes prompt.md, spawns detached, journals ProcessStarted, follows to the terminal
    /// event, classifies, returns.
    pub async fn run(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        todo!("WP3")
    }
    pub async fn resume_from(
        &self,
        spec: &LaunchSpec,
        pid: i32,
        offset: u64,
    ) -> anyhow::Result<RunOutcome> {
        todo!("WP3")
    }
}
