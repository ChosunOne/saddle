//! `patch`: several files changed in one call (D56).
//!
//! The format is the one models already know from OpenAI's apply-patch — `*** Begin Patch`,
//! `*** Add File:`, `*** Update File:`, `*** Delete File:`, `*** Move to:`, `*** End Patch` — so
//! no new convention has to be taught. A rename plus its call sites becomes one request instead of
//! one per file.
//!
//! Authority is unchanged: every path the patch touches is decided on its own, through the same
//! engine, before anything is written. One denial refuses the whole patch, because a half-applied
//! change is a state the user did not ask for and the model cannot reason about. Nothing is written
//! until every file's new contents have been computed in memory.

use std::fmt::Write as _;
use std::iter::Peekable;

use anyhow::Result;
use saddle_cap::Location;
use saddle_cap::Scope;
use serde_json::Value;

use super::{ToolOutput, str_field};
use crate::session::Session;

/// One file the patch touches.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Op {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Update {
        path: String,
        /// A rename, applied after the content is written.
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
    Delete {
        path: String,
    },
}

/// One replacement inside a file: the lines to find, and what they become.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    /// The nearest `@@` line, for a message that says where the failure was.
    at: String,
    old: Vec<String>,
    new: Vec<String>,
}

/// What the model sent, before anything is decided or written.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Patch {
    ops: Vec<Op>,
}

pub fn patch(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let text = match str_field(input, "patch") {
        Ok(v) => v,
        Err(out) => return Ok(out),
    };
    let patch = match Patch::parse(text) {
        Ok(p) => p,
        Err(e) => return Ok(ToolOutput::err(e)),
    };
    // Every path is resolved and decided before the first byte is written, so a denial anywhere
    // leaves the tree exactly as it was (D56).
    let mut planned: Vec<(Op, Option<Location>, Option<String>)> = Vec::new();
    for op in patch.ops {
        let (path, move_to) = match &op {
            Op::Add { path, .. } | Op::Update { path, .. } | Op::Delete { path } => (
                path.clone(),
                match &op {
                    Op::Update { move_to, .. } => move_to.clone(),
                    _ => None,
                },
            ),
        };
        let loc = match gate_path(session, &path)? {
            Ok(loc) => loc,
            Err(out) => return Ok(out),
        };
        // A rename needs write authority on the destination as well, and on the path it leaves.
        if let Some(to) = &move_to
            && let Err(out) = gate_path(session, to)?
        {
            return Ok(out);
        }
        planned.push((op, Some(loc), move_to));
    }

    // New contents, computed against what is on disk now.
    let mut writes: Vec<(Location, String)> = Vec::new();
    let mut removes: Vec<Location> = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    let mut diff = String::new();
    for (op, loc, move_to) in planned {
        let Some(loc) = loc else { continue };
        match op {
            Op::Add { path, lines } => {
                let body = join(&lines);
                writes.push((loc, body.clone()));
                changed.push(format!("added {path} ({} lines)", lines.len()));
                if let Some(d) = super::diff::lines("", &body) {
                    diff.push_str(&d);
                }
            }
            Op::Delete { path } => {
                removes.push(loc);
                changed.push(format!("deleted {path}"));
            }
            Op::Update { path, hunks, .. } => {
                let before = read_beneath(session, &loc).unwrap_or_default();
                let after = match apply(&before, &hunks) {
                    Ok(after) => after,
                    Err(e) => {
                        return Ok(ToolOutput::err(format!(
                            "{path}: {e} (nothing was written)"
                        )));
                    }
                };
                if before == after && move_to.is_none() {
                    changed.push(format!("{path} is unchanged"));
                    continue;
                }
                if let Some(d) = super::diff::lines(&before, &after) {
                    diff.push_str(&d);
                }
                match &move_to {
                    Some(to) => {
                        // The content goes to the new path and the old one is removed, which is
                        // what a rename is.
                        let dest = match gate_path(session, to)? {
                            Ok(loc) => loc,
                            Err(out) => return Ok(out),
                        };
                        writes.push((dest, after));
                        removes.push(loc);
                        changed.push(format!("moved {path} to {to}"));
                    }
                    None => {
                        writes.push((loc, after));
                        changed.push(format!("updated {path}"));
                    }
                }
            }
        }
    }
    // Two operations on the same path would make the order matter; refusing is clearer than
    // picking one.
    let mut seen: Vec<String> = Vec::new();
    for (loc, _) in &writes {
        let key = loc.to_string();
        if seen.contains(&key) {
            return Ok(ToolOutput::err(format!(
                "the patch writes {key} more than once; nothing was written"
            )));
        }
        seen.push(key);
    }

    let mut failures: Vec<String> = Vec::new();
    for (loc, body) in &writes {
        if let Err(e) = super::fs::write_location(session, loc, body.as_bytes()) {
            failures.push(format!("{loc}: {e}"));
        }
    }
    for loc in &removes {
        if let Err(e) = super::fs::remove_location(session, loc) {
            failures.push(format!("{loc}: {e}"));
        }
    }
    if !failures.is_empty() {
        return Ok(ToolOutput::err(format!(
            "the patch was partly applied; these failed: {}",
            failures.join("; ")
        )));
    }
    Ok(ToolOutput {
        diff: (!diff.is_empty()).then_some(diff),
        ..ToolOutput::ok(format!(
            "{} — {}",
            changed.join(", "),
            plural(changed.len())
        ))
    })
}

