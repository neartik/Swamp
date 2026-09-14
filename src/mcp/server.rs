#![allow(dead_code, unused_variables)]

use crate::dispatch::Dispatcher;
use crate::mcp::jsonrpc::{Request, Response};
use std::sync::Arc;

/// initialize / notifications/initialized / tools/list / tools/call.
pub async fn handle(disp: &Arc<Dispatcher>, req: Request) -> Option<Response> {
    todo!("WP6")
}
