#![allow(dead_code, unused_variables)]

use crate::model::node::WorkResultRef;
use crate::workspace::git::Git;
use camino::Utf8PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum MergeStrategy {
    #[default]
    Apply,
    Merge,
    CherryPick,
}

pub enum AdoptResult {
    Clean { commit: Option<String> },
    Conflicted { paths: Vec<Utf8PathBuf> },
    Rejected { reason: String },
}

pub async fn adopt(
    git: &Git,
    work: &WorkResultRef,
    strategy: MergeStrategy,
    into: Option<&str>,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<AdoptResult> {
    todo!("WP5")
}
