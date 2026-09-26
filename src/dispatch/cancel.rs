use crate::ids::{DispatchId, NodeId};
use crate::journal::paths::RunPaths;
use crate::journal::{JournalEvent, JournalHandle, RunView};
use crate::model::core::{CancelSource, NodeKind, NodeState};
use crate::model::dispatch::Phase;
use crate::worker::liveness;
use crate::worker::spawn::{Reaper, terminate};
use camino::Utf8Path;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How often a running task looks for a cancel another process asked for.
const WATCH_INTERVAL: Duration = Duration::from_millis(250);

/// Where the task's `Cancelled` transition is journaled.
pub enum Sink<'a> {
    /// The run's own writer, in the process that owns the run.
    Live(&'a JournalHandle),
    /// An append from another process; the owner's writer continues past it.
    Shared,
}

/// How the live attempt is stopped.
pub enum Stop<'a> {
    /// The owning process: its executor kills the process group when the token fires.
    Token(&'a CancellationToken),
    /// Any other process: the process group named by the attempt's pidfile.
    Kill { grace: Duration },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Journaled as cancelled; `pgid` is the process group of the attempt that was live.
    Cancelled { from: Phase, pgid: Option<i32> },
    /// Already terminal: nothing is journaled and nothing is killed.
    Ended(Phase),
    /// Already terminal, but an attempt was still running and its process group was killed.
    Killed { phase: Phase, pgid: i32 },
}

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    by: CancelSource,
}

/// Marks, journals `Cancelled` durably, then stops the attempt, the same way from every surface.
pub async fn cancel_node(
    paths: &RunPaths,
    view: &RunView,
    sink: Sink<'_>,
    logical: NodeId,
    by: CancelSource,
    stop: Stop<'_>,
) -> anyhow::Result<Outcome> {
    anyhow::ensure!(
        !is_brain(view, logical),
        "node {} is the brain of run {}: cancel the run instead",
        logical.short(),
        paths.run
    );
    let state = view
        .state_of(logical)
        .ok_or_else(|| anyhow::anyhow!("no task {logical} in run {}", paths.run))?;
    let from = Phase::from(&state);
    if state.is_terminal() {
        // A worker can outlive its task's terminal line when its supervisor died first.
        if let Stop::Kill { grace } = stop
            && let Some(pgid) = live_pgid(paths, view, logical)
        {
            terminate(pgid, grace, Reaper::Here).await?;
            return Ok(Outcome::Killed { phase: from, pgid });
        }
        return Ok(Outcome::Ended(from));
    }
    mark(paths, logical, by)?;
    let event = JournalEvent::NodeStateChanged {
        from,
        to: NodeState::Cancelled { by },
        why: format!("cancelled by {}", source_word(by)),
    };
    let appended = match sink {
        Sink::Live(j) => j.emit_durable(Some(logical), event).await,
        Sink::Shared => {
            crate::journal::writer::append_shared(&paths.journal(), paths.run, Some(logical), event)
                .await
        }
    };
    if let Err(e) = appended {
        // Left behind, the marker would suppress the owner's own Cancelled line.
        let _ = std::fs::remove_file(paths.cancel_marker(logical));
        return Err(e);
    }
    let live = live_pgid(paths, view, logical);
    match stop {
        Stop::Token(t) => t.cancel(),
        Stop::Kill { grace } => {
            if let Some(pgid) = live {
                terminate(pgid, grace, Reaper::Here).await?;
            }
        }
    }
    Ok(Outcome::Cancelled { from, pgid: live })
}

/// Who cancelled `logical`, once anyone has.
pub fn requested(paths: &RunPaths, logical: NodeId) -> Option<CancelSource> {
    read_marker(&paths.cancel_marker(logical))
}

/// Fires `token` as soon as a cancel for `logical` is marked, by this process or another.
pub async fn watch(paths: RunPaths, logical: NodeId, token: CancellationToken) {
    loop {
        tokio::select! {
            _ = token.cancelled() => return,
            _ = tokio::time::sleep(WATCH_INTERVAL) => {}
        }
        if requested(&paths, logical).is_some() {
            token.cancel();
            return;
        }
    }
}

pub fn source_word(by: CancelSource) -> &'static str {
    match by {
        CancelSource::User => "user",
        CancelSource::Brain => "brain",
        CancelSource::Timeout => "timeout",
        CancelSource::Shutdown => "shutdown",
    }
}

/// The direct tasks a dispatch cancel covers, the same list from every surface.
pub fn dispatch_tasks(view: &RunView, id: DispatchId) -> Vec<NodeId> {
    view.dispatches
        .get(&id)
        .map(|d| d.tasks.clone())
        .unwrap_or_default()
}

pub fn is_brain(view: &RunView, id: NodeId) -> bool {
    view.attempts(id).iter().any(|n| n.kind == NodeKind::Brain)
}

/// The latest attempt still running under a pidfile this machine can vouch for.
fn live_pgid(paths: &RunPaths, view: &RunView, logical: NodeId) -> Option<i32> {
    view.attempts(logical)
        .into_iter()
        .rev()
        .find_map(|n| match n.state {
            NodeState::Running { pgid, .. } if liveness::is_ours(&paths.pidfile(n.id)) => {
                Some(pgid)
            }
            _ => None,
        })
}

fn mark(paths: &RunPaths, logical: NodeId, by: CancelSource) -> anyhow::Result<()> {
    let path = paths.cancel_marker(logical);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_vec(&Marker { by })?)?;
    Ok(())
}

fn read_marker(path: &Utf8Path) -> Option<CancelSource> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice::<Marker>(&bytes).ok().map(|m| m.by)
}
