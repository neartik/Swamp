use crate::ids::NodeId;
use crate::journal::raw::RawSink;
use crate::model::event::WorkerEvent;
use crate::worker::adapter::{ParseState, ProviderAdapter};
use crate::worker::classify::MAX_LINE;
use camino::Utf8Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::sync::mpsc;

pub const POLL: Duration = Duration::from_millis(150);

/// One line read from the stream, capped: `consumed` is what the file advanced by,
/// which is not what landed in the buffer when the line was longer than the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CappedLine {
    pub consumed: u64,
    pub complete: bool,
    pub truncated: bool,
}

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
    let mut offset = offset;
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut rdr = BufReader::new(f);
    let mut buf = Vec::with_capacity(8 * 1024);
    loop {
        buf.clear();
        let line = read_capped_line(&mut rdr, &mut buf, MAX_LINE).await?;
        if line.consumed == 0 {
            if !alive() {
                break;
            }
            tokio::time::sleep(POLL).await;
            continue;
        }
        // A partial trailing write is held back until its newline arrives; a worker killed
        // mid-line never sends one, so the offset stops at the last whole line.
        if !line.complete {
            if !alive() {
                break;
            }
            rdr.seek(std::io::SeekFrom::Start(offset)).await?;
            tokio::time::sleep(POLL).await;
            continue;
        }
        offset += line.consumed;
        let text = String::from_utf8_lossy(&buf);
        // Lossy replacement can grow the byte count, so cap once more before parsing.
        let text = truncate_line(text.trim_end_matches(['\n', '\r']), MAX_LINE);
        let po = adapter.parse_line(text, st);
        if po.noise {
            st.unparsed += 1;
            sink.noise(text).await;
        }
        for ev in po.events {
            out.send((node, ev, offset)).await?;
        }
    }
    Ok(offset)
}

/// Reads one newline-terminated line, appending at most `max` bytes to `buf`. A 40 MB base64
/// blob on one line must degrade, not OOM, so the rest of the line is consumed and dropped
/// before anything tries to parse it.
pub async fn read_capped_line<R: tokio::io::AsyncBufRead + Unpin>(
    rdr: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<CappedLine> {
    let mut consumed = 0u64;
    let mut truncated = false;
    loop {
        let available = rdr.fill_buf().await?;
        if available.is_empty() {
            return Ok(CappedLine {
                consumed,
                complete: false,
                truncated,
            });
        }
        let (chunk, complete) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (&available[..=i], true),
            None => (available, false),
        };
        let take = max.saturating_sub(buf.len()).min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
        truncated |= take < chunk.len();
        let n = chunk.len();
        rdr.consume(n);
        consumed += n as u64;
        if complete {
            return Ok(CappedLine {
                consumed,
                complete: true,
                truncated,
            });
        }
    }
}

/// Byte-bounded and char-boundary safe.
pub fn truncate_line(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
