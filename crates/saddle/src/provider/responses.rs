use std::collections::BTreeMap;

use anyhow::{Context, Error as AnyError, Result, anyhow};
use serde_json::{Value, json};

use super::retry::{self, Transient};
use super::{
    Api, Conversation, Image, Item, Part, Provider, ProviderConfig, Reply, Sink, Stop, ToolCall,
    Usage, http, prompt_cache_key, prompt_cache_retention, ws,
};
use super::{Cancelled, SUMMARY_INTRO, sse};

pub const CLIENT_VERSION: &str = "0.169.0";

pub fn complete(p: &Provider, conv: &Conversation<'_>, sink: &mut dyn Sink) -> Result<Reply> {
    let url = format!("{}/responses", p.config.base_url.trim_end_matches('/'));
    let ws_url = ws::websocket_url(&p.config, &url).filter(|_| !conv.session.is_empty());
    complete_at(p, conv, sink, &url, ws_url.as_deref())
}

fn complete_at(
    p: &Provider,
    conv: &Conversation<'_>,
    sink: &mut dyn Sink,
    url: &str,
    ws_url: Option<&str>,
) -> Result<Reply> {
    let headers = p.request_headers_for(conv.session);
    let body = body(&p.config, conv);
    retry::once(sink, |retry, sink| {
        if retry && !p.retry_requests {
            return Err(anyhow!(
                "automatic retries are disabled for bounded plugin requests"
            ));
        }
        let mut pending = None;
        let streamed = match ws_url.filter(|_| !retry) {
            Some(ws_url) => match ws::stream(conv.session, ws_url, &headers, &body) {
                Ok((events, kept)) => {
                    pending = Some(kept);
                    events
                }
                Err(_) => {
                    sink.note("websocket unavailable; falling back to HTTP");
                    if sink.cancelled() {
                        return Err(Cancelled.into());
                    }
                    http::stream(p, url.to_owned(), headers.clone(), &body)?
                }
            },
            None => http::stream(p, url.to_owned(), headers.clone(), &body)?,
        };
        let reply = read_stream(streamed, sink)?;
        if let Some(pending) = pending {
            pending.finish(input(&[Item::Assistant(reply.parts.clone())]));
        }
        Ok(reply)
    })
}

pub fn body(cfg: &ProviderConfig, conv: &Conversation<'_>) -> Value {
    let tools: Vec<Value> = conv
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
                "strict": false,
            })
        })
        .collect();
    let mut body = json!({
        "model": cfg.model,
        "instructions": conv.system,
        "input": input(conv.items),
        "stream": true,
        "store": false,
    });
    if let Some(key) = prompt_cache_key(cfg, conv.session) {
        body["prompt_cache_key"] = key.into();
    }
    if let Some(ttl) = prompt_cache_retention(cfg) {
        body["prompt_cache_retention"] = ttl.into();
    }
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if let Some(n) = cfg.max_tokens {
        body["max_output_tokens"] = n.into();
    }
    let mut reasoning = json!({ "summary": "auto" });
    if let Some(effort) = &cfg.effort {
        reasoning["effort"] = effort.as_str().into();
    }
    body["reasoning"] = reasoning;
    body
}

