use crate::session::Session;
use crate::tools::ToolOutput;
use anyhow::Result;
use cap_std::fs::Dir;
use saddle_cap::{Location, Principal, RelPath, Scope};
use saddle_policy::Request;
use std::io;
use std::path::{Path, PathBuf};

pub fn gate_read(session: &mut Session, path: &str) -> Result<Result<Location, ToolOutput>> {
    gate(session, "read", Scope::FsRead, path)
}

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

pub fn read_at(session: &Session, loc: &Location) -> Result<Vec<u8>, ToolOutput> {
    match open_root(session, loc).and_then(|dir| dir.read(loc.rel.to_path())) {
        Ok(bytes) => Ok(bytes),
        Err(e) => Err(ToolOutput::err(format!("{}: {e}", display(session, loc)))),
    }
}

fn display(session: &Session, loc: &Location) -> String {
    match session.base_of(&loc.root) {
        Some(base) if loc.rel.is_root() => base.display().to_string(),
        Some(base) => base.join(loc.rel.to_path()).display().to_string(),
        None => loc.to_string(),
    }
}

pub fn text_of(path: &str, bytes: Vec<u8>) -> Result<String, ToolOutput> {
    String::from_utf8(bytes).map_err(|e| {
        ToolOutput::err(format!(
            "{path} is not UTF-8 ({} bytes)",
            e.as_bytes().len()
        ))
    })
}

pub fn looks_like_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xff, 0xd8, 0xff])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

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
    if session.is_tool_root(&loc.root) {
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
