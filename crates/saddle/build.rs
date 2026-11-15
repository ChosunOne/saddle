//! D148: the one convention no rustc or Clippy lint covers, checked when the binary is built. A
//! path is imported where its module keeps the imports, never written inline; `absolute_paths`
//! catches a path that starts with a module name, `items_after_statements` an import written after a
//! statement and `wildcard_imports` a glob, but `crate::`, `super::` and `self::` written inside an
//! expression are invisible to all three.
//!
//! This lives in a build script rather than a test because a build is the earliest place a person
//! sees it: the message names the file and line, and nothing is compiled until it is fixed.

use std::fmt::Write as _;
use std::fs;
use std::iter;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let Some(crates) = crate_dir.parent() else {
        return ExitCode::SUCCESS;
    };
    // A packaged build has no sibling crates to read; there is nothing to check and nothing to say.
    if !crates.is_dir() {
        return ExitCode::SUCCESS;
    }
    let mut files = Vec::new();
    walk(crates, &mut files);
    files.sort();
    for file in &files {
        println!("cargo::rerun-if-changed={}", file.display());
    }

    let mut offenders = String::new();
    let mut count = 0;
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let shown = file.strip_prefix(crates).unwrap_or(file);
        for (index, line) in text.lines().enumerate() {
            if is_import_or_comment(line) {
                continue;
            }
            if let Some(path) = inline_qualified_path(line) {
                count += 1;
                let _ = writeln!(
                    offenders,
                    "{}:{}: `{path}` is qualified where it is used; import the name at the top of \
                     the module instead",
                    shown.display(),
                    index + 1
                );
            }
        }
    }
    if count == 0 {
        return ExitCode::SUCCESS;
    }
    for line in offenders.lines() {
        println!("cargo::error={line}");
    }
    ExitCode::FAILURE
}

/// Every `.rs` file reachable from `dir`, so nothing is exempt by living in a subdirectory. Build
/// output is skipped: a target directory holds generated sources, which are not ours to edit.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "target" || name.starts_with('.'));
            if !skip {
                walk(&path, out);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// A line whose whole purpose is to say where names come from, or to say something in prose.
fn is_import_or_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    ["use ", "pub use ", "pub(crate) use ", "//", "/*", "*/", "*"]
        .iter()
        .any(|start| trimmed.starts_with(start))
}

/// The first `crate::`, `super::` or `self::` that is code rather than text.
fn inline_qualified_path(line: &str) -> Option<String> {
    let code = without_strings(line);
    let mut from = 0;
    while let Some(found) = code[from..].find("::") {
        let at = from + found;
        from = at + 2;
        let head = &code[..at];
        let Some(name) = ["crate", "super", "self"]
            .iter()
            .find(|name| head.ends_with(*name))
        else {
            continue;
        };
        // `mycrate::x` and `foo::self` are not this shape: the name must stand on its own.
        let before = &head[..head.len() - name.len()];
        let separate = before
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != ':');
        let after = code[at + 2..].chars().next();
        if separate && after.is_some_and(|c| c.is_alphanumeric() || c == '_') {
            let end = code[at..]
                .find(|c: char| !(c.is_alphanumeric() || c == ':' || c == '_' || c == '<'))
                .map_or(code.len(), |offset| at + offset);
            return Some(code[head.len() - name.len()..end].to_owned());
        }
    }
    None
}

/// The line with its string and character literals blanked out, so a path inside a message is not a
/// path.
fn without_strings(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == 'r' && matches!(chars.peek(), Some('"') | Some('#')) {
            let mut hashes = 0;
            while chars.peek() == Some(&'#') {
                chars.next();
                hashes += 1;
            }
            if chars.peek() == Some(&'"') {
                chars.next();
                let closing: String = iter::once('"').chain(iter::repeat_n('#', hashes)).collect();
                let mut tail = String::new();
                for c in chars.by_ref() {
                    tail.push(c);
                    if tail.ends_with(&closing) {
                        break;
                    }
                }
                out.push(' ');
                continue;
            }
            out.push(c);
            out.extend(iter::repeat_n('#', hashes));
            continue;
        }
        if c == '"' {
            let mut escaped = false;
            for c in chars.by_ref() {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    break;
                }
            }
            out.push(' ');
            continue;
        }
        if c == '\'' {
            // A character literal is `'x'` or `'\n'`; a lifetime (`'a`) never closes, so only a
            // shape that closes is swallowed.
            let mut look = chars.clone();
            let body = look.next();
            let closes = match body {
                Some('\\') => {
                    let mut count = 0;
                    for c in look.by_ref() {
                        if c == '\'' {
                            break;
                        }
                        count += 1;
                    }
                    (1..=4).contains(&count) && look.next().is_none_or(|c| c != '\\')
                }
                Some(_) => look.next() == Some('\''),
                None => false,
            };
            if closes {
                chars.next();
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                }
            }
            out.push(' ');
            continue;
        }
        out.push(c);
    }
    out
}
