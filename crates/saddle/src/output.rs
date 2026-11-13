//! Where line-mode output goes: a terminal for a person, or one JSON object per line for a
//! program (D52).
//!
//! The JSON form is the same events the TUI receives, so a caller gets the text as it streams, the
//! call and its result, what the reply used and cost, and how the turn ended — without parsing
//! prose. Nothing is written to stdout that is not a JSON object, and the host's own errors still
//! go to stderr with the `saddle:` prefix, so a consumer can read stdout as a stream of events and
//! treat a non-zero exit as a host failure.

use std::io::Write;

use serde_json::{Value, json};

use crate::agent::{End, Ui, call_detail};
use crate::compact;
use crate::provider::{Quota, Reply, Sink, ToolCall, Usage};
use crate::term::Terminal;
use crate::tools::ToolOutput;

/// Line-mode output: the human terminal, or the JSON stream.
pub enum Output {
    Human(Terminal),
    Json(Json),
}

impl Output {
    /// A line for the person, or a `note` event.
    pub fn info(&mut self, s: &str) {
        match self {
            Output::Human(t) => t.info(s),
            Output::Json(j) => j.emit(json!({ "type": "note", "text": s })),
        }
    }

    /// What a turn used, in total.
    pub fn usage(&mut self, u: Usage) {
        match self {
            Output::Human(t) => t.info(&format!(
                "tokens: {} in ({} cache read, {} cache write), {} out",
                u.input, u.cache_read, u.cache_write, u.output
            )),
            Output::Json(j) => j.emit(json!({
                "type": "usage",
                "input": u.input,
                "output": u.output,
                "cache_read": u.cache_read,
                "cache_write": u.cache_write,
            })),
        }
    }

    /// How the turn ended, which a program needs to decide what to do next.
    pub fn turn_end(&mut self, end: &End) {
        match self {
            Output::Human(t) => {
                if *end == End::Cancelled {
                    t.info("cancelled");
                }
            }
            Output::Json(j) => j.emit(json!({ "type": "turn_end", "end": end_name(end) })),
        }
    }

    /// The session's identity, said once at the start.
    pub fn opening(&mut self, provider: &str, model: &str, workspace: &str, audit: &str) {
        match self {
            Output::Human(t) => t.info(&format!("{provider} · {model} · audit {audit}")),
            Output::Json(j) => j.emit(json!({
                "type": "session",
                "provider": provider,
                "model": model,
                "workspace": workspace,
                "audit": audit,
            })),
        }
    }
}

/// The end of a turn, as a program spells it.
fn end_name(end: &End) -> &'static str {
    match end {
        End::Answered => "answered",
        End::Declined => "declined",
        End::Cut => "cut",
        End::StepLimit => "step_limit",
        End::Repeating => "repeating",
        End::Failed => "failed",
        End::Cancelled => "cancelled",
    }
}

/// One JSON object per line. The sink is anything writable so a test can read the stream back
/// without a terminal, which is the only honest way to check what a caller receives (D52).
pub struct Json {
    out: Box<dyn Write>,
}

impl Json {
    pub fn stdout() -> Self {
        Self {
            out: Box::new(std::io::stdout()),
        }
    }

    fn emit(&mut self, event: Value) {
        // The encoder escapes control characters, so model text cannot move a cursor or write a
        // clipboard, and the stream stays one object per line (D27, D52).
        let _ = writeln!(self.out, "{event}");
        let _ = self.out.flush();
    }
}

impl Sink for Output {
    fn text(&mut self, s: &str) {
        match self {
            Output::Human(t) => Sink::text(t, s),
            Output::Json(j) => j.emit(json!({ "type": "text", "text": s })),
        }
    }

    fn note(&mut self, s: &str) {
        self.info(&crate::term::clean(s));
    }

    fn reasoning(&mut self, s: &str) {
        match self {
            Output::Human(t) => Sink::reasoning(t, s),
            Output::Json(j) => j.emit(json!({ "type": "reasoning", "text": s })),
        }
    }

    fn quota(&mut self, quota: &Quota) {
        match self {
            Output::Human(t) => Sink::quota(t, quota),
            Output::Json(j) => j.emit(json!({
                "type": "quota",
                "tokens": quota.tokens,
                "tokens_limit": quota.tokens_limit,
                "requests": quota.requests,
                "requests_limit": quota.requests_limit,
                "credits": quota.credits,
                "percent": quota.percent,
                "reset": quota.reset,
                "windows": quota.windows,
            })),
        }
    }

    fn cancelled(&self) -> bool {
        match self {
            Output::Human(t) => Sink::cancelled(t),
            Output::Json(_) => crate::cancel::requested(),
        }
    }
}

impl Ui for Output {
    fn call(&mut self, call: &ToolCall) {
        match self {
            Output::Human(t) => t.call(call),
            Output::Json(j) => {
                let name = crate::term::clean(&call.name);
                j.emit(json!({
                    "type": "call",
                    "name": name,
                    "id": call.id,
                    // The arguments as the model sent them, or the parse failure. A caller that
                    // wants the detail line the terminal shows can build it from these.
                    "input": match &call.input {
                        Ok(v) => v.clone(),
                        Err(e) => json!({ "invalid": e }),
                    },
                    "detail": call_detail(call),
                }));
            }
        }
    }

