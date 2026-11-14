//! fetch schema, UTF-8 interpretation and bounded presentation belong to the guest (D143).

#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, Output, Tool, http_get};
use saddle_sdk::{Header, HttpRequest, HttpResponse};
use serde::Deserialize;
#[cfg(target_arch = "wasm32")]
use serde_json::json;
use std::collections::BTreeMap;

pub struct Fetch;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    url: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

pub fn fetch_with(
    arguments: &str,
    mut get: impl FnMut(HttpRequest) -> Result<HttpResponse, String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let reply = get(HttpRequest {
        url: args.url,
        headers: args
            .headers
            .into_iter()
            .map(|(name, value)| Header { name, value })
            .collect(),
    })?;
    let content_type = reply
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-type"))
        .map(|h| h.value.as_str())
        .unwrap_or_default();
    let bytes = match String::from_utf8(reply.body) {
        Ok(text) => text,
        Err(error) if reply.truncated && error.utf8_error().error_len().is_none() => {
            let end = error.utf8_error().valid_up_to();
            String::from_utf8(error.into_bytes()[..end].to_vec())
                .map_err(|_| "HTTP body is not UTF-8")?
        }
        Err(_) => return Err("HTTP body is not UTF-8 text".into()),
    };
    let mut end = bytes.len().min(60 * 1024);
    while !bytes.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = format!(
        "{}{}\n\n{}",
        reply.status,
        if content_type.is_empty() {
            String::new()
        } else {
            format!(" {content_type}")
        },
        &bytes[..end]
    );
    if reply.truncated || end < bytes.len() {
        text.push_str("\n\n(body truncated; capture limit 100 KiB, presentation 60 KiB)");
    }
    if text.len() > 65536 {
        return Err("HTTP presentation exceeds limit".into());
    }
    Ok(text)
}

#[cfg(target_arch = "wasm32")]
impl Tool for Fetch {
    fn describe() -> Definition {
        Definition {name:"fetch".into(),description:"GET a known HTTP(S) URL through delegated net grants. Every redirect is independently authorized; headers are removed on cross-origin redirects. Secrets belong in headers, never query strings; only header names are audited. Returns status, content type and UTF-8 text. Captures 100 KiB, presents 60 KiB; labels truncation. Private addresses require explicit reach in both parent and plugin grants. Use websearch to discover URLs.".into(),input_schema:json!({"type":"object","properties":{"url":{"type":"string"},"headers":{"type":"object","additionalProperties":{"type":"string"}}},"required":["url"],"additionalProperties":false}).to_string()}
    }
    fn invoke(arguments: String) -> Output {
        match fetch_with(&arguments, |request| http_get(&request)) {
            Ok(text) => Output {
                text,
                is_error: false,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requested_text_and_truncation_are_presented_without_binary_fallback() {
        let result = fetch_with(r#"{"url":"https://example.test"}"#, |_| {
            Ok(HttpResponse {
                status: 404,
                headers: vec![],
                body: b"missing".to_vec(),
                truncated: false,
            })
        })
        .unwrap();
        assert_eq!(result, "404\n\nmissing");
        assert!(
            fetch_with(r#"{"url":"https://example.test"}"#, |_| Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: vec![255],
                truncated: false
            }))
            .is_err()
        );
    }
}
