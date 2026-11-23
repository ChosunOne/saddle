//! attributed subprocess effects and shared execution limits (D142, D152, D153, D185).

use saddle_limits as budgets;
use std::env::{self, join_paths};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
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

/// holds the lowered sandbox and resource bounds for one launch.
pub struct Prepared {
    pub spec: SandboxSpec,
    deadline: Instant,
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
    Ok(Ok(Prepared {
        spec: lowered.spec,
        deadline,
        capture_bytes: budgets::exec_capture_bytes(),
    }))
}

impl Prepared {
    /// process cleanup runs on the monitor, even while the model is busy.
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
            if failure.is_some() {
                return;
            }
            if let Err(error) = tasks::service(session) {
                failure = Some(error);
                stop.store(true, Ordering::Relaxed);
                return;
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
    allow_children: bool,
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
        json!({"operation":"run-binary","allow_children":allow_children}),
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
    let expected = |path: &Path| session.launch_baselines.expected(profile, path);
    let executable_sha256 = match expected(&profile.executable) {
        Ok(digest) => digest,
        Err(why) => return Ok(Err(why)),
    };
    let helpers = if allow_children {
        match profile
            .helpers
            .iter()
            .map(|path| Ok((path.clone(), expected(path)?)))
            .collect::<Result<Vec<_>, String>>()
        {
            Ok(helpers) => helpers,
            Err(why) => return Ok(Err(why)),
        }
    } else {
        Vec::new()
    };
    // The lookup path is only a convenience. Landlock grants execute on exact pinned files.
    let path = if helpers.is_empty() {
        String::new()
    } else {
        join_paths(
            [&profile.executable]
                .into_iter()
                .chain(helpers.iter().map(|(path, _)| path))
                .filter_map(|p| p.parent()),
        )
        .context("building helper lookup path")?
        .into_string()
        .map_err(|_| anyhow!("helper lookup path is not UTF-8"))?
    };
    let mut environment = vec![("PATH".into(), path)];
    if let Some((key, value)) = session.host_env.iter().find(|(key, _)| key == "HOME") {
        environment.push((key.clone(), value.clone()));
    }
    let hosts = profile.net.clone();
    let ssh_hosts = profile.ssh_hosts.clone();
    let net_granted = session.engine.policy().grants(who).is_some_and(|g| {
        hosts
            .iter()
            .any(|host| g.net.iter().any(|grant| grant.allows("https", host, 443)))
            || ssh_hosts
                .iter()
                .any(|host| g.net.iter().any(|grant| grant.allows("ssh", host, 22)))
    });
    let mut bridge = if net_granted {
        let bridge =
            Bridge::start_named(dir.path()).context("starting named-launch network proxy")?;
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
        // D190: the workspace is a read claim of the named launch only. The helper still
        // opens it without symlink traversal and checks for protected-path overlap.
        read: profile
            .read
            .iter()
            .chain(profile.read_workspace.then_some(&session.setup.workspace))
            .cloned()
            .collect(),
        // D193: the workspace is writable by default. Landlock cannot carve a path out of a
        // granted root, so file-tool exclusions do not narrow this claim.
        write: profile
            .write_workspace
            .then(|| session.setup.workspace.clone())
            .into_iter()
            .collect(),
        net: bridge.as_ref().map(Bridge::spec),
        named_launch: Some(NamedLaunch {
            protected,
            agent_write,
            executable_sha256,
            helpers,
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
                match bridge.service_bounded(session, who, Some(call), Some((&hosts, &ssh_hosts))) {
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
