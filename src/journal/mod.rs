#![allow(dead_code, unused_variables)]

pub mod fold;
pub mod paths;
pub mod raw;
pub mod reader;
pub mod record;
pub mod writer;

pub use fold::{LlmDigest, Projection, RunView, TreeRow};
pub use paths::{Paths, RunPaths};
pub use raw::{RawSink, Redactor};
pub use reader::{Tailer, replay};
pub use record::{JournalEvent, JournalLine, NoteAuthor, TurnRole};
pub use writer::FsyncPolicy;

use crate::ids::{NodeId, RunId};

/// Cheap to clone: every producer holds one, the writer task owns the fd.
#[derive(Clone)]
pub struct JournalHandle {
    pub run: RunId,
    pub tx: tokio::sync::mpsc::UnboundedSender<(Option<NodeId>, JournalEvent)>,
    pub paths: std::sync::Arc<RunPaths>,
}

impl JournalHandle {
    /// Never blocks and never fails: a dead writer task only logs.
    pub fn emit(&self, node: Option<NodeId>, event: JournalEvent) {
        todo!("WP2")
    }
    /// Returns only after the line is on disk.
    pub async fn emit_durable(
        &self,
        node: Option<NodeId>,
        event: JournalEvent,
    ) -> anyhow::Result<u64> {
        todo!("WP2")
    }
    pub fn run(&self) -> RunId {
        todo!("WP2")
    }
    pub fn paths(&self) -> &RunPaths {
        todo!("WP2")
    }
}

pub struct Journal;

impl Journal {
    pub async fn open(
        paths: RunPaths,
        policy: FsyncPolicy,
        redact: &[String],
    ) -> anyhow::Result<(JournalHandle, tokio::task::JoinHandle<()>)> {
        todo!("WP2")
    }
}
