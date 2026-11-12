//! OpenAI-compatible chat completions, streamed.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{
    Api, Conversation, Item, Part, Provider, ProviderConfig, Reply, Sink, Stop, ToolCall, Usage,
    http,
};
use super::{Cancelled, SUMMARY_INTRO, sse};

pub fn complete(p: &Provider, conv: &Conversation<'_>, sink: &mut dyn Sink) -> Result<Reply> {
    let url = format!("{}/chat/completions", p.config.base_url);
    let mut headers = Vec::new();
    if let Some(key) = &p.key {
        headers.push(("authorization", format!("Bearer {key}")));
    }
    let events = http::stream(p, url, headers, &body(&p.config, conv))?;
    read_stream(events, sink)
}

pub fn body(cfg: &ProviderConfig, conv: &Conversation<'_>) -> Value {
    let tools: Vec<Value> = conv
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": { "name": t.name, "description": t.description, "parameters": t.schema },
            })
        })
        .collect();
    let mut body = json!({
        "model": cfg.model,
        "stream": true,
        "stream_options": { "include_usage": true },
        "messages": messages(conv.system, conv.items),
    });
    // An empty list is left out: some servers reject `"tools": []`.
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if let Some(n) = cfg.max_tokens {
        body["max_tokens"] = n.into();
    }
    if let Some(effort) = &cfg.effort {
        body["reasoning_effort"] = effort.as_str().into();
    }
    body
}

/// Renders the conversation. Reasoning goes back as `reasoning_content` only for the replies since
/// the last user message, where a thinking model continues its own tool loop; older reasoning is
/// dropped to save context.
pub fn messages(system: &str, items: &[Item]) -> Vec<Value> {
    let mut out = vec![json!({ "role": "system", "content": system })];
    let current_turn = items.iter().rposition(|i| i.text().is_some()).unwrap_or(0);
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::User(text) => out.push(json!({ "role": "user", "content": text })),
            // OpenAI carries an image as a second content part with a data URL, which is the only
            // shape it accepts for an inline picture (D58).
            Item::Attached { text, images } => {
                let mut content = vec![json!({ "type": "text", "text": text })];
                content.extend(images.iter().map(|i| {
                    json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{};base64,{}", i.media_type, i.data) },
                    })
                }));
                out.push(json!({ "role": "user", "content": content }));
            }
            Item::Summary(text) => out.push(json!({
                "role": "user",
                "content": format!("{SUMMARY_INTRO}{text}"),
            })),
            // A Chat Completions tool message has no image field, so a picture a tool returned
            // travels in the user message after it, which is what pi does and what the API
            // accepts (D58).
            Item::ToolResults(results) => {
                let mut images: Vec<Value> = Vec::new();
                for r in results {
                    // The tool message needs text; a picture with no words gets a placeholder so
                    // the model knows the result was not empty.
                    let content = if r.content.trim().is_empty() && !r.images.is_empty() {
                        "(see the attached image)".to_owned()
                    } else {
                        r.content.clone()
                    };
                    out.push(json!({ "role": "tool", "tool_call_id": r.id, "content": content }));
                    for image in &r.images {
                        images.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", image.media_type, image.data),
                            },
                        }));
                    }
                }
                if !images.is_empty() {
                    let mut content = vec![json!({
                        "type": "text",
                        "text": "Image(s) from the tool result:",
                    })];
                    content.extend(images);
                    out.push(json!({ "role": "user", "content": content }));
                }
            }
            Item::Assistant(parts) => {
                let text: String = parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect();
                let calls: Vec<Value> = parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Call(c) => Some(json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                // Servers parse this, so an unparseable input goes back as `{}`.
                                "arguments": c.input.as_ref().map_or_else(|_| "{}".to_owned(), Value::to_string),
                            },
                        })),
                        _ => None,
                    })
                    .collect();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut msg = json!({
                    "role": "assistant",
                    "content": if text.is_empty() { Value::Null } else { text.into() },
                });
                if !calls.is_empty() {
                    msg["tool_calls"] = calls.into();
                }
                if index > current_turn {
                    let reasoning: String = parts
                        .iter()
                        .filter_map(|p| match p {
                            Part::Native {
                                api: Api::Openai,
                                block,
                            } => block["reasoning_content"].as_str(),
                            _ => None,
                        })
                        .collect();
                    if !reasoning.is_empty() {
                        msg["reasoning_content"] = reasoning.into();
                    }
                }
                out.push(msg);
            }
        }
    }
    out
}

#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

