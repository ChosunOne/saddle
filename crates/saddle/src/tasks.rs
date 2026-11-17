//! session-owned, principal-isolated background execution, never detached commands (D153).

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

struct Task {
    id: String,
    ui_call: Option<String>,
    owner: Principal,
    call: String,
    stop: Arc<AtomicBool>,
    capture: Arc<Mutex<Capture>>,
    worker: Option<JoinHandle<Result<Completed>>>,
    bridge: Option<Bridge>,
    output: Option<Reply>,
    /// When the command was started, and when its reply landed (D172). The transcript times it from
    /// these rather than from when it first drew the task.
    started: Instant,
    stopped: Option<Instant>,
    denials: Vec<String>,
    announced: bool,
    shown: bool,
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

/// host-owned presentation state, never parsed from a tool's output (D159).
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
    /// When it started, and when it stopped: a running task's elapsed time is measured from the
    /// first, not from when the interface first saw it (D172).
    pub started: Instant,
    pub stopped: Option<Instant>,
}

impl View {
    /// How long the command has been running, or did run (D172).
    pub fn elapsed(&self, now: Instant) -> Duration {
        self.stopped
            .unwrap_or(now)
            .saturating_duration_since(self.started)
    }
}

/// `0.4s`, `12s`, `1m 05s`, `1h 02m`: a duration as a person reads a stopwatch (D172).
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

    /// link only tasks created during this serialized tool invocation, not earlier hook work.
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
                    None => State::Running,
                    Some(output) if output.timed_out => State::TimedOut,
                    Some(output) if output.cancelled => State::Cancelled,
                    Some(output) if output.code == Some(0) => State::Succeeded,
                    Some(_) => State::Failed,
                },
            })
            .collect()
    }

    pub fn pending(&self) -> bool {
        self.entries
            .iter()
            .any(|task| task.ui_call.is_some() && task.output.is_some() && !task.announced)
    }

    pub fn active(&self) -> bool {
        self.entries.iter().any(|task| task.worker.is_some())
    }
}

fn snapshot(task: &Task) -> Result<TaskReply> {
    let output = if let Some(output) = &task.output {
        output.clone()
    } else {
        let capture = task
            .capture
            .lock()
            .map_err(|_| anyhow!("task capture lock poisoned"))?;
        Reply {
            code: None,
            signal: None,
            stdout: capture.stdout.clone(),
            stderr: capture.stderr.clone(),
            timed_out: false,
            cancelled: false,
            truncated: capture.truncated,
            restored: Vec::new(),
            denials: task.denials.clone(),
        }
    };
    Ok(TaskReply {
        id: task.id.clone(),
        running: task.worker.is_some(),
        output,
    })
}

/// keeps the sole engine/audit log on the session thread; monitors own no policy.
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
                task.output = Some(execution::finish(
                    session,
                    &task.owner,
                    &task.call,
                    completed,
                    mem::take(&mut task.denials),
                )?);
                task.stopped = Some(Instant::now());
            }
        }
        Ok(())
    })();
    session.tasks = tasks;
    result.map_err(|error| HostFailure(error).into())
}

/// cancel and join before replacing grants or closing the audit log.
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
        capture,
        worker: Some(worker),
        bridge,
        output: None,
        denials: Vec::new(),
        announced: false,
        shown: false,
    };
    let reply = snapshot(&task)?;
    session.tasks.entries.push(task);
    Ok(Ok(reply))
}

/// each observation/control operation is authorized and audited under its requesting principal.
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
    let selected = session
        .tasks
        .entries
        .iter()
        .filter(|task| &task.owner == who && id.is_none_or(|id| task.id == id));
    let mut replies = Vec::new();
    for task in selected {
        if cancel {
            task.stop.store(true, Ordering::Relaxed);
        }
        let mut reply = snapshot(task)?;
        if id.is_none() {
            reply.output.stdout.clear();
            reply.output.stderr.clear();
        }
        replies.push(reply);
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

/// an idle turn is needed only for an unconsumed completion, not a running task.
pub fn pending(session: &mut Session) -> Result<bool> {
    service(session)?;
    Ok(session.tasks.pending())
}

/// notices contain only host-owned ids and state, never command output from an untrusted process.
pub fn notices(session: &mut Session, model: bool) -> Result<Vec<String>> {
    service(session)?;
    let mut notices = Vec::new();
    for task in &mut session.tasks.entries {
        if let Some(output) = &task.output
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
            notices.push(format!(
                "background {} ({}) {state}; inspect its output with the owning bash tool",
                task.id, task.owner
            ));
        }
    }
    Ok(notices)
}

/// human-only UI control; cancelling cannot widen the process's authority.
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
            format!(
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
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // A finished task shows how long it ran, not how long ago it finished.
        assert_eq!(finished.elapsed(now), Duration::from_secs(3));
        assert_eq!(
            finished.elapsed(now + Duration::from_secs(60)),
            Duration::from_secs(3)
        );
    }
}
