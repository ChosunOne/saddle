use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::config::CompactionConfig;
use crate::provider::{Item, Part};

pub use crate::provider::DEFAULT_CONTEXT_WINDOW;

pub const DEFAULT_RESERVE: u64 = 16_384;
pub const DEFAULT_KEEP_RECENT: u64 = 20_000;
pub const DEFAULT_IDLE_FRACTION: f64 = 0.5;
pub const DEFAULT_IDLE_SECONDS: u64 = 120;
pub const DEFAULT_BACKGROUND_FRACTION: f64 = 0.75;
const TOOL_RESULT_CHARS: usize = 2000;
const TOOL_INPUT_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub auto: bool,
    pub reserve: u64,
    pub keep_recent: u64,
    pub idle: bool,
    pub idle_fraction: f64,
    pub idle_after: Duration,
    pub background: bool,
    pub background_fraction: f64,
}

fn fraction(value: Option<f64>, default: f64) -> f64 {
    match value {
        Some(f) if f > 0.0 && f < 1.0 => f,
        _ => default,
    }
}

impl Settings {
    pub fn from_config(c: &CompactionConfig) -> Self {
        Self {
            auto: c.auto.unwrap_or(true),
            reserve: c.reserve_tokens.unwrap_or(DEFAULT_RESERVE),
            keep_recent: c.keep_recent_tokens.unwrap_or(DEFAULT_KEEP_RECENT),
            idle: c.idle.unwrap_or(true),
            idle_fraction: fraction(c.idle_fraction, DEFAULT_IDLE_FRACTION),
            idle_after: Duration::from_secs(c.idle_seconds.unwrap_or(DEFAULT_IDLE_SECONDS)),
            background: c.background.unwrap_or(true),
            background_fraction: fraction(c.background_fraction, DEFAULT_BACKGROUND_FRACTION),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::from_config(&CompactionConfig::default())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Manual,
    Threshold,
    Overflow,
    Idle,
    Background,
}

impl Reason {
    pub fn describe(self) -> &'static str {
        match self {
            Reason::Manual => "asked for",
            Reason::Threshold => "the context is nearly full",
            Reason::Overflow => "the provider said the context is too long",
            Reason::Idle => "the session was idle",
            Reason::Background => "the context passed three quarters while the turn went on",
        }
    }

    pub fn blocks(self) -> bool {
        !matches!(self, Reason::Background)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Started {
        reason: Reason,
        tokens: u64,
        window: u64,
        messages: usize,
    },
    Reasoning(String),
    Text(String),
    Done {
        summarized: usize,
        kept: usize,
        before: u64,
        after: u64,
    },
    Failed(String),
}

pub fn chars(item: &Item) -> usize {
    match item {
        Item::Contribution(value) => value.content.len(),
        Item::User(t) | Item::Summary(t) => t.len(),
        Item::Attached { text, images } => text.len() + images.len() * 64,
        Item::Assistant(parts) => parts
            .iter()
            .map(|p| match p {
                Part::Text(t) => t.len(),
                Part::Call(c) => {
                    c.name.len()
                        + c.input
                            .as_ref()
                            .map_or_else(String::len, |v| v.to_string().len())
                }
                Part::Image(_) => 64,
                Part::Native { block, .. } => block.to_string().len(),
            })
            .sum(),
        Item::ToolResults(rs) => rs.iter().map(|r| r.content.len() + r.id.len()).sum(),
    }
}

pub fn tokens(chars: usize) -> u64 {
    (chars / 4) as u64
}

pub fn cut_point(items: &[Item], keep: u64) -> usize {
    let mut used = 0;
    let mut at_prompt = None;
    let mut at_reply = None;
    for (i, item) in items.iter().enumerate().rev() {
        used += tokens(chars(item));
        if used > keep {
            break;
        }
        match item {
            Item::User(_) | Item::Attached { .. } => at_prompt = Some(i),
            Item::Assistant(_) => at_reply = Some(i),
            _ => {}
        }
    }
    let last = |want: fn(&Item) -> bool| items.iter().rposition(want);
    let cut = at_prompt
        .or(at_reply)
        .or_else(|| last(|i| matches!(i, Item::User(_) | Item::Attached { .. })))
        .or_else(|| last(|i| matches!(i, Item::Assistant(_))))
        .unwrap_or(0);
    if cut == 0 || (cut == 1 && matches!(items[0], Item::Summary(_))) {
        0
    } else {
        cut
    }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}[… {} more characters]", &s[..cut], s.len() - cut)
}

pub fn serialize(items: &[Item], max_chars: usize) -> String {
    let mut parts = Vec::new();
    for item in items {
        let mut s = String::new();
        match item {
            Item::Contribution(value) => {
                let _ = write!(s, "[Plugin fact]: {}", value.content);
            }
            Item::User(t) => {
                let _ = write!(s, "[User]: {t}");
            }
            Item::Attached { text, images } => {
                let _ = write!(s, "[User]: {text}");
                for image in images {
                    let _ = write!(
                        s,
                        "\n[attached image {} ({}x{})]",
                        image.name, image.width, image.height
                    );
                }
            }
            Item::Summary(t) => {
                let _ = write!(s, "[Summary of the conversation before this point]: {t}");
            }
            Item::Assistant(ps) => {
                for p in ps {
                    match p {
                        Part::Text(t) if !t.trim().is_empty() => {
                            let _ = writeln!(s, "[Assistant]: {t}");
                        }
                        Part::Call(c) => {
                            let input = c
                                .input
                                .as_ref()
                                .map_or_else(Clone::clone, |v| v.to_string());
                            let _ = writeln!(
                                s,
                                "[Assistant tool call]: {}({})",
                                c.name,
                                clip(&input, TOOL_INPUT_CHARS)
                            );
                        }
                        _ => {}
                    }
                }
            }
            Item::ToolResults(rs) => {
                for r in rs {
                    let label = if r.is_error {
                        "Tool error"
                    } else {
                        "Tool result"
                    };
                    let _ = writeln!(s, "[{label}]: {}", clip(&r.content, TOOL_RESULT_CHARS));
                }
            }
        }
        parts.push(s.trim_end().to_owned());
    }
    let mut total: usize = parts.iter().map(|p| p.len() + 2).sum();
    let mut start = 0;
    while total > max_chars && start + 1 < parts.len() {
        total -= parts[start].len() + 2;
        start += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push_str("[The oldest messages were left out to fit.]\n\n");
    }
    out.push_str(&parts[start..].join("\n\n"));
    out
}

pub const SYSTEM: &str = "You write handover summaries of coding sessions. Another assistant will \
continue the work from your summary alone, without the earlier messages. Reply with the summary \
only.";

pub fn request(transcript: &str, workspace: &Path, focus: Option<&str>) -> String {
    let focus = match focus {
        Some(f) => format!("\nThe user asked the summary to focus on: {f}\n"),
        None => String::new(),
    };
    format!(
        "Below is a coding session between a user and an assistant working in {}. Summarize it \
so the assistant can continue without the earlier messages. Use these sections, and leave a \
section out only if it has nothing in it:

## Goal
## Constraints and preferences
## Progress
### Done
### In progress
### Blocked
## Key decisions
## Next steps
## Critical context
Exact file paths, function and type names, commands, error messages and values the work depends \
on.

Be specific and complete; this replaces the conversation.{focus}
<conversation>
{transcript}
</conversation>",
        workspace.display()
    )
}

pub fn branch_request(transcript: &str, workspace: &Path) -> String {
    format!(
        "Below is a coding session between a user and an assistant working in {}. The user is \
about to take a different approach, so this branch is being abandoned. Summarize what it \
established, so the work is not wasted: what was tried, what was found, what is known about the \
problem now, and anything the next attempt should know. Keep it short — this is a note attached \
to the new branch, not a handover. If nothing in it is worth keeping, reply with exactly the \
word NONE and nothing else.

<abandoned branch>
{transcript}
</abandoned>",
        workspace.display()
    )
}

pub const BRANCH_NOTHING: &str = "NONE";

pub fn is_overflow(message: &str) -> bool {
    let m = message.to_lowercase();
    [
        "context length",
        "context_length",
        "maximum context",
        "context window",
        "too many tokens",
        "prompt is too long",
        "input is too long",
        "reduce the length",
        "token limit",
        "exceeds the context",
    ]
    .iter()
    .any(|p| m.contains(p))
}

#[cfg(test)]
pub mod tests_support {
    use super::{Item, Part};