fn plural(n: usize) -> String {
    if n == 1 {
        "1 file".to_owned()
    } else {
        format!("{n} files")
    }
}

fn join(lines: &[String]) -> String {
    let mut out = lines.join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    out
}

/// Resolves and decides one path, the same way every other write does.
fn gate_path(session: &mut Session, path: &str) -> Result<Result<Location, ToolOutput>> {
    super::fs::gate(session, "patch", Scope::FsWrite, path)
}

fn read_beneath(session: &Session, loc: &Location) -> Option<String> {
    super::fs::open_root(session, loc)
        .ok()
        .and_then(|dir| dir.read_to_string(loc.rel.to_path()).ok())
}

impl Patch {
    /// Parses the patch text. Strict on purpose: a patch that does not parse is refused before
    /// anything is decided, rather than guessed at.
    fn parse(text: &str) -> Result<Patch, String> {
        let mut lines = text.lines().peekable();
        // The begin marker is optional, since models sometimes omit it.
        let mut ops = Vec::new();
        let mut saw_any = false;
        while let Some(line) = lines.next() {
            let line = line.trim_end();
            if line == "*** Begin Patch" || line.trim().is_empty() {
                continue;
            }
            if line == "*** End Patch" {
                break;
            }
            let Some(rest) = line.strip_prefix("*** ") else {
                return Err(format!(
                    "unexpected line outside a file section: `{line}` (a section starts with \
                     `*** Add File:`, `*** Update File:` or `*** Delete File:`)"
                ));
            };
            saw_any = true;
            if let Some(path) = rest.strip_prefix("Add File:") {
                let path = path.trim().to_owned();
                let mut contents = Vec::new();
                while let Some(next) = lines.peek() {
                    if next.starts_with("*** ") {
                        break;
                    }
                    let next = lines.next().unwrap_or_default();
                    // Every line of an added file is a `+` line; a bare line is accepted too,
                    // since the marker carries no meaning for a file that did not exist.
                    contents.push(next.strip_prefix('+').unwrap_or(next).to_owned());
                }
                ops.push(Op::Add {
                    path,
                    lines: contents,
                });
            } else if let Some(path) = rest.strip_prefix("Delete File:") {
                ops.push(Op::Delete {
                    path: path.trim().to_owned(),
                });
            } else if let Some(path) = rest.strip_prefix("Update File:") {
                let path = path.trim().to_owned();
                let (hunks, move_to) = parse_hunks(&mut lines)?;
                ops.push(Op::Update {
                    path,
                    move_to,
                    hunks,
                });
            } else {
                return Err(format!("unknown section `{line}`"));
            }
        }
        if !saw_any || ops.is_empty() {
            return Err("the patch has no file sections".to_owned());
        }
        Ok(Patch { ops })
    }
}

