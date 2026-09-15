use serde_json::{Value, json};

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

const VERSION: &str = "2.0";

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

impl Request {
    /// No id means a notification: it is executed and never answered.
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

impl Response {
    pub fn ok(id: Option<Value>, value: Value) -> Self {
        Response {
            id,
            result: Ok(value),
        }
    }
    pub fn err(id: Option<Value>, error: RpcError) -> Self {
        Response {
            id,
            result: Err(error),
        }
    }
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, message)
    }
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }
    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND, format!("unknown method `{method}`"))
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, message)
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

pub fn parse_line(s: &str) -> Result<Request, RpcError> {
    let value: Value = serde_json::from_str(s.trim())
        .map_err(|e| RpcError::parse(format!("invalid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| RpcError::invalid_request("a request must be a JSON object"))?;

    if let Some(v) = obj.get("jsonrpc").and_then(Value::as_str)
        && v != VERSION
    {
        return Err(RpcError::invalid_request(format!(
            "unsupported jsonrpc version `{v}`"
        )));
    }
    let method = obj
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_request("a request must carry a string `method`"))?
        .to_owned();
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if !matches!(params, Value::Null | Value::Object(_) | Value::Array(_)) {
        return Err(RpcError::invalid_request(
            "`params` must be an object or an array",
        ));
    }
    Ok(Request {
        // A null id is an absent id: both are notifications.
        id: obj.get("id").filter(|v| !v.is_null()).cloned(),
        method,
        params,
    })
}

/// One NDJSON line, without the terminating newline: the transport frames.
pub fn encode(r: &Response) -> String {
    let id = r.id.clone().unwrap_or(Value::Null);
    let body = match &r.result {
        Ok(value) => json!({ "jsonrpc": VERSION, "id": id, "result": value }),
        Err(e) => json!({
            "jsonrpc": VERSION,
            "id": id,
            "error": { "code": e.code, "message": e.message },
        }),
    };
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_json_is_a_parse_error() {
        let e = parse_line("{not json").unwrap_err();
        assert_eq!(e.code, PARSE_ERROR);
    }

    #[test]
    fn a_request_without_a_method_is_invalid() {
        let e = parse_line(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err();
        assert_eq!(e.code, INVALID_REQUEST);
    }

    #[test]
    fn a_null_id_is_a_notification() {
        let r = parse_line(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).unwrap();
        assert!(r.is_notification());
        let r = parse_line(r#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        assert!(r.is_notification());
        assert_eq!(r.params, Value::Null);
    }

    #[test]
    fn responses_encode_as_one_line() {
        let ok = encode(&Response::ok(Some(json!(7)), json!({"a": 1})));
        assert_eq!(ok, r#"{"jsonrpc":"2.0","id":7,"result":{"a":1}}"#);
        assert!(!ok.contains('\n'));
        let err = encode(&Response::err(None, RpcError::method_not_found("nope")));
        assert_eq!(
            err,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32601,"message":"unknown method `nope`"}}"#
        );
    }
}