pub fn input(items: &[Item]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Item::Contribution(value) => out.push(message("user", &[text_part(&value.content)])),
            Item::User(text) => out.push(message("user", &[text_part(text)])),
            Item::Attached { text, images } => {
                let mut content = vec![text_part(text)];
                content.extend(images.iter().map(image_part));
                out.push(message("user", &content));
            }
            Item::Summary(text) => out.push(message(
                "user",
                &[text_part(&format!("{SUMMARY_INTRO}{text}"))],
            )),
            Item::ToolResults(results) => {
                let mut images = Vec::new();
                for r in results {
                    let output = if r.content.trim().is_empty() && !r.images.is_empty() {
                        "(an image follows)".to_owned()
                    } else {
                        r.content.clone()
                    };
                    out.push(json!({
                        "type": "function_call_output",
                        "call_id": r.id,
                        "output": output,
                    }));
                    images.extend(r.images.iter().map(image_part));
                }
                if !images.is_empty() {
                    out.push(message("user", &images));
                }
            }
            Item::Assistant(parts) => {
                for part in parts {
                    match part {
                        Part::Text(t) if !t.trim().is_empty() => {
                            out.push(message("assistant", &[output_text_part(t)]))
                        }
                        Part::Call(c) => out.push(json!({
                            "type": "function_call",
                            "call_id": c.id,
                            "name": c.name,
                            "arguments": c.input.as_ref().map_or_else(|_| "{}".to_owned(), Value::to_string),
                        })),
                        Part::Native { api, block } if *api == Api::OpenaiResponses => {
                            out.push(block.clone());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    out
}

fn text_part(text: &str) -> Value {
    json!({ "type": "input_text", "text": text })
}

fn output_text_part(text: &str) -> Value {
    json!({ "type": "output_text", "text": text })
}

fn image_part(image: &Image) -> Value {
    json!({
        "type": "input_image",
        "image_url": format!("data:{};base64,{}", image.media_type, image.data),
    })
}

fn message(role: &str, content: &[Value]) -> Value {
    json!({ "role": role, "content": content })
}

#[derive(Default)]
struct Pending {
    id: String,
    name: String,
    arguments: String,
    done: bool,
}

pub fn read_stream(mut events: impl sse::Events, sink: &mut dyn Sink) -> Result<Reply> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut reasoning_segment = None;
    let mut reasoning_break = false;
    let mut reasoning_items: Vec<Value> = Vec::new();
    let mut calls: BTreeMap<u64, Pending> = BTreeMap::new();
    let mut by_item: BTreeMap<String, u64> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut reasoning_tokens = 0;
    let mut model = String::new();
    let mut status = String::new();
    let mut incomplete = String::new();
    let mut dropped_reasoning = 0;
    let mut finished = false;

    while let Some(ev) = events.next(sink)? {
        if sink.cancelled() {
            return Err(Cancelled.into());
        }
        if ev.data.trim() == "[DONE]" {
            break;
        }
        let event: Value = serde_json::from_str(&ev.data)
            .with_context(|| format!("unreadable stream event: {}", ev.data))?;
        let kind = event["type"]
            .as_str()
            .or(Some(ev.event.as_str()))
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                if let Some(d) = event["delta"].as_str() {
                    sink.text(d);
                    text.push_str(d);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(d) = event["delta"].as_str().filter(|d| !d.is_empty()) {
                    let segment = (
                        event["output_index"].as_u64(),
                        event["summary_index"]
                            .as_u64()
                            .or_else(|| event["content_index"].as_u64()),
                        kind == "response.reasoning_summary_text.delta",
                    );
                    if !reasoning.is_empty()
                        && (reasoning_break
                            || reasoning_segment.is_some_and(|previous| previous != segment))
                    {
                        let trailing = reasoning
                            .bytes()
                            .rev()
                            .take_while(|b| *b == b'\n')
                            .take(2)
                            .count();
                        let leading = d.bytes().take_while(|b| *b == b'\n').take(2).count();
                        let separator = match 2usize.saturating_sub(trailing + leading) {
                            0 => "",
                            1 => "\n",
                            _ => "\n\n",
                        };
                        if !separator.is_empty() {
                            sink.reasoning(separator);
                            reasoning.push_str(separator);
                        }
                    }
                    sink.reasoning(d);
                    reasoning.push_str(d);
                    reasoning_segment = Some(segment);
                    reasoning_break = false;
                }
            }
            "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.reasoning_text.done" => {
                reasoning_break = !reasoning.is_empty();
            }
            "response.output_item.added" => {
                let item = &event["item"];
                if item["type"] == "reasoning" {
                    reasoning_break = !reasoning.is_empty();
                    reasoning_segment = None;
                }
                if item["type"] == "function_call" {
                    let index = event["output_index"].as_u64().unwrap_or(calls.len() as u64);
                    let pending = calls.entry(index).or_default();
                    pending.id = item["call_id"].as_str().unwrap_or_default().to_owned();
                    pending.name = item["name"].as_str().unwrap_or_default().to_owned();
                    if let Some(id) = item["id"].as_str() {
                        by_item.insert(id.to_owned(), index);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let index = event["output_index"]
                    .as_u64()
                    .or_else(|| {
                        event["item_id"]
                            .as_str()
                            .and_then(|id| by_item.get(id).copied())
                    })
                    .unwrap_or(0);
                if let Some(d) = event["delta"].as_str() {
                    calls.entry(index).or_default().arguments.push_str(d);
                }
            }
            "response.output_item.done" => {
                let item = &event["item"];
                if item["type"] == "reasoning" {
                    reasoning_items.push(item.clone());
                    reasoning_break = !reasoning.is_empty();
                    reasoning_segment = None;
                }
                if item["type"] == "function_call" {
                    let index = event["output_index"].as_u64().unwrap_or(0);
                    let pending = calls.entry(index).or_default();
                    pending.id = item["call_id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&pending.id)
                        .to_owned();
                    pending.name = item["name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&pending.name)
                        .to_owned();
                    if let Some(args) = item["arguments"].as_str() {
                        pending.arguments = args.to_owned();
                    }
                    pending.done = true;
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                let response = &event["response"];
                model = response["model"].as_str().unwrap_or_default().to_owned();
                status = response["status"].as_str().unwrap_or_default().to_owned();
                incomplete = response["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let u = &response["usage"];
                let input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                let cached = u["input_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.input = input_tokens.saturating_sub(cached);
                usage.cache_read = cached;
                usage.cache_write = u["input_tokens_details"]["cache_write_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.output = u["output_tokens"].as_u64().unwrap_or(usage.output);
                reasoning_tokens = u["output_tokens_details"]["reasoning_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                dropped_reasoning = dropped(&response["input_transformations"]);
                finished = true;
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                return Err(stream_error(error));
            }
            "error" => return Err(stream_error(&event)),
            _ => {}
        }
    }
    if !finished {
        return Err(
            Transient::new("the reply stream ended before the response was completed")
                .with_reason("stream ended before completion")
                .into(),
        );
    }

    let mut parts = Vec::new();
    if reasoning_items.is_empty() && !reasoning.is_empty() {
        parts.push(Part::Native {
            api: Api::OpenaiResponses,
            block: json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": reasoning }] }),
        });
    }
    parts.extend(reasoning_items.into_iter().map(|block| Part::Native {
        api: Api::OpenaiResponses,
        block,
    }));
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    let mut called = false;
    for (index, c) in calls {
        if c.name.is_empty() {
            continue;
        }
        called = true;
        let input = if c.arguments.trim().is_empty() {
            Ok(json!({}))
        } else {
            match serde_json::from_str::<Value>(&c.arguments) {
                Ok(v) if v.is_object() => Ok(v),
                _ => Err(c.arguments),
            }
        };
        parts.push(Part::Call(ToolCall {
            id: if c.id.is_empty() {
                format!("call_{index}")
            } else {
                c.id
            },
            name: c.name,
            input,
        }));
    }
    let stop = if called {
        Stop::ToolUse
    } else if status == "incomplete" {
        if incomplete == "max_output_tokens" {
            Stop::MaxTokens
        } else {
            Stop::Other
        }
    } else {
        Stop::EndTurn
    };
    Ok(Reply {
        parts,
        stop,
        stop_raw: if incomplete.is_empty() {
            status
        } else {
            incomplete
        },
        model,
        usage,
        reasoning_tokens,
        dropped_reasoning,
    })
}

fn dropped(transformations: &Value) -> u64 {
    let Some(list) = transformations.as_array() else {
        return 0;
    };
    list.iter()
        .filter(|t| {
            let named = t["item_type"]
                .as_str()
                .or_else(|| t["type"].as_str())
                .unwrap_or_default();
            named.is_empty() || named.contains("reasoning")
        })
        .count() as u64
}

fn message_of(error: &Value) -> String {
    error["message"]
        .as_str()
        .or_else(|| error["error"]["message"].as_str())
        .map_or_else(|| error.to_string(), str::to_owned)
}

fn stream_error(event: &Value) -> AnyError {
    let error = event.get("error").unwrap_or(event);
    let code = error["code"]
        .as_str()
        .or_else(|| event["code"].as_str())
        .or_else(|| error["type"].as_str())
        .unwrap_or_default();
    let status = event["status"]
        .as_u64()
        .or_else(|| event["status_code"].as_u64());
    let permanent = [code, error["type"].as_str().unwrap_or_default()]
        .into_iter()
        .any(|kind| {
            matches!(
                kind,
                "insufficient_quota"
                    | "usage_limit_reached"
                    | "billing_hard_limit_reached"
                    | "context_length_exceeded"
                    | "invalid_api_key"
                    | "authentication_error"
                    | "permission_denied"
            )
        });
    let transient = !permanent
        && (code == "websocket_connection_limit_reached"
            || match status {
                Some(status) => matches!(status, 408 | 409 | 429 | 500..=599),
                None => matches!(
                    code,
                    "server_error"
                        | "internal_error"
                        | "internal_server_error"
                        | "rate_limit_exceeded"
                        | "overloaded"
                        | "service_unavailable"
                        | "temporarily_unavailable"
                ),
            });
    let message = message_of(event);
    if transient {
        let retry_after = [event.get("headers"), error.get("headers")]
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .flat_map(|headers| headers.iter())
            .filter(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .filter_map(|(_, value)| value.as_str()?.trim().parse::<u64>().ok())
            .max();
        let reason = match code {
            "websocket_connection_limit_reached" => "websocket connection age limit reached",
            "rate_limit_exceeded" => "provider rate limit",
            _ if status == Some(429) => "provider rate limit",
            _ => "transient provider error",
        };
        Transient::new(message)
            .with_reason(reason)
            .with_retry_after(retry_after)
            .into()
    } else {
        anyhow!(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::CacheRetention;
    use crate::provider::{Auth, ProviderConfig, Reported, ToolDef, ToolResult};
    use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use tungstenite::{Message, accept};

    #[test]
    fn a_formula_is_sent_as_the_tex_and_never_as_a_picture() {
        let formula =
            "$$\\nabla \\times \\mathbf{E} = -\\frac{\\partial \\mathbf{B}}{\\partial t}$$";
        let items = [
            Item::User(format!("explain {formula}")),
            Item::Assistant(vec![Part::Text(format!("faraday: {formula}"))]),
        ];
        let input = input(&items);
        assert_eq!(input[0]["content"][0]["text"], format!("explain {formula}"));
        assert_eq!(
            input[1]["content"][0]["text"],
            format!("faraday: {formula}")
        );
        assert!(!json!(input).to_string().contains("input_image"));
    }

    struct Script(Vec<(&'static str, &'static str)>);

    struct Silent;
    impl Sink for Silent {
        fn text(&mut self, _s: &str) {}
        fn reasoning(&mut self, _s: &str) {}
        fn note(&mut self, _s: &str) {}
    }

    impl sse::Events for Script {
        fn next(&mut self, _sink: &mut dyn Sink) -> Result<Option<sse::Event>> {
            Ok(if self.0.is_empty() {
                None
            } else {
                let (event, data) = self.0.remove(0);
                Some(sse::Event {
                    event: event.to_owned(),
                    data: data.to_owned(),
                })
            })
        }
    }

    #[derive(Default)]
    struct Capture {
        text: String,
        reasoning: String,
        notes: Vec<String>,
        cancel_on_note: bool,
        cancelled: bool,
    }

    impl Sink for Capture {
        fn text(&mut self, text: &str) {
            self.text.push_str(text);
        }
        fn reasoning(&mut self, text: &str) {
            self.reasoning.push_str(text);
        }
        fn note(&mut self, note: &str) {
            self.notes.push(note.to_owned());
            self.cancelled |= self.cancel_on_note;
        }
        fn cancelled(&self) -> bool {
            self.cancelled
        }
    }

    fn accept_request(listener: &TcpListener) -> TcpStream {
        let start = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "request never arrived"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    }

    fn transports(
        ws_events: Option<Vec<Value>>,
        http_replies: Vec<Vec<Value>>,
    ) -> (String, String, JoinHandle<Vec<Value>>) {
        exchanges(ws_events.into_iter().collect(), http_replies)
    }

    fn exchanges(
        ws_replies: Vec<Vec<Value>>,
        http_replies: Vec<Vec<Value>>,
    ) -> (String, String, JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let mut requests = Vec::new();
            if !ws_replies.is_empty() {
                let mut socket = accept(accept_request(&listener)).unwrap();
                for events in ws_replies {
                    let Message::Text(frame) = socket.read().unwrap() else {
                        panic!("expected response.create");
                    };
                    requests.push(serde_json::from_str(frame.as_str()).unwrap());
                    for event in events {
                        socket
                            .send(Message::Text(event.to_string().into()))
                            .unwrap();
                    }
                }
            }
            for events in http_replies {
                let mut reader = BufReader::new(accept_request(&listener));
                let mut head = String::new();
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    head.push_str(&line.to_ascii_lowercase());
                }
                assert!(head.starts_with("post /responses "), "{head}");
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap();
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes).unwrap();
                requests.push(serde_json::from_slice(&bytes).unwrap());
                let body: String = events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect();
                write!(reader.get_mut(), "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            requests
        });
        (
            format!("http://{address}/responses"),
            format!("ws://{address}/responses"),
            handle,
        )
    }

    fn completed(text: &str) -> Vec<Value> {
        vec![
            json!({"type":"response.output_text.delta","delta":text}),
            json!({"type":"response.completed","response":{"id":"resp_ok","status":"completed","model":"m"}}),
        ]
    }

    fn request(http: &str, ws: Option<&str>, sink: &mut dyn Sink) -> Result<Reply> {
        let provider = Provider::new("mock", config(), None, &Reported::default());
        let items = [Item::User("hello".into())];
        let conv = Conversation {
            system: "s",
            tools: &[],
            items: &items,
            session: "recovery-test",
        };
        complete_at(&provider, &conv, sink, http, ws)
    }

    #[test]
    fn a_websocket_that_drops_after_send_recovers_the_same_request_over_http() {
        let (http, ws, handle) = transports(Some(Vec::new()), vec![completed("recovered")]);
        let mut sink = Capture::default();
        let reply = request(&http, Some(&ws), &mut sink).unwrap();
        assert_eq!(sink.text, "recovered");
        assert_eq!(reply.parts, [Part::Text("recovered".into())]);
        assert_eq!(sink.notes.len(), 1);
        assert!(sink.notes[0].contains("retrying once over HTTP"));
        let mut requests = handle.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["type"], "response.create");
        requests[0].as_object_mut().unwrap().remove("type");
        assert_eq!(
            requests[0], requests[1],
            "the full input, not a continuation"
        );
        assert!(requests[1].get("previous_response_id").is_none());
    }

    #[test]
    fn a_failed_delta_recovers_with_the_entire_conversation() {
        let (http, ws, handle) = exchanges(
            vec![completed("first"), Vec::new()],
            vec![completed("second")],
        );
        let mut sink = Capture::default();
        let first = request(&http, Some(&ws), &mut sink).unwrap();
        let provider = Provider::new("mock", config(), None, &Reported::default());
        let items = [
            Item::User("hello".into()),
            Item::Assistant(first.parts),
            Item::User("again".into()),
        ];
        let conv = Conversation {
            system: "s",
            tools: &[],
            items: &items,
            session: "recovery-test",
        };
        let mut sink = Capture::default();
        complete_at(&provider, &conv, &mut sink, &http, Some(&ws)).unwrap();
        assert_eq!(sink.text, "second");
        let requests = handle.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1]["previous_response_id"], "resp_ok");
        assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
        assert_eq!(requests[2], body(&provider.config, &conv));
        assert_eq!(requests[2]["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn connection_age_limit_and_server_fault_events_recover_without_resending_the_prompt() {
        for event in [
            json!({"type":"error","status":400,"error":{"code":"websocket_connection_limit_reached","message":"connection reached 60 minutes"}}),
            json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"try again"}}}),
        ] {
            let (http, ws, handle) = transports(Some(vec![event]), vec![completed("ok")]);
            let mut sink = Capture::default();
            request(&http, Some(&ws), &mut sink).unwrap();
            assert_eq!(sink.text, "ok");
            assert_eq!(sink.notes.len(), 1);
            assert_eq!(handle.join().unwrap().len(), 2);
        }
    }

    #[test]
    fn permanent_websocket_errors_are_not_retried() {
        for event in [
            json!({"type":"error","status":401,"error":{"code":"invalid_api_key","message":"sign in"}}),
            json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"quota exhausted"}}),
            json!({"type":"error","status":400,"error":{"type":"invalid_request_error","message":"invalid request"}}),
        ] {
            let (http, ws, handle) = transports(Some(vec![event]), Vec::new());
            let mut sink = Capture::default();
            assert!(request(&http, Some(&ws), &mut sink).is_err());
            assert!(sink.notes.is_empty());
            assert_eq!(handle.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn a_dropped_partial_reply_is_not_replayed_or_spliced() {
        for kind in [
            "response.output_text.delta",
            "response.reasoning_summary_text.delta",
        ] {
            let (http, ws, handle) = transports(
                Some(vec![json!({"type":kind,"delta":"partial"})]),
                Vec::new(),
            );
            let mut sink = Capture::default();
            assert!(request(&http, Some(&ws), &mut sink).is_err());
            assert_eq!(format!("{}{}", sink.text, sink.reasoning), "partial");
            assert!(sink.notes.is_empty());
            assert_eq!(handle.join().unwrap().len(), 1);
        }
    }

    #[test]
    fn cancellation_on_the_retry_notice_prevents_another_request() {
        let (http, ws, handle) = transports(Some(Vec::new()), Vec::new());
        let mut sink = Capture {
            cancel_on_note: true,
            ..Capture::default()
        };
        let error = request(&http, Some(&ws), &mut sink).unwrap_err();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn a_second_interrupted_http_stream_is_the_end_of_recovery() {
        let (http, _, handle) = transports(None, vec![Vec::new(), Vec::new()]);
        let mut sink = Capture::default();
        assert!(request(&http, None, &mut sink).is_err());
        assert_eq!(sink.notes.len(), 1);
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn buffered_tool_arguments_from_a_failed_attempt_are_discarded() {
        let partial = json!({"type":"response.output_item.added","output_index":0,
            "item":{"type":"function_call","id":"fc_bad","call_id":"call_bad","name":"write"}});
        let (http, ws, handle) = transports(Some(vec![partial]), vec![completed("no tool")]);
        let mut sink = Capture::default();
        let reply = request(&http, Some(&ws), &mut sink).unwrap();
        assert_eq!(reply.calls().count(), 0);
        assert_eq!(reply.parts, [Part::Text("no tool".into())]);
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn a_temporary_rate_limit_uses_the_servers_retry_after() {
        let event = json!({"type":"error","status":429,
            "error":{"code":"rate_limit_exceeded","message":"try again"},
            "headers":{"Retry-After":"0"}});
        let (http, ws, handle) = transports(Some(vec![event]), vec![completed("ok")]);
        let mut sink = Capture::default();
        request(&http, Some(&ws), &mut sink).unwrap();
        assert_eq!(sink.text, "ok");
        assert!(sink.notes[0].ends_with("in 0s"));
        assert_eq!(handle.join().unwrap().len(), 2);
    }

    #[test]
    fn handshake_failure_falls_back_to_http_with_a_notice() {
        let (http, _, handle) = transports(None, vec![completed("http")]);
        let mut sink = Capture::default();
        request(&http, Some("not a websocket URL"), &mut sink).unwrap();
        assert_eq!(sink.text, "http");
        assert_eq!(sink.notes, ["websocket unavailable; falling back to HTTP"]);
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn malformed_events_and_deliberate_incomplete_replies_are_not_retried() {
        let mut attempts = 0;
        let error = retry::once(&mut Silent, |_, sink| {
            attempts += 1;
            read_stream(Script(vec![("", "not JSON")]), sink)
        })
        .unwrap_err();
        assert!(error.to_string().contains("unreadable stream event"));
        assert_eq!(attempts, 1);

        let incomplete = json!({"type":"response.incomplete","response":{
            "id":"resp_partial","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}});
        let (http, ws, handle) = transports(Some(vec![incomplete]), Vec::new());
        let mut sink = Capture::default();
        let reply = request(&http, Some(&ws), &mut sink).unwrap();
        assert_eq!(reply.stop, Stop::MaxTokens);
        assert!(sink.notes.is_empty());
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn error_classification_does_not_turn_auth_or_quota_into_a_transient_failure() {
        for event in [
            json!({"status":401,"error":{"code":"server_error","message":"sign in"}}),
            json!({"status":403,"error":{"code":"server_error","message":"forbidden"}}),
            json!({"status":429,"error":{"code":"rate_limit_exceeded","type":"usage_limit_reached"}}),
            json!({"status":429,"error":{"code":"insufficient_quota"}}),
            json!({"status":400,"error":{"code":"invalid_value"}}),
        ] {
            assert!(
                stream_error(&event).downcast_ref::<Transient>().is_none(),
                "{event}"
            );
        }
        for event in [
            json!({"status_code":503,"error":{"message":"temporarily unavailable"}}),
            json!({"error":{"type":"server_error"}}),
            json!({"status":400,"error":{"code":"websocket_connection_limit_reached"}}),
        ] {
            assert!(
                stream_error(&event).downcast_ref::<Transient>().is_some(),
                "{event}"
            );
        }
    }

    fn config() -> ProviderConfig {
        ProviderConfig {
            api: Api::OpenaiResponses,
            base_url: "https://chatgpt.test/backend-api/codex".into(),
            model: "m".into(),
            api_key_env: None,
            max_tokens: None,
            context_window: None,
            effort: None,
            price: None,
            usage_url: None,
            search_url: None,
            headers: Default::default(),
            key_header: None,
            cache_retention: Default::default(),
            cache_affinity: false,
            auth: Auth::ApiKey,
            oauth: None,
        }
    }

    #[test]
    fn a_request_to_openai_carries_the_cache_key_and_no_other_server_does() {
        let items = vec![Item::User("hello".into())];
        let conv = |session: &'static str| Conversation {
            system: "s",
            tools: &[],
            items: &items,
            session,
        };
        assert_eq!(body(&config(), &conv("s1")).get("prompt_cache_key"), None);

        let direct = ProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            ..config()
        };
        let sent = body(&direct, &conv("s1"));
        assert_eq!(sent["prompt_cache_key"], "s1");
        assert_eq!(sent.get("prompt_cache_retention"), None);

        let long = ProviderConfig {
            cache_retention: CacheRetention::Long,
            ..direct
        };
        assert_eq!(body(&long, &conv("s1"))["prompt_cache_retention"], "24h");
        assert_eq!(
            body(&long, &conv("")).get("prompt_cache_key"),
            None,
            "no saved conversation, nothing to route"
        );
    }

    #[test]
    fn the_conversation_is_sent_as_typed_items() {
        let items = vec![
            Item::User("hi".into()),
            Item::Assistant(vec![
                Part::Text("looking".into()),
                Part::Call(ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: Ok(json!({ "path": "a" })),
                }),
            ]),
            Item::ToolResults(vec![ToolResult {
                provenance: None,
                id: "call_1".into(),
                content: "the file".into(),
                is_error: false,
                images: vec![],
                diff: None,
            }]),
        ];
        let conv = Conversation {
            system: "be brief",
            tools: &[],
            items: &items,
            session: "",
        };
        let sent_body = body(&config(), &conv);
        assert_eq!(sent_body["instructions"], "be brief");
        assert_eq!(sent_body["store"], false);
        assert_eq!(sent_body["stream"], true);
        let sent = sent_body["input"].as_array().unwrap();
        assert_eq!(sent[0]["role"], "user");
        assert_eq!(sent[0]["content"][0]["type"], "input_text");
        assert_eq!(sent[1]["role"], "assistant");
        assert_eq!(sent[1]["content"][0]["type"], "output_text");
        assert_eq!(sent[2]["type"], "function_call");
        assert_eq!(sent[2]["call_id"], "call_1");
        assert_eq!(sent[2]["arguments"], r#"{"path":"a"}"#);
        assert_eq!(sent[3]["type"], "function_call_output");
        assert_eq!(sent[3]["call_id"], "call_1");
        assert_eq!(sent[3]["output"], "the file");
        assert!(sent_body.get("tools").is_none());
        let tools = vec![ToolDef {
            name: "read".into(),
            description: "reads".into(),
            schema: json!({ "type": "object" }),
        }];
        let body = body(
            &config(),
            &Conversation {
                system: "s",
                tools: &tools,
                items: &items,
                session: "",
            },
        );
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }

    #[test]
    fn tool_schemas_keep_optional_arguments_optional_on_responses() {
        let schemas = [
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "background": {"type": "boolean"},
                    "action": {"type": "string", "enum": ["run", "status", "list", "cancel"]},
                    "task_id": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1}
                },
                "additionalProperties": false
            }),
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "options": {
                        "type": "object",
                        "properties": {"limit": {"type": "integer"}}
                    }
                },
                "required": ["path"]
            }),
        ];
        let tools: Vec<ToolDef> = ["bash", "plugin_tool"]
            .into_iter()
            .zip(&schemas)
            .map(|(name, schema)| ToolDef {
                name: name.into(),
                description: "test tool".into(),
                schema: schema.clone(),
            })
            .collect();
        let sent = body(
            &config(),
            &Conversation {
                system: "s",
                tools: &tools,
                items: &[],
                session: "",
            },
        );
        for (tool, schema) in sent["tools"].as_array().unwrap().iter().zip(&schemas) {
            assert_eq!(tool["strict"], false);
            assert_eq!(&tool["parameters"], schema);
        }
    }

    #[test]
    fn a_streamed_turn_becomes_text_and_tool_calls() {
        let events = Script(vec![
            (
                "response.created",
                r#"{"type":"response.created","response":{"model":"m"}}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"Hello "}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"there"}"#,
            ),
            (
                "response.output_item.added",
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"bash","arguments":""}}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"command\":"}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"\"ls\"}"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"bash","arguments":"{\"command\":\"ls\"}"}}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"model":"gpt-test","status":"completed","usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":40}}}}"#,
            ),
        ]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.model, "gpt-test");
        assert_eq!(reply.usage.input, 60, "cached tokens are not input");
        assert_eq!(reply.usage.cache_read, 40);
        assert_eq!(reply.usage.output, 7);
        let texts: Vec<&str> = reply
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["Hello there"]);
        let calls: Vec<&ToolCall> = reply.calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_9");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].input.as_ref().unwrap()["command"], "ls");
        assert!(reply.parts.iter().any(|p| matches!(
            p,
            Part::Native {
                api: Api::OpenaiResponses,
                ..
            }
        )));
    }

    #[test]
    fn a_plain_answer_ends_the_turn() {
        let events = Script(vec![
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"just words"}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"model":"m","status":"completed","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ),
        ]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(reply.stop, Stop::EndTurn);
        assert_eq!(reply.usage.input, 5);
        assert_eq!(reply.calls().count(), 0);
    }

    #[test]
    fn a_failure_is_reported_rather_than_an_empty_reply() {
        let events = Script(vec![(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"half"}"#,
        )]);
        let mut sink = Silent;
        assert!(read_stream(events, &mut sink).is_err());

        let events = Script(vec![(
            "response.failed",
            r#"{"type":"response.failed","response":{"error":{"message":"quota exhausted"}}}"#,
        )]);
        let err = read_stream(events, &mut sink).unwrap_err().to_string();
        assert!(err.contains("quota exhausted"), "{err}");
    }

    #[test]
    fn a_truncated_answer_says_why() {
        let events = Script(vec![(
            "response.incomplete",
            r#"{"type":"response.incomplete","response":{"model":"m","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":1,"output_tokens":9}}}"#,
        )]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(reply.stop, Stop::MaxTokens);
        assert_eq!(reply.stop_raw, "max_output_tokens");
    }

    #[test]
    fn reasoning_summary_parts_are_separated_without_changing_native_replay() {
        let events = Script(vec![
            (
                "response.reasoning_summary_part.added",
                r#"{"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":0}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"***Testing "}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"the library***"}"#,
            ),
            (
                "response.reasoning_summary_text.done",
                r#"{"type":"response.reasoning_summary_text.done","output_index":0,"summary_index":0}"#,
            ),
            (
                "response.reasoning_summary_part.done",
                r#"{"type":"response.reasoning_summary_part.done","output_index":0,"summary_index":0}"#,
            ),
            (
                "response.reasoning_summary_part.added",
                r#"{"type":"response.reasoning_summary_part.added","output_index":0,"summary_index":1}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"***Reviewing results***"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"***Testing the library***"},{"type":"summary_text","text":"***Reviewing results***"}],"encrypted_content":"opaque","extra":"keep"}}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"status":"completed"}}"#,
            ),
        ]);
        let mut sink = Capture::default();
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(
            sink.reasoning,
            "***Testing the library***\n\n***Reviewing results***"
        );
        let original = json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"***Testing the library***"},{"type":"summary_text","text":"***Reviewing results***"}],"encrypted_content":"opaque","extra":"keep"});
        assert_eq!(
            reply.parts,
            [Part::Native {
                api: Api::OpenaiResponses,
                block: original.clone()
            }]
        );
        assert_eq!(input(&[Item::Assistant(reply.parts)])[0], original);
    }

    #[test]
    fn reasoning_indices_delimit_parts_and_items_not_individual_deltas() {
        let events = Script(vec![
            (
                "response.reasoning_summary_text.delta",
                r#"{"output_index":0,"summary_index":0,"delta":"first "}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"output_index":0,"summary_index":0,"delta":"entry"}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"output_index":0,"summary_index":1,"delta":"second entry"}"#,
            ),
            (
                "response.reasoning_summary_text.delta",
                r#"{"output_index":1,"summary_index":0,"delta":"third entry"}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"status":"completed"}}"#,
            ),
        ]);
        let mut sink = Capture::default();
        let reply = read_stream(events, &mut sink).unwrap();
        let expected = "first entry\n\nsecond entry\n\nthird entry";
        assert_eq!(sink.reasoning, expected);
        assert_eq!(
            reply.parts,
            [Part::Native {
                api: Api::OpenaiResponses,
                block: json!({"type":"reasoning","summary":[{"type":"summary_text","text":expected}]})
            }]
        );
    }

    #[test]
    fn completed_unindexed_reasoning_items_and_raw_content_keep_their_boundaries() {
        let events = Script(vec![
            (
                "response.reasoning_text.delta",
                r#"{"content_index":0,"delta":"raw "}"#,
            ),
            (
                "response.reasoning_text.delta",
                r#"{"content_index":0,"delta":"entry"}"#,
            ),
            (
                "response.reasoning_text.delta",
                r#"{"content_index":1,"delta":"next content"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"item":{"type":"reasoning","summary":[{"text":"raw entry"},{"text":"next content"}]}}"#,
            ),
            (
                "response.reasoning_text.delta",
                r#"{"content_index":1,"delta":"next item"}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"status":"completed"}}"#,
            ),
        ]);
        let mut sink = Capture::default();
        read_stream(events, &mut sink).unwrap();
        assert_eq!(sink.reasoning, "raw entry\n\nnext content\n\nnext item");
    }

    #[test]
    fn unindexed_reasoning_boundaries_do_not_add_leading_trailing_or_duplicate_gaps() {
        for (first, second) in [
            (r#"{"delta":"first\n"}"#, r#"{"delta":"second"}"#),
            (r#"{"delta":"first"}"#, r#"{"delta":"\n\nsecond"}"#),
            (r#"{"delta":"first\n"}"#, r#"{"delta":"\nsecond"}"#),
            (r#"{"delta":"first\n\n"}"#, r#"{"delta":"second"}"#),
        ] {
            let events = Script(vec![
                ("response.reasoning_summary_part.added", "{}"),
                ("response.reasoning_summary_text.delta", r#"{"delta":""}"#),
                ("response.reasoning_summary_text.delta", first),
                ("response.reasoning_summary_text.done", "{}"),
                ("response.reasoning_summary_part.done", "{}"),
                ("response.reasoning_summary_part.added", "{}"),
                ("response.reasoning_summary_text.delta", second),
                ("response.reasoning_summary_text.done", "{}"),
                (
                    "response.completed",
                    r#"{"response":{"status":"completed"}}"#,
                ),
            ]);
            let mut sink = Capture::default();
            read_stream(events, &mut sink).unwrap();
            assert_eq!(sink.reasoning, "first\n\nsecond");
        }
    }

    #[test]
    fn a_reasoning_item_is_carried_back_exactly_as_it_arrived() {
        let events = Script(vec![
            (
                "response.reasoning_summary_text.delta",
                r#"{"type":"response.reasoning_summary_text.delta","delta":"weighing it up"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"weighing it up"}],"encrypted_content":"gAAAA-opaque"}}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"type":"response.output_text.delta","delta":"done"}"#,
            ),
            (
                "response.completed",
                r#"{"type":"response.completed","response":{"model":"m","status":"completed","usage":{"input_tokens":5,"output_tokens":2}}}"#,
            ),
        ]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        let kept: Vec<&Value> = reply
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Native {
                    api: Api::OpenaiResponses,
                    block,
                } => Some(block),
                _ => None,
            })
            .collect();
        assert_eq!(
            kept.len(),
            1,
            "the item, not the item and a rebuilt one: {kept:?}"
        );
        assert_eq!(kept[0]["id"], "rs_1");
        assert_eq!(kept[0]["encrypted_content"], "gAAAA-opaque");

        let items = vec![
            Item::User("first".into()),
            Item::Assistant(reply.parts.clone()),
            Item::User("second".into()),
        ];
        let sent = input(&items);
        assert_eq!(sent[1]["type"], "reasoning");
        assert_eq!(sent[1]["id"], "rs_1");
        assert_eq!(sent[1]["encrypted_content"], "gAAAA-opaque");
    }

    #[test]
    fn a_summary_is_asked_for_and_the_legacy_include_is_not_sent() {
        let items = vec![Item::User("hello".into())];
        let conv = Conversation {
            system: "s",
            tools: &[],
            items: &items,
            session: "",
        };
        let plain = body(&config(), &conv);
        assert_eq!(plain["reasoning"]["summary"], "auto");
        assert!(
            plain["reasoning"].get("effort").is_none(),
            "default sends no effort"
        );
        assert!(
            plain.get("include").is_none(),
            "encrypted reasoning arrives unasked for in stateless mode"
        );

        let mut cfg = config();
        cfg.effort = Some("high".into());
        let with_effort = body(&cfg, &conv);
        assert_eq!(with_effort["reasoning"]["effort"], "high");
        assert_eq!(with_effort["reasoning"]["summary"], "auto");
        assert!(with_effort["reasoning"].get("context").is_none());
    }

    #[test]
    fn a_cache_write_and_the_reasoning_the_api_dropped_are_read() {
        let events = Script(vec![(
            "response.completed",
            r#"{"type":"response.completed","response":{"model":"m","status":"completed","usage":{"input_tokens":900,"output_tokens":7,"input_tokens_details":{"cached_tokens":100,"cache_write_tokens":800}},"input_transformations":[{"item_type":"reasoning","reason":"prefix_mismatch"}]}}"#,
        )]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(reply.usage.cache_read, 100);
        assert_eq!(reply.usage.cache_write, 800);
        assert_eq!(reply.dropped_reasoning, 1);
    }

    #[test]
    fn the_part_of_the_output_that_was_thinking_is_read() {
        let events = Script(vec![(
            "response.completed",
            r#"{"type":"response.completed","response":{"model":"m","status":"completed","usage":{"input_tokens":5,"output_tokens":900,"output_tokens_details":{"reasoning_tokens":850}}}}"#,
        )]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(reply.reasoning_tokens, 850);
        assert_eq!(reply.usage.output, 900);
    }

    #[test]
    fn nothing_dropped_reads_as_zero() {
        let events = Script(vec![(
            "response.completed",
            r#"{"type":"response.completed","response":{"model":"m","status":"completed","usage":{"input_tokens":5,"output_tokens":2}}}"#,
        )]);
        let mut sink = Silent;
        let reply = read_stream(events, &mut sink).unwrap();
        assert_eq!(reply.dropped_reasoning, 0);
        assert_eq!(reply.usage.cache_write, 0);
    }
}
