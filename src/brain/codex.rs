use crate::brain::{Brain, BrainEvent, EVENT_QUEUE, Launch, drain_stderr, drive, finish};
use crate::journal::JournalEvent;
use crate::journal::record::TurnRole;
use crate::model::core::{NodeState, SessionHandle};
use crate::worker::adapter::SessionPlan;
use async_trait::async_trait;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Resume-per-turn session: `exec` has no streaming stdin, so every user turn is a fresh
/// `exec resume <thread>` and continuity comes from the CLI's own thread store.
pub struct CodexBrain {
    pub rx: mpsc::Receiver<BrainEvent>,
    tx: mpsc::Sender<BrainEvent>,
    launch: Arc<Launch>,
    turn: Option<JoinHandle<()>>,
    offset: Arc<AtomicU64>,
    session: Option<SessionHandle>,
}

impl CodexBrain {
    pub(crate) fn new(launch: Launch) -> Self {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        CodexBrain {
            rx,
            tx,
            launch: Arc::new(launch),
            turn: None,
            offset: Arc::new(AtomicU64::new(0)),
            session: None,
        }
    }

    /// The thread id is minted by the CLI mid turn, so it is picked up from the shared slot.
    fn refresh(&mut self) {
        if let Some(handle) = self.launch.latest_session() {
            self.session = Some(handle);
        }
    }
}

#[async_trait]
impl Brain for CodexBrain {
    async fn start(&mut self) -> anyhow::Result<()> {
        self.launch.journal_spawn().await?;
        if let Some(handle) = self.launch.planned_session() {
            self.launch.journal_session(handle.clone()).await?;
            self.session = Some(handle);
        }
        Ok(())
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        self.refresh();
        let mut spec = self.launch.spec.clone();
        if let Some(handle) = self.session.clone() {
            spec.session = SessionPlan::Resume(handle);
        }
        let argv = self.launch.adapter.build_argv(&spec)?;
        let mut child = self.launch.spawn(&argv, Stdio::piped())?;

        // `-` on the argv: the prompt is stdin, and EOF is what starts the turn.
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("the brain was spawned without a stdin pipe"))?;
        stdin.write_all(text.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;
        self.launch.journal.emit(
            Some(self.launch.node()),
            JournalEvent::BrainTurn {
                role: TurnRole::User,
                text: text.to_owned(),
            },
        );

        if let Some(err) = child.stderr.take() {
            drain_stderr(&self.launch, err);
        }
        let out = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("the brain was spawned without a stdout pipe"))?;
        let launch = Arc::clone(&self.launch);
        let tx = self.tx.clone();
        let offset = Arc::clone(&self.offset);
        self.turn = Some(tokio::spawn(async move {
            let driven = drive(&launch, &tx, out, offset.load(Ordering::Relaxed)).await;
            offset.store(driven.offset, Ordering::Relaxed);
            let status = child.wait().await;
            // A turn that dies without a terminal event would otherwise hang the REPL.
            if !driven.finished {
                let detail = match status {
                    Ok(s) => format!("the brain exited with {s} before finishing the turn"),
                    Err(e) => format!("the brain could not be reaped: {e}"),
                };
                let _ = tx.send(BrainEvent::Fatal { message: detail }).await;
            }
        }));
        Ok(())
    }

    fn events(&mut self) -> &mut mpsc::Receiver<BrainEvent> {
        &mut self.rx
    }

    /// One process per turn, so cancelling the turn is cancelling the process.
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        if let Some(turn) = self.turn.take() {
            turn.abort();
        }
        self.refresh();
        Ok(())
    }

    async fn shutdown(mut self: Box<Self>) -> anyhow::Result<()> {
        if let Some(mut turn) = self.turn.take()
            && tokio::time::timeout(self.launch.turn_timeout, &mut turn)
                .await
                .is_err()
        {
            tracing::warn!("the brain's last turn outlived its timeout");
            turn.abort();
        }
        self.refresh();
        finish(&self.launch, NodeState::Succeeded).await;
        Ok(())
    }

    fn session(&self) -> Option<&SessionHandle> {
        self.session.as_ref()
    }
}
