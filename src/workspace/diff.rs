#![allow(dead_code, unused_variables)]

use crate::model::core::FileChange;
use crate::workspace::git::Git;
use camino::{Utf8Path, Utf8PathBuf};

pub struct DiffSummary {
    pub files: Vec<FileChange>,
    pub insertions: u32,
    pub deletions: u32,
    pub head: String,
    pub patch: Utf8PathBuf,
    pub empty: bool,
}

/// Git is authoritative for what a worker touched; the event stream is only a live estimate.
pub async fn collect(
    git: &Git,
    wt: &Utf8Path,
    base: &str,
    out: &Utf8Path,
) -> anyhow::Result<DiffSummary> {
    todo!("WP5")
}
