//! The one mutation the read-mostly surfaces make: a confirmed cancel, run off the UI loop.

use crate::dispatch::cancel::{Outcome, Sink, Stop, cancel_node};
use crate::ids::{DispatchId, NodeId, RunId};
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::model::core::CancelSource;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTarget {
    Task { run: RunId, logical: NodeId },
    Dispatch { run: RunId, id: DispatchId },
}

impl CancelTarget {
    pub fn run(&self) -> RunId {
        match self {
            CancelTarget::Task { run, .. } | CancelTarget::Dispatch { run, .. } => *run,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelDone {
    pub target: CancelTarget,
    /// How many tasks were stopped.
    pub result: Result<usize, String>,
}

/// Cancels `tasks` one after the other on a task of its own, so a grace period never freezes a
/// frame, and reports on `tx`.
pub fn spawn_cancel(
    paths: RunPaths,
    target: CancelTarget,
    tasks: Vec<NodeId>,
    grace: Duration,
    tx: UnboundedSender<CancelDone>,
) {
    tokio::spawn(async move {
        let result = cancel_all(&paths, &tasks, grace)
            .await
            .map_err(|e| format!("{e:#}"));
        let _ = tx.send(CancelDone { target, result });
    });
}

async fn cancel_all(paths: &RunPaths, tasks: &[NodeId], grace: Duration) -> anyhow::Result<usize> {
    let mut stopped = 0;
    for logical in tasks {
        // Re-read per task: an earlier task's grace period gives this one time to settle.
        let view = RunView::load(&paths.dir, false)?;
        let outcome = cancel_node(
            paths,
            &view,
            Sink::Shared,
            *logical,
            CancelSource::User,
            Stop::Kill { grace },
        )
        .await?;
        if matches!(outcome, Outcome::Cancelled { .. } | Outcome::Killed { .. }) {
            stopped += 1;
        }
    }
    Ok(stopped)
}
