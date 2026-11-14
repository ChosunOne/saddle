//! component host calls use the session's one engine and audit log (D129, D131).

use std::error::Error as StdError;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{Read, Seek, SeekFrom};
use std::mem;
use std::path::Path as NativePath;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Error as HostError, Result, anyhow};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_loader::{HostServices, Invocation, MAX_FILE_BYTES, Prepared};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
use saddle_wit::exports::saddle::plugin::tool::{Definition, Output};
use saddle_wit::saddle::host::fs_list::Page as DirectoryPage;
use saddle_wit::saddle::host::fs_read::{Failure, Path};
use saddle_wit::saddle::host::fs_read_range::Chunk;
use saddle_wit::saddle::host::tool_result::ImageInfo;
use serde_json::json;

use super::filesystem;
use crate::provider::Image;
use crate::session::Session;
use crate::tools::fs::resolve_beneath;
use crate::{cancel, fs_access};

pub enum Job {
    Describe,
    Invoke(String),
}
pub enum Reply {
    Description(Definition),
    Output { output: Output, images: Vec<Image> },
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

type HostReply<T, E = Failure> = Result<Result<T, E>, String>;
type FileReply = HostReply<Chunk>;
enum Event {
    Read {
        who: Principal,
        deadline: Instant,
        path: Path,
        range: Option<(u64, u32)>,
        answer: SyncSender<FileReply>,
    },
    Resolve {
        who: Principal,
        deadline: Instant,
        value: String,
        answer: SyncSender<HostReply<Path>>,
    },
    List {
        who: Principal,
        deadline: Instant,
        path: Path,
        after: String,
        limit: u32,
        answer: SyncSender<HostReply<DirectoryPage>>,
    },
    Image {
        who: Principal,
        deadline: Instant,
        name: String,
        bytes: Vec<u8>,
        answer: SyncSender<HostReply<ImageInfo, String>>,
    },
    Finished(Result<Reply, String>),
}

pub struct Host {
    events: SyncSender<Event>,
}
impl HostServices for Host {
    fn emit_image(
        &mut self,
        call: &Invocation,
        name: String,
        bytes: Vec<u8>,
    ) -> Result<Result<ImageInfo, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Image {
                who: call.principal().clone(),
                deadline: call.deadline(),
                name,
                bytes,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin image request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn resolve_path(&mut self, call: &Invocation, value: String) -> Result<Result<Path, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Resolve {
                who: call.principal().clone(),
                deadline: call.deadline(),
                value,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin path request timed out"))?
            .map_err(|why| anyhow!(why))
    }

    fn read_directory(
        &mut self,
        call: &Invocation,
        path: Path,
        after: String,
        limit: u32,
    ) -> Result<Result<DirectoryPage, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::List {
                who: call.principal().clone(),
                deadline: call.deadline(),
                path,
                after,
                limit,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin directory request timed out"))?
            .map_err(|why| anyhow!(why))
    }

    fn read_file(&mut self, call: &Invocation, path: Path) -> Result<Result<Vec<u8>, Failure>> {
        Ok(self.request(call, path, None)?.map(|chunk| chunk.bytes))
    }

    fn read_range(
        &mut self,
        call: &Invocation,
        path: Path,
        offset: u64,
        limit: u32,
    ) -> Result<Result<Chunk, Failure>> {
        self.request(call, path, Some((offset, limit)))
    }
}
impl Host {
    fn request(
        &mut self,
        call: &Invocation,
        path: Path,
        range: Option<(u64, u32)>,
    ) -> Result<Result<Chunk, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Read {
                who: call.principal().clone(),
                deadline: call.deadline(),
                path,
                range,
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
    let invoking = matches!(job, Job::Invoke(_));
    let mut images = Vec::new();
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
                    prepared
                        .invoke(host, &arguments, flag)
                        .map(|output| Reply::Output {
                            output,
                            images: Vec::new(),
                        })
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
                range,
                answer,
            }) => {
                respond(
                    read_file(session, &who, call, deadline, path, range),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Resolve {
                who,
                deadline,
                value,
                answer,
            }) => {
                respond(
                    filesystem::resolve_path(session, &who, call, deadline, value),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::List {
                who,
                deadline,
                path,
                after,
                limit,
                answer,
            }) => {
                respond(
                    filesystem::read_directory(session, &who, call, deadline, path, after, limit),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Image {
                who,
                deadline,
                name,
                bytes,
                answer,
            }) => {
                respond(
                    emit_image(
                        session,
                        &who,
                        call,
                        deadline,
                        name,
                        bytes,
                        if invoking { Some(&mut images) } else { None },
                    ),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Finished(result)) => {
                break result.map(|reply| match reply {
                    Reply::Output { output, .. } if !output.is_error => Reply::Output {
                        output,
                        images: mem::take(&mut images),
                    },
                    reply => reply,
                });
            }
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

fn emit_image(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    name: String,
    bytes: Vec<u8>,
    images: Option<&mut Vec<Image>>,
) -> Result<Result<ImageInfo, String>> {
    let request = Request::Plain(Scope::Tool);
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(who, &request, &decision, call, json!({"operation":"emit-image", "bytes":bytes.len(), "sha256":sha256_hex(&bytes), "name_sha256":sha256_hex(name.as_bytes())}))?;
    if !decision.allow {
        return Ok(Err(decision.explanation));
    }
    if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Ok(Err("plugin image cancelled or timed out".into()));
    }
    let Some(images) = images else {
        return Ok(Err("images may only be emitted during invocation".into()));
    };
    if !images.is_empty() || name.len() > 256 {
        return Ok(Err("image result exceeds name or count limit".into()));
    }
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    let mut image = match Image::read(NativePath::new(&name), bytes) {
        Ok(image) => image,
        Err(why) => return Ok(Err(why)),
    };
    image.name = format!("{who}:{}", image.name);
    let info = ImageInfo {
        media_type: image.media_type.clone(),
        width: image.width,
        height: image.height,
    };
    images.push(image);
    Ok(Ok(info))
}

fn respond<T, E>(
    result: Result<Result<T, E>>,
    answer: SyncSender<HostReply<T, E>>,
    fatal: &mut Option<HostError>,
    cancelled: &AtomicBool,
) {
    let response = match result {
        Ok(value) => Ok(value),
        Err(error) => {
            let why = format!("{error:#}");
            *fatal = Some(error);
            cancelled.store(true, Ordering::Relaxed);
            Err(why)
        }
    };
    let _ = answer.send(response);
}

fn denied(
    session: &mut Session,
    who: &Principal,
    call: &str,
    path: &Path,
    range: Option<(u64, u32)>,
    why: String,
) -> Result<Result<Chunk, Failure>> {
    let mut args = json!({"call_id":call, "root_sha256":sha256_hex(path.root.as_bytes()), "relative_sha256":sha256_hex(path.relative.as_bytes())});
    if let Some((offset, limit)) = range {
        args["offset"] = offset.into();
        args["limit"] = limit.into();
    }
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::FsRead),
        Some(false),
        args,
        &why,
    )?;
    Ok(Err(Failure::Denied(why)))
}

fn gate(
    session: &mut Session,
    who: &Principal,
    call: &str,
    loc: &Location,
    range: Option<(u64, u32)>,
) -> Result<Option<Failure>> {
    let request = Request::FsRead { loc };
    let decision = session.engine.decide(who, &request);
    if let Some(range) = range {
        session
            .audit
            .decision_for_read_range(who, &request, &decision, call, range)?;
    } else {
        session
            .audit
            .decision_for_call(who, &request, &decision, call)?;
    }
    Ok((!decision.allow).then_some(Failure::Denied(decision.explanation)))
}

fn read_file(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    path: Path,
    range: Option<(u64, u32)>,
) -> Result<Result<Chunk, Failure>> {
    if range.is_some_and(|(_, limit)| limit == 0 || u64::from(limit) > MAX_FILE_BYTES as u64) {
        return denied(
            session,
            who,
            call,
            &path,
            range,
            "read range limit must be between 1 and 65536 bytes".into(),
        );
    }
    if path.root.len() > 64 || path.relative.len() > 4096 {
        return denied(
            session,
            who,
            call,
            &path,
            range,
            "plugin path is too long".into(),
        );
    }
    let (root, rel) = match (RootName::new(&path.root), RelPath::new(&path.relative)) {
        (Ok(root), Ok(rel)) => (root, rel),
        _ => {
            return denied(
                session,
                who,
                call,
                &path,
                range,
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
            range,
            "plugin read cancelled or timed out".into(),
        );
    }
    let loc = Location::new(root, rel);
    if let Some(failure) = gate(session, who, call, &loc, range)? {
        return Ok(Err(failure));
    }
    let dir = session
        .file_roots
        .get(&loc.root)
        .context("plugin has no filesystem root")?
        .try_clone()?;
    let real = match resolve_beneath(&dir, &loc.rel) {
        Ok(rel) => Location::new(loc.root.clone(), rel),
        Err(error) => return denied(session, who, call, &path, range, error.to_string()),
    };
    if real != loc
        && let Some(failure) = gate(session, who, call, &real, range)?
    {
        return Ok(Err(failure));
    }
    let bytes = (|| -> Result<Result<Chunk, Failure>> {
        let mut file = fs_access::file(&dir, &real.rel)?;
        if let Some((offset, _)) = range {
            file.seek(SeekFrom::Start(offset))?;
        }
        let max = range.map_or(MAX_FILE_BYTES, |(_, limit)| limit as usize);
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(Err(Failure::Io(
                    "plugin read cancelled or timed out".into(),
                )));
            }
            let want = (max + 1 - bytes.len()).min(chunk.len());
            let n = file.read(&mut chunk[..want])?;
            if n == 0 {
                return Ok(Ok(Chunk { bytes, eof: true }));
            }
            if bytes.len() + n > max && range.is_none() {
                return Ok(Err(Failure::TooLarge));
            }
            bytes.extend_from_slice(&chunk[..n]);
            if bytes.len() > max {
                bytes.truncate(max);
                return Ok(Ok(Chunk { bytes, eof: false }));
            }
        }
    })();
    Ok(match bytes {
        Ok(result) => result,
        Err(error) => Err(Failure::Io(error.to_string())),
    })
}
