#![allow(dead_code, unused_variables)]

use crate::config::Config;
use crate::dispatch::pool::AccountPool;
use crate::ids::{NodeId, NodeIds};
use crate::journal::JournalHandle;
use crate::model::core::Provider;
use crate::model::failure::Failure;
use crate::model::node::NodeRecord;
use crate::model::result::TaskRequest;
use crate::worker::RunOutcome;
use crate::worker::adapter::LaunchSpec;
use crate::workspace::WorkspaceManager;
use std::sync::Arc;
use tokio::time::Instant;

/// Everything the attempt loop needs, so the loop itself stays a pure policy statement.
pub struct NodeCtx {
    pub cfg: Arc<Config>,
    pub pool: Arc<AccountPool>,
    pub workspace: Arc<WorkspaceManager>,
    pub journal: JournalHandle,
    pub provider_order: Vec<Provider>,
    pub cross_provider: bool,
    pub max_attempts: u32,
    pub deadline: Instant,
    pub parent: Option<NodeId>,
}

impl NodeCtx {
    pub fn new_node_ids(&self) -> NodeIds {
        todo!("WP4")
    }
    pub fn new_session_id(&self, p: Provider) -> Option<String> {
        todo!("WP4")
    }
}

/// The record of every attempt made for one logical node, plus the outcome that stands.
pub struct NodeOutcome {
    pub logical: NodeId,
    pub attempts: Vec<NodeRecord>,
    pub outcome: Option<RunOutcome>,
    pub failure: Option<Failure>,
}

impl NodeOutcome {
    pub fn ok(out: RunOutcome) -> Self {
        todo!("WP4")
    }
    pub fn failed(f: Failure) -> Self {
        todo!("WP4")
    }
}

pub async fn run_node(cx: &NodeCtx, spec: LaunchSpec, task: &TaskRequest) -> NodeOutcome {
    todo!("WP4")
}
