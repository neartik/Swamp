#![allow(dead_code, unused_variables)]

use crate::journal::fold::Projection;
use crate::journal::record::JournalLine;
use camino::{Utf8Path, Utf8PathBuf};

pub fn replay<P: Projection>(journal: &Utf8Path, p: P) -> anyhow::Result<P::Out> {
    todo!("WP2")
}

pub struct Tailer {
    pub path: Utf8PathBuf,
    pub offset: u64,
}

impl Tailer {
    pub fn open(journal: &Utf8Path) -> anyhow::Result<Self> {
        todo!("WP2")
    }
    /// Complete lines only; a partial trailing line is held back until its newline arrives.
    pub async fn poll(&mut self) -> anyhow::Result<Vec<JournalLine>> {
        todo!("WP2")
    }
}
