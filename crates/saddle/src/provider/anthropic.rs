use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{
    Api, Conversation, Item, Part, Provider, ProviderConfig, Reply, Sink, Stop, ToolCall,
    ToolResult, Usage, http,
};
use super::{Cancelled, SUMMARY_INTRO, sse};

pub const OFFICIAL_BASE_URL: &str = "https://api.anthropic.com";
pub(super) const VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const DEFAULT_MAX_TOKENS: u32 = 64_000;

fn official(cfg: &ProviderConfig) -> bool {
    cfg.base_url == OFFICIAL_BASE_URL
}

pub fn complete(p: &Provider, conv: &Conversation<'_>, sink: &mut dyn Sink) -> Result<Reply> {
    let url = format!("{}/v1/messages", p.config.base_url);
    let mut headers = vec![("anthropic-version".to_owned(), VERSION.to_owned())];
    headers.extend(p.request_headers());
    if official(&p.config) {
        headers.push(("anthropic-beta".to_owned(), FALLBACK_BETA.to_owned()));
    }
    let events = http::stream(p, url, headers, &body(&p.config, conv))?;
    read_stream(events, sink)
}

pub fn body(cfg: &ProviderConfig, conv: &Conversation<'_>) -> Value {
    let official = official(cfg);
    let tools: Vec<Value> = conv
        .tools
        .iter()
        .map(|t| {
            let mut tool = json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            });
            if official {
                tool["eager_input_streaming"] = true.into();
            }
            tool
        })
        .collect();
    let mut body = json!({
        "model": cfg.model,
        "max_tokens": cfg.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        "stream": true,
        "system": conv.system,
        "messages": messages(conv.items),
        "cache_control": { "type": "ephemeral" },
    });
    if !tools.is_empty() {
        body["tools"] = tools.into();
    }
    if let Some(effort) = &cfg.effort {
        body["output_config"] = json!({ "effort": effort });
    }
    if official {
        body["fallbacks"] = "default".into();
    }
    body
}

pub fn messages(items: &[Item]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for item in items {
        let (role, content): (&str, Vec<Value>) = match item {
            Item::Contribution(value) => {
                ("user", vec![json!({"type":"text","text":value.content})])
            }
            Item::User(text) => ("user", vec![json!({ "type": "text", "text": text })]),
            Item::Attached { text, images } => {
                let mut blocks = vec![json!({ "type": "text", "text": text })];
                blocks.extend(
                    images
                        .iter()
                        .filter_map(|i| render(&Part::Image(i.clone()))),
                );
                ("user", blocks)
            }
            Item::Summary(text) => (
                "user",
                vec![json!({ "type": "text", "text": format!("{SUMMARY_INTRO}{text}") })],
            ),
            Item::ToolResults(results) => {
                let mut blocks: Vec<Value> = Vec::new();
                let mut moved: Vec<Value> = Vec::new();
                for r in results {
                    let (content, carry) = tool_result_content(r);
                    blocks.push(json!({
                        "type": "tool_result",
                        "tool_use_id": r.id,
                        "content": content,
                        "is_error": r.is_error,
                    }));
                    moved.extend(carry);
                }
                blocks.extend(moved);
                ("user", blocks)
            }
            Item::Assistant(parts) => ("assistant", parts.iter().filter_map(render).collect()),
        };
        if content.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last["role"] == role => {
                if let Some(blocks) = last["content"].as_array_mut() {
                    blocks.extend(content);
                }
            }
            _ => out.push(json!({ "role": role, "content": content })),
        }
    }
    out
}

fn render(part: &Part) -> Option<Value> {
    match part {
        Part::Text(t) if t.is_empty() => None,
        Part::Text(t) => Some(json!({ "type": "text", "text": t })),
        Part::Call(c) => Some(json!({
            "type": "tool_use",
            "id": c.id,
            "name": c.name,
            "input": c.input.clone().unwrap_or_else(|_| json!({})),
        })),
        Part::Native {
            api: Api::Anthropic,
            block,
        } => Some(block.clone()),
        Part::Image(image) => Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": image.media_type,
                "data": image.data,
            },
        })),
        Part::Native { .. } => None,
    }
}

