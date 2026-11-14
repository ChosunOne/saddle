//! bounded, attributed mutation batches; guests supply plans, never authority (D140).

use std::env;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path as NativePath};
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::{Result, anyhow};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt, Permissions, PermissionsExt};
use libc::{O_NOFOLLOW, O_NONBLOCK};
use saddle_cap::roots::normalize;
use saddle_cap::{Location, Principal, RelPath, Scope};
use saddle_loader::{MAX_MUTATION_BYTES, MAX_MUTATIONS};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
use saddle_wit::saddle::host::fs_write::{Change, Expectation};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::session::{Session, session_id};
use crate::tools::fs::resolve_beneath;
use crate::{cancel, fs_access};

struct Planned {
    change: Change,
    root: Dir,
    loc: Location,
}
struct Staged {
    parent: Dir,
    name: String,
    temporary: Option<String>,
}
impl Drop for Staged {
    fn drop(&mut self) {
        if let Some(name) = &self.temporary {
            let _ = self.parent.remove_file(name);
        }
    }
}

fn check_time(deadline: Instant) -> Result<()> {
    if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Err(anyhow!("filesystem mutation cancelled or timed out"));
    }
    Ok(())
}

fn details(change: &Change) -> Value {
    json!({
        "operation":if change.content.is_some() {"write"} else {"remove"},
        "bytes":change.content.as_ref().map_or(0, Vec::len),
        "sha256":change.content.as_ref().map(|bytes| sha256_hex(bytes)),
        "expected":match &change.expected { Expectation::Any => "any", Expectation::Missing => "missing", Expectation::Sha256(_) => "sha256" },
    })
}

fn gate(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    change: &Change,
) -> Result<Option<String>> {
    let request = Request::FsWrite { loc };
    let decision = session.engine.decide(who, &request);
    session
        .audit
        .decision_for_call_details(who, &request, &decision, call, details(change))?;
    Ok((!decision.allow).then_some(format!("denied: {}", decision.explanation)))
}

fn path(session: &Session, value: &str) -> Result<Location> {
    let absolute = if value == "~" || value.starts_with("~/") {
        let home = env::var_os("HOME").ok_or_else(|| anyhow!("home directory is unavailable"))?;
        NativePath::new(&home).join(value.strip_prefix("~/").unwrap_or(""))
    } else if NativePath::new(value).is_absolute() {
        NativePath::new(value).to_path_buf()
    } else {
        session.setup.workspace.join(value)
    };
    session
        .roots()
        .locate(&normalize(&absolute))
        .ok_or_else(|| anyhow!("mutation path is outside every writable policy root"))
}

fn precondition_gate(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    change: &Change,
) -> Result<Option<String>> {
    if !matches!(change.expected, Expectation::Sha256(_)) {
        return Ok(None);
    }
    let request = Request::FsRead { loc };
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({"operation":"mutation-precondition"}),
    )?;
    Ok((!decision.allow).then_some(format!("denied: {}", decision.explanation)))
}

fn expected(plan: &Planned, deadline: Instant) -> Result<()> {
    check_time(deadline)?;
    let metadata = match plan.root.symlink_metadata(plan.loc.rel.to_path()) {
        Ok(metadata) if metadata.is_file() => Some(metadata),
        Ok(_) => return Err(anyhow!("mutation target is not a regular file")),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    match &plan.change.expected {
        Expectation::Any => Ok(()),
        Expectation::Missing if metadata.is_none() => Ok(()),
        Expectation::Missing => Err(anyhow!("mutation target already exists")),
        Expectation::Sha256(want) => {
            if metadata.is_none() {
                return Err(anyhow!("mutation precondition failed: file disappeared"));
            }
            let mut file = fs_access::file(&plan.root, &plan.loc.rel)?;
            let mut hash = Sha256::new();
            let mut total = 0;
            let mut bytes = [0; 65536];
            loop {
                check_time(deadline)?;
                let n = file.read(&mut bytes)?;
                if n == 0 {
                    break;
                }
                total += n;
                if total > MAX_MUTATION_BYTES {
                    return Err(anyhow!("conditional mutation target exceeds 1 MiB"));
                }
                hash.update(&bytes[..n]);
            }
            let actual = format!("{:x}", hash.finalize());
            if !actual.eq_ignore_ascii_case(want) {
                return Err(anyhow!(
                    "mutation precondition failed: file changed; read it again"
                ));
            }
            Ok(())
        }
    }
}

fn parent(plan: &Planned, deadline: Instant) -> Result<(Dir, String)> {
    let path = plan.loc.rel.to_path();
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("mutation requires a file path"))?
        .to_owned();
    let mut dir = plan.root.try_clone()?;
    if let Some(parent) = plan.loc.rel.parent() {
        for part in parent.to_path().components() {
            check_time(deadline)?;
            let Component::Normal(part) = part else {
                continue;
            };
            if plan.change.content.is_some() {
                match dir.create_dir(part) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            dir = fs_access::directory(&dir, &RelPath::new(part)?)?;
        }
    }
    Ok((dir, name))
}

fn stage(plan: &Planned, deadline: Instant) -> Result<Staged> {
    let (parent, name) = parent(plan, deadline)?;
    let mut staged = Staged {
        parent,
        name,
        temporary: None,
    };
    if let Some(bytes) = &plan.change.content {
        let temporary = format!(".saddle-stage-{}", session_id());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .follow(FollowSymlinks::No)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK);
        let mut file = staged.parent.open_with(&temporary, &options)?;
        staged.temporary = Some(temporary);
        for chunk in bytes.chunks(65536) {
            check_time(deadline)?;
            file.write_all(chunk)?;
        }
        if let Ok(metadata) = staged.parent.symlink_metadata(&staged.name) {
            if !metadata.is_file() {
                return Err(anyhow!("mutation target changed type"));
            }
            file.set_permissions(Permissions::from_mode(
                metadata.permissions().mode() & 0o777,
            ))?;
        }
        file.sync_all()?;
    }
    Ok(staged)
}

