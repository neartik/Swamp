use crate::dispatch::Dispatcher;
use crate::mcp::jsonrpc::{Request, Response, RpcError};
use crate::mcp::tools;
use serde_json::{Value, json};
use std::sync::Arc;

/// The MCP revision this server speaks. Both CLIs negotiate it in `initialize`.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SERVER_NAME: &str = "swamp";

/// initialize / notifications/initialized / tools/list / tools/call.
pub async fn handle(disp: &Arc<Dispatcher>, req: Request) -> Option<Response> {
    let result = route(disp, &req).await;
    // A notification is executed and never answered, not even when it failed.
    let id = req.id.clone()?;
    Some(Response {
        id: Some(id),
        result,
    })
}

async fn route(disp: &Arc<Dispatcher>, req: &Request) -> Result<Value, RpcError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": crate::VERSION },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": listing() })),
        "tools/call" => call(disp, &req.params).await,
        // Unknown notifications are ignored on purpose: clients send lifecycle chatter.
        m if m.starts_with("notifications/") => Ok(Value::Null),
        m => Err(RpcError::method_not_found(m)),
    }
}

fn listing() -> Vec<Value> {
    tools::schemas()
        .into_iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect()
}

async fn call(disp: &Arc<Dispatcher>, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("tools/call needs a string `name`"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = tools::call(disp, name, args).await?;
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|e| e.to_string());
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": value,
        "isError": false,
    }))
}
