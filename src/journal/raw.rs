use crate::ids::NodeId;
use crate::journal::paths::RunPaths;
use regex::{Regex, RegexSet};
use std::borrow::Cow;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufWriter};

const MASK: &str = "[redacted]";

/// Verbatim per-node stream, stderr and noise sinks.
pub struct RawSink {
    pub node: NodeId,
    pub redact: Arc<Redactor>,
    noise: BufWriter<tokio::fs::File>,
    stderr: BufWriter<tokio::fs::File>,
}

impl RawSink {
    pub async fn open(
        paths: &RunPaths,
        node: NodeId,
        redact: Arc<Redactor>,
    ) -> anyhow::Result<Self> {
        let dir = paths.node_dir(node);
        tokio::fs::create_dir_all(&dir).await?;
        Ok(RawSink {
            node,
            redact,
            noise: BufWriter::new(append(&paths.noise(node)).await?),
            stderr: BufWriter::new(append(&paths.stderr(node)).await?),
        })
    }

    pub async fn noise(&mut self, line: &str) {
        let text = self.redact.apply(line);
        write_line(&mut self.noise, &text).await;
    }

    pub async fn stderr_line(&mut self, line: &str) {
        let text = self.redact.apply(line);
        write_line(&mut self.stderr, &text).await;
    }

    pub async fn flush(&mut self) -> anyhow::Result<()> {
        self.noise.flush().await?;
        self.stderr.flush().await?;
        Ok(())
    }
}

async fn append(path: &camino::Utf8Path) -> anyhow::Result<tokio::fs::File> {
    Ok(tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?)
}

/// Sinks never fail a run: a broken raw file is logged, not propagated.
async fn write_line(w: &mut BufWriter<tokio::fs::File>, line: &str) {
    let mut buf = line.trim_end_matches('\n').to_owned();
    buf.push('\n');
    if let Err(e) = w.write_all(buf.as_bytes()).await {
        tracing::warn!("raw sink write failed: {e}");
    }
}

pub struct Redactor {
    pub set: RegexSet,
    patterns: Vec<Regex>,
}

impl Redactor {
    pub fn new(patterns: &[String]) -> anyhow::Result<Self> {
        let set = RegexSet::new(patterns)?;
        let compiled = patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Redactor {
            set,
            patterns: compiled,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Every pattern is matched against the ORIGINAL text and the spans are masked together,
    /// so one pattern cannot consume the anchor another one needs.
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        if self.patterns.is_empty() || !self.set.is_match(s) {
            return Cow::Borrowed(s);
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for i in self.set.matches(s) {
            spans.extend(self.patterns[i].find_iter(s).map(|m| (m.start(), m.end())));
        }
        spans.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for (start, end) in spans {
            match merged.last_mut() {
                Some((_, last_end)) if start <= *last_end => *last_end = (*last_end).max(end),
                _ => merged.push((start, end)),
            }
        }

        let mut out = String::with_capacity(s.len());
        let mut cursor = 0usize;
        for (start, end) in merged {
            out.push_str(&s[cursor..start]);
            out.push_str(MASK);
            cursor = end;
        }
        out.push_str(&s[cursor..]);
        Cow::Owned(out)
    }

    /// Redacts every string inside a JSON document. Applying the patterns to the encoded
    /// line instead would let a greedy match eat the quote that closes the string.
    pub fn redact_value(&self, v: &mut serde_json::Value) {
        match v {
            serde_json::Value::String(s) => {
                if let Cow::Owned(masked) = self.apply(s) {
                    *s = masked;
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            serde_json::Value::Object(map) => {
                for (_, value) in map.iter_mut() {
                    self.redact_value(value);
                }
            }
            _ => {}
        }
    }
}
