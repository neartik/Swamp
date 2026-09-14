#![allow(dead_code, unused_variables)]

use crate::ids::NodeId;
use camino::{Utf8Path, Utf8PathBuf};
use std::ffi::OsString;
use std::time::Duration;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy)]
pub struct Detached {
    pub pid: i32,
    pub pgid: i32,
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Clone)]
pub struct NodeIo {
    pub node: NodeId,
    pub prompt: Utf8PathBuf,
    pub stdout: Utf8PathBuf,
    pub stderr: Utf8PathBuf,
    pub pidfile: Utf8PathBuf,
    pub depth: u32,
}

pub fn spawn_detached(
    argv: &[OsString],
    env: &[(OsString, OsString)],
    cwd: &Utf8Path,
    io: &NodeIo,
) -> anyhow::Result<Detached> {
    todo!("WP3")
}

/// SIGTERM to -pgid, wait `grace`, then SIGKILL. Reaps the worker's own grandchildren.
pub async fn terminate(pgid: i32, grace: Duration) -> anyhow::Result<()> {
    todo!("WP3")
}
