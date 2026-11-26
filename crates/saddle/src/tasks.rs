use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use saddle_cap::{Principal, RootName, Scope};
use saddle_limits as budgets;
use saddle_policy::{Kind, Request};
use saddle_sandbox::Capture;
use saddle_wit::saddle::host::exec::{Command, Reply};
use saddle_wit::saddle::host::exec_tasks::Task as TaskReply;
use serde_json::json;

use crate::execution::{self, Completed};
use crate::net::bridge::Bridge;
use crate::plugins::HostFailure;
use crate::session::{Session, session_id};

const CANCEL_REAP: Duration = Duration::from_secs(1);

fn holder(who: &Principal) -> Principal {
    match who {
        Principal::Plugin(id) if id.as_str().starts_with("standard-") => Principal::Agent,
        other => other.clone(),
    }
}

enum TaskOutput {
    Live(Arc<Mutex<Capture>>),
    Finished(Reply),
}

struct Task {
    id: String,
    ui_call: Option<String>,
    owner: Principal,
    call: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Completed>>>,
    bridge: Option<Bridge>,
    output: TaskOutput,
    started: Instant,
    stopped: Option<Instant>,
    denials: Vec<String>,
    announced: bool,
    shown: bool,
    wake: Wake,
}

impl Task {
    fn complete(&mut self, output: Reply) {
        self.output = TaskOutput::Finished(output);
        self.stopped = Some(Instant::now());
    }

    fn wakes(&self) -> bool {
        match self.wake {
            Wake::Armed => true,
            Wake::Quiet => false,
            Wake::Unsaid => self.ui_call.is_some(),
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(bridge) = self.bridge.take() {
            bridge.finish();
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Wake {
    #[default]
    Unsaid,
    Armed,
    Quiet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Unavailable,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
            Self::Unavailable => "no longer tracked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    pub id: String,
    pub call: Option<String>,
    pub state: State,
    pub started: Instant,
    pub stopped: Option<Instant>,
}

impl View {
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.stopped
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }
}

pub fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if elapsed.as_secs_f64() < 10.0 {
        format!("{:.1}s", elapsed.as_secs_f64())
    } else if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60)
    }
}

#[derive(Default)]
pub struct Tasks {
    entries: Vec<Task>,
}

impl Tasks {
    pub fn ids(&self) -> Vec<String> {
        self.entries.iter().map(|task| task.id.clone()).collect()
    }

    pub fn associate_new(&mut self, before: &[String], call: &str) {
        for task in &mut self.entries {
            if !before.contains(&task.id) {
                task.ui_call = Some(call.to_owned());
            }
        }
    }

    pub fn views(&self) -> Vec<View> {
        self.entries
            .iter()
            .map(|task| View {
                id: task.id.clone(),
                call: task.ui_call.clone(),
                started: task.started,
                stopped: task.stopped,
                state: match &task.output {
                    TaskOutput::Live(_) => State::Running,
                    TaskOutput::Finished(output) if output.timed_out => State::TimedOut,
                    TaskOutput::Finished(output) if output.cancelled => State::Cancelled,
                    TaskOutput::Finished(output) if output.code == Some(0) => State::Succeeded,
                    TaskOutput::Finished(_) => State::Failed,
                },
            })
            .collect()
    }

    pub fn pending(&self) -> bool {
        self.entries.iter().any(|task| {
            task.wakes() && matches!(task.output, TaskOutput::Finished(_)) && !task.announced
        })
    }

    pub fn waiting(&self) -> bool {
        self.entries
            .iter()
            .any(|task| task.wakes() && task.worker.is_some())
    }

    fn set_wake(&mut self, id: &str, who: &Principal, wake: bool) -> Option<&Task> {
        let caller = holder(who);
        let task = self
            .entries
            .iter_mut()
            .find(|task| task.id == id && holder(&task.owner) == caller)?;
        task.wake = if wake { Wake::Armed } else { Wake::Quiet };
        Some(task)
    }

    pub fn active(&self) -> bool {
        self.entries.iter().any(|task| task.worker.is_some())
    }
}

#[derive(Clone, Copy)]
enum StreamDetail {
    Include,
    Omit,
}

impl StreamDetail {
    fn copy(self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Include => bytes.to_vec(),
            Self::Omit => Vec::new(),
        }
    }
}