fn tool_result_content(r: &ToolResult) -> (Value, Vec<Value>) {
    if r.images.is_empty() {
        return (json!(r.content), Vec::new());
    }
    let images: Vec<Value> = r
        .images
        .iter()
        .filter_map(|i| render(&Part::Image(i.clone())))
        .collect();
    if r.is_error {
        let caption = format!(
            "image{} from the failed call {}:",
            if images.len() == 1 { "" } else { "s" },
            r.id
        );
        let mut carry = vec![json!({ "type": "text", "text": caption })];
        carry.extend(images);
        return (json!(r.content), carry);
    }
    let mut blocks = vec![json!({ "type": "text", "text": r.content })];
    blocks.extend(images);
    (json!(blocks), Vec::new())
}

struct Open {
    block: Value,
    partial_json: String,
}

pub fn read_stream(mut events: impl sse::Events, sink: &mut dyn Sink) -> Result<Reply> {
    let mut blocks: Vec<Option<Open>> = Vec::new();
    let mut usage = Usage::default();
    let mut model = String::new();
    let mut stop_raw = String::new();
    let mut finished = false;

    while let Some(ev) = events.next(sink)? {
        if sink.cancelled() {
            return Err(Cancelled.into());
        }
        let data: Value = serde_json::from_str(&ev.data)
            .with_context(|| format!("unreadable stream event: {}", ev.data))?;
        let kind = data["type"].as_str().unwrap_or(&ev.event);
        match kind {
            "message_start" => {
                let message = &data["message"];
                model = message["model"].as_str().unwrap_or_default().to_owned();
                read_usage(&message["usage"], &mut usage);
            }
            "content_block_start" => {
                let index = index(&data)?;
                let block = data["content_block"].clone();
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = block["text"].as_str().filter(|t| !t.is_empty()) {
                            sink.text(t);
                        }
                    }
                    Some("fallback") => sink.note(&format!(
                        "{} declined; {} continues",
                        block["from"]["model"].as_str().unwrap_or("the model"),
                        block["to"]["model"]
                            .as_str()
                            .unwrap_or("the fallback model"),
                    )),
                    _ => {}
                }
                if blocks.len() <= index {
                    blocks.resize_with(index + 1, || None);
                }
                blocks[index] = Some(Open {
                    block,
                    partial_json: String::new(),
                });
            }
            "content_block_delta" => {
                let index = index(&data)?;
                let open = blocks
                    .get_mut(index)
                    .and_then(Option::as_mut)
                    .with_context(|| format!("delta for unopened block {index}"))?;
                apply_delta(open, &data["delta"], sink);
            }
            "message_delta" => {
                if let Some(s) = data["delta"]["stop_reason"].as_str() {
                    stop_raw = s.to_owned();
                }
                read_usage(&data["usage"], &mut usage);
            }
            "message_stop" => {
                finished = true;
                break;
            }
            "error" => bail!(
                "{}: {}",
                data["error"]["type"].as_str().unwrap_or("error"),
                data["error"]["message"].as_str().unwrap_or("no message")
            ),
            _ => {}
        }
    }
    if !finished {
        bail!("the reply stream ended before message_stop");
    }

    let mut parts: Vec<Part> = blocks.into_iter().flatten().map(finish).collect();
    drop_before_fallback(&mut parts);
    let stop = match stop_raw.as_str() {
        "end_turn" | "stop_sequence" => Stop::EndTurn,
        "tool_use" => Stop::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => Stop::MaxTokens,
        "refusal" => Stop::Refusal,
        _ => Stop::Other,
    };
    Ok(Reply {
        parts,
        stop,
        dropped_reasoning: 0,
        reasoning_tokens: 0,
        stop_raw,
        model,
        usage,
    })
}

fn index(data: &Value) -> Result<usize> {
    data["index"]
        .as_u64()
        .map(|i| i as usize)
        .context("stream event without a block index")
}

