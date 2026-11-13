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
                // D97: a write is billed above the ordinary input rate, so leaving it out prices a
                // cold turn as if it were a plain read (D36).
                usage.cache_write = u["input_tokens_details"]["cache_write_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.output = u["output_tokens"].as_u64().unwrap_or(usage.output);
                dropped_reasoning = dropped(&response["input_transformations"]);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Auth, ProviderConfig, ToolDef, ToolResult};

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
            auth: Auth::ApiKey,
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
        let tools = vec![ToolDef {
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