    fn attached(&mut self, images: &[crate::provider::Image]) {
        match self {
            Output::Human(t) => t.attached(images),
            Output::Json(j) => {
                for image in images {
                    j.emit(json!({
                        "type": "attached",
                        "name": image.name,
                        "media_type": image.media_type,
                        "width": image.width,
                        "height": image.height,
                    }));
                }
            }
        }
    }

    fn result(&mut self, out: &ToolOutput) {
        match self {
            Output::Human(t) => t.result(out),
            Output::Json(j) => j.emit(json!({
                "type": "result",
                "ok": out.ok,
                "content": out.content,
                // The transcript-only diff, so a consumer can show the change (D35).
                "diff": out.diff,
            })),
        }
    }

    fn reply(&mut self, reply: &Reply, cost: f64) {
        match self {
            Output::Human(t) => t.reply(reply, cost),
            Output::Json(j) => j.emit(json!({
                "type": "reply",
                "model": reply.model,
                "stop": reply.stop_raw,
                "usage": {
                    "input": reply.usage.input,
                    "output": reply.usage.output,
                    "cache_read": reply.usage.cache_read,
                    "cache_write": reply.usage.cache_write,
                },
                "cost": cost,
            })),
        }
    }

    fn compaction(&mut self, event: compact::Event) {
        match self {
            Output::Human(t) => t.compaction(event),
            Output::Json(j) => match event {
                // The streamed reasoning and summary end up in the conversation, so the JSON
                // stream reports the phases and leaves the prose out of it (D52).
                compact::Event::Started {
                    reason,
                    tokens,
                    window,
                    messages,
                } => j.emit(json!({
                    "type": "compaction",
                    "phase": "started",
                    "reason": reason.describe(),
                    "tokens": tokens,
                    "window": window,
                    "messages": messages,
                })),
                compact::Event::Done {
                    summarized,
                    kept,
                    before,
                    after,
                } => j.emit(json!({
                    "type": "compaction",
                    "phase": "done",
                    "summarized": summarized,
                    "kept": kept,
                    "before": before,
                    "after": after,
                })),
                compact::Event::Failed(why) => {
                    j.emit(json!({ "type": "compaction", "phase": "failed", "why": why }))
                }
                compact::Event::Reasoning(_) | compact::Event::Text(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JSON sink into a buffer, so the stream can be read back.
    fn captured() -> (Json, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sink = Json {
            out: Box::new(Shared(buf.clone())),
        };
        (sink, buf)
    }

    fn events(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> Vec<Value> {
        let raw = buf.lock().unwrap().clone();
        String::from_utf8(raw)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l:?}")))
            .collect()
    }

    #[test]
    fn every_event_is_one_line_and_parses() {
        let (json, buf) = captured();
        let mut out = Output::Json(json);
        out.opening("mock", "mock-model", "/ws", "/state/audit/a.jsonl");
        Sink::text(&mut out, "hello\nworld");
        out.info("a note");
        out.usage(Usage {
            input: 10,
            output: 2,
            cache_read: 5,
            cache_write: 0,
        });
        out.turn_end(&End::Answered);
        let events = events(&buf);
        assert_eq!(events.len(), 5, "{events:?}");
        assert_eq!(events[0]["type"], "session");
        assert_eq!(events[0]["workspace"], "/ws");
        // A newline inside the text stays inside one JSON line.
        assert_eq!(events[1]["text"], "hello\nworld");
        assert_eq!(events[2]["type"], "note");
        assert_eq!(events[3]["type"], "usage");
        assert_eq!(events[3]["cache_read"], 5);
        assert_eq!(events[4]["end"], "answered");
    }

    #[test]
    fn a_tool_call_carries_its_arguments_and_a_broken_one_says_so() {
        let (json, buf) = captured();
        let mut out = Output::Json(json);
        out.call(&ToolCall {
            id: "t1".into(),
            name: "ls".into(),
            input: Ok(json!({"path": "src"})),
        });
        out.call(&ToolCall {
            id: "t2".into(),
            name: "read".into(),
            input: Err("not json".into()),
        });
        let events = events(&buf);
        assert_eq!(events[0]["input"], json!({"path": "src"}));
        assert_eq!(events[1]["input"]["invalid"], "not json");
        assert_eq!(events[1]["name"], "read");
    }

    #[test]
    fn a_turn_end_is_spelled_for_a_program() {
        // The words a caller switches on are stable, and `cancelled` is not `failed` (D52).
        assert_eq!(end_name(&End::Answered), "answered");
        assert_eq!(end_name(&End::Cancelled), "cancelled");
        assert_eq!(end_name(&End::Repeating), "repeating");
        assert_eq!(end_name(&End::StepLimit), "step_limit");
    }
}
