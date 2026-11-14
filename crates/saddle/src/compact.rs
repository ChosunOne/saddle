//! Compaction (D33): when the conversation nears the model's context window, the older part is
//! replaced by a summary the model writes, and the recent part is kept word for word.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use crate::config::CompactionConfig;
use crate::provider::{Item, Part};

pub use crate::provider::DEFAULT_CONTEXT_WINDOW;

pub const DEFAULT_RESERVE: u64 = 16_384;
pub const DEFAULT_KEEP_RECENT: u64 = 20_000;
/// Half the window: the point where a session left alone is worth summarizing (D77).
pub const DEFAULT_IDLE_FRACTION: f64 = 0.5;
/// Two minutes: long enough not to be a pause between typing, short enough to have happened before
/// a person comes back to the terminal.
pub const DEFAULT_IDLE_SECONDS: u64 = 120;
/// Three quarters: near enough to the hard threshold to be worth summarizing, far enough that there
/// is room to finish the summary while the turn uses what is left (D78).
pub const DEFAULT_BACKGROUND_FRACTION: f64 = 0.75;
/// Tool output is cut to this many characters in the text the summary is written from.
const TOOL_RESULT_CHARS: usize = 2000;
/// Tool input is cut to this many characters there too.
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

/// A share of the window, clamped to something a person meant.
///
/// `0` would compact a session the moment it had one message and `1` would never fire before the
/// hard threshold, so both are read as unset rather than obeyed.
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
    /// The context neared the window.
    Threshold,
    /// The provider refused a request as too long.
    Overflow,
    /// The session was left alone and the context was half full (D77).
    Idle,
    /// The context reached the background threshold and the turn carried on (D78).
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

    /// Whether this compaction stops the turn, which is what the interface shows as an activity.
    ///
    /// A background one does not: the turn is still answering, and saying "compacting" while it
    /// writes would be a lie about what the terminal is waiting for (D78).
    pub fn blocks(self) -> bool {
        !matches!(self, Reason::Background)
    }
}

/// What the UI is told while a compaction runs. Reasoning and summary text stream in as the model
/// writes them.
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

/// Characters an item adds to a request, for estimating tokens.
pub fn chars(item: &Item) -> usize {
    match item {
        Item::User(t) | Item::Summary(t) => t.len(),
        Item::Attached { text, images } => text.len() + images.len() * 64,
        Item::Assistant(parts) => parts
            .iter()
            // Images are described rather than sent: the summary is about the work, and a
            // picture would cost more than it explains (D58).
            .map(|p| match p {
                Part::Text(t) => t.len(),
                Part::Call(c) => {
                    c.name.len()
                        + c.input
                            .as_ref()
                            .map_or_else(String::len, |v| v.to_string().len())
                }
                // An attachment is worth a line in the summary, not its bytes.
                Part::Image(_) => 64,
                Part::Native { block, .. } => block.to_string().len(),
            })
            .sum(),
        Item::ToolResults(rs) => rs.iter().map(|r| r.content.len() + r.id.len()).sum(),
    }
}

/// A rough token count: four characters a token.
pub fn tokens(chars: usize) -> u64 {
    (chars / 4) as u64
}

/// Where to cut: items before the index are summarized, the rest kept. The kept part holds at most
/// about `keep` tokens and starts at a prompt where possible, else at a reply, and never at tool
/// results, which must stay with their call. Returns 0 when there is nothing to summarize.
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
        // Even the last exchange is over the budget: keep it anyway.
        .or_else(|| last(|i| matches!(i, Item::User(_) | Item::Attached { .. })))
        .or_else(|| last(|i| matches!(i, Item::Assistant(_))))
        .unwrap_or(0);
    // Cutting at the start, or keeping nothing but a summary, would change nothing.
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

/// The conversation as text, so the model summarizes it rather than continuing it. Reasoning is
/// left out and tool output is cut. Past `max_chars`, the oldest part is dropped.
pub fn serialize(items: &[Item], max_chars: usize) -> String {
    let mut parts = Vec::new();
    for item in items {
        let mut s = String::new();
        match item {
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

/// The request for a summary of `transcript`, with the user's focus if they gave one.
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

/// The request for a summary of the branch being left, so what it found is not lost (D54).
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

/// What the model says when an abandoned branch held nothing worth a note.
pub const BRANCH_NOTHING: &str = "NONE";

/// Whether a provider error means the request was too long for the context window.
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

/// Builds a conversation of about `tokens` tokens, for the tests that need a full window (D77).
///
/// Not compiled into the binary: it exists so a test can say "a session at 60% of a 100k window"
/// without writing out a transcript.
#[cfg(test)]
pub mod tests_support {
    use super::{Item, Part};

    /// Items of a thousand tokens each, alternating a prompt and a reply, so a cut point has
    /// boundaries to fall on and the kept part can fit under `keep_recent`.
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
        // Each item is about 100 tokens; keep 250 tokens.
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
        // One huge last turn: keep it anyway.
        let big = vec![user(400), reply(400), user(400), reply(100_000)];
        assert_eq!(cut_point(&big, 250), 2);
        // A summary followed by one turn has nothing more to give.
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
