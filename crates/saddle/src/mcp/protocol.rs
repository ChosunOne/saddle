use serde_json::{Value, json};

#[derive(Debug, PartialEq)]
pub struct Reply {
    pub id: u64,
    pub result: Result<Value, String>,
}

pub fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

pub fn reply(text: &str) -> Option<Reply> {
    let value: Value = serde_json::from_str(text).ok()?;
    let id = value.get("id").and_then(Value::as_u64)?;
    let outcome = match (value.get("error"), value.get("result")) {
        (Some(error), _) => Err(error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the server said nothing about it")
            .to_owned()),
        (None, Some(result)) => Ok(result.clone()),
        (None, None) => Err("a reply with neither a result nor an error".to_owned()),
    };
    Some(Reply {
        id,
        result: outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_and_a_notification_differ_only_by_their_id() {
        assert_eq!(
            request(7, "tools/list", json!({})),
            r#"{"id":7,"jsonrpc":"2.0","method":"tools/list","params":{}}"#
        );
        assert_eq!(
            notification("notifications/initialized", json!({})),
            r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#
        );
    }

    #[test]
    fn a_reply_is_matched_by_its_id_and_an_error_is_a_failure_not_a_host_error() {
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}"#),
            Some(Reply {
                id: 3,
                result: Ok(json!({"tools": []}))
            })
        );
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32601,"message":"no such method"}}"#),
            Some(Reply {
                id: 4,
                result: Err("no such method".to_owned())
            })
        );
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#),
            None
        );
        assert_eq!(reply("not json at all"), None);
    }
}

#[derive(Debug, PartialEq)]
pub enum Message {
    Reply(Reply),
    Request {
        id: u64,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
    },
}

pub fn message(text: &str) -> Option<Message> {
    let value: Value = serde_json::from_str(text).ok()?;
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").and_then(Value::as_u64);
    Some(match (method, id) {
        (Some(method), Some(id)) => Message::Request {
            id,
            method: method.to_owned(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        },
        (Some(method), None) => Message::Notification {
            method: method.to_owned(),
        },
        (None, _) => Message::Reply(reply(text)?),
    })
}

pub fn response(id: u64, result: Result<Value, String>) -> String {
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string(),
        Err(message) => json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": message}})
        .to_string(),
    }
}
