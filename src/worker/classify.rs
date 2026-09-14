#![allow(dead_code, unused_variables)]

use crate::model::failure::Failure;
use crate::worker::adapter::ExitContext;

pub const MAX_LINE: usize = 8 * 1024 * 1024;

/// Layered: telemetry, then the structured result, then config regexes, then the exit code.
pub fn classify(cx: &ExitContext<'_>) -> Option<Failure> {
    todo!("WP3")
}