/// The hunks of one update, and its `*** Move to:` if it has one.
fn parse_hunks<'a, I>(lines: &mut Peekable<I>) -> Result<(Vec<Hunk>, Option<String>), String>
where
    I: Iterator<Item = &'a str>,
{
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut move_to = None;
    let mut current: Option<Hunk> = None;
    while let Some(next) = lines.peek() {
        let peeked = next.trim_end();
        // `*** Move to:` belongs to this section; every other `***` starts the next one.
        if peeked.starts_with("*** ") && !peeked.starts_with("*** Move to:") {
            break;
        }
        let line = lines.next().unwrap_or_default().trim_end();
        if let Some(path) = line.strip_prefix("*** Move to:") {
            move_to = Some(path.trim().to_owned());
            continue;
        }
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(Hunk {
                at: line.trim().to_owned(),
                old: Vec::new(),
                new: Vec::new(),
            });
            continue;
        }
        let Some(hunk) = current.as_mut() else {
            // Lines before the first `@@` are accepted as a hunk of their own, since a patch with
            // one replacement often omits the marker.
            if line.starts_with(['+', '-', ' ']) {
                current = Some(Hunk {
                    at: String::new(),
                    old: Vec::new(),
                    new: Vec::new(),
                });
                let hunk = current.as_mut().unwrap_or_else(|| unreachable!());
                push_hunk_line(hunk, line);
            } else if !line.is_empty() {
                return Err(format!("a change line with no `@@` above it: `{line}`"));
            }
            continue;
        };
        push_hunk_line(hunk, line);
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    for hunk in &hunks {
        if hunk.old.is_empty() {
            return Err(format!(
                "a hunk{} removes nothing, so there is nothing to match",
                if hunk.at.is_empty() {
                    String::new()
                } else {
                    format!(" at `{}`", hunk.at)
                }
            ));
        }
    }
    if hunks.is_empty() && move_to.is_none() {
        return Err("an update with no hunks and no move".to_owned());
    }
    Ok((hunks, move_to))
}

fn push_hunk_line(hunk: &mut Hunk, line: &str) {
    match line.strip_prefix('+') {
        Some(added) => hunk.new.push(added.to_owned()),
        None => match line.strip_prefix('-') {
            Some(removed) => hunk.old.push(removed.to_owned()),
            // A context line is in both sides, so the match is anchored and the line is kept.
            None => {
                let kept = line.strip_prefix(' ').unwrap_or(line).to_owned();
                hunk.old.push(kept.clone());
                hunk.new.push(kept);
            }
        },
    }
}

/// Applies the hunks to `text`, refusing if any of them does not match exactly once (D56).
fn apply(text: &str, hunks: &[Hunk]) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    // `str::lines` drops a trailing newline, which is remembered so it can be put back.
    let trailing = text.ends_with('\n') || text.is_empty();
    for hunk in hunks {
        let places = find(&lines, &hunk.old);
        let mut message = String::new();
        match places.len() {
            0 => {
                let _ = write!(
                    message,
                    "hunk `{}` does not match: {} line{} not found",
                    if hunk.at.is_empty() {
                        "at the start"
                    } else {
                        &hunk.at
                    },
                    hunk.old.len(),
                    if hunk.old.len() == 1 { " is" } else { "s are" }
                );
                return Err(message);
            }
            1 => {}
            n => {
                let _ = write!(
                    message,
                    "hunk `{}` matches {n} places; add surrounding lines until it is unique",
                    if hunk.at.is_empty() {
                        "at the start"
                    } else {
                        &hunk.at
                    }
                );
                return Err(message);
            }
        }
        let at = places[0];
        lines.splice(at..at + hunk.old.len(), hunk.new.iter().cloned());
    }
    let mut out = lines.join("\n");
    if trailing && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Where `needle` appears, as a whole run of lines.
