//! native-style minimal line hunks in the guest, before filesystem effects (D146).

use saddle_limits as budgets;
use similar::{ChangeTag, TextDiff};
use std::fmt::Write as _;

/// format Myers line hunks with native line numbers, three context lines and gap markers.
/// guest fuel/memory bound computation; no unsupported native clock is used in Wasm.
pub fn lines(path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let groups = diff.grouped_ops(3);
    if groups.is_empty() {
        return String::new();
    }
    let mut out = format!(" 0 {path}\n");
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
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
                if out.len() > budgets::diff_present_bytes() {
                    let mut end = budgets::diff_present_bytes().saturating_sub(2);
                    while !out.is_char_boundary(end) {
                        end -= 1;
                    }
                    out.truncate(end);
                    if budgets::diff_present_bytes() >= 2 {
                        out.push_str("~\n");
                    } else {
                        out.push('~');
                    }
                    return out;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_a_tiny_configured_diff_budget_is_respected() {
        let limits = budgets::Budgets {
            diff_present_bytes: 1,
            ..budgets::Budgets::default()
        };
        budgets::using(&limits, || assert_eq!(lines("file", "", "changed"), "~"));
    }

    #[test]
    fn distant_edits_have_native_minimal_hunks_not_one_large_replacement() {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old
            .replace("line 5\n", "line five\n")
            .replace("line 18\n", "");
        assert_eq!(
            lines("file", &old, &new),
            " 0 file\n 2 line 2\n 3 line 3\n 4 line 4\n-5 line 5\n+5 line five\n 6 line 6\n 7 line 7\n 8 line 8\n~\n 15 line 15\n 16 line 16\n 17 line 17\n-18 line 18\n 18 line 19\n 19 line 20\n"
        );
        assert_eq!(lines("file", "same\n", "same\n"), "");
        assert_eq!(
            lines("file", "", "new\nfile\n"),
            " 0 file\n+1 new\n+2 file\n"
        );
    }
}
