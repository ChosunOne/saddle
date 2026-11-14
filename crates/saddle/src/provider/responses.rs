//! OpenAI's Responses API, streamed: the wire format a ChatGPT subscription token reaches (D85).
//!
//! It differs from `/chat/completions` in more than the path. The conversation goes in as a flat
//! list of typed *items* rather than role-tagged messages with tool calls hanging off them, the
//! system prompt is a field of its own rather than the first message, tool calls and their results
//! refer to each other by `call_id` rather than by a message index, and the stream is a sequence of
//! named events rather than deltas on a choice. That is why it is a module and not a flag.
//!
//! Only what saddle sends is understood: text, images, function calls and their outputs, and the
//! reasoning summaries a subscription model emits. Anything else the server sends is ignored rather
//! than refused, because the event vocabulary here is larger than any one turn uses and grows.

use std::collections::BTreeMap;

use anyhow::{Context, Error as AnyError, Result, anyhow};
use serde_json::{Value, json};

use super::retry::{self, Transient};
use super::{
    Api, Conversation, Item, Part, Provider, ProviderConfig, Reply, Sink, Stop, ToolCall, Usage,
    http, prompt_cache_key, prompt_cache_retention, ws,
};
use super::{Cancelled, SUMMARY_INTRO, sse};

/// The Codex CLI version this client reports when it asks what the backend serves (D85).
///
/// The catalogue is gated on it: the backend answers a version it does not recognise with an empty
/// list and no error, and an older one with a shorter list, so a stale number here is a picker that
/// quietly loses models. It is a version and not a list of models on purpose — the list is the
/// service's to change, and this is the only claim saddle makes about being a client of it.
///
/// Bump it when the list comes back empty or short. `CODEX_CLIENT_VERSION` is checked against
/// `npm view @openai/codex version`, and there is a test below that notices when it is old.
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
    // D125: a successful send does not prove the socket is alive. Recover read-time failures too,
    // but only before output. The second attempt always uses HTTP and the whole input, not a delta.
    retry::once(sink, |retry, sink| {
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
            // The next request's input shape, not the server's response.output shape (D101).
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
            })
        })
        .collect();
    let mut body = json!({
        "model": cfg.model,
        "instructions": conv.system,
        "input": input(conv.items),
        "stream": true,
        // The subscription endpoint keeps no conversation of its own; every request carries it.
        "store": false,
    });
    // D100: the same key and lifetime the chat-completions surface sends, under this API's own
    // names. Neither goes to a server that is not OpenAI's.
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
    // D95: a summary is only emitted when it is asked for, so without this key the TUI has nothing
    // to show even though the model reasoned. `auto` is the most detailed summarizer a model has;
    // the named ones differ by model and `concise` is refused outright by some of them.
    //
    // `reasoning.context` is deliberately left out, which means `auto`: the model's own default.
    // Naming `all_turns` would be refused by models that do not have it, and the client does not
    // need it — every reasoning item is replayed below, so the prefix is the same either way.
    let mut reasoning = json!({ "summary": "auto" });
    if let Some(effort) = &cfg.effort {
        reasoning["effort"] = effort.as_str().into();
    }
    body["reasoning"] = reasoning;
    // The encrypted reasoning comes back by itself in stateless mode, so the legacy
    // `reasoning.encrypted_content` include is not sent: it buys nothing and a strict server may
    // refuse a value it does not know.
    body
}

/// The conversation as Responses items.
pub fn input(items: &[Item]) -> Vec<Value> {
    let mut out = Vec::new();
    for item in items {
        match item {
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
                // A tool's picture cannot ride in its output, so it follows as a user message, as
                // it does for chat completions (D58).
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
                            // The server parses this, so an input that could not be parsed on the
                            // way in goes back as `{}`.
                            "arguments": c.input.as_ref().map_or_else(|_| "{}".to_owned(), Value::to_string),
                        })),
                        // D95: every reasoning item goes back, in the position it was produced, for
                        // the whole conversation. Deleting an earlier one is a change to the prefix:
                        // it stops the prompt cache matching from that point, and the API answers
                        // by dropping the reasoning that follows it as well.
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

