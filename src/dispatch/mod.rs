#![allow(dead_code, unused_variables)]

pub mod account;
pub mod cooldown;
pub mod persist;
pub mod policy;
pub mod pool;
pub mod retry;

pub use account::{Account, AccountState, Health};
pub use policy::SelectionPolicy;
pub use pool::{AccountPool, Lease, NoCapacity};
pub use retry::{NodeCtx, NodeOutcome, run_node};

use crate::config::Config;
use crate::ids::NodeId;
use crate::journal::JournalHandle;
use crate::model::result::{NodeResult, TaskRequest};
use crate::worker::Executor;
use crate::workspace::WorkspaceManager;
use std::sync::Arc;
use std::time::Duration;

/// Owns the pool and the semaphores. One per run.
pub struct Dispatcher {
    pub cfg: Arc<Config>,
    pub pool: Arc<AccountPool>,
    pub exec: Arc<Executor>,
    pub workspace: Arc<WorkspaceManager>,
    pub journal: JournalHandle,
}

impl Dispatcher {
    pub fn new(
        cfg: Arc<Config>,
        pool: Arc<AccountPool>,
        exec: Arc<Executor>,
        ws: Arc<WorkspaceManager>,
        journal: JournalHandle,
    ) -> Arc<Self> {
        todo!("WP4")
    }
    pub async fn dispatch_batch(
        self: &Arc<Self>,
        parent: NodeId,
        tasks: Vec<TaskRequest>,
        max_wait: Duration,
    ) -> Vec<NodeResult> {
        todo!("WP4")
    }
    pub async fn dispatch_one(self: &Arc<Self>, parent: NodeId, task: TaskRequest) -> NodeResult {
        todo!("WP4")
    }
    pub async fn await_nodes(&self, ids: &[NodeId], timeout: Option<Duration>) -> Vec<NodeResult> {
        todo!("WP4")
    }
    pub async fn cancel(&self, id: NodeId) -> anyhow::Result<()> {
        todo!("WP4")
    }
    pub fn result(&self, id: NodeId) -> Option<NodeResult> {
        todo!("WP4")
    }
    pub fn pool(&self) -> &Arc<AccountPool> {
        todo!("WP4")
    }
}
