//! component host calls use the session's one engine and audit log (D129, D131).

use saddle_limits as budgets;
use std::error::Error as StdError;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::mem;
use std::path::Path as NativePath;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use crate::tasks;
use anyhow::{Context, Error as HostError, Result, anyhow};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_loader::{HostServices, Invocation, Prepared, Service, ServiceReply, TaskOperation};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
use saddle_wit::events::exports::saddle::plugin::plugin_events::{
    Event as PluginEvent, Subscription,
};
use saddle_wit::exports::saddle::plugin::tool::{Definition, Output};
use saddle_wit::harness::exports::saddle::plugin::observer::{
    Event as HarnessEvent, Kind as HarnessKind,
};
use saddle_wit::saddle::host::exec_tasks::Task;
use saddle_wit::saddle::host::fs_list::Page as DirectoryPage;
use saddle_wit::saddle::host::fs_read::{Failure, Path};
use saddle_wit::saddle::host::fs_read_range::Chunk;
use saddle_wit::saddle::host::fs_write::Change;
use saddle_wit::saddle::host::run_binary::{Launch, Reply as LaunchReply};
use saddle_wit::saddle::host::tool_result::ImageInfo;
use saddle_wit::saddle::widgets::types::Panel;
use saddle_wit::veto::exports::saddle::plugin::veto::{Decision as VetoDecision, PendingToolCall};
use serde_json::json;

use super::{filesystem, hooks, mutations, services};
use crate::execution;
use crate::network::Operation;
use crate::provider::{Image, Provenance};
use crate::session::Session;
use crate::tools::timeout;
use crate::{cancel, fs_access, questions, transcript};
use crate::{network, search_transport};
use saddle_wit::saddle::host::ask::{Outcome as AskOutcome, Question};
use saddle_wit::saddle::host::exec::{Command, Reply as ExecReply};
use saddle_wit::saddle::host::http::{Request as HttpRequest, Response as HttpResponse};
use saddle_wit::saddle::host::web_search::Snapshot as SearchSnapshot;

