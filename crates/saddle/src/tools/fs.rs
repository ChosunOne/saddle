//! read, write and edit. Files are opened beneath their root with cap-std, so a symlink cannot
//! lead out of the root. A symlink can still lead to another path inside the root, so every access
//! is decided on the requested path and, when it differs, on the path the symlinks resolve to.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cap_std::fs::Dir;
use saddle_cap::{Location, Principal, RelPath, Scope};
use saddle_policy::Request;
use serde_json::Value;

use super::{ToolOutput, num_field, str_field};
use crate::session::Session;

macro_rules! field {
    ($input:expr, $name:literal) => {
        match str_field($input, $name) {
            Ok(v) => v,
            Err(out) => return Ok(out),
        }
    };
}

/// A page of at most this many lines: enough for a whole source file, small enough that a log or
/// a generated table cannot swallow the context (D51).
pub const MAX_READ_LINES: usize = 2_000;

/// And at most this many bytes, because one line can be a minified bundle.
pub const MAX_READ_BYTES: usize = 50 * 1024;

pub fn read(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let path = field!(input, "path");
    let offset = match num_field(input, "offset") {
        Ok(v) => v,
        Err(out) => return Ok(out),
    };
    let limit = match num_field(input, "limit") {
        Ok(v) => v,
        Err(out) => return Ok(out),
    };
    // A picture is handed to the model as a picture rather than refused as "not UTF-8": reading
    // a screenshot is the point of asking for one (D58).
    if let Ok(bytes) = read_bytes(session, path)?
        && looks_like_image(&bytes)
    {
        return Ok(match crate::provider::Image::read(Path::new(path), bytes) {
            Ok(image) => ToolOutput {
                images: vec![image.clone()],
                ..ToolOutput::ok(format!(
                    "{path} is an image: {}×{}, {}",
                    image.width, image.height, image.media_type
                ))
            },
            Err(why) => ToolOutput::err(format!("{path}: {why}")),
        });
    }
    Ok(match read_text(session, path)? {
        Ok(text) => {
            let (body, note) = page(&text, offset, limit);
            match note {
                Some(note) => ToolOutput::ok(format!("{body}\n\n{note}")),
                None => ToolOutput::ok(body),
            }
        }
        Err(out) => out,
    })
}

/// The last character boundary at or below `at`, so a page never splits a UTF-8 sequence.
/// (`str::floor_char_boundary` would do it, but it is newer than the MSRV.)
fn floor_boundary(s: &str, mut at: usize) -> usize {
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// One page of `text`, and a note when there is more of it (D51).
///
/// `offset` is the 1-based line to start at, so it is what `grep` reports and what a note hands
/// back. Lines are not numbered in the body: the model edits by matching text exactly, so a
/// number in front of every line invites it to copy the number into `edit`'s `old` and fail.
fn page(text: &str, offset: Option<u64>, limit: Option<u64>) -> (String, Option<String>) {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let first = offset.unwrap_or(1).max(1) as usize;
    if total == 0 {
        return (String::new(), Some("the file is empty".to_owned()));
    }
    if first > total {
        return (
            String::new(),
            Some(format!(
                "offset {first} is past the end of the file, which has {total} lines"
            )),
        );
    }
    // A page ends on whichever comes first: the line cap, the caller's limit, or the byte budget.
    let want = limit.map_or(MAX_READ_LINES, |n| (n as usize).min(MAX_READ_LINES));
    let mut out = String::new();
    let mut last = first - 1;
    let mut cut_short = false;
    // Whether the page holds a line that was too long to hand over in full.
    let mut truncated = false;
    for (i, line) in lines.iter().enumerate().skip(first - 1) {
        if last - (first - 1) + 1 > want || out.len() + line.len() + 1 > MAX_READ_BYTES {
            cut_short = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        last = i + 1;
    }
    // A single line over the whole budget is handed over truncated rather than refused: the point
    // of the page is to bound the context, and a minified file still has an answer in it.
    if last == first - 1 {
        let line = lines[first - 1];
        let at = floor_boundary(line, MAX_READ_BYTES.min(line.len()));
        out = format!(
            "{}\n[line {first} is {} bytes; the first {at} are shown]",
            &line[..at],
            line.len()
        );
        truncated = true;
        last = first;
        // The page held exactly this one line, so whether there is more depends only on whether
        // the file has a line after it.
        cut_short = false;
    }
    let note = if last < total || cut_short {
        Some(format!(
            "[showing lines {first}-{last} of {total}; call read again with offset={} for more]",
            last + 1
        ))
    } else {
        None
    };
    // A file that fits on one page is returned exactly as it is on disk. Splitting it into lines
    // would drop a trailing newline, and `edit` matches text exactly, so a file would be read
    // without the newline it ends with and then never match (D51).
    if note.is_none() && first == 1 && !truncated {
        return (text.to_owned(), None);
    }
    // A page that ends with the file keeps the newline the file ends with, for the same reason.
    if last == total && text.ends_with('\n') {
        out.push('\n');
    }
    (out, note)
}

/// Reads a UTF-8 file as the agent, through the same gate as the read tool: each access is
/// decided and logged, and symlinks are resolved beneath the root. Used by the read tool, by
/// `@file` attachments and by project context files.
pub fn read_text(session: &mut Session, path: &str) -> Result<Result<String, ToolOutput>> {
    let loc = match gate(session, Scope::FsRead, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(Err(out)),
    };
    Ok(
        match open_root(session, &loc).and_then(|dir| dir.read(loc.rel.to_path())) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(text) => Ok(text),
                Err(e) => Err(ToolOutput::err(format!(
                    "{path} is not UTF-8 ({} bytes)",
                    e.as_bytes().len()
                ))),
            },
            Err(e) => Err(ToolOutput::err(format!("{path}: {e}"))),
        },
    )
}

