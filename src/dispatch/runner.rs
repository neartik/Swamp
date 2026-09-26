use crate::dispatch::NodeRunner;
use crate::ids::NodeId;
use crate::model::core::Tier;
use crate::model::node::WorkResultRef;
use crate::worker::adapter::LaunchSpec;
use crate::worker::{Executor, RunOutcome};
use crate::workspace::{NodeWorktree, WorkspaceManager};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Real worktrees and detached processes, shared by the dispatcher and run --no-brain.
pub struct DirectRunner {
    exec: Arc<Executor>,
    workspace: Arc<WorkspaceManager>,
}

impl DirectRunner {
    pub fn new(exec: Arc<Executor>, workspace: Arc<WorkspaceManager>) -> Self {
        DirectRunner { exec, workspace }
    }
}

#[async_trait]
impl NodeRunner for DirectRunner {
    async fn workspace(&self, logical: NodeId, attempt: u32) -> anyhow::Result<NodeWorktree> {
        self.workspace.create(logical, attempt).await
    }

    async fn run(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        self.exec.run(spec, timeout, cancel).await
    }

    async fn finalize(
        &self,
        wt: &NodeWorktree,
        title: &str,
        tier: Tier,
    ) -> anyhow::Result<Option<WorkResultRef>> {
        self.workspace.finalize(wt, title, tier).await
    }
}
