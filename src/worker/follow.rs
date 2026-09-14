#![allow(dead_code, unused_variables)]

use crate::ids::NodeId;
use crate::journal::raw::RawSink;
use crate::model::event::WorkerEvent;
use crate::worker::adapter::{ParseState, ProviderAdapter};
use camino::Utf8Path;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Tails stream.jsonl from a byte offset, drives the parser, emits normalized events.
/// Returns the offset consumed, so a restart resumes exactly here.
#[allow(clippy::too_many_arguments)]
pub async fn follow(
    node: NodeId,
    path: &Utf8Path,
    offset: u64,
    adapter: Arc<dyn ProviderAdapter>,
    st: &mut ParseState,
    sink: &mut RawSink,
    out: mpsc::Sender<(NodeId, WorkerEvent, u64)>,
    alive: Arc<dyn Fn() -> bool + Send + Sync>,
) -> anyhow::Result<u64> {
    todo!("WP3")
}
