#![allow(dead_code, unused_variables)]

use crate::journal::record::JournalLine;
use camino::Utf8Path;
use std::time::Duration;

/// `always` | `barrier` | `interval:<dur>` | `never`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    Always,
    Barrier,
    Interval(Duration),
    Never,
}

impl std::str::FromStr for FsyncPolicy {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        todo!("WP2")
    }
}

/// Owns the journal fd. One instance per run, driven by the single writer task.
pub struct Writer {
    pub path: camino::Utf8PathBuf,
    pub policy: FsyncPolicy,
    pub seq: u64,
}

impl Writer {
    /// Repairs a torn tail by truncating back to the last newline and continues `seq` from there.
    pub async fn open(path: &Utf8Path, policy: FsyncPolicy) -> anyhow::Result<Self> {
        todo!("WP2")
    }
    pub async fn append(&mut self, line: &JournalLine) -> anyhow::Result<u64> {
        todo!("WP2")
    }
    pub async fn sync(&mut self) -> anyhow::Result<()> {
        todo!("WP2")
    }
}
