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

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{Api, Conversation, Item, Part, Provider, Reply, Sink, Stop, ToolCall, Usage, http};
use super::{Cancelled, SUMMARY_INTRO, sse};

pub fn complete(p: &Provider, conv: &Conversation<'_>, sink: &mut dyn Sink) -> Result<Reply> {
    let url = format!("{}/responses", p.config.base_url.trim_end_matches('/'));
    let headers = p.request_headers();
    let events = http::stream(p, url, headers, &body(&p.config, conv))?;
    read_stream(events, sink)
}

pub fn body(cfg: &super::ProviderConfig, conv: &Conversation<'_>) -> Value {
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
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if let Some(n) = cfg.max_tokens {
        body["max_output_tokens"] = n.into();
    }
    if let Some(effort) = &cfg.effort {
        body["reasoning"] = json!({ "effort": effort });
        // Without this the reasoning that came back cannot be sent back, and a thinking model
        // loses the thread between tool calls.
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    body
}

/// The conversation as Responses items.
pub fn input(items: &[Item]) -> Vec<Value> {
    let current_turn = items.iter().rposition(|i| i.text().is_some()).unwrap_or(0);
    let mut out = Vec::new();
    for (index, item) in items.iter().enumerate() {
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
                        // Reasoning only goes back for the replies since the last user message, so a
                        // tool loop continues its own thinking and older turns do not pay for it.
                        Part::Native { api, block }
                            if *api == Api::OpenaiResponses && index >= current_turn =>
                        {
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
    let mut calls: BTreeMap<u64, Pending> = BTreeMap::new();
    // The event stream identifies a call by its output index and by `item_id`; both are kept so a
    // delta lands on the right one whichever the server sends.
    let mut by_item: BTreeMap<String, u64> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut model = String::new();
    let mut status = String::new();
    let mut incomplete = String::new();
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
            "response.completed" | "response.incomplete" => {
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
                usage.output = u["output_tokens"].as_u64().unwrap_or(usage.output);
                finished = true;
            }
            "response.failed" => {
                let error = &event["response"]["error"];
                bail!("{}", message_of(error));
            }
            "error" => bail!("{}", message_of(&event)),
            _ => {}
        }
    }
    if !finished {
        bail!("the reply stream ended before the response was completed");
    }

    let mut parts = Vec::new();
    if !reasoning.is_empty() {
        parts.push(Part::Native {
            api: Api::OpenaiResponses,
            block: json!({ "type": "reasoning", "summary": [{ "type": "summary_text", "text": reasoning }] }),
        });
    }
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
    })
}

fn message_of(error: &Value) -> String {
    error["message"]
        .as_str()
        .or_else(|| error["error"]["message"].as_str())
        .map_or_else(|| error.to_string(), str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderConfig, ToolResult};

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
            auth: crate::provider::Auth::ApiKey,
            oauth: None,
        }
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
        let tools = vec![crate::provider::ToolDef {
            name: "read",
            description: "reads",
            schema: json!({ "type": "object" }),
        }];
        let body = body(
            &config(),
            &Conversation {
                system: "s",
                tools: &tools,
                items: &items,
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
}
