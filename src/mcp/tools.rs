#![allow(dead_code, unused_variables)]

use crate::dispatch::Dispatcher;
use crate::ids::NodeId;
use crate::mcp::jsonrpc::RpcError;
use serde_json::Value;
use std::sync::Arc;

pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub fn schemas() -> Vec<ToolSchema> {
    todo!("WP6")
}

pub async fn call(disp: &Arc<Dispatcher>, name: &str, args: Value) -> Result<Value, RpcError> {
    todo!("WP6")
}

/// Worker output is attacker-influenced data. Truncate and wrap before it reaches the brain.
pub fn wrap_untrusted(node: NodeId, text: &str, max_bytes: usize) -> String {
    todo!("WP6")
}