fn find(haystack: &[String], needle: &[String]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    (0..=haystack.len() - needle.len())
        .filter(|&i| haystack[i..i + needle.len()] == *needle)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Patch {
        Patch::parse(text).unwrap()
    }

    #[test]
    fn the_sections_parse_into_operations() {
        let p = parse(
            "*** Begin Patch\n\
             *** Add File: src/new.rs\n\
             +fn main() {}\n\
             +// done\n\
             *** Update File: src/lib.rs\n\
             @@ fn area\n\
             -let x = 1;\n\
             +let x = 2;\n\
              keep me\n\
             *** Delete File: old.txt\n\
             *** End Patch\n",
        );
        assert_eq!(
            p.ops,
            [
                Op::Add {
                    path: "src/new.rs".into(),
                    lines: vec!["fn main() {}".into(), "// done".into()],
                },
                Op::Update {
                    path: "src/lib.rs".into(),
                    move_to: None,
                    hunks: vec![Hunk {
                        at: "@@ fn area".into(),
                        old: vec!["let x = 1;".into(), "keep me".into()],
                        new: vec!["let x = 2;".into(), "keep me".into()],
                    }],
                },
                Op::Delete {
                    path: "old.txt".into()
                },
            ]
        );
    }

    #[test]
    fn a_move_and_a_patch_without_the_begin_marker_both_parse() {
        let p = parse(
            "*** Update File: a.txt\n\
             *** Move to: b.txt\n\
             @@\n\
             -old\n\
             +new\n",
        );
        assert_eq!(
            p.ops,
            [Op::Update {
                path: "a.txt".into(),
                move_to: Some("b.txt".into()),
                hunks: vec![Hunk {
                    at: "@@".into(),
                    old: vec!["old".into()],
                    new: vec!["new".into()],
                }],
            }]
        );
    }

    #[test]
    fn a_patch_that_does_not_parse_is_refused_with_the_reason() {
        // Anything outside a section is a mistake worth naming, not something to skip.
        let e = Patch::parse("*** Begin Patch\ngarbage here\n*** End Patch").unwrap_err();
        assert!(e.contains("unexpected line outside a file section"), "{e}");
        assert!(Patch::parse("*** Begin Patch\n*** End Patch").is_err());
        assert!(
            Patch::parse("*** Wat File: x")
                .unwrap_err()
                .contains("unknown section")
        );
        // An update with nothing in it would match anything, so it is refused.
        assert!(
            Patch::parse("*** Update File: a\n")
                .unwrap_err()
                .contains("no hunks"),
            "an empty update"
        );
        // A hunk that only adds has nothing to find.
        let e = Patch::parse("*** Update File: a\n@@\n+only\n").unwrap_err();
        assert!(e.contains("removes nothing"), "{e}");
    }

    #[test]
    fn hunks_apply_to_exactly_the_matching_lines() {
        let text = "fn a() {}\nlet x = 1;\nkeep me\nlet y = 2;\n";
        let hunk = Hunk {
            at: "@@".into(),
            old: vec!["let x = 1;".into(), "keep me".into()],
            new: vec!["let x = 2;".into(), "keep me".into(), "let z = 3;".into()],
        };
        assert_eq!(
            apply(text, &[hunk]).unwrap(),
            "fn a() {}\nlet x = 2;\nkeep me\nlet z = 3;\nlet y = 2;\n"
        );
        // The trailing newline is kept, and a file without one stays without one.
        assert!(!apply("a\nb", &[] as &[Hunk]).unwrap().ends_with('\n'));
        assert_eq!(apply("a\nb", &[] as &[Hunk]).unwrap(), "a\nb");
    }

    #[test]
    fn a_hunk_that_matches_nothing_or_too_much_fails_rather_than_guessing() {
        let text = "one\ntwo\none\ntwo\n";
        let twice = Hunk {
            at: "@@ dup".into(),
            old: vec!["one".into(), "two".into()],
            new: vec!["three".into()],
        };
        let e = apply(text, &[twice]).unwrap_err();
        assert!(e.contains("matches 2 places"), "{e}");
        let missing = Hunk {
            at: "@@ absent".into(),
            old: vec!["not here".into()],
            new: vec!["x".into()],
        };
        let e = apply(text, &[missing]).unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert!(e.contains("@@ absent"), "the message names the hunk: {e}");
    }

    #[test]
    fn an_added_file_keeps_its_lines_and_ends_with_a_newline() {
        assert_eq!(join(&["a".to_owned(), "b".to_owned()]), "a\nb\n");
        assert_eq!(join(&[]), "");
    }
}