fn snapshot(task: &Task, streams: StreamDetail) -> Result<TaskReply> {
    let output = match &task.output {
        TaskOutput::Finished(output) => Reply {
            code: output.code,
            signal: output.signal,
            stdout: streams.copy(&output.stdout),
            stderr: streams.copy(&output.stderr),
            timed_out: output.timed_out,
            cancelled: output.cancelled,
            truncated: output.truncated,
            denials: output.denials.clone(),
        },
        TaskOutput::Live(capture) => {
            let capture = capture
                .lock()
                .map_err(|_| anyhow!("task capture lock poisoned"))?;
            Reply {
                code: None,
                signal: None,
                stdout: streams.copy(&capture.stdout),
                stderr: streams.copy(&capture.stderr),
                timed_out: false,
                cancelled: false,
                truncated: capture.truncated,
                denials: task.denials.clone(),
            }
        }
    };
    Ok(TaskReply {
        id: task.id.clone(),
        running: task.worker.is_some(),
        output,
    })
}

pub fn service(session: &mut Session) -> Result<()> {
    let mut tasks = mem::take(&mut session.tasks);
    let result = (|| {
        for task in &mut tasks.entries {
            if let Some(bridge) = &mut task.bridge {
                match bridge.service_as(session, &task.owner, Some(&task.call)) {
                    Ok(notes) => task.denials.extend(
                        notes.into_iter().take(
                            session
                                .setup
                                .tool_limits
                                .exec_report_denials
                                .saturating_sub(task.denials.len()),
                        ),
                    ),
                    Err(error) => {
                        task.stop.store(true, Ordering::Relaxed);
                        return Err(error);
                    }
                }
            }
            if task
                .worker
                .as_ref()
                .is_some_and(|worker| worker.is_finished())
            {
                let worker = task.worker.take().context("task monitor disappeared")?;
                let completed = worker
                    .join()
                    .map_err(|_| anyhow!("task monitor panicked"))??;
                if let Some(bridge) = task.bridge.take() {
                    bridge.finish();
                }
                let output = execution::finish(
                    session,
                    &task.owner,
                    &task.call,
                    completed,
                    mem::take(&mut task.denials),
                )?;
                task.complete(output);
            }
        }
        Ok(())
    })();
    session.tasks = tasks;
    result.map_err(|error| HostFailure(error).into())
}

pub fn stop_all(session: &mut Session) -> Result<()> {
    for task in &session.tasks.entries {
        task.stop.store(true, Ordering::Relaxed);
    }
    while session.tasks.active() {
        service(session)?;
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

pub fn start(
    session: &mut Session,
    who: &Principal,
    call: &str,
    request: Command,
) -> Result<Result<TaskReply, String>> {
    service(session)?;
    if session.tasks.entries.len() >= budgets::exec_tasks() {
        if let Some(index) = session
            .tasks
            .entries
            .iter()
            .position(|task| task.worker.is_none())
        {
            session.tasks.entries.remove(index);
        } else {
            return Ok(Err("background task retention limit reached".into()));
        }
    }
    let mut prepared = match execution::prepare(
        session,
        who,
        call,
        request.argv,
        Duration::from_secs(request.timeout_secs),
    )? {
        Ok(prepared) => prepared,
        Err(why) => return Ok(Err(why)),
    };
    let tmp = session
        .roots()
        .get(&RootName::tmp())
        .context("no temporary root")?;
    let bridge = Bridge::start(tmp).ok();
    if let Some(bridge) = &bridge {
        prepared.spec.net = Some(bridge.spec());
    }
    let id = format!("task-{}", session_id());
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::Exec),
        Some(true),
        json!({"call_id":call,"operation":"task-start","task":id}),
        "background execution started",
    )?;
    let stop = Arc::new(AtomicBool::new(false));
    let capture = Arc::new(Mutex::new(Capture::default()));
    let flag = stop.clone();
    let live = capture.clone();
    let worker = thread::Builder::new()
        .name("saddle-task".into())
        .spawn(move || prepared.run(&flag, &mut || {}, Some(live)))
        .context("starting task monitor")?;
    let task = Task {
        ui_call: None,
        started: Instant::now(),
        stopped: None,
        id,
        owner: who.clone(),
        call: call.into(),
        stop,
        worker: Some(worker),
        bridge,
        output: TaskOutput::Live(capture),
        denials: Vec::new(),
        announced: false,
        shown: false,
        wake: Wake::Unsaid,
    };
    let reply = snapshot(&task, StreamDetail::Include)?;
    session.tasks.entries.push(task);
    Ok(Ok(reply))
}

