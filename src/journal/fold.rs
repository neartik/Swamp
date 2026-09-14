#![allow(dead_code, unused_variables)]

use crate::dispatch::account::AccountState;
use crate::ids::{NodeId, RunId};
use crate::journal::record::JournalLine;
use crate::model::core::{AccountId, NodeState, Usage};
use crate::model::event::WorkerEvent;
use crate::model::node::NodeRecord;
use camino::{Utf8Path, Utf8PathBuf};
use std::collections::BTreeMap;
use time::OffsetDateTime;

pub trait Projection {
    type Out;
    fn apply(&mut self, l: &JournalLine);
    fn finish(self) -> Self::Out;
}

#[derive(Debug, Clone)]
pub struct RunHeader {
    pub run: RunId,
    pub swamp_version: String,
    pub schema: u32,
    pub argv: Vec<String>,
    pub cwd: Utf8PathBuf,
    pub repo: Option<Utf8PathBuf>,
    pub base: Option<String>,
    pub config_sha256: String,
    pub task: Option<String>,
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Default)]
pub struct RunView {
    pub header: Option<RunHeader>,
    pub nodes: BTreeMap<NodeId, NodeRecord>,
    pub children: BTreeMap<NodeId, Vec<NodeId>>,
    pub roots: Vec<NodeId>,
    /// Attempt chains, collapsed in the tree view.
    pub by_logical: BTreeMap<NodeId, Vec<NodeId>>,
    pub accounts: BTreeMap<AccountId, AccountState>,
    /// Only when `with_events`.
    pub events: BTreeMap<NodeId, Vec<WorkerEvent>>,
    pub totals: Usage,
    pub cost_usd: f64,
    /// False if any node's cost is unknown.
    pub cost_complete: bool,
    pub last_seq: u64,
    pub finished: bool,
}

/// One collapsed row: a logical node plus every attempt that served it.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub logical: NodeId,
    pub depth: u32,
    pub title: String,
    pub state: NodeState,
    pub attempts: Vec<NodeId>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Totals {
    pub nodes: u32,
    pub failed: u32,
    pub usage: Usage,
    pub cost_usd: f64,
    pub cost_complete: bool,
}

impl RunView {
    /// Pure and idempotent: replaying the same prefix always yields the same state.
    pub fn apply(&mut self, l: &JournalLine) {
        todo!("WP2")
    }
    pub fn load(dir: &Utf8Path, with_events: bool) -> anyhow::Result<Self> {
        todo!("WP2")
    }
    /// Running nodes with no NodeFinished and a dead pid become Orphaned.
    pub fn mark_orphans(&mut self, alive: &dyn Fn(NodeId) -> bool) {
        todo!("WP2")
    }
    pub fn tree(&self) -> Vec<TreeRow> {
        todo!("WP2")
    }
    pub fn totals(&self) -> Totals {
        todo!("WP2")
    }
}

/// Byte-budgeted compact rendering for the brain's swamp_status tool.
/// Same fold, different output.
pub struct LlmDigest {
    pub max_bytes: usize,
}

impl Projection for LlmDigest {
    type Out = String;
    fn apply(&mut self, l: &JournalLine) {
        todo!("WP2")
    }
    fn finish(self) -> Self::Out {
        todo!("WP2")
    }
}
