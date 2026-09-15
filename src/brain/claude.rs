use crate::brain::{Brain, BrainEvent, EVENT_QUEUE, Launch, drain_stderr, drive, finish};
use crate::journal::JournalEvent;
use crate::journal::record::TurnRole;
use crate::model::core::{NodeState, SessionHandle};
use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Persistent stream-json session over stdin/stdout: one process for the whole chat, so the
/// prompt cache stays warm and a turn is one JSON line written to fd0.
pub struct ClaudeBrain {
    pub rx: mpsc::Receiver<BrainEvent>,
    tx: mpsc::Sender<BrainEvent>,
    launch: Arc<Launch>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    reader: Option<JoinHandle<crate::brain::Driven>>,
    session: Option<SessionHandle>,
}

impl ClaudeBrain {
    pub(crate) fn new(launch: Launch) -> Self {
        let (tx, rx) = mpsc::channel(EVENT_QUEUE);
        ClaudeBrain {
            rx,
            tx,
            launch: Arc::new(launch),
            child: None,
            stdin: None,
            reader: None,
            session: None,
        }
    }
}

#[async_trait]
impl Brain for ClaudeBrain {
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let argv = self.launch.adapter.build_argv(&self.launch.spec)?;
        self.launch.journal_spawn().await?;
        // The session id exists before the process does, so a crash is resumable.
        if let Some(handle) = self.launch.planned_session() {
            self.launch.journal_session(handle.clone()).await?;
            self.session = Some(handle);
        }

        let mut child = self.launch.spawn(&argv, Stdio::piped())?;
        self.stdin = child.stdin.take();
        if let Some(err) = child.stderr.take() {
            drain_stderr(&self.launch, err);
        }
        let out = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("the brain was spawned without a stdout pipe"))?;
        let launch = Arc::clone(&self.launch);
        let tx = self.tx.clone();
        self.reader = Some(tokio::spawn(
            async move { drive(&launch, &tx, out, 0).await },
        ));
        self.child = Some(child);
        Ok(())
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        let session = self.session.as_ref().map(|s| s.id.clone());
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("the brain is not running: call start() first"))?;
        let mut line = json!({
            "type": "user",
            "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
            "parent_tool_use_id": null,
        });
        if let Some(id) = session {
            line["session_id"] = json!(id);
        }
        stdin.write_all(line.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        self.launch.journal.emit(
            Some(self.launch.node()),
            JournalEvent::BrainTurn {
                role: TurnRole::User,
                text: text.to_owned(),
            },
        );
        Ok(())
    }

    fn events(&mut self) -> &mut mpsc::Receiver<BrainEvent> {
        &mut self.rx
    }

    /// The streaming input format carries control requests; an older CLI ignores the line.
    async fn interrupt(&mut self) -> anyhow::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        let line = json!({
            "type": "control_request",
            "request_id": uuid::Uuid::new_v4().to_string(),
            "request": { "subtype": "interrupt" },
        });
        stdin.write_all(line.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn shutdown(mut self: Box<Self>) -> anyhow::Result<()> {
        // Closing fd0 is how a stream-json session ends; killing is the fallback.
        self.stdin = None;
        if let Some(reader) = self.reader.take() {
            let _ = tokio::time::timeout(self.launch.turn_timeout, reader).await;
        }
        if let Some(child) = self.child.take() {
            crate::brain::terminate(child).await?;
        }
        finish(&self.launch, NodeState::Succeeded).await;
        Ok(())
    }

    fn session(&self) -> Option<&SessionHandle> {
        self.session.as_ref()
    }
}
