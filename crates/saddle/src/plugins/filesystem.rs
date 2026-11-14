//! bounded filesystem metadata services; names are filtered by the delegate's policy (D132).

use std::collections::BTreeMap;
use std::env;
use std::path::Path as NativePath;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Context, Result};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_loader::MAX_DIRECTORY_ENTRIES;
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
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
) -> Result<Result<T, Failure>> {
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::FsRead),
        Some(false),
        json!({"call_id":call,"details":details}),
        why,
    )?;
    Ok(Err(Failure::Denied(why.into())))
}

fn gate(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    details: Value,
) -> Result<Option<Failure>> {
    let request = Request::FsRead { loc };
    let decision = session.engine.decide(who, &request);
    session
        .audit
        .decision_for_call_details(who, &request, &decision, call, details)?;
    Ok((!decision.allow).then_some(Failure::Denied(decision.explanation)))
}

pub fn resolve_path(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    value: String,
) -> Result<Result<Path, Failure>> {
    let details = json!({"operation":"resolve-path", "path_sha256":sha256_hex(value.as_bytes())});
    if value.len() > 4096 || expired(deadline) {
        return refuse(
            session,
            who,
            call,
            details,
            "plugin path is too long, cancelled or timed out",
        );
    }
    let path = if value == "~" || value.starts_with("~/") {
        let Some(home) = env::var_os("HOME") else {
            return refuse(session, who, call, details, "home directory is unavailable");
        };
        NativePath::new(&home).join(value.strip_prefix("~/").unwrap_or(""))
    } else if NativePath::new(&value).is_absolute() {
        NativePath::new(&value).to_path_buf()
    } else {
        session.setup.workspace.join(&value)
    };
    // Only true roots are addressable here. D66 tool-visible directories are NOT delegatable roots.
    let Some(loc) = session.roots().locate(&path) else {
        return refuse(
            session,
            who,
            call,
            details,
            "plugin path is outside every root",
        );
    };
    if let Some(failure) = gate(
        session,
        who,
        call,
        &loc,
        json!({"operation":"resolve-path"}),
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
    after: String,
    limit: u32,
) -> Result<Result<Page, Failure>> {
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
            );
        }
    };
    let loc = Location::new(root, relative);
    if let Some(failure) = gate(session, who, call, &loc, details.clone())? {
        return Ok(Err(failure));
    }
    let root = session
        .file_roots
        .get(&loc.root)
        .context("plugin has no filesystem root")?
        .try_clone()?;
    let relative = match resolve_beneath(&root, &loc.rel) {
        Ok(relative) => relative,
        Err(error) => return refuse(session, who, call, details, &error.to_string()),
    };
    let real = Location::new(loc.root.clone(), relative);
    if real != loc
        && let Some(failure) = gate(session, who, call, &real, details)?
    {
        return Ok(Err(failure));
    }
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
                session
                    .engine
                    .decide(
                        who,
                        &Request::FsRead {
                            loc: &Location::new(base.root.clone(), relative),
                        },
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