pub fn read_stream(mut events: impl sse::Events, sink: &mut dyn Sink) -> Result<Reply> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<u64, PendingCall> = BTreeMap::new();
    let mut finish = String::new();
    let mut usage = Usage::default();
    let mut model = String::new();
    let mut done = false;

    while let Some(ev) = events.next(sink)? {
        if sink.cancelled() {
            return Err(Cancelled.into());
        }
        if ev.data.trim() == "[DONE]" {
            done = true;
            break;
        }
        let chunk: Value = serde_json::from_str(&ev.data)
            .with_context(|| format!("unreadable stream chunk: {}", ev.data))?;
        if let Some(err) = chunk.get("error") {
            bail!(
                "{}",
                err["message"]
                    .as_str()
                    .map_or_else(|| err.to_string(), str::to_owned)
            );
        }
        if model.is_empty() {
            model = chunk["model"].as_str().unwrap_or_default().to_owned();
        }
        if chunk["usage"].is_object() {
            // prompt_tokens includes the cached part here, unlike Anthropic's input_tokens.
            // `input` is made to mean the uncached part for both, so the parts add up.
            let u = &chunk["usage"];
            let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
            let cached = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .or_else(|| u["prompt_cache_hit_tokens"].as_u64())
                .unwrap_or(0);
            usage.input = prompt.saturating_sub(cached);
            usage.cache_read = cached;
            usage.output = u["completion_tokens"].as_u64().unwrap_or(usage.output);
        }
        let Some(choice) = chunk["choices"].get(0) else {
            continue;
        };
        let delta = &choice["delta"];
        // Servers name it either way (vLLM, DeepSeek: reasoning_content; others: reasoning).
        if let Some(r) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            sink.reasoning(r);
            reasoning.push_str(r);
        }
        if let Some(s) = delta["content"].as_str() {
            sink.text(s);
            text.push_str(s);
        }
        for tc in delta["tool_calls"].as_array().into_iter().flatten() {
            let pending = calls.entry(tc["index"].as_u64().unwrap_or(0)).or_default();
            if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
                pending.id = id.to_owned();
            }
            if let Some(name) = tc["function"]["name"].as_str().filter(|s| !s.is_empty())
                && pending.name.is_empty()
            {
                pending.name = name.to_owned();
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                pending.arguments.push_str(args);
            }
        }
        if let Some(f) = choice["finish_reason"].as_str() {
            finish = f.to_owned();
        }
    }
    if !done && finish.is_empty() {
        bail!("the reply stream ended without a finish reason");
    }

    let mut parts = Vec::new();
    if !reasoning.is_empty() {
        parts.push(Part::Native {
            api: Api::Openai,
            block: json!({ "reasoning_content": reasoning }),
        });
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    for (index, c) in calls {
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
    let stop = match finish.as_str() {
        "stop" => Stop::EndTurn,
        "tool_calls" | "function_call" => Stop::ToolUse,
        "length" => Stop::MaxTokens,
        "content_filter" => Stop::Refusal,
        _ => Stop::Other,
    };
    Ok(Reply {
        parts,
        stop,
        stop_raw: finish,
        model,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Api, ToolResult};

    #[test]
    fn an_attached_image_is_sent_as_a_data_url_beside_its_text() {
        let image = crate::provider::Image {
            media_type: "image/jpeg".into(),
            data: "BBBB".into(),
            name: "cat.jpg".into(),
            width: 10,
            height: 20,
        };
        let items = [Item::Attached {
            text: "describe".into(),
            images: vec![image],
        }];
        let msgs = messages("sys", &items);
        let user = &msgs[1];
        assert_eq!(user["role"], "user");
        let content = user["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        // OpenAI takes an image as a second part with a data URL (D58).
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"],
            "data:image/jpeg;base64,BBBB"
        );
        // A prompt with pictures still counts as the current turn, so the reasoning that goes with
        // this API stays attached to the right message.
        assert!(msgs.iter().any(|m| m["role"] == "user"));
    }

    #[test]
    fn a_tool_result_image_travels_in_the_user_message_after_it() {
        // A Chat Completions tool message has no image field, so the picture follows it as a user
        // message, which is what the API accepts (D58).
        let image = crate::provider::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            name: "shot.png".into(),
            width: 2,
            height: 3,
        };
        let items = [Item::ToolResults(vec![ToolResult {
            id: "c1".into(),
            content: "the screenshot".into(),
            is_error: false,
            diff: None,
            images: vec![image],
        }])];
        let msgs = messages("sys", &items);
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["content"], "the screenshot");
        assert_eq!(tool["tool_call_id"], "c1");
        let after = msgs.iter().find(|m| m["role"] == "user").unwrap();
        let content = after["content"].as_array().unwrap();
        assert!(content[0]["text"].as_str().unwrap().contains("tool result"));
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn a_picture_with_no_words_still_gets_a_tool_message() {
        let image = crate::provider::Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            name: "shot.png".into(),
            width: 1,
            height: 1,
        };
        let items = [Item::ToolResults(vec![ToolResult {
            id: "c1".into(),
            content: "  ".into(),
            is_error: false,
            diff: None,
            images: vec![image],
        }])];
        let msgs = messages("sys", &items);
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        // The API requires content on a tool message, so a placeholder says the result was not
        // empty (D58).
        assert!(tool["content"].as_str().unwrap().contains("attached image"));
    }

    struct Collect(String);
    impl Sink for Collect {
        fn text(&mut self, s: &str) {
            self.0.push_str(s)
        }
        fn note(&mut self, _: &str) {}
    }

    fn sse(chunks: &[Value]) -> String {
        let mut s: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
        s.push_str("data: [DONE]\n\n");
        s
    }

    #[test]
    fn stream_with_text_and_tool_calls_split_across_chunks() {
        let stream = sse(&[
            json!({"model":"deepseek-chat","choices":[{"index":0,"delta":{"role":"assistant","content":"Look"}}]}),
            json!({"choices":[{"index":0,"delta":{"content":"ing."}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":""}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":" \"a.txt\"}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"name":"bash","arguments":"{\"command\": \"ls\"}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":30,"completion_tokens":9,"prompt_cache_hit_tokens":20}}),
        ]);
        let mut sink = Collect(String::new());
        let reply = read_stream(sse::Reader::new(stream.as_bytes()), &mut sink).unwrap();
        assert_eq!(sink.0, "Looking.");
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.model, "deepseek-chat");
        assert_eq!(
            reply.usage,
            Usage {
                input: 10,
                output: 9,
                cache_read: 20,
                cache_write: 0
            }
        );
        assert_eq!(
            reply.parts,
            vec![
                Part::Text("Looking.".into()),
                Part::Call(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: Ok(json!({"path":"a.txt"}))
                }),
                Part::Call(ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: Ok(json!({"command":"ls"}))
                }),
            ]
        );
    }

    #[test]
    fn reasoning_is_kept_and_sent_back_only_within_the_turn() {
        let stream = sse(&[
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"think "}}]}),
            json!({"choices":[{"index":0,"delta":{"reasoning":"more"}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        let reply = read_stream(
            sse::Reader::new(stream.as_bytes()),
            &mut Collect(String::new()),
        )
        .unwrap();
        let thinking = Part::Native {
            api: Api::Openai,
            block: json!({"reasoning_content": "think more"}),
        };
        assert_eq!(reply.parts[0], thinking);

        let earlier = Item::Assistant(vec![thinking.clone(), Part::Text("old".into())]);
        let current = Item::Assistant(reply.parts.clone());
        let items = vec![
            Item::User("one".into()),
            earlier,
            Item::User("two".into()),
            current,
        ];
        let m = messages("s", &items);
        assert!(m[2].get("reasoning_content").is_none());
        assert_eq!(m[4]["reasoning_content"], "think more");
    }

    #[test]
    fn errors_and_truncated_streams_fail() {
        let s = "data: {\"error\":{\"message\":\"model not found\"}}\n\n";
        let e =
            read_stream(sse::Reader::new(s.as_bytes()), &mut Collect(String::new())).unwrap_err();
        assert_eq!(e.to_string(), "model not found");
        let s = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        assert!(read_stream(sse::Reader::new(s.as_bytes()), &mut Collect(String::new())).is_err());
    }

    #[test]
    fn history_renders_as_chat_messages() {
        let items = vec![
            Item::User("hi".into()),
            Item::Assistant(vec![
                Part::Native {
                    api: Api::Anthropic,
                    block: json!({"type":"thinking"}),
                },
                Part::Call(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: Ok(json!({"path":"a"})),
                }),
                Part::Call(ToolCall {
                    id: "c2".into(),
                    name: "bash".into(),
                    input: Err("{bad".into()),
                }),
            ]),
            Item::ToolResults(vec![
                ToolResult {
                    id: "c1".into(),
                    content: "text".into(),
                    images: Vec::new(),
                    is_error: false,
                    diff: None,
                },
                ToolResult {
                    id: "c2".into(),
                    content: "bad input".into(),
                    images: Vec::new(),
                    is_error: true,
                    diff: None,
                },
            ]),
        ];
        let m = messages("sys", &items);
        assert_eq!(m.len(), 5);
        assert_eq!(m[0], json!({"role":"system","content":"sys"}));
        assert_eq!(m[2]["content"], Value::Null);
        assert_eq!(
            m[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(m[2]["tool_calls"][1]["function"]["arguments"], "{}");
        assert_eq!(
            m[4],
            json!({"role":"tool","tool_call_id":"c2","content":"bad input"})
        );
    }
}