fn read_usage(u: &Value, usage: &mut Usage) {
    let field = |name: &str| u[name].as_u64();
    if let Some(n) = field("input_tokens") {
        usage.input = n;
    }
    if let Some(n) = field("output_tokens") {
        usage.output = n;
    }
    if let Some(n) = field("cache_read_input_tokens") {
        usage.cache_read = n;
    }
    if let Some(n) = field("cache_creation_input_tokens") {
        usage.cache_write = n;
    }
}

fn apply_delta(open: &mut Open, delta: &Value, sink: &mut dyn Sink) {
    match delta["type"].as_str() {
        Some("text_delta") => {
            let t = delta["text"].as_str().unwrap_or_default();
            sink.text(t);
            append(&mut open.block, "text", t);
        }
        Some("thinking_delta") => {
            let t = delta["thinking"].as_str().unwrap_or_default();
            sink.reasoning(t);
            append(&mut open.block, "thinking", t);
        }
        Some("input_json_delta") => {
            open.partial_json
                .push_str(delta["partial_json"].as_str().unwrap_or_default());
        }
        Some("citations_delta") => {
            if !open.block["citations"].is_array() {
                open.block["citations"] = json!([]);
            }
            if let Some(list) = open.block["citations"].as_array_mut() {
                list.push(delta["citation"].clone());
            }
        }
        _ => {
            if let Some(fields) = delta.as_object() {
                for (k, v) in fields.iter().filter(|(k, _)| *k != "type") {
                    if let Some(s) = v.as_str() {
                        append(&mut open.block, k, s);
                    }
                }
            }
        }
    }
}

fn append(block: &mut Value, field: &str, s: &str) {
    let mut current = block[field].as_str().unwrap_or_default().to_owned();
    current.push_str(s);
    block[field] = current.into();
}

fn finish(open: Open) -> Part {
    let Open {
        block,
        partial_json,
    } = open;
    match block["type"].as_str() {
        Some("text") => Part::Text(block["text"].as_str().unwrap_or_default().to_owned()),
        Some("tool_use") => {
            let input = if partial_json.trim().is_empty() {
                Ok(match &block["input"] {
                    Value::Object(_) => block["input"].clone(),
                    _ => json!({}),
                })
            } else {
                match serde_json::from_str::<Value>(&partial_json) {
                    Ok(v) if v.is_object() => Ok(v),
                    _ => Err(partial_json),
                }
            };
            Part::Call(ToolCall {
                id: block["id"].as_str().unwrap_or_default().to_owned(),
                name: block["name"].as_str().unwrap_or_default().to_owned(),
                input,
            })
        }
        _ => Part::Native {
            api: Api::Anthropic,
            block,
        },
    }
}

fn is_fallback(part: &Part) -> bool {
    matches!(part, Part::Native { block, .. } if block["type"] == "fallback")
}

