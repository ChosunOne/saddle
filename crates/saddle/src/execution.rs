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
use cap_std::fs::{OpenOptions, OpenOptionsExt};
use saddle_cap::sandbox::{ExecRequest, NamedLaunch, PROXY_PORT, SandboxSpec, lower_exec};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_policy::{Kind, Request};
use saddle_sandbox::{Capture, Output, launch_availability, run, run_observed};
use saddle_wit::saddle::host::exec::Reply;
use saddle_wit::saddle::host::run_binary::{Launch, Reply as LaunchReply};
use serde_json::json;
use tempfile::Builder;

use crate::config::sandbox_overrides::resolve_missing;
use crate::net::bridge::Bridge;
use crate::session::Session;
use crate::tools::timeout;
use crate::{cancel, tasks};

pub const HELPER_ARG: &str = "__sandbox";

/// pins the workspace lock through process termination (D142).
pub struct Prepared {
    pub spec: SandboxSpec,
    deadline: Instant,
    _lock: File,
    capture_bytes: usize,
}

pub struct Completed {
    output: io::Result<Output>,
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
    Ok(Ok(Prepared {
        spec: lowered.spec,
        deadline,
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
        // Self, including the workspace lock, drops only once the helper is reaped (D142).
        Ok(Completed { output })
    }
}

pub fn finish(
    session: &mut Session,
    who: &Principal,
    call: &str,
    completed: Completed,
    denials: Vec<String>,
) -> Result<Reply> {
    let output = completed.output.context("running the sandbox helper")?;
    session.audit.event(Kind::Decision, Some(who), Some(Scope::Exec), Some(true), json!({"call_id":call,"operation":"exec-completed","code":output.code,"signal":output.signal,"timed_out":output.timed_out,"cancelled":output.cancelled,"stdout_bytes":output.stdout.len(),"stderr_bytes":output.stderr.len()}), "execution completed")?;
    Ok(reply(output, denials))
}

fn reply(output: Output, denials: Vec<String>) -> Reply {
    Reply {
        code: output.code,
        signal: output.signal,
        stdout: output.stdout,
        stderr: output.stderr,
        timed_out: output.timed_out,
        cancelled: output.cancelled,
        truncated: output.truncated,
        denials,
    }
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

/// authorizes and runs one protected profile, without borrowing ordinary shell authority (D184).
pub fn launch(
    session: &mut Session,
    who: &Principal,
    call: &str,
    request: Launch,
    deadline: Instant,
    effects: &mut bool,
) -> Result<Result<LaunchReply, String>> {
    let Launch {
        profile: name,
        argv,
        ..
    } = request;
    let request = Request::RunBinary { profile: &name };
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({"operation":"run-binary"}),
    )?;
    if !decision.allow {
        return Ok(Err(format!("denied: {}", decision.explanation)));
    }
    let Some(profile) = session.setup.launch_profiles.get(&name) else {
        return Ok(Err("denied: launch profile is undefined".into()));
    };
    if let Err(why) = launch_availability() {
        return Ok(Err(format!("denied: {why}")));
    }
    let limit = Duration::from_secs(
        profile
            .timeout_secs
            .map_or(budgets::exec_seconds() as u64, |s| u64::from(s.get())),
    );
    let timeout =
        match timeout::remaining(limit.min(deadline.saturating_duration_since(Instant::now()))) {
            Ok(timeout) if !timeout.is_zero() => timeout,
            _ => return Ok(Err("named launch timed out before starting".into())),
        };
    let end = Instant::now() + timeout;
    let Some(program) = profile.executable.to_str() else {
        return Ok(Err("launch executable path is not UTF-8".into()));
    };
    if program
        .len()
        .saturating_add(argv.iter().map(String::len).sum::<usize>())
        > budgets::exec_argument_bytes()
    {
        return Ok(Err("named launch exceeds argument bounds".into()));
    }
    // Bound trusted executable and runtime claims against ordinary user-configured write reach.
    // All writable roots are included, even if this profile or caller doesn't hold fs-write.
    let claim_paths = |paths: Vec<PathBuf>| {
        paths
            .into_iter()
            .map(|path| resolve_missing(&path))
            .collect::<Result<Vec<_>>>()
    };
    let protected = claim_paths(session.setup.paths.protected())?;
    let write = session
        .setup
        .extras
        .write
        .iter()
        .cloned()
        .chain(session.roots().iter().map(|(_, path)| path.to_path_buf()))
        .collect();
    let agent_write = claim_paths(write)?;
    let dir = Builder::new()
        .prefix("launch-")
        .tempdir_in(&session.setup.paths.state)
        .context("creating private named-launch directory")?;
    let mut environment = vec![("PATH".into(), String::new())];
    if let Some((key, value)) = session.host_env.iter().find(|(key, _)| key == "HOME") {
        environment.push((key.clone(), value.clone()));
    }
    let hosts = profile.net.clone();
    let net_granted = session.engine.policy().grants(who).is_some_and(|g| {
        hosts
            .iter()
            .any(|host| g.net.iter().any(|grant| grant.allows("https", host, 443)))
    });
    let mut bridge = if net_granted {
        let bridge = Bridge::start(dir.path()).context("starting named-launch network proxy")?;
        let proxy = format!("http://127.0.0.1:{PROXY_PORT}");
        for key in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
            environment.push((key.into(), proxy.clone()));
        }
        Some(bridge)
    } else {
        None
    };
    let spec = SandboxSpec {
        argv: [program.to_owned()].into_iter().chain(argv).collect(),
        cwd: dir.path().into(),
        env: environment,
        read: profile.read.clone(),
        write: Vec::new(),
        net: bridge.as_ref().map(Bridge::spec),
        named_launch: Some(NamedLaunch {
            protected,
            agent_write,
        }),
    };
    let helper = env::current_exe().context("locating the saddle binary")?;
    let stop = AtomicBool::new(cancel::FLAG.load(Ordering::Relaxed));
    let mut denials = Vec::new();
    let mut failure = None;
    let left = end.saturating_duration_since(Instant::now());
    if left.is_zero() || stop.load(Ordering::Relaxed) {
        if let Some(bridge) = bridge {
            bridge.finish();
        }
        return Ok(Err(
            "named launch cancelled or timed out before starting".into()
        ));
    }
    *effects = true;
    let result = run(
        &helper,
        &[HELPER_ARG],
        &spec,
        left,
        budgets::exec_capture_bytes(),
        &stop,
        &mut || {
            if cancel::FLAG.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(bridge) = bridge.as_mut() {
                match bridge.service_bounded(session, who, Some(call), Some(&hosts)) {
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
    );
    if let Some(bridge) = bridge {
        bridge.finish();
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let output = result.context("running the named launch helper")?;
    session.audit.event(Kind::Decision, Some(who), Some(Scope::RunBinary), Some(true), json!({"call_id":call,"operation":"run-binary-completed","profile":name,"code":output.code,"signal":output.signal,"timed_out":output.timed_out,"cancelled":output.cancelled,"stdout_bytes":output.stdout.len(),"stderr_bytes":output.stderr.len()}), "named launch completed")?;
    Ok(Ok(LaunchReply {
        code: output.code,
        signal: output.signal,
        stdout: output.stdout,
        stderr: output.stderr,
        timed_out: output.timed_out,
        cancelled: output.cancelled,
        truncated: output.truncated,
        denials,
    }))
}
