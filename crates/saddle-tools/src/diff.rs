//! bounded human-only line presentation, computed before effects in guest code (D141).

use saddle_limits as budgets;
use std::fmt::Write as _;

pub fn lines(path: &str, old: Option<&str>, new: &str) -> String {
    let mut out = format!(
        " 0 {path}{}\n",
        if old.is_none() {
            " (new contents; preimage not read)"
        } else {
            ""
        }
    );
    let before: Vec<_> = old.unwrap_or_default().lines().collect();
    let after: Vec<_> = new.lines().collect();
    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    if old.is_some() && before == after {
        return String::new();
    }
    let start = prefix.saturating_sub(3);
    if start > 0 {
        out.push_str("~\n");
    }
    for (i, text) in after.iter().enumerate().take(prefix).skip(start) {
        append(&mut out, ' ', i, text);
    }
    for (i, text) in before
        .iter()
        .enumerate()
        .take(before.len() - suffix)
        .skip(prefix)
    {
        append(&mut out, '-', i, text);
    }
    for (i, text) in after
        .iter()
        .enumerate()
        .take(after.len() - suffix)
        .skip(prefix)
    {
        append(&mut out, '+', i, text);
    }
    let end = (after.len() - suffix + 3).min(after.len());
    for (i, text) in after
        .iter()
        .enumerate()
        .take(end)
        .skip(after.len() - suffix)
    {
        append(&mut out, ' ', i, text);
    }
    if end < after.len() || out.len() >= budgets::diff_present_bytes() {
        out.push_str("~\n");
    }
    out
}

fn append(out: &mut String, tag: char, index: usize, text: &str) {
    if out.len() >= budgets::diff_present_bytes() {
        return;
    }
    let remaining = budgets::diff_present_bytes().saturating_sub(out.len() + 32);
    let mut end = text.len().min(remaining);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let _ = writeln!(
        out,
        "{tag}{} {}{}",
        index + 1,
        &text[..end],
        if end < text.len() { "…" } else { "" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_lines_are_numbered_and_blind_writes_are_labelled() {
        assert_eq!(
            lines("file", Some("a\nold\nz\n"), "a\nnew\nz\n"),
            " 0 file\n 1 a\n-2 old\n+2 new\n 3 z\n"
        );
        assert!(lines("file", None, "new").contains("preimage not read"));
        assert!(lines("file", Some("same"), "same").is_empty());
        assert!(lines("file", None, &"é".repeat(100000)).len() < 65536);
    }
}