fn image_part(image: &super::Image) -> Value {
    json!({
        "type": "input_image",
        "image_url": format!("data:{};base64,{}", image.media_type, image.data),
    })
}

fn message(role: &str, content: &[Value]) -> Value {
    json!({ "role": role, "content": content })
}

/// One tool call while its arguments stream in.
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
    // D95: the reasoning items exactly as the server sent them. They carry an opaque
    // `encrypted_content` and an id, and only the server can read either: what comes back here must
    // go back out unchanged, or the model starts the next step without the thinking it already did.
    let mut reasoning_items: Vec<Value> = Vec::new();
    let mut calls: BTreeMap<u64, Pending> = BTreeMap::new();
    // The event stream identifies a call by its output index and by `item_id`; both are kept so a
    // delta lands on the right one whichever the server sends.
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
                if let Some(d) = event["delta"].as_str() {
                    sink.reasoning(d);
                    reasoning.push_str(d);
                }
            }
            "response.output_item.added" => {
                let item = &event["item"];
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
                    // Verbatim: id, summary, encrypted_content and whatever else the server put
                    // there. This is the thing D95 exists for.
                    reasoning_items.push(item.clone());
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
                        // `.done` carries the whole argument string; the deltas before it were the
                        // same bytes arriving in pieces.
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
                // D97: a write is billed above the ordinary input rate, so leaving it out prices a
                // cold turn as if it were a plain read (D36).
                usage.cache_write = u["input_tokens_details"]["cache_write_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.output = u["output_tokens"].as_u64().unwrap_or(usage.output);
                // D99: the same split, under this API's name for it.
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
            Transient::new("the reply stream ended before the response was completed").into(),
        );
    }

    let mut parts = Vec::new();
    if reasoning_items.is_empty() && !reasoning.is_empty() {
        // A server that streamed a summary without sending an item of its own: the text is still
        // kept, so the transcript and the display have it.
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

/// How many input items the API said it removed before answering (D97).
///
/// The field is reported but not documented field by field, so an entry that does not say what it
/// touched counts as a removal: a drop that was silently ignored because the schema moved is the
/// one thing this number must never do.
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
        Transient::new(message).with_retry_after(retry_after).into()
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

    /// a real upgrade and HTTP fallback on one endpoint, with no external credentials or requests.
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
                // Deliberately drop without a close handshake, including after a successful send.
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
        // D100: the same two fields under this surface's own names. Its configured host is the
        // subscription backend, not `api.openai.com`, so the default configuration carries neither.
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
        // The Responses API has no tool messages: a call and its output are items that name each
        // other by call_id, and the system prompt is a field rather than the first message.
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
        // No tools means no `tools` key, and a tool has its own shape here.
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
        // Reasoning comes back as an item of its own, so the next request can carry it.
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
        // A stream that stops before `response.completed` is a broken reply, not a short one.
        let events = Script(vec![(
            "response.output_text.delta",
            r#"{"type":"response.output_text.delta","delta":"half"}"#,
        )]);
        let mut sink = Silent;
        assert!(read_stream(events, &mut sink).is_err());

        // And a failure the server states is reported in its own words.
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
    fn a_reasoning_item_is_carried_back_exactly_as_it_arrived() {
        // D95: the item carries an opaque `encrypted_content` that only the server can read.
        // Rebuilding it from the summary text throws that away, and with `store: false` it is the
        // only continuity between one step of a tool loop and the next.
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

        // And it goes back out unchanged — in an earlier turn as much as this one.
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
        // D95: without `summary` the server answers with an empty summary array, so the interface
        // has nothing to show however much the model thought.
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
        // `context` is left to the model: naming `all_turns` is refused by models without it, and
        // every reasoning item is replayed anyway, so the prefix is the same either way.
        assert!(with_effort["reasoning"].get("context").is_none());
    }

    #[test]
    fn a_cache_write_and_the_reasoning_the_api_dropped_are_read() {
        // D97: a write is billed above the ordinary input rate, and a drop is the API telling us
        // it rewrote our prompt. Both are invisible in the token counts alone.
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
        // D99: the same split under this API's name for it.
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
