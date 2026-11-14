//! a component's host calls are serviced by the session's one engine and audit log (D129).

use std::error::Error as StdError;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_loader::{HostServices, Invocation, MAX_FILE_BYTES, Prepared};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
use saddle_wit::exports::saddle::plugin::tool::{Definition, Output};
use saddle_wit::saddle::host::fs_read::{Failure, Path};
use serde_json::json;

use crate::session::Session;
use crate::tools::fs::resolve_beneath;
use crate::{cancel, fs_access};

pub enum Job {
    Describe,
    Invoke(String),
}
pub enum Reply {
    Description(Definition),
    Output(Output),
}

/// audit and host failures stop the session; they are never downgraded to a broken extension.
#[derive(Debug)]
pub struct Fatal(pub anyhow::Error);
impl Display for Fatal {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "plugin host failure: {:#}", self.0)
    }
}
impl StdError for Fatal {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.0.as_ref())
    }
}

type FileReply = Result<Result<Vec<u8>, Failure>, String>;
enum Event {
    Read {
        who: Principal,
        deadline: Instant,
        path: Path,
        answer: SyncSender<FileReply>,
    },
    Finished(Result<Reply, String>),
}

pub struct Host {
    events: SyncSender<Event>,
}
impl HostServices for Host {
    fn read_file(&mut self, call: &Invocation, path: Path) -> Result<Result<Vec<u8>, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Read {
                who: call.principal().clone(),
                deadline: call.deadline(),
                path,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        let timeout = call.deadline().saturating_duration_since(Instant::now());
        result
            .recv_timeout(timeout)
            .map_err(|_| anyhow!("plugin file request timed out"))?
            .map_err(|why| anyhow!(why))
    }
}

pub fn execute(
    prepared: Arc<Prepared<Host>>,
    session: &mut Session,
    job: Job,
    call: &str,
) -> Result<Result<Reply, String>> {
    let (events, pending) = mpsc::sync_channel(1);
    let host = Host {
        events: events.clone(),
    };
    let cancelled = Arc::new(AtomicBool::new(cancel::FLAG.load(Ordering::Relaxed)));
    let flag = cancelled.clone();
    let worker = thread::Builder::new()
        .name("saddle-plugin".into())
        .spawn(move || {
            let result = match job {
                Job::Describe => prepared.describe(host, flag).map(Reply::Description),
                Job::Invoke(arguments) => {
                    prepared.invoke(host, &arguments, flag).map(Reply::Output)
                }
            }
            .map_err(|e| e.to_string());
            let _ = events.send(Event::Finished(result));
        })
        .map_err(|error| Fatal(anyhow!(error)))?;
    let mut fatal = None;
    let result = loop {
        if cancel::FLAG.load(Ordering::Relaxed) {
            cancelled.store(true, Ordering::Relaxed);
        }
        match pending.recv_timeout(Duration::from_millis(10)) {
            Ok(Event::Read {
                who,
                deadline,
                path,
                answer,
            }) => {
                let result = read_file(session, &who, call, deadline, path);
                let response = match result {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        let why = format!("{error:#}");
                        fatal = Some(error);
                        cancelled.store(true, Ordering::Relaxed);
                        Err(why)
                    }
                };
                let _ = answer.send(response);
            }
            Ok(Event::Finished(result)) => break result,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                break Err("plugin worker stopped without a reply".into());
            }
        }
    };
    cancelled.store(true, Ordering::Relaxed);
    if worker.join().is_err() {
        return Err(Fatal(anyhow!("plugin worker panicked")).into());
    }
    if let Some(error) = fatal {
        return Err(Fatal(error).into());
    }
    Ok(result)
}

fn denied(
    session: &mut Session,
    who: &Principal,
    call: &str,
    path: &Path,
    why: String,
) -> Result<Result<Vec<u8>, Failure>> {
    session.audit.event(Kind::Decision, Some(who), Some(Scope::FsRead), Some(false), json!({"call_id":call, "root_sha256":sha256_hex(path.root.as_bytes()), "relative_sha256":sha256_hex(path.relative.as_bytes())}), &why)?;
    Ok(Err(Failure::Denied(why)))
}

fn read_file(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    path: Path,
) -> Result<Result<Vec<u8>, Failure>> {
    if path.root.len() > 64 || path.relative.len() > 4096 {
        return denied(session, who, call, &path, "plugin path is too long".into());
    }
    let (root, rel) = match (RootName::new(&path.root), RelPath::new(&path.relative)) {
        (Ok(root), Ok(rel)) => (root, rel),
        _ => {
            return denied(
                session,
                who,
                call,
                &path,
                "invalid root-relative plugin path".into(),
            );
        }
    };
    if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return denied(
            session,
            who,
            call,
            &path,
            "plugin read cancelled or timed out".into(),
        );
    }
    let loc = Location::new(root, rel);
    let request = Request::FsRead { loc: &loc };
    let decision = session.engine.decide(who, &request);
    session
        .audit
        .decision_for_call(who, &request, &decision, call)?;
    if !decision.allow {
        return Ok(Err(Failure::Denied(decision.explanation)));
    }
    let dir = session
        .file_roots
        .get(&loc.root)
        .context("plugin has no filesystem root")?
        .try_clone()?;
    let real = match resolve_beneath(&dir, &loc.rel) {
        Ok(rel) => Location::new(loc.root.clone(), rel),
        Err(error) => return denied(session, who, call, &path, error.to_string()),
    };
    if real != loc {
        let request = Request::FsRead { loc: &real };
        let decision = session.engine.decide(who, &request);
        session
            .audit
            .decision_for_call(who, &request, &decision, call)?;
        if !decision.allow {
            return Ok(Err(Failure::Denied(decision.explanation)));
        }
    }
    let bytes = (|| -> Result<Result<Vec<u8>, Failure>> {
        let mut file = fs_access::file(&dir, &real.rel)?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(Err(Failure::Io(
                    "plugin read cancelled or timed out".into(),
                )));
            }
            let want = (MAX_FILE_BYTES + 1 - bytes.len()).min(chunk.len());
            let n = file.read(&mut chunk[..want])?;
            if n == 0 {
                return Ok(Ok(bytes));
            }
            if bytes.len() + n > MAX_FILE_BYTES {
                return Ok(Err(Failure::TooLarge));
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
    })();
    Ok(match bytes {
        Ok(result) => result,
        Err(error) => Err(Failure::Io(error.to_string())),
    })
}
