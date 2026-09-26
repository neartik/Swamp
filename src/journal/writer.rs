use crate::journal::raw::Redactor;
use crate::journal::record::{JournalEvent, JournalLine};
use camino::Utf8Path;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

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

/// A durable line on its way to the writer task, acked once it is synced.
pub(crate) type DurableTx = tokio::sync::mpsc::UnboundedSender<(
    Option<crate::ids::NodeId>,
    JournalEvent,
    tokio::sync::oneshot::Sender<anyhow::Result<u64>>,
)>;

/// Owns the journal fd. One instance per run, driven by the single writer task.
pub struct Writer {
    pub path: camino::Utf8PathBuf,
    pub policy: FsyncPolicy,
    pub seq: u64,
    /// Set when a writer task drains the queue, so durable lines queue behind it.
    pub(crate) durable: Option<DurableTx>,
    file: tokio::fs::File,
    lock: Arc<std::fs::File>,
    /// Bytes this writer knows are in the file; more means another process appended.
    len: u64,
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
        let lock = Arc::new(std::fs::File::open(path)?);

        let guard = Guard::acquire(&lock, path).await?;
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
        drop(guard);

        Ok(Writer {
            path: path.to_path_buf(),
            policy,
            seq,
            durable: None,
            file,
            lock,
            len: valid,
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
        let guard = Guard::acquire(&self.lock, &self.path).await?;
        let seq = self.catch_up().await?.max(line.seq);
        let mut text = if seq == line.seq {
            self.encode(line)?
        } else {
            self.encode(&JournalLine {
                seq,
                ..line.clone()
            })?
        };
        text.push('\n');
        self.file.write_all(text.as_bytes()).await?;
        self.file.flush().await?;
        drop(guard);
        self.len += text.len() as u64;
        self.seq = seq + 1;
        self.pending += 1;
        if self.should_sync(&line.event) {
            self.sync().await?;
        }
        Ok(seq)
    }

    /// Continues past any line another process appended since this writer last wrote.
    async fn catch_up(&mut self) -> anyhow::Result<u64> {
        // Waits for our own in-flight write, so the length below is not stale.
        self.file.flush().await?;
        let len = self.file.metadata().await?.len();
        if len > self.len {
            let mut f = tokio::fs::File::open(&self.path).await?;
            f.seek(std::io::SeekFrom::Start(self.len)).await?;
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes).await?;
            let (valid, next) = repair_point(&bytes);
            self.len += valid;
            self.seq = self.seq.max(next);
        }
        Ok(self.seq)
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

const SHARED_TORN_RETRIES: u32 = 40;
const SHARED_TORN_WAIT: Duration = Duration::from_millis(25);
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(1);

/// An exclusive flock on the journal, so every appender computes and writes its seq atomically.
struct Guard(Arc<std::fs::File>);

impl Guard {
    async fn acquire(file: &Arc<std::fs::File>, path: &Utf8Path) -> anyhow::Result<Self> {
        let deadline = Instant::now() + LOCK_TIMEOUT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Guard(file.clone())),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    tokio::time::sleep(LOCK_POLL).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    anyhow::bail!("timed out waiting for the journal lock on {path}")
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Appends one line from a process that does not own the run's writer, such as `swamp cancel`.
pub async fn append_shared(
    path: &Utf8Path,
    run: crate::ids::RunId,
    node: Option<crate::ids::NodeId>,
    event: JournalEvent,
) -> anyhow::Result<u64> {
    let mut file = tokio::fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .await?;
    let lock = Arc::new(std::fs::File::open(path)?);
    for _ in 0..SHARED_TORN_RETRIES {
        let guard = Guard::acquire(&lock, path).await?;
        let mut bytes = Vec::new();
        file.seek(std::io::SeekFrom::Start(0)).await?;
        file.read_to_end(&mut bytes).await?;
        if !bytes.is_empty() && !bytes.ends_with(b"\n") {
            drop(guard);
            tokio::time::sleep(SHARED_TORN_WAIT).await;
            continue;
        }
        let (_, seq) = repair_point(&bytes);
        let line = JournalLine {
            seq,
            at: time::OffsetDateTime::now_utc(),
            run,
            node,
            event,
        };
        let mut text = serde_json::to_string(&line)?;
        text.push('\n');
        file.write_all(text.as_bytes()).await?;
        file.flush().await?;
        drop(guard);
        file.sync_data().await?;
        return Ok(seq);
    }
    // A tail torn for this long is a crash leftover, which `open` repairs.
    let mut w = Writer::open(path, FsyncPolicy::Always).await?;
    let line = JournalLine {
        seq: w.seq,
        at: time::OffsetDateTime::now_utc(),
        run,
        node,
        event,
    };
    let seq = w.append(&line).await?;
    w.sync().await?;
    Ok(seq)
}

/// Records that must be on disk before the side effect they announce.
fn is_barrier(event: &JournalEvent) -> bool {
    matches!(
        event,
        JournalEvent::RunStarted { .. }
            | JournalEvent::NodeSpawned { .. }
            | JournalEvent::ProcessStarted { .. }
            | JournalEvent::ProcessExited { .. }
            | JournalEvent::DispatchIssued { .. }
            | JournalEvent::TaskQueued { .. }
            | JournalEvent::DispatchRejected { .. }
            | JournalEvent::DispatchSettled { .. }
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
