//! JSON-RPC 2.0 framing for the MCP stdio transport: one message per line, no
//! embedded newlines, and no batches (removed from MCP in revision 2025-06-18).

use serde_json::{Map, Value, json};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub enum Message {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    /// A reply from the client; this server never sends requests, so it is only logged.
    Response,
}

/// A message that cannot be served. `id` is the request's when it could be
/// read, `null` otherwise, as JSON-RPC 2.0 section 5 asks.
pub struct Invalid {
    pub id: Value,
    pub code: i64,
    pub message: String,
}

impl Invalid {
    fn new(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            id,
            code,
            message: message.into(),
        }
    }

    pub fn reply(&self) -> String {
        failure(&self.id, self.code, &self.message)
    }
}

pub fn parse(line: &str) -> Result<Message, Invalid> {
    let value: Value =
        serde_json::from_str(line).map_err(|e| Invalid::new(Value::Null, PARSE_ERROR, format!("not JSON: {e}")))?;
    let Value::Object(mut obj) = value else {
        return Err(Invalid::new(
            Value::Null,
            INVALID_REQUEST,
            "expected one JSON-RPC object per line (batches are not supported)",
        ));
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(Invalid::new(Value::Null, INVALID_REQUEST, "jsonrpc must be \"2.0\""));
    }
    let id = match obj.remove("id") {
        None => None,
        Some(id @ Value::String(_)) => Some(id),
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Some(Value::Number(n)),
        Some(_) => {
            return Err(Invalid::new(
                Value::Null,
                INVALID_REQUEST,
                "id must be a string or an integer",
            ));
        }
    };
    let method = match obj.remove("method") {
        Some(Value::String(m)) => m,
        Some(_) => {
            return Err(Invalid::new(
                id.unwrap_or_default(),
                INVALID_REQUEST,
                "method must be a string",
            ));
        }
        None if id.is_some() && (obj.contains_key("result") || obj.contains_key("error")) => {
            return Ok(Message::Response);
        }
        None => {
            return Err(Invalid::new(
                id.unwrap_or_default(),
                INVALID_REQUEST,
                "method is missing",
            ));
        }
    };
    let params = obj.remove("params").unwrap_or_else(|| Value::Object(Map::new()));
    let Some(id) = id else {
        return Ok(Message::Notification { method, params });
    };
    if !params.is_object() {
        return Err(Invalid::new(id, INVALID_PARAMS, "params must be an object"));
    }
    Ok(Message::Request { id, method, params })
}

/// `Value`'s `Display` writes compact JSON, which escapes every newline inside
/// strings, so the reply always fits on one line.
pub fn success(id: &Value, result: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

pub fn failure(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invalid(line: &str) -> (Value, i64) {
        match parse(line) {
            Err(e) => (e.id, e.code),
            Ok(_) => panic!("{line} parsed"),
        }
    }

    #[test]
    fn requests_and_notifications() {
        let Ok(Message::Request { id, method, params }) =
            parse(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"cursor":"x"}}"#)
        else {
            panic!("not a request");
        };
        assert_eq!(
            (id, method.as_str(), &params["cursor"]),
            (json!(7), "tools/list", &json!("x"))
        );

        let Ok(Message::Request { id, params, .. }) = parse(r#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#) else {
            panic!("not a request");
        };
        assert_eq!((id, params), (json!("a"), json!({})));

        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Ok(Message::Notification { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#),
            Ok(Message::Response)
        ));
    }

    #[test]
    fn malformed_messages() {
        assert_eq!(invalid("{not json"), (Value::Null, PARSE_ERROR));
        assert_eq!(
            invalid(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).1,
            INVALID_REQUEST
        );
        assert_eq!(
            invalid(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#).1,
            INVALID_REQUEST
        );
        assert_eq!(
            invalid(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#),
            (Value::Null, INVALID_REQUEST)
        );
        assert_eq!(
            invalid(r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#).1,
            INVALID_REQUEST
        );
        assert_eq!(invalid(r#"{"jsonrpc":"2.0","id":4}"#), (json!(4), INVALID_REQUEST));
        assert_eq!(
            invalid(r#"{"jsonrpc":"2.0","id":5,"method":"ping","params":[1]}"#),
            (json!(5), INVALID_PARAMS)
        );
    }

    #[test]
    fn replies_are_single_lines() {
        let line = success(&json!(1), json!({"text": "two\nlines"}));
        assert!(!line.contains('\n'));
        let back: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(back["result"]["text"], "two\nlines");
        let err: Value = serde_json::from_str(&failure(&Value::Null, PARSE_ERROR, "bad")).expect("json");
        assert_eq!(
            (err["id"].clone(), err["error"]["code"].clone()),
            (Value::Null, json!(PARSE_ERROR))
        );
    }
}