/// The raw bytes of a file, through the same gate as [`read_text`]. Used for an attachment, which
/// has to look at the bytes before deciding whether they are text or a picture (D58).
pub fn read_bytes(session: &mut Session, path: &str) -> Result<Result<Vec<u8>, ToolOutput>> {
    let loc = match gate(session, Scope::FsRead, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(Err(out)),
    };
    Ok(
        match open_root(session, &loc).and_then(|dir| dir.read(loc.rel.to_path())) {
            Ok(bytes) => Ok(bytes),
            Err(e) => Err(ToolOutput::err(format!("{path}: {e}"))),
        },
    )
}

/// Whether these bytes are one of the image formats the APIs accept. The magic bytes decide, so a
/// text file named `.png` is not mistaken for one (D58).
pub fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

pub fn write(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let path = field!(input, "path");
    let content = field!(input, "content");
    let loc = match gate(session, Scope::FsWrite, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(out),
    };
    // The previous contents, if any, are read for the user's diff only.
    let before = open_root(session, &loc)
        .and_then(|dir| dir.read_to_string(loc.rel.to_path()))
        .ok();
    Ok(match write_beneath(session, &loc, content.as_bytes()) {
        Ok(()) => {
            let what = if before.is_some() {
                "replaced"
            } else {
                "created"
            };
            ToolOutput {
                diff: super::diff::lines(before.as_deref().unwrap_or(""), content),
                ..ToolOutput::ok(format!(
                    "{what} {path} ({} bytes, {} lines)",
                    content.len(),
                    content.lines().count()
                ))
            }
        }
        Err(e) => ToolOutput::err(format!("{path}: {e}")),
    })
}

/// Replaces exactly one occurrence of `old` with `new`.
pub fn edit(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let path = field!(input, "path");
    let old = field!(input, "old");
    let new = field!(input, "new");
    let loc = match gate(session, Scope::FsRead, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(out),
    };
    if let Err(out) = gate(session, Scope::FsWrite, path)? {
        return Ok(out);
    }
    let text = match open_root(session, &loc).and_then(|d| d.read_to_string(loc.rel.to_path())) {
        Ok(t) => t,
        Err(e) => return Ok(ToolOutput::err(format!("{path}: {e}"))),
    };
    match text.matches(old).count() {
        0 => return Ok(ToolOutput::err(format!("{path}: `old` does not occur"))),
        1 => {}
        n => {
            return Ok(ToolOutput::err(format!(
                "{path}: `old` occurs {n} times; include more context"
            )));
        }
    }
    let updated = text.replacen(old, new, 1);
    let line = text[..text.find(old).unwrap_or(0)].lines().count().max(1);
    Ok(match write_beneath(session, &loc, updated.as_bytes()) {
        Ok(()) => ToolOutput {
            diff: super::diff::lines(&text, &updated),
            ..ToolOutput::ok(format!("edited {path} at line {line}"))
        },
        Err(e) => ToolOutput::err(format!("{path}: {e}")),
    })
}

