//! The one mutation the read-mostly surfaces make: a confirmed cancel, run off the UI loop.

use crate::dispatch::cancel::{Outcome, Sink, Stop, cancel_node, dispatch_tasks};
use crate::ids::{DispatchId, NodeId, RunId};
use crate::journal::fold::RunView;
use crate::journal::paths::RunPaths;
use crate::model::core::{CancelSource, NodeState};
use crate::ui::chat::theme::Role;
use crate::ui::fmt;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// How long a notice stays in place of the hints.
pub const NOTICE_TTL: Duration = Duration::from_secs(5);

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

    /// What a cancel of this target reaches.
    pub fn tasks(&self, view: &RunView) -> Vec<NodeId> {
        match *self {
            CancelTarget::Task { logical, .. } => vec![logical],
            CancelTarget::Dispatch { id, .. } => dispatch_tasks(view, id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelDone {
    pub target: CancelTarget,
    /// How many tasks were stopped.
    pub result: Result<usize, String>,
}

/// A line shown in place of the hints; `until: None` stays until it is replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub role: Role,
    pub until: Option<Instant>,
}

impl Notice {
    pub fn new(text: String, role: Role, ttl: Option<Duration>) -> Notice {
        Notice {
            text,
            role,
            until: ttl.map(|t| Instant::now() + t),
        }
    }

    pub fn live(&self, now: Instant) -> bool {
        self.until.is_none_or(|u| now < u)
    }

    /// Held while the spawned cancel runs.
    pub fn cancelling(label: &str) -> Notice {
        Notice::new(format!("cancelling {label}\u{2026}"), Role::Meta, None)
    }

    /// What the spawned cancel reported.
    pub fn done(target: &CancelTarget, label: &str, result: &Result<usize, String>) -> Notice {
        let (text, role) = match (result, target) {
            (Ok(0), CancelTarget::Task { .. }) => {
                (format!("{label} had already ended"), Role::Meta)
            }
            (Ok(0), CancelTarget::Dispatch { .. }) => {
                (format!("{label}: nothing left to cancel"), Role::Meta)
            }
            (Ok(_), _) => (format!("cancelled {label}"), Role::Meta),
            (Err(e), _) => (format!("cancel {label} failed: {e}"), Role::Err),
        };
        Notice::new(text, role, Some(NOTICE_TTL))
    }
}

pub fn brain_text(run: RunId) -> String {
    format!("the brain stops with swamp cancel {}", run.short())
}

pub fn already_text(label: &str, state: &NodeState) -> String {
    format!("{label} is already {}", fmt::state_word(state))
}

/// `cancel 9g5f04 "rebuild the index"? y / n`, the title cut so the whole line fits.
pub fn prompt_text(label: &str, title: &str, width: usize) -> String {
    let frame = format!("cancel {label} \"\"? y / n");
    let room = width.saturating_sub(frame.chars().count()).max(1);
    format!("cancel {label} \"{}\"? y / n", fmt::truncate(title, room))
}

/// Cancels `tasks` in turn off the UI loop and reports on `tx`.
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
