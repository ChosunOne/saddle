//! bounded filesystem metadata services; names are filtered by the delegate's policy (D132).

use std::collections::BTreeMap;
use std::env;
use std::path::Path as NativePath;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use cap_std::fs::Dir;
use saddle_cap::roots::normalize;
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_loader::MAX_DIRECTORY_ENTRIES;
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Decision, Kind, Request};
use saddle_wit::saddle::host::fs_list::{Entry, EntryKind, Page};
use saddle_wit::saddle::host::fs_read::{Failure, Path};
use serde_json::{Value, json};

use crate::session::Session;
use crate::tools::fs::resolve_beneath;
use crate::{cancel, fs_access};

const MAX_VISITED: usize = 100_000;

fn expired(deadline: Instant) -> bool {
    cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline
}

fn refuse<T>(
    session: &mut Session,
    who: &Principal,
    call: &str,
    details: Value,
    why: &str,
    managed: bool,
) -> Result<Result<T, Failure>> {
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(if managed {
            Scope::ToolRead
        } else {
            Scope::FsRead
        }),
        Some(false),
        json!({"call_id":call,"details":details}),
        why,
    )?;
    Ok(Err(Failure::Denied(why.into())))
}

fn tool_read_info<'a>(session: &'a Session, loc: &Location) -> (String, &'a [String]) {
    let path = session
        .base_of(&loc.root)
        .map(|base| base.join(loc.rel.to_path()));
    let roles = path
        .as_ref()
        .filter(|path| session.roots().locate(path).is_none())
        .filter(|_| session.is_tool_root(&loc.root))
        .and_then(|path| session.tool_root_for(path))
        .filter(|root| !root.protected)
        .map(|root| root.tools.as_slice())
        .unwrap_or(&[]);
    (
        path.map(|path| path.display().to_string())
            .unwrap_or_else(|| loc.to_string()),
        roles,
    )
}

pub(crate) fn read_decision(
    session: &Session,
    who: &Principal,
    loc: &Location,
    managed: bool,
) -> Decision {
    if managed {
        let (path, roles) = tool_read_info(session, loc);
        session.engine.decide(
            who,
            &Request::ToolRead {
                path: &path,
                available_tools: roles,
            },
        )
    } else {
        session.engine.decide(who, &Request::FsRead { loc })
    }
}

pub(crate) fn gate(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    details: Value,
    managed: bool,
) -> Result<Option<Failure>> {
    let decision = read_decision(session, who, loc, managed);
    if managed {
        let (path, roles) = tool_read_info(session, loc);
        let roles = roles.to_vec();
        let request = Request::ToolRead {
            path: &path,
            available_tools: &roles,
        };
        session
            .audit
            .decision_for_call_details(who, &request, &decision, call, details)?;
    } else {
        session.audit.decision_for_call_details(
            who,
            &Request::FsRead { loc },
            &decision,
            call,
            details,
        )?;
    }
    Ok((!decision.allow).then_some(Failure::Denied(decision.explanation)))
}

pub(crate) fn open_read_path(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    details: Value,
    managed: bool,
) -> Result<Result<(Dir, Location), Failure>> {
    if let Some(failure) = gate(session, who, call, loc, details.clone(), managed)? {
        return Ok(Err(failure));
    }
    let opened = (|| -> Result<(Dir, Location)> {
        let real = if managed {
            session.resolve_tool(loc)?
        } else {
            let dir = session
                .file_roots
                .get(&loc.root)
                .context("plugin has no filesystem root")?;
            Location::new(loc.root.clone(), resolve_beneath(dir, &loc.rel)?)
        };
        let dir = if managed {
            session
                .tool_root(&real.root)
                .and_then(|root| root.handle.as_ref())
                .context("configured tool path cannot be opened as a pinned directory")?
        } else {
            session
                .file_roots
                .get(&real.root)
                .context("plugin has no filesystem root")?
        }
        .try_clone()?;
        Ok((dir, real))
    })();
    let (dir, real) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            session.audit.event(
                Kind::Decision,
                Some(who),
                Some(if managed {
                    Scope::ToolRead
                } else {
                    Scope::FsRead
                }),
                Some(false),
                json!({"call_id":call,"details":details}),
                error.to_string(),
            )?;
            return Ok(Err(Failure::Denied(error.to_string())));
        }
    };
    if real != *loc
        && let Some(failure) = gate(session, who, call, &real, details, managed)?
    {
        return Ok(Err(failure));
    }
    Ok(Ok((dir, real)))
}