/// Locates `raw`, decides on it, and decides again on its symlink-resolved location if that
/// differs. Every decision is logged.
pub(crate) fn gate(
    session: &mut Session,
    scope: Scope,
    raw: &str,
) -> Result<Result<Location, ToolOutput>> {
    let who = Principal::Agent;
    let loc = match session.locate(&who, scope, raw)? {
        Ok(loc) => loc,
        Err(why) => return Ok(Err(ToolOutput::denied(&why))),
    };
    let real = match open_root(session, &loc).and_then(|dir| resolve_beneath(&dir, &loc.rel)) {
        Ok(rel) => Location::new(loc.root.clone(), rel),
        Err(e) => return Ok(Err(ToolOutput::err(format!("{raw}: {e}")))),
    };
    for target in [&loc, &real] {
        let req = match scope {
            Scope::FsRead => Request::FsRead { loc: target },
            _ => Request::FsWrite { loc: target },
        };
        let d = session.decide(&who, &req)?;
        if !d.allow {
            return Ok(Err(ToolOutput::denied(&d.explanation)));
        }
        if real == loc {
            break;
        }
    }
    Ok(Ok(loc))
}

pub(crate) fn open_root(session: &Session, loc: &Location) -> io::Result<Dir> {
    let path = session.roots().get(&loc.root).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no root `{}`", loc.root))
    })?;
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
}

/// The location `rel` refers to after following symlinks inside `dir`, one component at a time.
/// Dangling links are followed too, since writing through one creates its target. A link to an
/// absolute path or out of the root is an error, as it would be for cap-std.
fn resolve_beneath(dir: &Dir, rel: &RelPath) -> io::Result<RelPath> {
    const MAX_HOPS: usize = 40;
    let mut pending: Vec<String> = Vec::new();
    push_components(&mut pending, &rel.to_path())?;
    let mut resolved: Vec<String> = Vec::new();
    let mut hops = 0;
    while let Some(part) = pending.pop() {
        match part.as_str() {
            "." => continue,
            ".." => {
                if resolved.pop().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "symlink leads out of the root",
                    ));
                }
                continue;
            }
            _ => {}
        }
        let candidate: PathBuf = resolved.iter().chain([&part]).collect();
        match dir.symlink_metadata(&candidate) {
            Ok(meta) if meta.file_type().is_symlink() => {
                hops += 1;
                if hops > MAX_HOPS {
                    return Err(io::Error::other("too many levels of symlinks"));
                }
                let target = dir.read_link_contents(&candidate)?;
                if target.is_absolute() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "symlink to an absolute path",
                    ));
                }
                push_components(&mut pending, &target)?;
            }
            Ok(_) => resolved.push(part),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                resolved.push(part)
            }
            Err(e) => return Err(e),
        }
    }
    RelPath::new(resolved.iter().collect::<PathBuf>()).map_err(io::Error::other)
}

/// Pushes the components of `path` so that the first one is popped first.
fn push_components(stack: &mut Vec<String>, path: &Path) -> io::Result<()> {
    let parts: Vec<String> = path
        .components()
        .map(|c| {
            c.as_os_str()
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path is not UTF-8"))
        })
        .collect::<io::Result<_>>()?;
    stack.extend(parts.into_iter().rev());
    Ok(())
}

/// Writes `bytes` at a resolved location, creating parent directories. Shared with `patch`, so
/// both go through one place that knows how a write beneath a root is done (D56).
pub(crate) fn write_location(session: &Session, loc: &Location, bytes: &[u8]) -> io::Result<()> {
    write_beneath(session, loc, bytes)
}