pub enum Job {
    Describe,
    Invoke(String),
    HarnessSubscriptions,
    EventSubscriptions,
    Observe(HarnessEvent),
    Receive(PluginEvent),
    Veto(PendingToolCall),
    Render,
}
pub enum Reply {
    Description(Definition),
    HarnessSubscriptions(Vec<HarnessKind>),
    EventSubscriptions(Vec<Subscription>),
    Handled(Result<(), String>),
    Veto(VetoDecision),
    Panel(Result<Panel, String>),
    Output {
        output: Output,
        images: Vec<Image>,
        diff: Option<String>,
    },
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
    Service {
        who: Principal,
        origin: Provenance,
        deadline: Instant,
        service: Service,
        answer: SyncSender<HostReply<ServiceReply, String>>,
    },
    Emit {
        who: Principal,
        deadline: Instant,
        name: String,
        data: String,
        answer: SyncSender<HostReply<(), String>>,
    },
    Task {
        who: Principal,
        operation: TaskOperation,
        answer: SyncSender<HostReply<Vec<Task>, String>>,
    },
    Preimage {
        who: Principal,
        deadline: Instant,
        path: Path,
        answer: SyncSender<HostReply<Option<Vec<u8>>>>,
    },
    Label {
        who: Principal,
        deadline: Instant,
        root: String,
        answer: SyncSender<HostReply<String>>,
    },
    Http {
        who: Principal,
        deadline: Instant,
        request: HttpRequest,
        answer: SyncSender<HostReply<HttpResponse, String>>,
    },
    Search {
        who: Principal,
        deadline: Instant,
        query: Option<(String, u32)>,
        answer: SyncSender<HostReply<Option<SearchSnapshot>, String>>,
    },
    Execute {
        who: Principal,
        request: Command,
        answer: SyncSender<HostReply<ExecReply, String>>,
    },
    Launch {
        who: Principal,
        request: Launch,
        allow_children: bool,
        deadline: Instant,
        answer: SyncSender<HostReply<LaunchReply, String>>,
    },
    Diff {
        who: Principal,
        text: String,
        answer: SyncSender<HostReply<(), String>>,
    },
    Ask {
        who: Principal,
        question: Question,
        allowance: Option<Duration>,
        answer: SyncSender<HostReply<AskOutcome, String>>,
    },
    Mutate {
        who: Principal,
        deadline: Instant,
        changes: Vec<Change>,
        answer: SyncSender<HostReply<(), String>>,
    },
    Query {
        who: Principal,
        deadline: Instant,
        sql: String,
        /// The conversation the guest asked for: empty for the one in hand (D163).
        session: String,
        answer: SyncSender<HostReply<String, String>>,
    },
    Read {
        managed: bool,
        who: Principal,
        deadline: Instant,
        path: Path,
        range: Option<(u64, u32)>,
        answer: SyncSender<FileReply>,
    },
    Resolve {
        managed: bool,
        who: Principal,
        deadline: Instant,
        value: String,
        answer: SyncSender<HostReply<Path>>,
    },
    List {
        managed: bool,
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
    call_id: String,
    version: String,
    component_sha256: String,
}
impl Host {
    fn request_search(
        &mut self,
        call: &Invocation,
        query: Option<(String, u32)>,
    ) -> Result<Result<Option<SearchSnapshot>, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Search {
                who: call.principal().clone(),
                deadline: call
                    .deadline()
                    .min(Instant::now() + Duration::from_secs(budgets::network_seconds() as u64)),
                query,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin search request timed out"))?
            .map_err(|why| anyhow!(why))
    }
}
impl HostServices for Host {
    fn service(
        &mut self,
        call: &Invocation,
        service: Service,
    ) -> Result<Result<ServiceReply, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        let origin = Provenance {
            principal: call.principal().to_string(),
            scope: services::scope(&service),
            call_id: self.call_id.clone(),
            component_sha256: self.component_sha256.clone(),
            version: self.version.clone(),
        };
        self.events
            .send(Event::Service {
                who: call.principal().clone(),
                origin,
                deadline: call.deadline(),
                service,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("extension service timed out"))?
            .map_err(|why| anyhow!(why))
    }

    fn emit_event(
        &mut self,
        call: &Invocation,
        name: String,
        data: String,
    ) -> Result<Result<(), String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Emit {
                who: call.principal().clone(),
                deadline: call.deadline(),
                name,
                data,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin event emission timed out"))?
            .map_err(|why| anyhow!(why))
    }

    fn read_preimage(
        &mut self,
        call: &Invocation,
        path: Path,
    ) -> Result<Result<Option<Vec<u8>>, Failure>> {
        let (answer, reply) = mpsc::sync_channel(1);
        self.events.send(Event::Preimage {
            who: call.principal().clone(),
            deadline: call.deadline(),
            path,
            answer,
        })?;
        reply
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(HostError::from)?
            .map_err(HostError::msg)
    }