/// preflights the entire batch before staging or publishing any requested file effects.
pub fn apply(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    changes: Vec<Change>,
    effects: &mut bool,
) -> Result<Result<(), String>> {
    if changes.is_empty() || changes.len() > MAX_MUTATIONS || changes.iter().any(|change| change.path.len() > 4096
        || matches!(&change.expected, Expectation::Sha256(hash) if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())))
        || changes.iter().map(|change| change.content.as_ref().map_or(0, Vec::len)).sum::<usize>() > MAX_MUTATION_BYTES {
        session.audit.event(Kind::Decision, Some(who), Some(Scope::FsWrite), Some(false), json!({"call_id":call,"operation":"apply"}), "invalid mutation batch or exceeds 32 paths/1 MiB")?;
        return Ok(Err("invalid mutation batch or exceeds 32 paths/1 MiB".into()));
    }
    let mut plans: Vec<Planned> = Vec::new();
    for change in changes {
        let loc = match path(session, &change.path) {
            Ok(loc) => loc,
            Err(error) => {
                session.audit.event(Kind::Decision, Some(who), Some(Scope::FsWrite), Some(false), json!({"call_id":call,"path_sha256":sha256_hex(change.path.as_bytes()),"details":details(&change)}), error.to_string())?;
                return Ok(Err(error.to_string()));
            }
        };
        if let Some(why) = gate(session, who, call, &loc, &change)? {
            return Ok(Err(format!("{why}; nothing was written")));
        }
        if let Some(why) = precondition_gate(session, who, call, &loc, &change)? {
            return Ok(Err(format!("{why}; nothing was written")));
        }
        let root = session
            .file_roots
            .get(&loc.root)
            .ok_or_else(|| anyhow!("mutation has no pinned root"))?
            .try_clone()?;
        let real = match resolve_beneath(&root, &loc.rel) {
            Ok(relative) => Location::new(loc.root.clone(), relative),
            Err(error) => return reject(session, who, call, &loc, &change, error.to_string()),
        };
        if real != loc
            && let Some(why) = gate(session, who, call, &real, &change)?
        {
            return Ok(Err(format!("{why}; nothing was written")));
        }
        if real != loc
            && let Some(why) = precondition_gate(session, who, call, &real, &change)?
        {
            return Ok(Err(format!("{why}; nothing was written")));
        }
        if real.rel.is_root() {
            return reject(
                session,
                who,
                call,
                &real,
                &change,
                "mutation requires a file path".into(),
            );
        }
        if plans.iter().any(|plan| {
            plan.loc.root == real.root
                && (plan.loc.rel.to_path().starts_with(real.rel.to_path())
                    || real.rel.to_path().starts_with(plan.loc.rel.to_path()))
        }) {
            return reject(
                session,
                who,
                call,
                &real,
                &change,
                "duplicate or overlapping mutation targets".into(),
            );
        }
        plans.push(Planned {
            change,
            root,
            loc: real,
        });
    }
    let prepared = (|| -> Result<Vec<Staged>> {
        for plan in &plans {
            expected(plan, deadline)?;
        }
        let mut staged = Vec::new();
        for plan in &plans {
            // Missing deletes are idempotent and need no parent creation.
            if plan.change.content.is_none() && !plan.root.try_exists(plan.loc.rel.to_path())? {
                staged.push(None);
            } else {
                staged.push(Some(stage(plan, deadline)?));
            }
        }
        for plan in &plans {
            expected(plan, deadline)?;
        }
        Ok(staged.into_iter().flatten().collect())
    })();
    let mut staged = match prepared {
        Ok(staged) => staged,
        Err(error) => {
            session.audit.event(
                Kind::Decision,
                Some(who),
                Some(Scope::FsWrite),
                Some(false),
                json!({"call_id":call,"operation":"prepare"}),
                error.to_string(),
            )?;
            return Ok(Err(format!("{error}; no target files were changed")));
        }
    };
    // Each publish is atomic for that entry. This is not a filesystem-wide transaction.
    let mut committed = 0;
    for entry in &mut staged {
        let result = (|| -> io::Result<()> {
            if let Some(temporary) = &entry.temporary {
                let metadata = entry.parent.symlink_metadata(temporary)?;
                if !metadata.is_file() {
                    return Err(io::Error::other("staging entry is not a regular file"));
                }
                entry.parent.rename(temporary, &entry.parent, &entry.name)?;
                entry.temporary = None;
            } else {
                match entry.parent.remove_file(&entry.name) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            session.audit.event(
                Kind::Decision,
                Some(who),
                Some(Scope::FsWrite),
                Some(false),
                json!({"call_id":call,"operation":"commit","committed":committed}),
                error.to_string(),
            )?;
            return Ok(Err(format!(
                "mutation batch partly applied ({committed} entries committed): {error}"
            )));
        }
        committed += 1;
        *effects = true;
    }
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::FsWrite),
        Some(true),
        json!({"call_id":call,"operation":"commit","entries":committed}),
        "mutation batch committed",
    )?;
    Ok(Ok(()))
}

fn reject(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    change: &Change,
    why: String,
) -> Result<Result<(), String>> {
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::FsWrite),
        Some(false),
        json!({"call_id":call,"path":loc.to_string(),"details":details(change)}),
        &why,
    )?;
    Ok(Err(format!("{why}; nothing was written")))
}