fn drop_before_fallback(parts: &mut Vec<Part>) {
    let Some(boundary) = parts.iter().rposition(is_fallback) else {
        return;
    };
    let mut i = 0;
    parts.retain(|p| {
        let keep = i >= boundary || matches!(p, Part::Text(_)) || is_fallback(p);
        i += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Auth;
    use crate::provider::Image;
    use crate::provider::ToolDef;
    use crate::provider::ToolResult;

    fn image(name: &str) -> Image {
        Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            name: name.into(),
            width: 2,
            height: 3,
        }
    }

    #[test]
    fn a_tool_result_carries_its_image_inside_itself() {
        let items = [Item::ToolResults(vec![ToolResult {
            provenance: None,
            id: "c1".into(),
            content: "here is the screenshot".into(),
            is_error: false,
            diff: None,
            images: vec![image("shot.png")],
        }])];
        let msgs = messages(&items);
        let blocks = msgs[0]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        let inner = blocks[0]["content"].as_array().unwrap();
        assert_eq!(inner[0]["type"], "text");
        assert_eq!(inner[0]["text"], "here is the screenshot");
        assert_eq!(inner[1]["type"], "image");
        assert_eq!(inner[1]["source"]["media_type"], "image/png");
        assert_eq!(inner[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn an_error_result_keeps_its_text_and_its_images_follow_it() {
        let items = [Item::ToolResults(vec![ToolResult {
            provenance: None,
            id: "c9".into(),
            content: "denied: no exec grant".into(),
            is_error: true,
            diff: None,
            images: vec![image("shot.png")],
        }])];
        let blocks = messages(&items)[0]["content"].as_array().unwrap().clone();
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["content"], "denied: no exec grant");
        assert_eq!(blocks[1]["type"], "text");
        assert!(
            blocks[1]["text"].as_str().unwrap().contains("c9"),
            "{:?}",
            blocks[1]
        );
        assert_eq!(blocks[2]["type"], "image");
    }

    #[test]
    fn a_result_with_no_image_is_sent_as_plain_content() {
        let items = [Item::ToolResults(vec![ToolResult {
            provenance: None,
            id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            diff: None,
            images: Vec::new(),
        }])];
        let blocks = messages(&items)[0]["content"].as_array().unwrap().clone();
        assert_eq!(blocks[0]["content"], "ok");
    }

    #[test]
    fn an_attached_image_is_sent_as_an_image_block_beside_its_text() {
        let image = Image {
            media_type: "image/png".into(),
            data: "AAAA".into(),
            name: "shot.png".into(),
            width: 2,
            height: 3,
        };
        let items = [Item::Attached {
            text: "what is wrong here?".into(),
            images: vec![image],
        }];
        let msgs = messages(&items);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "what is wrong here?");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/png");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    struct Collect(String, Vec<String>);
    impl Sink for Collect {
        fn text(&mut self, s: &str) {
            self.0.push_str(s)
        }
        fn note(&mut self, s: &str) {
            self.1.push(s.into())
        }
    }

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect()
    }

    #[test]
    fn stream_with_thinking_text_and_a_split_tool_call() {
        let stream = sse(&[
            json!({"type":"message_start","message":{"model":"claude-opus-5-5","usage":{"input_tokens":12,"cache_read_input_tokens":3,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"ping"}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Writ"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ing."}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"write","input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"a."}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"txt\", \"content\": \"x\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}),
            json!({"type":"message_stop"}),
        ]);
        let mut sink = Collect(String::new(), vec![]);
        let reply = read_stream(sse::Reader::new(stream.as_bytes()), &mut sink).unwrap();
        assert_eq!(sink.0, "Writing.");
        assert_eq!(reply.stop, Stop::ToolUse);
        assert_eq!(reply.model, "claude-opus-5-5");
        assert_eq!(
            reply.usage,
            Usage {
                input: 12,
                output: 40,
                cache_read: 3,
                cache_write: 0
            }
        );
        assert_eq!(
            reply.parts,
            vec![
                Part::Native {
                    api: Api::Anthropic,
                    block: json!({"type":"thinking","thinking":"hmm","signature":"sig"})
                },
                Part::Text("Writing.".into()),
                Part::Call(ToolCall {
                    id: "t1".into(),
                    name: "write".into(),
                    input: Ok(json!({"path":"a.txt","content":"x"}))
                }),
            ]
        );
    }

    #[test]
    fn invalid_tool_json_is_kept_raw_and_errors_fail() {
        let stream = sse(&[
            json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"bash","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"ec"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]);
        let reply = read_stream(
            sse::Reader::new(stream.as_bytes()),
            &mut Collect(String::new(), vec![]),
        )
        .unwrap();
        assert_eq!(reply.stop, Stop::MaxTokens);
        let call = reply.calls().next().unwrap();
        assert_eq!(call.input, Err("{\"command\": \"ec".into()));

        let stream = sse(&[
            json!({"type":"message_start","message":{"model":"m","usage":{}}}),
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        ]);
        let e = read_stream(
            sse::Reader::new(stream.as_bytes()),
            &mut Collect(String::new(), vec![]),
        )
        .unwrap_err();
        assert_eq!(e.to_string(), "overloaded_error: Overloaded");

        let cut = sse(&[json!({"type":"message_start","message":{"model":"m","usage":{}}})]);
        assert!(
            read_stream(
                sse::Reader::new(cut.as_bytes()),
                &mut Collect(String::new(), vec![])
            )
            .is_err()
        );
    }

    #[test]
    fn fallback_drops_the_declined_models_thinking_and_calls() {
        let thinking = |s: &str| Part::Native {
            api: Api::Anthropic,
            block: json!({"type":"thinking","thinking":s,"signature":"x"}),
        };
        let fallback = Part::Native {
            api: Api::Anthropic,
            block: json!({"type":"fallback","from":{"model":"a"},"to":{"model":"b"}}),
        };
        let call = |id: &str| {
            Part::Call(ToolCall {
                id: id.into(),
                name: "read".into(),
                input: Ok(json!({})),
            })
        };
        let mut parts = vec![
            thinking("declined"),
            Part::Text("partial".into()),
            call("early"),
            fallback.clone(),
            thinking("kept"),
            call("late"),
        ];
        drop_before_fallback(&mut parts);
        assert_eq!(
            parts,
            vec![
                Part::Text("partial".into()),
                fallback,
                thinking("kept"),
                call("late")
            ]
        );
    }

    #[test]
    fn a_formula_is_sent_as_the_tex_and_never_as_a_picture() {
        let formula = "$$\\int_0^1 x^2 \\, dx = \\frac{1}{3}$$";
        let items = vec![
            Item::User(format!("check {formula} please")),
            Item::Assistant(vec![Part::Text(format!("it follows: {formula}"))]),
        ];
        let m = messages(&items);
        assert_eq!(
            m[0]["content"][0]["text"],
            format!("check {formula} please")
        );
        assert_eq!(m[1]["content"][0]["text"], format!("it follows: {formula}"));
        assert!(!json!(m).to_string().contains("\"image\""));
    }

    #[test]
    fn history_renders_with_native_blocks_and_merged_user_turns() {
        let items = vec![
            Item::User("hi".into()),
            Item::Assistant(vec![
                Part::Native {
                    api: Api::Anthropic,
                    block: json!({"type":"thinking","thinking":"","signature":"s"}),
                },
                Part::Native {
                    api: Api::Openai,
                    block: json!({"reasoning_content":"ignored here"}),
                },
                Part::Text(String::new()),
                Part::Call(ToolCall {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: Err("{bad".into()),
                }),
            ]),
            Item::ToolResults(vec![ToolResult {
                provenance: None,
                id: "t1".into(),
                content: "nope".into(),
                images: Vec::new(),
                is_error: true,
                diff: None,
            }]),
            Item::User("again".into()),
        ];
        let m = messages(&items);
        assert_eq!(m.len(), 3);
        assert_eq!(
            m[1]["content"],
            json!([
                {"type":"thinking","thinking":"","signature":"s"},
                {"type":"tool_use","id":"t1","name":"bash","input":{}},
            ])
        );
        assert_eq!(
            m[2]["content"],
            json!([
                {"type":"tool_result","tool_use_id":"t1","content":"nope","is_error":true},
                {"type":"text","text":"again"},
            ])
        );
    }

    #[test]
    fn official_endpoint_only_features() {
        let cfg = |base: &str| ProviderConfig {
            api: Api::Anthropic,
            base_url: base.into(),
            model: "claude-opus-5-5".into(),
            api_key_env: None,
            max_tokens: None,
            context_window: None,
            effort: Some("high".into()),
            price: None,
            usage_url: None,
            search_url: None,
            headers: Default::default(),
            key_header: None,
            cache_retention: Default::default(),
            cache_affinity: false,
            auth: Auth::ApiKey,
            oauth: None,
        };
        let tools = [ToolDef {
            name: "read".into(),
            description: "d".into(),
            schema: json!({"type":"object"}),
        }];
        let conv = Conversation {
            system: "s",
            tools: &tools,
            items: &[],
            session: "",
        };
        let b = body(&cfg(OFFICIAL_BASE_URL), &conv);
        assert_eq!(b["fallbacks"], "default");
        assert_eq!(b["tools"][0]["eager_input_streaming"], true);
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["max_tokens"], DEFAULT_MAX_TOKENS);
        let b = body(&cfg("http://127.0.0.1:9"), &conv);
        assert!(b.get("fallbacks").is_none());
        assert!(b["tools"][0].get("eager_input_streaming").is_none());
    }
}
