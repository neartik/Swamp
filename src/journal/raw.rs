#![allow(dead_code, unused_variables)]

use crate::ids::NodeId;
use crate::journal::paths::RunPaths;
use std::borrow::Cow;
use std::sync::Arc;

/// Verbatim per-node stream, stderr and noise sinks.
pub struct RawSink {
    pub node: NodeId,
    pub redact: Arc<Redactor>,
}

impl RawSink {
    pub async fn open(
        paths: &RunPaths,
        node: NodeId,
        redact: Arc<Redactor>,
    ) -> anyhow::Result<Self> {
        todo!("WP2")
    }
    pub async fn noise(&mut self, line: &str) {
        todo!("WP2")
    }
    pub async fn stderr_line(&mut self, line: &str) {
        todo!("WP2")
    }
    pub async fn flush(&mut self) -> anyhow::Result<()> {
        todo!("WP2")
    }
}

pub struct Redactor {
    pub set: regex::RegexSet,
}

impl Redactor {
    pub fn new(patterns: &[String]) -> anyhow::Result<Self> {
        todo!("WP2")
    }
    pub fn apply<'a>(&self, s: &'a str) -> Cow<'a, str> {
        todo!("WP2")
    }
}
