#![allow(dead_code, unused_variables)]

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Request {
    pub id: Option<Value>,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone)]
pub struct Response {
    pub id: Option<Value>,
    pub result: Result<Value, RpcError>,
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

pub fn parse_line(s: &str) -> Result<Request, RpcError> {
    todo!("WP6")
}

pub fn encode(r: &Response) -> String {
    todo!("WP6")
}