    pub fn conversation(tokens: usize) -> Vec<Item> {
        let mut items = Vec::new();
        let mut left = tokens * 4;
        let mut prompt = true;
        while left > 0 {
            let take = left.min(4_000);
            left -= take;
            let text = "x".repeat(take);
            items.push(if prompt {
                Item::User(text)
            } else {
                Item::Assistant(vec![Part::Text(text)])
            });
            prompt = !prompt;
        }
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ToolCall, ToolResult};
    use serde_json::json;

    fn user(n: usize) -> Item {
        Item::User("u".repeat(n))
    }

    fn reply(n: usize) -> Item {
        Item::Assistant(vec![Part::Text("a".repeat(n))])
    }

    fn call() -> Item {
        Item::Assistant(vec![Part::Call(ToolCall {
            id: "c".into(),
            name: "bash".into(),
            input: Ok(json!({"command": "ls"})),
        })])
    }

    fn result(n: usize) -> Item {
        Item::ToolResults(vec![ToolResult {
            provenance: None,
            id: "c".into(),
            content: "r".repeat(n),
            images: Vec::new(),
            is_error: false,
            diff: None,
        }])
    }

    #[test]
    fn the_cut_keeps_recent_turns_and_never_splits_a_call() {
        let items = vec![
            user(400),
            reply(400),
            user(400),
            call(),
            result(400),
            reply(400),
        ];
        assert_eq!(
            cut_point(&items, 250),
            3,
            "keeps the call, its result and the reply"
        );
        assert_eq!(cut_point(&items, 350), 2, "prefers starting at a prompt");
        assert_eq!(
            cut_point(&items, 10_000),
            0,
            "everything fits: nothing to do"
        );
        let big = vec![user(400), reply(400), user(400), reply(100_000)];
        assert_eq!(cut_point(&big, 250), 2);
        let done = vec![Item::Summary("s".into()), user(400), reply(100_000)];
        assert_eq!(cut_point(&done, 10), 0);
    }

    #[test]
    fn serialization_cuts_tool_output_and_drops_the_oldest_when_too_long() {
        let items = vec![
            Item::Summary("before".into()),
            user(10),
            call(),
            result(5000),
        ];
        let text = serialize(&items, 100_000);
        assert!(text.starts_with("[Summary of the conversation before this point]: before"));
        assert!(text.contains("[Assistant tool call]: bash({\"command\":\"ls\"})"));
        assert!(text.contains("[… 3000 more characters]"));
        let short = serialize(&items, 1000);
        assert!(short.starts_with("[The oldest messages were left out to fit.]"));
        assert!(!short.contains("before"));
    }

    #[test]
    fn overflow_messages() {
        assert!(is_overflow(
            "mock returned 400: This model's maximum context length is 131072 tokens"
        ));
        assert!(is_overflow(
            "prompt is too long: 210000 tokens > 200000 maximum"
        ));
        assert!(!is_overflow("mock returned 401: invalid api key"));
    }
}
