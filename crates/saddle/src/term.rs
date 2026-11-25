//! Terminal output. The reply goes to stdout; tool calls and notes go to stderr.
//!
//! Model and tool text is untrusted, so control characters are removed before printing (D27).
//! Raw escapes could move the cursor over host output, write the clipboard (OSC 52) or plant
//! links (OSC 8).

use crate::cancel;
use crate::provider::Image;
use std::io;
use std::io::{IsTerminal, Write};

use crate::agent::{Ui, call_detail};
use crate::compact;
use crate::provider::{Reply, Sink, ToolCall};
use crate::tools::ToolOutput;
use crate::tools::timeout;

pub struct Terminal {
    /// stdout holds text without a trailing newline.
    mid_line: bool,
    /// Reasoning is being written to stderr.
    reasoning: bool,
    dim: &'static str,
    reset: &'static str,
}

impl Terminal {
    pub fn new() -> Self {
        let color = io::stderr().is_terminal();
        Self {
            mid_line: false,
            reasoning: false,
            dim: if color { "\x1b[2m" } else { "" },
            reset: if color { "\x1b[0m" } else { "" },
        }
    }

    fn end_line(&mut self) {
        if self.reasoning {
            eprintln!("{}", self.reset);
            self.reasoning = false;
        }
        if self.mid_line {
            println!();
            self.mid_line = false;
        }
    }

    /// A host message on stderr.
    pub fn info(&mut self, s: &str) {
        self.end_line();
        eprintln!("{}{}{}", self.dim, clean(s), self.reset);
    }
}

/// Removes control characters other than newline and tab.
pub fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}

fn one_line(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or_default();
    let more = s.lines().nth(1).is_some();
    let mut out: String = first.chars().take(max).collect();
    if more || first.chars().count() > max {
        out.push('…');
    }
    out
}

impl Sink for Terminal {
    fn text(&mut self, s: &str) {
        let s = clean(s);
        if s.is_empty() {
            return;
        }
        if self.reasoning {
            eprintln!("{}", self.reset);
            self.reasoning = false;
        }
        print!("{s}");
        let _ = io::stdout().flush();
        self.mid_line = !s.ends_with('\n');
    }

    fn note(&mut self, s: &str) {
        self.info(s);
    }

    /// Reasoning goes to stderr, dimmed, so stdout carries only the answer.
    fn reasoning(&mut self, s: &str) {
        let s = clean(s);
        if s.is_empty() {
            return;
        }
        if !self.reasoning {
            if self.mid_line {
                println!();
                self.mid_line = false;
            }
            eprint!("{}", self.dim);
            self.reasoning = true;
        }
        eprint!("{s}");
    }

    fn cancelled(&self) -> bool {
        cancel::requested()
    }
}

impl Ui for Terminal {
    fn call(&mut self, call: &ToolCall) {
        self.end_line();
        let limit = match timeout::requested(call) {
            Some(seconds) => format!(" · {seconds}s limit"),
            None => String::new(),
        };
        eprintln!(
            "{}● {} {}{limit}{}",
            self.dim,
            clean(&call.name),
            one_line(&clean(&call_detail(call)), 120),
            self.reset
        );
    }

    fn attached(&mut self, images: &[Image]) {
        for image in images {
            self.info(&format!(
                "attached {} ({}×{}, {})",
                image.name, image.width, image.height, image.media_type
            ));
        }
    }

    fn result(&mut self, out: &ToolOutput) {
        let mark = if out.ok { "⎿" } else { "✗" };
        eprintln!(
            "{}  {mark} {}{}",
            self.dim,
            one_line(&clean(out.visible_content()), 160),
            self.reset
        );
    }

    /// Compaction goes to stderr: a line when it starts and ends, and the reasoning and summary
    /// dimmed as they stream.
    fn compaction(&mut self, event: compact::Event) {
        match event {
            compact::Event::Started {
                reason,
                tokens,
                window,
                messages,
            } => self.info(&format!(
                "compacting {messages} messages ({}; ~{tokens} of {window} tokens)",
                reason.describe()
            )),
            compact::Event::Reasoning(s) | compact::Event::Text(s) => {
                Sink::reasoning(self, &s);
            }
            compact::Event::Done {
                summarized,
                kept,
                before,
                after,
            } => self.info(&format!(
                "compacted: {summarized} messages summarized, {kept} kept; ~{before} -> ~{after} tokens"
            )),
            compact::Event::Failed(why) => self.info(&format!("compaction failed: {why}")),
        }
    }

    fn reply(&mut self, _reply: &Reply, _cost: f64) {
        self.end_line();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_removed() {
        assert_eq!(
            clean("a\x1b]52;c;aGk=\x07b\x1b[2Jc\r\n\td\u{9b}e"),
            "a]52;c;aGk=b[2Jc\n\tde"
        );
        assert_eq!(one_line("first\nsecond", 10), "first…");
        assert_eq!(one_line("abcdef", 3), "abc…");
    }
}
