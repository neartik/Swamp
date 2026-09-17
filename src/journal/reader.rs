use crate::journal::fold::Projection;
use crate::journal::record::JournalLine;
use camino::{Utf8Path, Utf8PathBuf};
use std::io::BufRead;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Byte-offset poll interval: the path and the writer are both known, so no watcher is needed.
pub const POLL_INTERVAL: Duration = Duration::from_millis(150);

pub fn replay<P: Projection>(journal: &Utf8Path, mut p: P) -> anyhow::Result<P::Out> {
    let file = std::fs::File::open(journal)
        .map_err(|e| anyhow::Error::new(e).context(format!("opening {journal}")))?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = Vec::new();
    let mut torn = 0u32;
    let mut unparsed = 0u32;

    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        if !buf.ends_with(b"\n") {
            // A crash mid-write leaves a partial last line. Expected, not an error.
            torn += 1;
            break;
        }
        match parse(&buf) {
            Parsed::Empty => {}
            Parsed::Line(l) => p.apply(&l),
            Parsed::Bad => unparsed += 1,
        }
    }
    if torn > 0 || unparsed > 0 {
        tracing::warn!("{journal}: skipped {unparsed} unparsable and {torn} partial lines");
    }
    Ok(p.finish())
}

enum Parsed {
    Line(Box<JournalLine>),
    Empty,
    Bad,
}

fn parse(raw: &[u8]) -> Parsed {
    let line = raw.strip_suffix(b"\n").unwrap_or(raw);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.iter().all(u8::is_ascii_whitespace) {
        return Parsed::Empty;
    }
    match serde_json::from_slice::<JournalLine>(line) {
        Ok(l) => Parsed::Line(Box::new(l)),
        Err(e) => {
            tracing::debug!("unparsable journal line: {e}");
            Parsed::Bad
        }
    }
}

#[derive(Debug)]
pub struct Tailer {
    pub path: Utf8PathBuf,
    pub offset: u64,
}

impl Tailer {
    pub fn open(journal: &Utf8Path) -> anyhow::Result<Self> {
        Ok(Tailer {
            path: journal.to_path_buf(),
            offset: 0,
        })
    }

    /// Complete lines only; a partial trailing line is held back until its newline arrives.
    pub async fn poll(&mut self) -> anyhow::Result<Vec<JournalLine>> {
        let first = self.read_new().await?;
        if !first.is_empty() {
            return Ok(first);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        self.read_new().await
    }

    async fn read_new(&mut self) -> anyhow::Result<Vec<JournalLine>> {
        let mut file = match tokio::fs::File::open(&self.path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(anyhow::Error::new(e).context(format!("opening {}", self.path))),
        };
        let len = file.metadata().await?.len();
        if len < self.offset {
            self.offset = len;
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        file.seek(std::io::SeekFrom::Start(self.offset)).await?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        file.read_to_end(&mut buf).await?;

        let Some(last) = buf.iter().rposition(|b| *b == b'\n') else {
            return Ok(Vec::new());
        };
        self.offset += last as u64 + 1;

        let mut out = Vec::new();
        for raw in buf[..=last].split_inclusive(|b| *b == b'\n') {
            match parse(raw) {
                Parsed::Line(l) => out.push(*l),
                Parsed::Empty => {}
                Parsed::Bad => tracing::warn!("{}: skipping unparsable line", self.path),
            }
        }
        Ok(out)
    }
}
