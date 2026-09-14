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
use crate::journal::writer::Writer;
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{Mutex, mpsc};

/// Cheap to clone: every producer holds one, the writer task owns the fd.
#[derive(Clone)]
pub struct JournalHandle {
    pub run: RunId,
    pub tx: tokio::sync::mpsc::UnboundedSender<(Option<NodeId>, JournalEvent)>,
    pub paths: std::sync::Arc<RunPaths>,
    /// Shared with the writer task so a durable emit can bypass the queue.
    pub writer: Arc<Mutex<Writer>>,
}

impl JournalHandle {
    /// Never blocks and never fails: a dead writer task only logs.
    pub fn emit(&self, node: Option<NodeId>, event: JournalEvent) {
        if self.tx.send((node, event)).is_err() {
            tracing::warn!(run = %self.run, "journal writer is gone; event dropped");
        }
    }

    /// Returns only after the line is on disk.
    pub async fn emit_durable(
        &self,
        node: Option<NodeId>,
        event: JournalEvent,
    ) -> anyhow::Result<u64> {
        let mut w = self.writer.lock().await;
        let line = line(w.seq, self.run, node, event);
        let seq = w.append(&line).await?;
        w.sync().await?;
        Ok(seq)
    }

    pub fn run(&self) -> RunId {
        self.run
    }

    pub fn paths(&self) -> &RunPaths {
        &self.paths
    }
}

fn line(seq: u64, run: RunId, node: Option<NodeId>, event: JournalEvent) -> JournalLine {
    JournalLine {
        seq,
        at: OffsetDateTime::now_utc(),
        run,
        node,
        event,
    }
}

pub struct Journal;

impl Journal {
    pub async fn open(
        paths: RunPaths,
        policy: FsyncPolicy,
        redact: &[String],
    ) -> anyhow::Result<(JournalHandle, tokio::task::JoinHandle<()>)> {
        tokio::fs::create_dir_all(&paths.dir).await?;
        private_dir(&paths.dir).await?;

        let redactor = Arc::new(Redactor::new(redact)?);
        let mut w = Writer::open(&paths.journal(), policy).await?;
        w.set_redactor(redactor);
        let writer = Arc::new(Mutex::new(w));

        let (tx, mut rx) = mpsc::unbounded_channel::<(Option<NodeId>, JournalEvent)>();
        let run = paths.run;
        let task = {
            let writer = writer.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(writer::BATCH_INTERVAL);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        msg = rx.recv() => match msg {
                            Some((node, event)) => {
                                let mut w = writer.lock().await;
                                let l = line(w.seq, run, node, event);
                                if let Err(e) = w.append(&l).await {
                                    tracing::error!(run = %run, "journal append failed: {e}");
                                }
                            }
                            None => break,
                        },
                        _ = ticker.tick() => {
                            let mut w = writer.lock().await;
                            if let Err(e) = w.tick().await {
                                tracing::error!(run = %run, "journal sync failed: {e}");
                            }
                        }
                    }
                }
                let mut w = writer.lock().await;
                if let Err(e) = w.sync().await {
                    tracing::error!(run = %run, "final journal sync failed: {e}");
                }
            })
        };

        Ok((
            JournalHandle {
                run,
                tx,
                paths: Arc::new(paths),
                writer,
            },
            task,
        ))
    }
}

/// The run dir holds the control socket, so it is 0700 before anything lands in it.
async fn private_dir(dir: &camino::Utf8Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}
