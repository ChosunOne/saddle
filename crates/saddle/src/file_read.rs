//! harness-selected context/attachments and shared no-escape path resolution, not a tool implementation (D147).

use crate::session::Session;
use crate::tools::ToolOutput;
use anyhow::Result;
use cap_std::fs::Dir;
use saddle_cap::{Location, Principal, RelPath, Scope};
use saddle_policy::Request;
use std::io;
use std::path::{Path, PathBuf};

/// The gate alone, for a caller that decides once and then reads the bytes itself (D10).
pub fn gate_read(session: &mut Session, path: &str) -> Result<Result<Location, ToolOutput>> {
    gate(session, "read", Scope::FsRead, path)
}

/// Reads a UTF-8 file as the agent, through the same gate as the read tool: each access is
/// decided and logged, and symlinks are resolved beneath the root. Used by the read tool, by
/// `@file` attachments and by project context files.
pub fn read_text(session: &mut Session, path: &str) -> Result<Result<String, ToolOutput>> {
    let loc = match gate(session, "read", Scope::FsRead, path)? {
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

/// The bytes at a location the gate has already decided, so a caller that must look at content
/// before knowing whether it wants the bytes or the text decides once.
pub fn read_at(session: &Session, loc: &Location) -> Result<Vec<u8>, ToolOutput> {
    match open_root(session, loc).and_then(|dir| dir.read(loc.rel.to_path())) {
        Ok(bytes) => Ok(bytes),
        Err(e) => Err(ToolOutput::err(format!("{}: {e}", display(session, loc)))),
    }
}

/// A path as the model wrote it, or the location when only the location is left.
fn display(session: &Session, loc: &Location) -> String {
    match session.base_of(&loc.root) {
        Some(base) if loc.rel.is_root() => base.display().to_string(),
        Some(base) => base.join(loc.rel.to_path()).display().to_string(),
        None => loc.to_string(),
    }
}

/// UTF-8 or a refusal, from bytes already read.
pub fn text_of(path: &str, bytes: Vec<u8>) -> Result<String, ToolOutput> {
    String::from_utf8(bytes).map_err(|e| {
        ToolOutput::err(format!(
            "{path} is not UTF-8 ({} bytes)",
            e.as_bytes().len()
        ))
    })
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

/// Locates `raw`, decides on it, and decides again on its symlink-resolved location if that
/// differs. Every decision is logged.
pub(crate) fn gate(
    session: &mut Session,
    tool: &str,
    scope: Scope,
    raw: &str,
) -> Result<Result<Location, ToolOutput>> {
    let who = Principal::Agent;
    let loc = match session.locate(&who, tool, scope, raw)? {
        Ok(loc) => loc,
        Err(why) => return Ok(Err(ToolOutput::denied(&why))),
    };
    // A tool-visible directory is resolved by the host, and its link target may be another
    // declared entry (D66); a root is resolved beneath itself, as before.
    let real = match session.is_tool_root(&loc.root) {
        true => match session.resolve_tool(&loc) {
            Ok(real) => real,
            Err(e) => return Ok(Err(ToolOutput::err(format!("{raw}: {e}")))),
        },
        false => match open_root(session, &loc).and_then(|dir| resolve_beneath(&dir, &loc.rel)) {
            Ok(rel) => Location::new(loc.root.clone(), rel),
            Err(e) => return Ok(Err(ToolOutput::err(format!("{raw}: {e}")))),
        },
    };
    // D66: a `[sandbox]` directory is not covered by a grant, and `locate` has already checked
    // the tool against the list the config names and recorded the decision. Nothing inside it is
    // reserved: a grant is a grant, `.git` included (D178).
    if session.is_tool_root(&loc.root) {
        // The resolved location, not the one that came in: a link inside a tool-visible directory
        // would otherwise be walked again by cap-std, which refuses it.
        return Ok(Ok(real));
    }
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
    if let Some(dir) = session.file_roots.get(&loc.root) {
        return dir.try_clone();
    }
    let path = session.base_of(&loc.root).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no root `{}`", loc.root))
    })?;
    Dir::open_ambient_dir(path, cap_std::ambient_authority())
}

/// The location `rel` refers to after following symlinks inside `dir`, one component at a time.
/// Dangling links are followed too, since writing through one creates its target. A link to an
/// absolute path or out of the root is an error, as it would be for cap-std.
pub(crate) fn resolve_beneath(dir: &Dir, rel: &RelPath) -> io::Result<RelPath> {
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
