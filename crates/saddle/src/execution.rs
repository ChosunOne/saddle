//! attributed subprocess effects, lowered from the caller's grants, not tool workflows (D142).

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use saddle_cap::sandbox::{ExecRequest, lower_exec};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_policy::{Kind, Request};
use saddle_sandbox::run as run_sandbox;
use saddle_wit::saddle::host::exec::Reply;
use serde_json::json;

use crate::net::bridge::Bridge;
use crate::session::Session;
use crate::{cancel, reserved};

pub const OUTPUT_LIMIT: usize = 256 * 1024;
pub const HELPER_ARG: &str = "__sandbox";

pub fn execute(
    session: &mut Session,
    who: &Principal,
    call: &str,
    argv: Vec<String>,
    requested_timeout: Duration,
    effects: &mut bool,
) -> Result<Result<Reply, String>> {
    if argv.is_empty()
        || argv.len() > 128
        || argv.iter().map(String::len).sum::<usize>() > 65536
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
    let mut lowered = lower_exec(
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
        .max(Duration::from_secs(1))
        .min(lowered.timeout);
    let tmp = session
        .roots()
        .get(&RootName::tmp())
        .context("no temporary root")?
        .to_path_buf();
    let mut bridge = Bridge::start(&tmp).ok();
    if let Some(bridge) = &bridge {
        lowered.spec.net = Some(bridge.spec());
    }
    let ws = session
        .file_roots
        .get(&RootName::workspace())
        .context("no pinned workspace")?
        .try_clone()?;
    let before = reserved::take(&ws).context("snapshotting reserved paths")?;
    let helper = env::current_exe().context("locating the saddle binary")?;
    let stop = AtomicBool::new(cancel::FLAG.load(Ordering::Relaxed));
    let mut denials = Vec::new();
    let mut failure = None;
    *effects = true;
    let output = run_sandbox(
        &helper,
        &[HELPER_ARG],
        &lowered.spec,
        timeout,
        OUTPUT_LIMIT,
        &stop,
        &mut || {
            if cancel::FLAG.load(Ordering::Relaxed) {
                stop.store(true, Ordering::Relaxed);
            }
            if let Some(bridge) = bridge.as_mut() {
                match bridge.service_as(session, who, Some(call)) {
                    Ok(notes) => {
                        if denials.len() < 32 {
                            denials.extend(notes.into_iter().take(32 - denials.len()));
                        }
                    }
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
    let changes = reserved::restore(&ws, &before).context("restoring reserved paths")?;
    for (path, change) in &changes {
        session.audit.event(Kind::Restore, Some(who), Some(Scope::FsWrite), Some(false), json!({"call_id":call, "path":format!("workspace:{}",path.display()), "change":format!("{change:?}").to_lowercase()}), format!("exec changed reserved path workspace:{}; restored", path.display()))?;
    }
    if let Some(error) = failure {
        return Err(error);
    }
    let output = output.context("running the sandbox helper")?;
    session.audit.event(Kind::Decision, Some(who), Some(Scope::Exec), Some(true), json!({"call_id":call,"operation":"exec-completed","code":output.code,"signal":output.signal,"timed_out":output.timed_out,"cancelled":output.cancelled,"stdout_bytes":output.stdout.len(),"stderr_bytes":output.stderr.len()}), "execution completed")?;
    Ok(Ok(Reply {
        code: output.code,
        signal: output.signal,
        stdout: output.stdout,
        stderr: output.stderr,
        timed_out: output.timed_out,
        cancelled: output.cancelled,
        truncated: output.truncated,
        restored: changes
            .iter()
            .map(|(path, _)| path.display().to_string())
            .collect(),
        denials,
    }))
}