pub fn access(
    session: &mut Session,
    who: &Principal,
    call: &str,
    id: Option<&str>,
    cancel: bool,
) -> Result<Result<Vec<TaskReply>, String>> {
    let request = Request::ExecTask {
        operation: if cancel {
            "task-cancel"
        } else {
            "task-inspect"
        },
        id,
    };
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({"operation":if cancel {"task-cancel"} else {"task-inspect"},"task":id}),
    )?;
    if !decision.allow {
        return Ok(Err(format!("denied: {}", decision.explanation)));
    }
    service(session)?;
    let caller = holder(who);
    if cancel {
        let cancelled: Vec<String> = session
            .tasks
            .entries
            .iter()
            .filter(|task| holder(&task.owner) == caller && id.is_none_or(|id| task.id == id))
            .map(|task| task.id.clone())
            .collect();
        for task in &session.tasks.entries {
            if cancelled.contains(&task.id) {
                task.stop.store(true, Ordering::Relaxed);
            }
        }
        let deadline = Instant::now() + CANCEL_REAP;
        while Instant::now() < deadline
            && session
                .tasks
                .entries
                .iter()
                .any(|task| cancelled.contains(&task.id) && task.worker.is_some())
        {
            service(session)?;
            thread::sleep(Duration::from_millis(5));
        }
    }
    let streams = if id.is_some() {
        StreamDetail::Include
    } else {
        StreamDetail::Omit
    };
    let mut replies = Vec::new();
    for task in session
        .tasks
        .entries
        .iter()
        .filter(|task| holder(&task.owner) == caller && id.is_none_or(|id| task.id == id))
    {
        replies.push(snapshot(task, streams)?);
    }
    if id.is_some() && replies.is_empty() {
        session.audit.event(
            Kind::Decision,
            Some(who),
            Some(Scope::Exec),
            Some(false),
            json!({"call_id":call,"operation":"task-ownership","task":id}),
            "unknown task for this principal",
        )?;
        return Ok(Err("unknown task for this principal".into()));
    }
    Ok(Ok(replies))
}

pub fn wake(
    session: &mut Session,
    who: &Principal,
    call: &str,
    id: &str,
    wake: bool,
) -> Result<Result<Vec<TaskReply>, String>> {
    let request = Request::ExecTask {
        operation: "task-wake",
        id: Some(id),
    };
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({"operation":"task-wake","task":id,"wake":wake}),
    )?;
    if !decision.allow {
        return Ok(Err(format!("denied: {}", decision.explanation)));
    }
    service(session)?;
    let Some(task) = session.tasks.set_wake(id, who, wake) else {
        session.audit.event(
            Kind::Decision,
            Some(who),
            Some(Scope::Exec),
            Some(false),
            json!({"call_id":call,"operation":"task-ownership","task":id}),
            "unknown task for this principal",
        )?;
        return Ok(Err("unknown task for this principal".into()));
    };
    let reply = snapshot(task, StreamDetail::Include)?;
    Ok(Ok(vec![reply]))
}

pub fn waiting(session: &mut Session) -> Result<bool> {
    service(session)?;
    Ok(session.tasks.waiting())
}

pub fn pending(session: &mut Session) -> Result<bool> {
    service(session)?;
    Ok(session.tasks.pending())
}

fn notice_line(id: &str, owner: &Principal, state: &str) -> String {
    if holder(owner) == Principal::Agent {
        format!("background {id} ({owner}) {state}; read it with the tasks tool (action: status)")
    } else {
        format!("background {id} ({owner}) {state}; only the principal that started it can read it")
    }
}

pub fn notices(session: &mut Session, model: bool) -> Result<Vec<String>> {
    service(session)?;
    let mut notices = Vec::new();
    for task in &mut session.tasks.entries {
        if let TaskOutput::Finished(output) = &task.output
            && !(if model { task.announced } else { task.shown })
        {
            if model {
                task.announced = true;
            } else {
                task.shown = true;
            }
            let state = if output.timed_out {
                "timed out"
            } else if output.cancelled {
                "cancelled"
            } else if output.code == Some(0) {
                "completed"
            } else {
                "failed"
            };
            notices.push(notice_line(&task.id, &task.owner, state));
        }
    }
    Ok(notices)
}