pub fn resolve_path(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    value: String,
    managed: bool,
) -> Result<Result<Path, Failure>> {
    let details = json!({"operation":"resolve-path", "path_sha256":sha256_hex(value.as_bytes())});
    if value.len() > 4096 || expired(deadline) {
        return refuse(
            session,
            who,
            call,
            details,
            "plugin path is too long, cancelled or timed out",
            managed,
        );
    }
    let path = if value == "~" || value.starts_with("~/") {
        let Some(home) = env::var_os("HOME") else {
            return refuse(
                session,
                who,
                call,
                details,
                "home directory is unavailable",
                managed,
            );
        };
        NativePath::new(&home).join(value.strip_prefix("~/").unwrap_or(""))
    } else if NativePath::new(&value).is_absolute() {
        NativePath::new(&value).to_path_buf()
    } else {
        session.setup.workspace.join(&value)
    };
    let path = normalize(&path);
    let loc = if managed && session.roots().locate(&path).is_none() {
        session.tool_root_for(&path).and_then(|root| {
            RelPath::new(path.strip_prefix(&root.dir).ok()?)
                .ok()
                .map(|rel| Location::new(root.name.clone(), rel))
        })
    } else if !managed {
        session.roots().locate(&path)
    } else {
        None
    };
    let Some(loc) = loc else {
        return refuse(
            session,
            who,
            call,
            details,
            if managed {
                "plugin path is outside declared tool paths or belongs to a policy root"
            } else {
                "plugin path is outside every root"
            },
            managed,
        );
    };
    if let Some(failure) = gate(
        session,
        who,
        call,
        &loc,
        json!({"operation":"resolve-path"}),
        managed,
    )? {
        return Ok(Err(failure));
    }
    Ok(Ok(Path {
        root: loc.root.to_string(),
        relative: loc.rel.as_slash(),
    }))
}

pub fn read_directory(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    path: Path,
    page: (String, u32),
    managed: bool,
) -> Result<Result<Page, Failure>> {
    let (after, limit) = page;
    let details = json!({"operation":"read-directory", "after":after, "limit":limit});
    if path.root.len() > 64
        || path.relative.len() > 4096
        || after.len() > 1024
        || limit == 0
        || limit as usize > MAX_DIRECTORY_ENTRIES
        || expired(deadline)
    {
        return refuse(
            session,
            who,
            call,
            json!({"operation":"read-directory","root_sha256":sha256_hex(path.root.as_bytes()),"relative_sha256":sha256_hex(path.relative.as_bytes()),"after_sha256":sha256_hex(after.as_bytes()),"limit":limit}),
            "invalid, cancelled or timed-out directory request",
            managed,
        );
    }
    let (root, relative) = match (RootName::new(&path.root), RelPath::new(&path.relative)) {
        (Ok(root), Ok(relative)) => (root, relative),
        _ => {
            return refuse(
                session,
                who,
                call,
                details,
                "invalid root-relative plugin path",
                managed,
            );
        }
    };
    let loc = Location::new(root, relative);
    let (root, real) = match open_read_path(session, who, call, &loc, details, managed)? {
        Ok(opened) => opened,
        Err(failure) => return Ok(Err(failure)),
    };
    let result = (|| -> Result<Result<Page, Failure>> {
        let directory = fs_access::directory(&root, &real.rel)?;
        let mut candidates = BTreeMap::new();
        let mut total = 0;
        let mut skipped = 0;
        for (visited, entry) in directory.entries()?.enumerate() {
            if visited >= MAX_VISITED {
                return Ok(Err(Failure::TooLarge));
            }
            if expired(deadline) {
                return Ok(Err(Failure::Io(
                    "plugin directory read cancelled or timed out".into(),
                )));
            }
            let entry = entry?;
            let Ok(name) = entry.file_name().into_string() else {
                skipped += 1;
                continue;
            };
            if name.len() > 1024 {
                skipped += 1;
                continue;
            }
            // Apply both the requested alias and resolved directory's exclusions, without one
            // audit record per filtered child. A symlink is listed as a symlink, never followed.
            let allowed = [&loc, &real].into_iter().all(|base| {
                let Ok(relative) = RelPath::new(base.rel.to_path().join(&name)) else {
                    return false;
                };
                read_decision(
                    session,
                    who,
                    &Location::new(base.root.clone(), relative),
                    managed,
                )
                .allow
            });
            if !allowed {
                skipped += 1;
                continue;
            }
            total += 1;
            if name <= after {
                continue;
            }
            let kind = entry.file_type()?;
            let kind = if kind.is_symlink() {
                EntryKind::Symlink
            } else if kind.is_dir() {
                EntryKind::Directory
            } else if kind.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            candidates.insert(name, kind);
            if candidates.len() > limit as usize + 1 {
                candidates.pop_last();
            }
        }
        let more = candidates.len() > limit as usize;
        if more {
            candidates.pop_last();
        }
        let entries = candidates
            .into_iter()
            .map(|(name, kind)| Entry { name, kind })
            .collect();
        Ok(Ok(Page {
            entries,
            total,
            skipped,
            more,
        }))
    })();
    Ok(match result {
        Ok(page) => page,
        Err(error) => Err(Failure::Io(error.to_string())),
    })
}
