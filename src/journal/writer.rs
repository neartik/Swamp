use crate::journal::raw::Redactor;
use crate::journal::record::{JournalEvent, JournalLine};
use camino::Utf8Path;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Records buffered before `Barrier` forces a sync.
const BATCH_RECORDS: u32 = 64;
/// Wall clock before `Barrier` forces a sync.
pub const BATCH_INTERVAL: Duration = Duration::from_millis(250);

/// `always` | `barrier` | `interval:<dur>` | `never`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FsyncPolicy {
    Always,
    #[default]
    Barrier,
    Interval(Duration),
    Never,
}

impl std::str::FromStr for FsyncPolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("interval:").or(s.strip_prefix("interval ")) {
            return Ok(Self::Interval(parse_duration(rest.trim())?));
        }
        match s.to_ascii_lowercase().as_str() {
            "always" => Ok(Self::Always),
            "barrier" => Ok(Self::Barrier),
            "never" => Ok(Self::Never),
            other => anyhow::bail!(
                "unknown fsync policy `{other}`, expected always, barrier, interval:<dur> or never"
            ),
        }
    }
}

fn parse_duration(s: &str) -> anyhow::Result<Duration> {
    #[derive(Deserialize)]
    struct D(#[serde(with = "humantime_serde")] Duration);
    let v = serde_json::Value::String(s.to_owned());
    let D(d) = serde_json::from_value(v)
        .map_err(|e| anyhow::anyhow!("invalid duration `{s}` in fsync policy: {e}"))?;
    Ok(d)
}

/// Owns the journal fd. One instance per run, driven by the single writer task.
pub struct Writer {
    pub path: camino::Utf8PathBuf,
    pub policy: FsyncPolicy,
    pub seq: u64,
    file: tokio::fs::File,
    redact: Option<Arc<Redactor>>,
    pending: u32,
    last_sync: Instant,
}

impl Writer {
    /// Repairs a torn tail by truncating back to the last newline and continues `seq` from there.
    pub async fn open(path: &Utf8Path, policy: FsyncPolicy) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)
            .await?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        let (valid, seq) = repair_point(&bytes);
        if valid != bytes.len() as u64 {
            tracing::warn!(
                "journal {path} had a torn tail: truncating {} bytes",
                bytes.len() as u64 - valid
            );
            file.set_len(valid).await?;
            file.sync_data().await?;
        }

        Ok(Writer {
            path: path.to_path_buf(),
            policy,
            seq,
            file,
            redact: None,
            pending: 0,
            last_sync: Instant::now(),
        })
    }

    pub fn set_redactor(&mut self, redact: Arc<Redactor>) {
        self.redact = if redact.is_empty() {
            None
        } else {
            Some(redact)
        };
    }

    pub async fn append(&mut self, line: &JournalLine) -> anyhow::Result<u64> {
        let mut text = self.encode(line)?;
        text.push('\n');
        self.file.write_all(text.as_bytes()).await?;
        self.seq = line.seq + 1;
        self.pending += 1;
        if self.should_sync(&line.event) {
            self.sync().await?;
        }
        Ok(line.seq)
    }

    pub async fn sync(&mut self) -> anyhow::Result<()> {
        self.file.flush().await?;
        self.file.sync_data().await?;
        self.pending = 0;
        self.last_sync = Instant::now();
        Ok(())
    }

    /// Time-based half of the batching policy, driven by the writer task's ticker.
    pub async fn tick(&mut self) -> anyhow::Result<()> {
        let due = match self.policy {
            FsyncPolicy::Barrier => self.last_sync.elapsed() >= BATCH_INTERVAL,
            FsyncPolicy::Interval(d) => self.last_sync.elapsed() >= d,
            FsyncPolicy::Always | FsyncPolicy::Never => false,
        };
        if due && self.pending > 0 {
            self.sync().await?;
        }
        Ok(())
    }

    fn should_sync(&self, event: &JournalEvent) -> bool {
        match self.policy {
            FsyncPolicy::Always => true,
            FsyncPolicy::Never => false,
            FsyncPolicy::Interval(d) => self.last_sync.elapsed() >= d,
            FsyncPolicy::Barrier => {
                is_barrier(event)
                    || self.pending >= BATCH_RECORDS
                    || self.last_sync.elapsed() >= BATCH_INTERVAL
            }
        }
    }

    fn encode(&self, line: &JournalLine) -> anyhow::Result<String> {
        let text = serde_json::to_string(line)?;
        let Some(redact) = &self.redact else {
            return Ok(text);
        };
        if !redact.set.is_match(&text) {
            return Ok(text);
        }
        let mut value: serde_json::Value = serde_json::from_str(&text)?;
        redact.redact_value(&mut value);
        Ok(serde_json::to_string(&value)?)
    }
}

/// Records that must be on disk before the side effect they announce.
fn is_barrier(event: &JournalEvent) -> bool {
    matches!(
        event,
        JournalEvent::RunStarted { .. }
            | JournalEvent::NodeSpawned { .. }
            | JournalEvent::ProcessStarted { .. }
            | JournalEvent::WorktreeCreated { .. }
            | JournalEvent::NodeFinished { .. }
            | JournalEvent::Adopted { .. }
            | JournalEvent::RunFinished { .. }
    )
}

/// Byte length of the last complete line plus the sequence number to continue from.
fn repair_point(bytes: &[u8]) -> (u64, u64) {
    let Some(last) = bytes.iter().rposition(|b| *b == b'\n') else {
        return (0, 0);
    };
    let valid = last as u64 + 1;
    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u64,
    }
    let mut max = None;
    for line in bytes[..=last].split(|b| *b == b'\n') {
        if let Ok(SeqOnly { seq }) = serde_json::from_slice::<SeqOnly>(line) {
            max = Some(max.map_or(seq, |m: u64| m.max(seq)));
        }
    }
    (valid, max.map_or(0, |m| m + 1))
}