    fn label_root(&mut self, call: &Invocation, root: String) -> Result<Result<String, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Label {
                who: call.principal().clone(),
                deadline: call.deadline(),
                root,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("root label request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn http_get(
        &mut self,
        call: &Invocation,
        request: HttpRequest,
    ) -> Result<Result<HttpResponse, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Http {
                who: call.principal().clone(),
                deadline: call
                    .deadline()
                    .min(Instant::now() + Duration::from_secs(budgets::network_seconds() as u64)),
                request,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin HTTP request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn search_web(
        &mut self,
        call: &Invocation,
        query: String,
        count: u32,
    ) -> Result<Result<SearchSnapshot, String>> {
        Ok(self
            .request_search(call, Some((query, count)))?
            .and_then(|snapshot| {
                snapshot.ok_or_else(|| "search transport returned no snapshot".into())
            }))
    }
    fn last_search(&mut self, call: &Invocation) -> Result<Result<Option<SearchSnapshot>, String>> {
        self.request_search(call, None)
    }
    fn execute(
        &mut self,
        call: &Invocation,
        request: Command,
    ) -> Result<Result<ExecReply, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Execute {
                who: call.principal().clone(),
                request,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin execution request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn run_binary(
        &mut self,
        call: &Invocation,
        request: Launch,
        allowance: Duration,
        allow_children: bool,
    ) -> Result<Result<LaunchReply, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        let deadline = call.deadline().min(Instant::now() + allowance);
        self.events
            .send(Event::Launch {
                who: call.principal().clone(),
                request,
                allow_children,
                deadline,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("named launch request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn task(
        &mut self,
        call: &Invocation,
        operation: TaskOperation,
    ) -> Result<Result<Vec<Task>, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Task {
                who: call.principal().clone(),
                operation,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin task request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn emit_diff(&mut self, call: &Invocation, text: String) -> Result<Result<(), String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Diff {
                who: call.principal().clone(),
                text,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin diff request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn ask(
        &mut self,
        call: &Invocation,
        question: Question,
        allowance: Option<Duration>,
    ) -> Result<Result<AskOutcome, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Ask {
                who: call.principal().clone(),
                question,
                allowance,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        // A person answers in their own time: with no allowance this waits for the answer itself,
        // which the session sends when the question is answered, dismissed or the turn is cancelled
        // (D197). An allowance is only outlived by a poll, so an answer is never dropped on a
        // rounding edge.
        let received = match allowance {
            Some(allowance) => result.recv_timeout(allowance + Duration::from_millis(500)),
            None => result.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        received
            .map_err(|_| anyhow!("question answer timed out"))?
            .map_err(|why| anyhow!(why))
    }

    fn apply_changes(
        &mut self,
        call: &Invocation,
        changes: Vec<Change>,
    ) -> Result<Result<(), String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Mutate {
                who: call.principal().clone(),
                deadline: call.deadline(),
                changes,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin mutation request timed out"))?
            .map_err(|why| anyhow!(why))
    }
    fn resolve_tool_path(
        &mut self,
        call: &Invocation,
        value: String,
    ) -> Result<Result<Path, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Resolve {
                managed: true,
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
    fn read_tool_range(
        &mut self,
        call: &Invocation,
        path: Path,
        offset: u64,
        limit: u32,
    ) -> Result<Result<Chunk, Failure>> {
        self.request(call, path, Some((offset, limit)), true)
    }
    fn read_tool_directory(
        &mut self,
        call: &Invocation,
        path: Path,
        after: String,
        limit: u32,
    ) -> Result<Result<DirectoryPage, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::List {
                managed: true,
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
    fn query_transcript(
        &mut self,
        call: &Invocation,
        sql: String,
        session: String,
    ) -> Result<Result<String, String>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Query {
                who: call.principal().clone(),
                deadline: call.deadline(),
                sql,
                session,
                answer,
            })
            .map_err(|_| anyhow!("plugin session ended"))?;
        result
            .recv_timeout(call.deadline().saturating_duration_since(Instant::now()))
            .map_err(|_| anyhow!("plugin transcript request timed out"))?
            .map_err(|why| anyhow!(why))
    }
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
                managed: false,
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
                managed: false,
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
        Ok(self
            .request(call, path, None, false)?
            .map(|chunk| chunk.bytes))
    }

    fn read_range(
        &mut self,
        call: &Invocation,
        path: Path,
        offset: u64,
        limit: u32,
    ) -> Result<Result<Chunk, Failure>> {
        self.request(call, path, Some((offset, limit)), false)
    }
}
impl Host {
    fn request(
        &mut self,
        call: &Invocation,
        path: Path,
        range: Option<(u64, u32)>,
        managed: bool,
    ) -> Result<Result<Chunk, Failure>> {
        let (answer, result) = mpsc::sync_channel(1);
        self.events
            .send(Event::Read {
                managed,
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
    let limits = session.setup.tool_limits.clone();
    budgets::using(&limits, || execute_inner(prepared, session, job, call))
}

/// Whether a job is part of a turn the person can cancel, and so should stop when they do: a tool
/// call, a hook, a veto or a panel drawn while the turn runs. Registration — `describe` and the two
/// subscription reads — happens between turns, at load or reload, and is bounded by the plugin
/// deadline instead (D183).
fn turn_scoped(job: &Job) -> bool {
    matches!(
        job,
        Job::Invoke(_) | Job::Observe(_) | Job::Receive(_) | Job::Veto(_) | Job::Render
    )
}

fn execute_inner(
    prepared: Arc<Prepared<Host>>,
    session: &mut Session,
    job: Job,
    call: &str,
) -> Result<Result<Reply, String>> {
    let invoking = matches!(
        job,
        Job::Invoke(_) | Job::Observe(_) | Job::Receive(_) | Job::Veto(_)
    );
    let rendering = matches!(job, Job::Render);
    let mut filesystem_effects = false;
    let mut execution_effects = false;
    let mut network_effects = false;
    let mut diff = None;
    let mut images = Vec::new();
    let (events, pending) = mpsc::sync_channel(1);
    let host = Host {
        events: events.clone(),
        call_id: call.into(),
        version: prepared.version().into(),
        component_sha256: prepared
            .hash()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    };
    // D183: only a job that belongs to a turn is cancelled with one. Loading a component answers
    // `describe` and reads its subscriptions with no turn in flight, so a flag left set by an Esc
    // must not decide whether the session can register its tools.
    let turn_scoped = turn_scoped(&job);
    let cancelled = Arc::new(AtomicBool::new(
        turn_scoped && cancel::FLAG.load(Ordering::Relaxed),
    ));
    let flag = cancelled.clone();
    let deadline = timeout::deadline();
    // Reserve host frames beyond Wasmtime's configured guest stack; never silently exceed the thread.
    let stack_bytes = prepared
        .stack_bytes()
        .checked_add(2 * 1024 * 1024)
        .context("plugin thread stack size overflow")?;
    let worker = thread::Builder::new()
        .stack_size(stack_bytes)
        .name("saddle-plugin".into())
        .spawn(move || {
            let result = match job {
                Job::Veto(call) => prepared.veto(host, call, flag, deadline).map(Reply::Veto),
                Job::Render => prepared.render(host, flag, deadline).map(Reply::Panel),
                Job::Describe => prepared.describe(host, flag).map(Reply::Description),
                Job::HarnessSubscriptions => prepared
                    .harness_subscriptions(host, flag)
                    .map(Reply::HarnessSubscriptions),
                Job::EventSubscriptions => prepared
                    .event_subscriptions(host, flag)
                    .map(Reply::EventSubscriptions),
                Job::Observe(event) => prepared
                    .observe(host, event, flag, deadline)
                    .map(Reply::Handled),
                Job::Receive(event) => prepared
                    .receive(host, event, flag, deadline)
                    .map(Reply::Handled),
                Job::Invoke(arguments) => prepared
                    .invoke_until(host, &arguments, flag, deadline)
                    .map(|output| Reply::Output {
                        output,
                        diff: None,
                        images: Vec::new(),
                    }),
            }
            .map_err(|e| e.to_string());
            let _ = events.send(Event::Finished(result));
        })
        .map_err(|error| Fatal(anyhow!(error)))?;
    let mut fatal = None;
    let result = loop {
        if let Err(error) = tasks::service(session) {
            fatal = Some(error);
            cancelled.store(true, Ordering::Relaxed);
        }
        if (turn_scoped && cancel::FLAG.load(Ordering::Relaxed)) || timeout::expired() {
            cancelled.store(true, Ordering::Relaxed);
        }
        match pending.recv_timeout(Duration::from_millis(10)) {
            Ok(Event::Service {
                who,
                origin,
                deadline,
                service,
                answer,
            }) => {
                let permitted = fatal.is_none()
                    && (invoking || (rendering && matches!(&service, Service::PersistGet(_))));
                respond(
                    services::execute(session, &who, origin, service, permitted, deadline),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Emit {
                who,
                deadline,
                name,
                data,
                answer,
            }) => {
                respond(
                    hooks::publish(session, &who, call, &name, data, invoking, deadline),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Preimage {
                who,
                deadline,
                path,
                answer,
            }) => {
                respond(
                    read_preimage(session, &who, call, deadline, path),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }

            Ok(Event::Label {
                who,
                deadline,
                root,
                answer,
            }) => {
                respond(
                    filesystem::label_root(session, &who, call, deadline, root),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Http {
                who,
                deadline,
                request,
                answer,
            }) => {
                let result = if invoking {
                    network_effects = true;
                    network::request(
                        session,
                        &who,
                        call,
                        Operation {
                            method: "GET",
                            url: &request.url,
                            headers: request.headers,
                            body: "",
                            limit: budgets::http_capture_bytes(),
                            deadline,
                            redirects: true,
                        },
                    )
                } else {
                    session
                        .audit
                        .event(
                            Kind::Decision,
                            Some(&who),
                            Some(Scope::Net),
                            Some(false),
                            json!({"call_id":call,"operation":"http"}),
                            "HTTP is refused during describe",
                        )
                        .map(|_| Err("HTTP is refused during describe".into()))
                        .map_err(Into::into)
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Search {
                who,
                deadline,
                query,
                answer,
            }) => {
                let result = if invoking {
                    match query {
                        Some((query, count)) => {
                            network_effects = true;
                            search_transport::search(session, &who, call, query, count, deadline)
                                .map(|result| result.map(Some))
                        }
                        None => search_transport::last(session, &who, call),
                    }
                } else {
                    session
                        .audit
                        .event(
                            Kind::Decision,
                            Some(&who),
                            Some(Scope::Net),
                            Some(false),
                            json!({"call_id":call,"operation":"search"}),
                            "search is refused during describe",
                        )
                        .map(|_| Err("search is refused during describe".into()))
                        .map_err(Into::into)
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Task {
                who,
                operation,
                answer,
            }) => {
                let result = if !invoking {
                    Ok(Err("task operations are refused during describe".into()))
                } else {
                    match operation {
                        TaskOperation::Start(request) => {
                            execution_effects = true;
                            tasks::start(session, &who, call, request)
                                .map(|result| result.map(|task| vec![task]))
                        }
                        TaskOperation::Inspect(id) => {
                            tasks::access(session, &who, call, Some(&id), false)
                        }
                        TaskOperation::List => tasks::access(session, &who, call, None, false),
                        TaskOperation::Cancel(id) => {
                            tasks::access(session, &who, call, Some(&id), true)
                        }
                        TaskOperation::Wake(id, wake) => {
                            tasks::wake(session, &who, call, &id, wake)
                        }
                    }
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Execute {
                who,
                request,
                answer,
            }) => {
                let result = if invoking {
                    execution::execute(
                        session,
                        &who,
                        call,
                        request.argv,
                        Duration::from_secs(request.timeout_secs),
                        &mut execution_effects,
                    )
                } else {
                    session
                        .audit
                        .event(
                            Kind::Decision,
                            Some(&who),
                            Some(Scope::Exec),
                            Some(false),
                            json!({"call_id":call,"operation":"exec"}),
                            "execution is refused during describe",
                        )
                        .map(|_| Err("execution is refused during describe".into()))
                        .map_err(Into::into)
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Launch {
                who,
                request,
                allow_children,
                deadline,
                answer,
            }) => {
                let result = if invoking {
                    execution::launch(
                        session,
                        &who,
                        call,
                        request,
                        allow_children,
                        deadline,
                        &mut execution_effects,
                    )
                } else {
                    session.audit.event(Kind::Decision, Some(&who), Some(Scope::RunBinary), Some(false), json!({"call_id":call,"operation":"run-binary","profile":request.profile}), "named launch is refused outside invocation")
                        .map(|_| Err("named launch is refused outside invocation".into()))
                        .map_err(Into::into)
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Diff { who, text, answer }) => {
                let request = Request::Plain(Scope::Tool);
                let decision = session.engine.decide(&who, &request);
                let result = session.audit.decision_for_call_details(&who, &request, &decision, call, json!({"operation":"emit-diff", "bytes":text.len(), "sha256":sha256_hex(text.as_bytes())})).map(|_| {
                    if !invoking || !decision.allow || text.len() > budgets::diff_bytes() || diff.is_some() {
                        Err("diff presentation refused outside invocation or exceeds count/size/permission limit".into())
                    } else {
                        diff = Some(text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect());
                        Ok(())
                    }
                }).map_err(Into::into);
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Ask {
                who,
                question,
                allowance,
                answer,
            }) => {
                let result = if invoking {
                    questions::ask(session, &who, call, allowance, question)
                } else {
                    session
                        .audit
                        .event(
                            Kind::Decision,
                            Some(&who),
                            Some(Scope::Tool),
                            Some(false),
                            json!({"call_id":call,"operation":"ask"}),
                            "a question is refused outside invocation",
                        )
                        .map(|_| Err("a question is refused outside invocation".into()))
                        .map_err(HostError::from)
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Mutate {
                who,
                deadline,
                changes,
                answer,
            }) => {
                let result = if invoking {
                    mutations::apply(
                        session,
                        &who,
                        call,
                        deadline,
                        changes,
                        &mut filesystem_effects,
                    )
                } else {
                    session
                        .audit
                        .event(
                            Kind::Decision,
                            Some(&who),
                            Some(Scope::FsWrite),
                            Some(false),
                            json!({"call_id":call,"operation":"apply"}),
                            "filesystem mutations are refused during describe",
                        )
                        .map(|_| Err("filesystem mutations are refused during describe".into()))
                        .map_err(HostError::from)
                };
                respond(result, answer, &mut fatal, &cancelled);
            }
            Ok(Event::Query {
                who,
                deadline,
                sql,
                session: target,
                answer,
            }) => {
                respond(
                    transcript::execute(session, &who, &sql, &target, Some(call), Some(deadline))
                        .map(|result| result.map(|value| value.to_string())),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Read {
                managed,
                who,
                deadline,
                path,
                range,
                answer,
            }) => {
                respond(
                    read_file(session, &who, call, deadline, path, range, managed),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::Resolve {
                managed,
                who,
                deadline,
                value,
                answer,
            }) => {
                respond(
                    filesystem::resolve_path(session, &who, call, deadline, value, managed),
                    answer,
                    &mut fatal,
                    &cancelled,
                );
            }
            Ok(Event::List {
                managed,
                who,
                deadline,
                path,
                after,
                limit,
                answer,
            }) => {
                respond(
                    filesystem::read_directory(
                        session,
                        &who,
                        call,
                        deadline,
                        path,
                        (after, limit),
                        managed,
                    ),
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
                let result = result.map_err(|why| {
                    if filesystem_effects { format!("{why}; filesystem changes were committed before failure; inspect paths before retrying") } else if execution_effects { format!("{why}; execution may have produced effects before failure; inspect results before retrying") } else if network_effects { format!("{why}; network requests may have completed before failure") } else { why }
                });
                break result.map(|reply| match reply {
                    Reply::Output { output, .. } if !output.is_error => Reply::Output {
                        output,
                        diff: diff.take(),
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
    if images.len() >= budgets::image_count()
        || name.len() > budgets::image_name_bytes()
        || bytes.len().saturating_add(
            images
                .iter()
                .map(|image| {
                    image.data.len() / 4 * 3
                        - image.data.chars().rev().take_while(|c| *c == '=').count()
                })
                .sum::<usize>(),
        ) > budgets::image_bytes()
    {
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
    managed: bool,
) -> Result<Result<Chunk, Failure>> {
    let mut args = json!({"call_id":call, "root_sha256":sha256_hex(path.root.as_bytes()), "relative_sha256":sha256_hex(path.relative.as_bytes())});
    if let Some((offset, limit)) = range {
        args["offset"] = offset.into();
        args["limit"] = limit.into();
    }
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(if managed {
            Scope::ToolRead
        } else {
            Scope::FsRead
        }),
        Some(false),
        args,
        &why,
    )?;
    Ok(Err(Failure::Denied(why)))
}

fn read_file(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    path: Path,
    range: Option<(u64, u32)>,
    managed: bool,
) -> Result<Result<Chunk, Failure>> {
    if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Ok(Err(Failure::Io(
            "preimage read cancelled or timed out".into(),
        )));
    }
    let (path, managed) = filesystem::decode_path(path, managed);
    if range
        .is_some_and(|(_, limit)| limit == 0 || u64::from(limit) > budgets::range_bytes() as u64)
    {
        return denied(
            session,
            who,
            call,
            &path,
            range,
            "read range limit must be positive and within the configured range budget".into(),
            managed,
        );
    }
    if path.root.len() > 64 || path.relative.len() > budgets::path_bytes() {
        return denied(
            session,
            who,
            call,
            &path,
            range,
            "plugin path is too long".into(),
            managed,
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
                managed,
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
            managed,
        );
    }
    let loc = Location::new(root, rel);
    let details = range.map_or_else(
        || json!({}),
        |(offset, limit)| json!({"offset":offset,"limit":limit}),
    );
    let (dir, real) = match filesystem::open_read_path(session, who, call, &loc, details, managed)?
    {
        Ok(opened) => opened,
        Err(failure) => return Ok(Err(failure)),
    };
    let bytes = (|| -> Result<Result<Chunk, Failure>> {
        let mut file = fs_access::file(&dir, &real.rel)?;
        if let Some((offset, _)) = range {
            file.seek(SeekFrom::Start(offset))?;
        }
        let max = range.map_or(budgets::range_bytes(), |(_, limit)| limit as usize);
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

fn read_preimage(
    session: &mut Session,
    who: &Principal,
    call: &str,
    deadline: Instant,
    path: Path,
) -> Result<Result<Option<Vec<u8>>, Failure>> {
    let (path, managed) = filesystem::decode_path(path, false);
    let (root, relative) = match (RootName::new(&path.root), RelPath::new(&path.relative)) {
        (Ok(root), Ok(relative)) if path.relative.len() <= budgets::path_bytes() => {
            (root, relative)
        }
        _ => return Ok(Err(Failure::InvalidPath("invalid preimage path".into()))),
    };
    let loc = Location::new(root, relative);
    let (dir, real) = match filesystem::open_read_path(
        session,
        who,
        call,
        &loc,
        json!({"operation":"read-preimage"}),
        managed,
    )? {
        Ok(opened) => opened,
        Err(failure) => return Ok(Err(failure)),
    };
    let mut file = match fs_access::file(&dir, &real.rel) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(Ok(None));
        }
        Err(error) => return Ok(Err(Failure::Io(error.to_string()))),
    };
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Ok(Err(Failure::Io(
                "preimage read cancelled or timed out".into(),
            )));
        }
        let want = chunk.len().min(
            budgets::mutation_bytes()
                .saturating_add(1)
                .saturating_sub(bytes.len()),
        );
        match file.read(&mut chunk[..want]) {
            Ok(0) => return Ok(Ok(Some(bytes))),
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(error) => return Ok(Err(Failure::Io(error.to_string()))),
        }
        if bytes.len() > budgets::mutation_bytes() {
            return Ok(Err(Failure::TooLarge));
        }
    }
}
