//! Line diffs of the files the agent edits, for the transcript only; the model never sees them.
//!
//! Each line starts with a tag: ' ' context, '-' removed, '+' added, '~' a gap between hunks.
//! Then the line number (old for removed lines, new otherwise), a space, and the text.

use std::fmt::Write as _;
use std::time::Duration;

use similar::{ChangeTag, TextDiffConfig};

/// Context lines around each change.
const CONTEXT: usize = 3;
/// Beyond this much text, no diff is made.
const MAX_BYTES: usize = 2 * 1024 * 1024;

pub fn lines(old: &str, new: &str) -> Option<String> {
    if old.len() + new.len() > MAX_BYTES {
        return None;
    }
    let diff = TextDiffConfig::default()
        .timeout(Duration::from_millis(200))
        .diff_lines(old, new);
    let mut out = String::new();
    for (i, group) in diff.grouped_ops(CONTEXT).iter().enumerate() {
        if i > 0 {
            out.push_str("~\n");
        }
        for op in group {
            for change in diff.iter_changes(op) {
                let (tag, number) = match change.tag() {
                    ChangeTag::Delete => ('-', change.old_index()),
                    ChangeTag::Insert => ('+', change.new_index()),
                    ChangeTag::Equal => (' ', change.new_index()),
                };
                let text = change.value().trim_end_matches(['\n', '\r']);
                let _ = writeln!(out, "{tag}{} {text}", number.map_or(0, |n| n + 1));
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hunks_with_context_and_line_numbers() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 5\n", "line five\n")
            .replace("line 18\n", "");
        let d = lines(&old, &new).unwrap();
        let shown: Vec<&str> = d.lines().collect();
        assert_eq!(
            shown,
            [
                " 2 line 2",
                " 3 line 3",
                " 4 line 4",
                "-5 line 5",
                "+5 line five",
                " 6 line 6",
                " 7 line 7",
                " 8 line 8",
                "~",
                " 15 line 15",
                " 16 line 16",
                " 17 line 17",
                "-18 line 18",
                " 18 line 19",
                " 19 line 20",
            ]
        );
        assert_eq!(lines("same\n", "same\n"), None);
        assert_eq!(lines("", "new\nfile\n").unwrap(), "+1 new\n+2 file\n");
    }
}