/// Removes a file at a resolved location. A missing file is not an error: the patch said it should
/// be gone, and it is.
pub(crate) fn remove_location(session: &Session, loc: &Location) -> io::Result<()> {
    let dir = open_root(session, loc)?;
    match dir.remove_file(loc.rel.to_path()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn write_beneath(session: &Session, loc: &Location, bytes: &[u8]) -> io::Result<()> {
    let dir = open_root(session, loc)?;
    if let Some(parent) = loc.rel.parent().filter(|p| !p.is_root()) {
        dir.create_dir_all(parent.to_path())?;
    }
    dir.write(loc.rel.to_path(), bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_file_is_one_page_and_is_returned_exactly_as_it_is_on_disk() {
        // The trailing newline survives, because `edit` matches text exactly (D51).
        let (body, note) = page("one\ntwo\n", None, None);
        assert_eq!(body, "one\ntwo\n");
        assert_eq!(note, None);
        assert_eq!(page("no newline", None, None).0, "no newline");
    }

    #[test]
    fn a_long_file_is_cut_at_the_line_cap_and_offers_the_next_offset() {
        let text: String = (1..=5_000).map(|n| format!("line {n}\n")).collect();
        let (body, note) = page(&text, None, None);
        assert_eq!(body.lines().count(), MAX_READ_LINES);
        assert_eq!(body.lines().next(), Some("line 1"));
        assert_eq!(body.lines().last(), Some("line 2000"));
        // The note names the next line and the argument that asks for it (D51).
        let note = note.unwrap();
        assert!(note.contains("lines 1-2000 of 5000"), "{note}");
        assert!(note.contains("offset=2001"), "{note}");

        // The second page starts where the note said.
        let (body, note) = page(&text, Some(2001), None);
        assert_eq!(body.lines().next(), Some("line 2001"));
        assert!(note.unwrap().contains("offset=4001"));
    }

    #[test]
    fn a_limit_and_an_offset_narrow_the_page() {
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        let (body, note) = page(&text, Some(10), Some(5));
        assert_eq!(body, "line 10\nline 11\nline 12\nline 13\nline 14");
        assert!(note.unwrap().contains("lines 10-14 of 100"));

        // A limit larger than the page cap is the cap, not the caller's number.
        let text: String = (1..=5_000).map(|n| format!("line {n}\n")).collect();
        let (body, _) = page(&text, None, Some(1_000_000));
        assert_eq!(body.lines().count(), MAX_READ_LINES);

        // The last page carries no note, because there is nothing after it.
        let (_, note) = page(&text, Some(4_990), Some(20));
        assert_eq!(note, None);
    }

    #[test]
    fn the_byte_budget_ends_a_page_that_the_line_cap_would_not() {
        // One kilobyte a line: fifty of them is over the budget and well under the line cap.
        let line = "x".repeat(1024);
        let text = vec![line.clone(); 100].join("\n");
        let (body, note) = page(&text, None, None);
        assert!(body.len() <= MAX_READ_BYTES, "{} bytes", body.len());
        assert!(body.lines().count() < MAX_READ_LINES);
        assert!(note.unwrap().contains("for more"));
    }

    #[test]
    fn one_line_over_the_budget_is_shown_truncated_rather_than_refused() {
        // A minified bundle: a single line far past the budget.
        let text = "y".repeat(MAX_READ_BYTES * 3);
        let (body, note) = page(&text, None, None);
        assert!(body.len() < MAX_READ_BYTES + 200, "{} bytes", body.len());
        assert!(body.contains("is"), "{body}");
        assert!(body.contains("bytes; the first"), "{body}");
        assert_eq!(note, None, "there is no next line to offer");
    }

    #[test]
    fn an_offset_past_the_end_and_an_empty_file_say_so() {
        let (body, note) = page("a\nb\n", Some(9), None);
        assert_eq!(body, "");
        let note = note.unwrap();
        assert!(note.contains("past the end"), "{note}");
        let (body, note) = page("", None, None);
        assert_eq!(body, "");
        assert_eq!(note.as_deref(), Some("the file is empty"));
        // Offset 0 is the first line, not an error: it is what a caller who counts from zero sends.
        assert_eq!(page("a\nb\n", Some(0), None).0, "a\nb\n");
    }

    #[test]
    fn the_page_does_not_number_lines() {
        // A number in front of every line invites it into `edit`'s `old`, which then fails.
        let (body, _) = page("fn main() {}\n", None, None);
        assert_eq!(body, "fn main() {}\n");
    }
}
