#![allow(dead_code, unused_variables)]

use crate::model::node::ExitInfo;
use camino::Utf8Path;
use std::future::Future;
use std::time::Duration;

/// Writes pid plus process start ticks.
pub fn write_pidfile(path: &Utf8Path, pid: i32) -> anyhow::Result<()> {
    todo!("WP3")
}

/// PID-reuse safe: compares the recorded process start time, not just the pid.
pub fn is_ours(path: &Utf8Path) -> bool {
    todo!("WP3")
}

#[allow(clippy::manual_async_fn)]
pub fn wait_exit(pid: i32, poll: Duration) -> impl Future<Output = Option<ExitInfo>> {
    async move { todo!("WP3") }
}
