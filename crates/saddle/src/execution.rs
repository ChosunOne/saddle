//! attributed subprocess effects and shared reservation lifetimes (D142, D152, D153).

use saddle_limits as budgets;
use std::env;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
use saddle_cap::sandbox::{ExecRequest, SandboxSpec, lower_exec};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_policy::{Kind, Request};
use saddle_sandbox::{Capture, Output, run_observed};
use saddle_wit::saddle::host::exec::Reply;
use serde_json::json;

use crate::net::bridge::Bridge;
use crate::reserved::{Change, Snapshot};
use crate::session::Session;
use crate::tools::timeout;
use crate::{cancel, reserved, tasks};

pub const HELPER_ARG: &str = "__sandbox";

/// pins the reservation lock through process termination and reserved-path restoration.
pub struct Prepared {
    pub spec: SandboxSpec,
    deadline: Instant,
    ws: Dir,
    before: Snapshot,
    _lock: File,
    capture_bytes: usize,
}

pub struct Completed {
    output: io::Result<Output>,
    changes: Vec<(PathBuf, Change)>,
}

/// one lowering for foreground and background execution; no second policy or sandbox source.
pub fn prepare(
    session: &mut Session,
    who: &Principal,
    call: &str,
    argv: Vec<String>,
    requested_timeout: Duration,
) -> Result<Result<Prepared, String>> {
    if argv.is_empty()
        || argv.len() > budgets::exec_arguments()
        || argv.iter().map(String::len).sum::<usize>() > budgets::exec_argument_bytes()
        || argv.iter().any(|value| value.contains('\0'))
    {
        return Ok(Err("invalid command or exceeds argument bounds".into()));
    }
    let cwd = Location::new(RootName::workspace(), RelPath::default());
    let request = Request::Exec {
        argv: &argv,
        cwd: &cwd,
    };
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({"operation":"exec"}),
    )?;
    if !decision.allow {
        return Ok(Err(format!("denied: {}", decision.explanation)));
    }
    if let Err(reason) = &session.sandbox {
        return Ok(Err(format!("denied: exec is unavailable: {reason}")));
    }
    let grants = session
        .engine
        .policy()
        .grants(who)
        .context("execution principal has no grants")?;
    let lowered = lower_exec(
        grants,
        session.roots(),
        &session.setup.extras,
        ExecRequest {
            argv,
            cwd: &cwd,
            host_env: &session.host_env,
        },
    )?;
    let timeout = requested_timeout
        .min(Duration::from_secs(budgets::exec_seconds() as u64))
        .max(Duration::from_secs(1))
        .min(lowered.timeout);
    let timeout = match timeout::remaining(timeout) {
        Ok(timeout) => timeout,
        Err(why) => return Ok(Err(why.to_string())),
    };
    let deadline = Instant::now() + timeout;
    let ws = session
        .file_roots
        .get(&RootName::workspace())
        .context("no pinned workspace")?
        .try_clone()?;
    // A separately opened description (not dup): flock serializes cooperating Saddle sessions too.
    let lock = ws
        .open_with(
            ".",
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY),
        )?
        .into_std();
    // SAFETY: lock is an owned, live directory fd. No path or filesystem authority is added.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(Err("workspace shell execution is busy; inspect or cancel the active task before starting another command".into()));
        }
        return Err(error).context("locking workspace execution");
    }
    let before = reserved::take(&ws).context("snapshotting reserved paths")?;
    Ok(Ok(Prepared {
        spec: lowered.spec,
        deadline,
        ws,
        before,
        _lock: lock,
        capture_bytes: budgets::exec_capture_bytes(),
    }))
}

impl Prepared {
    /// process cleanup and restoration run on the monitor, even while the model is busy.
    pub fn run(
        self,
        cancel: &AtomicBool,
        service: &mut dyn FnMut(),
        capture: Option<Arc<Mutex<Capture>>>,
    ) -> Result<Completed> {
        let helper = env::current_exe().context("locating the saddle binary")?;
        let left = self.deadline.saturating_duration_since(Instant::now());
        let output = if left.is_zero() {
            Ok(Output {
                code: None,
                signal: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
                cancelled: false,
                truncated: false,
            })
        } else {
            run_observed(
                &helper,
                &[HELPER_ARG],
                &self.spec,
                left,
                self.capture_bytes,
                cancel,
                service,
                capture,
            )
        };
        let changes =
            reserved::restore(&self.ws, &self.before).context("restoring reserved paths")?;
        // self, including the workspace lock, drops only after restore.
        Ok(Completed { output, changes })
    }
}

pub fn finish(
    session: &mut Session,
    who: &Principal,
    call: &str,
    completed: Completed,
    denials: Vec<String>,
) -> Result<Reply> {
    for (path, change) in &completed.changes {
        session.audit.event(Kind::Restore, Some(who), Some(Scope::FsWrite), Some(false), json!({"call_id":call,"path":format!("workspace:{}",path.display()),"change":format!("{change:?}").to_lowercase()}), format!("exec changed reserved path workspace:{}; restored",path.display()))?;
    }
    let output = completed.output.context("running the sandbox helper")?;
    session.audit.event(Kind::Decision, Some(who), Some(Scope::Exec), Some(true), json!({"call_id":call,"operation":"exec-completed","code":output.code,"signal":output.signal,"timed_out":output.timed_out,"cancelled":output.cancelled,"stdout_bytes":output.stdout.len(),"stderr_bytes":output.stderr.len()}), "execution completed")?;
    Ok(Reply {
        code: output.code,
        signal: output.signal,
        stdout: output.stdout,
        stderr: output.stderr,
        timed_out: output.timed_out,
        cancelled: output.cancelled,
        truncated: output.truncated,
        restored: completed
            .changes
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect(),
        denials,
    })
}

pub fn execute(
    session: &mut Session,
    who: &Principal,
    call: &str,
    argv: Vec<String>,
    requested_timeout: Duration,
    effects: &mut bool,
) -> Result<Result<Reply, String>> {
    tasks::service(session)?;
    let mut prepared = match prepare(session, who, call, argv, requested_timeout)? {
        Ok(prepared) => prepared,
        Err(why) => return Ok(Err(why)),
    };
    let tmp = session
        .roots()
        .get(&RootName::tmp())
        .context("no temporary root")?
        .to_path_buf();
    let mut bridge = Bridge::start(&tmp).ok();
    if let Some(bridge) = &bridge {
        prepared.spec.net = Some(bridge.spec());
    }
    let stop = AtomicBool::new(cancel::FLAG.load(Ordering::Relaxed));
    let mut denials = Vec::new();
    let mut failure = None;
    *effects = true;
    let completed = prepared.run(
        &stop,
        &mut || {
            if cancel::FLAG.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(bridge) = bridge.as_mut() {
                match bridge.service_as(session, who, Some(call)) {
                    Ok(notes) => denials.extend(
                        notes
                            .into_iter()
                            .take(budgets::exec_report_denials().saturating_sub(denials.len())),
                    ),
                    Err(error) => {
                        failure = Some(error);
                        stop.store(true, Ordering::Relaxed);
                    }
                }
            }
        },
        None,
    );
    if let Some(bridge) = bridge {
        bridge.finish();
    }
    let reply = finish(session, who, call, completed?, denials)?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(Ok(reply))
}
