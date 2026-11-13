//! JSON-RPC 2.0 as MCP frames it on a stdio transport: one JSON object per line, requests carrying
//! an id, notifications carrying none (D107). Nothing here knows what a tool is.

use serde_json::{Value, json};

/// A reply to a request we sent, carrying the id it answers.
#[derive(Debug, PartialEq)]
pub struct Reply {
    pub id: u64,
    pub result: Result<Value, String>,
}

/// A request, as one line without the newline.
pub fn request(id: u64, method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

/// A notification, which by definition has no id and gets no reply.
pub fn notification(method: &str, params: Value) -> String {
    json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
}

/// Reads one line, keeping the id so a reply can be matched to its request. A line with no whole
/// number for an id is a notification rather than a reply, and not this function's business.
pub fn reply(text: &str) -> Option<Reply> {
    let value: Value = serde_json::from_str(text).ok()?;
    let id = value.get("id").and_then(Value::as_u64)?;
    let outcome = match (value.get("error"), value.get("result")) {
        // A reply carries exactly one of the two; an error is a failure, not a host error.
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
        // No id: the server must not answer a notification.
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
        // A notification from the server is not a reply, and a line that is not JSON is nothing.
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#),
            None
        );
        assert_eq!(reply("not json at all"), None);
    }
}