pub fn human(session: &mut Session, argument: &str) -> Result<String> {
    service(session)?;
    if let Some(id) = argument.strip_prefix("cancel ") {
        let task = session
            .tasks
            .entries
            .iter()
            .find(|task| task.id == id.trim())
            .context("unknown task")?;
        session
            .audit
            .event(
                Kind::Decision,
                Some(&task.owner),
                Some(Scope::Exec),
                Some(true),
                json!({"operation":"task-cancel","task":task.id,"call_id":task.call,"via":"human"}),
                "background cancellation requested by the person",
            )
            .map_err(|error| HostFailure(error.into()))?;
        task.stop.store(true, Ordering::Relaxed);
        return Ok(format!("cancellation requested for {}", task.id));
    }
    if !argument.is_empty() {
        return Ok("usage: /tasks or /tasks cancel <id>".into());
    }
    if session.tasks.entries.is_empty() {
        return Ok("no background tasks".into());
    }
    Ok(session
        .tasks
        .entries
        .iter()
        .map(|task| {
            let mut line = format!(
                "{}  {}  {}  {}",
                task.id,
                task.owner,
                if task.worker.is_some() {
                    "running"
                } else {
                    "finished"
                },
                elapsed_text(
                    task.stopped
                        .unwrap_or_else(Instant::now)
                        .saturating_duration_since(task.started)
                )
            );
            if task.wakes() {
                line.push_str("  wake");
            }
            line
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use saddle_cap::{PluginId, Principal};
    use saddle_sandbox::Capture;
    use saddle_wit::saddle::host::exec::Reply;

    use super::{
        State, StreamDetail, Task, TaskOutput, View, Wake, elapsed_text, notice_line, snapshot,
    };

    #[test]
    fn listing_omits_streams_and_completion_releases_live_capture() {
        let capture = Arc::new(Mutex::new(Capture {
            stdout: b"partial out".to_vec(),
            stderr: b"partial err".to_vec(),
            truncated: true,
        }));
        let live_capture = Arc::downgrade(&capture);
        let mut task = Task {
            id: "task-1".into(),
            ui_call: None,
            owner: Principal::Agent,
            call: "call-1".into(),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            bridge: None,
            output: TaskOutput::Live(capture),
            started: Instant::now(),
            stopped: None,
            denials: Vec::new(),
            announced: false,
            shown: false,
            wake: Wake::Quiet,
        };
        let partial = snapshot(&task, StreamDetail::Include).unwrap().output;
        assert_eq!(partial.stdout, b"partial out");
        assert_eq!(partial.stderr, b"partial err");
        let listed = snapshot(&task, StreamDetail::Omit).unwrap().output;
        assert_eq!(listed.stdout.capacity(), 0);
        assert_eq!(listed.stderr.capacity(), 0);
        assert!(listed.truncated);

        task.complete(Reply {
            code: Some(7),
            signal: None,
            stdout: b"final out".to_vec(),
            stderr: b"final err".to_vec(),
            timed_out: false,
            cancelled: false,
            truncated: true,
            denials: vec!["denial".into()],
        });
        assert!(live_capture.upgrade().is_none());
        let listed = snapshot(&task, StreamDetail::Omit).unwrap();
        assert!(!listed.running);
        assert_eq!(listed.output.stdout.capacity(), 0);
        assert_eq!(listed.output.stderr.capacity(), 0);
        assert_eq!(listed.output.code, Some(7));
        assert!(listed.output.truncated);
        assert_eq!(listed.output.denials, ["denial"]);
        let saved = snapshot(&task, StreamDetail::Include).unwrap().output;
        assert_eq!(saved.stdout, b"final out");
        assert_eq!(saved.stderr, b"final err");
    }

    #[test]
    fn a_completion_notice_says_where_to_read_it_and_whose_it_is() {
        let agent = notice_line("task-1", &Principal::Agent, "completed");
        assert!(agent.contains("read it with the tasks tool"), "{agent}");
        let plugin = notice_line(
            "task-2",
            &Principal::Plugin(PluginId::new("reader").unwrap()),
            "completed",
        );
        assert!(
            plugin.contains("only the principal that started it can read it"),
            "{plugin}"
        );
        assert!(!plugin.contains("tasks tool"), "{plugin}");
    }

    #[test]
    fn a_duration_reads_as_a_stopwatch() {
        assert_eq!(elapsed_text(Duration::from_millis(400)), "0.4s");
        assert_eq!(elapsed_text(Duration::from_secs(9)), "9.0s");
        assert_eq!(elapsed_text(Duration::from_secs(10)), "10s");
        assert_eq!(elapsed_text(Duration::from_secs(65)), "1m 05s");
        assert_eq!(elapsed_text(Duration::from_secs(3725)), "1h 02m");
    }

    #[test]
    fn a_running_task_is_timed_against_the_frame_and_a_finished_one_keeps_its_total() {
        let now = Instant::now();
        let started = now - Duration::from_secs(4);
        let running = View {
            id: "task-1".into(),
            call: None,
            state: State::Running,
            started,
            stopped: None,
        };
        assert_eq!(running.elapsed(now), Duration::from_secs(4));
        assert_eq!(running.elapsed(now + Duration::from_secs(1)).as_secs(), 5);
        let finished = View {
            state: State::Succeeded,
            stopped: Some(started + Duration::from_secs(3)),
            ..running
        };
        assert_eq!(finished.elapsed(now), Duration::from_secs(3));
        assert_eq!(
            finished.elapsed(now + Duration::from_secs(60)),
            Duration::from_secs(3)
        );
    }
}
