//! bash: `sh -c <command>` in the workspace, inside the exec sandbox (D18, D19), with the reserved
//! paths snapshotted and restored around it (D20).

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Context, Result};
use cap_std::fs::Dir;
use saddle_cap::sandbox::{ExecRequest, lower_exec};
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_policy::{Kind, Request};
use serde_json::{Value, json};

use super::{ToolOutput, str_field};
use crate::net::bridge::Bridge;
use crate::reserved;
use crate::session::Session;

pub const OUTPUT_LIMIT: usize = 256 * 1024;
const HELPER_ARG: &str = "__sandbox";

pub fn run(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let command = match str_field(input, "command") {
        Ok(c) => c,
        Err(out) => return Ok(out),
    };
    let who = Principal::Agent;
    let argv = vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()];
    let cwd = Location::new(RootName::workspace(), RelPath::default());

    let d = session.decide(
        &who,
        &Request::Exec {
            argv: &argv,
            cwd: &cwd,
        },
    )?;
    if !d.allow {
        return Ok(ToolOutput::denied(&d.explanation));
    }
    if let Err(reason) = &session.sandbox {
        let explanation = format!("exec is unavailable: {reason}");
        session.audit.event(
            Kind::Decision,
            Some(&who),
            Some(Scope::Exec),
            Some(false),
            json!({ "argv": argv }),
            &explanation,
        )?;
        return Ok(ToolOutput::denied(&explanation));
    }

    let grants = session
        .engine
        .policy()
        .grants(&who)
        .context("agent has no grants")?;
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
    let timeout = match input.get("timeout_secs").and_then(Value::as_u64) {
        Some(s) => lowered.timeout.min(Duration::from_secs(s.max(1))),
        None => lowered.timeout,
    };

    // The command's network: a socket here, a relay in its namespace, and this process deciding
    // every connection the relay carries back (D79).
    let tmp = session
        .roots()
        .get(&RootName::tmp())
        .context("no temporary root")?
        .to_path_buf();
    // A bridge that cannot be made is not a reason to refuse the command: it runs with no network
    // at all, which is what every command had before D79 and is strictly narrower (D79).
    let mut bridge = Bridge::start(&tmp).ok();
    if let Some(bridge) = &bridge {
        lowered.spec.net = Some(bridge.spec());
    }
    let mut denials: Vec<String> = Vec::new();

    let ws = Dir::open_ambient_dir(&session.setup.workspace, cap_std::ambient_authority())?;
    let before = reserved::take(&ws).context("snapshotting reserved paths")?;
    let helper = std::env::current_exe().context("locating the saddle binary")?;
    let out = saddle_sandbox::run(
        &helper,
        &[HELPER_ARG],
        &lowered.spec,
        timeout,
        OUTPUT_LIMIT,
        &crate::cancel::FLAG,
        &mut || {
            if let Some(bridge) = bridge.as_mut() {
                denials.extend(bridge.service(session));
            }
        },
    );
    if let Some(bridge) = bridge {
        bridge.finish();
    }
    let changes = reserved::restore(&ws, &before).context("restoring reserved paths")?;
    for (path, change) in &changes {
        session.audit.event(
            Kind::Restore,
            Some(&who),
            Some(Scope::FsWrite),
            Some(false),
            json!({ "path": format!("workspace:{}", path.display()), "change": format!("{change:?}").to_lowercase() }),
            format!("exec changed reserved path workspace:{}; restored", path.display()),
        )?;
    }
    let out = out.context("running the sandbox helper")?;

    let mut text = String::new();
    match (out.code, out.signal) {
        _ if out.cancelled => writeln!(text, "cancelled by the user")?,
        _ if out.timed_out => writeln!(text, "timed out after {}s", timeout.as_secs())?,
        (Some(code), _) => writeln!(text, "exit code {code}")?,
        (None, Some(sig)) => writeln!(text, "killed by signal {sig}")?,
        (None, None) => writeln!(text, "exited")?,
    }
    for (label, bytes) in [("stdout", &out.stdout), ("stderr", &out.stderr)] {
        if !bytes.is_empty() {
            writeln!(
                text,
                "--- {label} ---\n{}",
                String::from_utf8_lossy(bytes).trim_end()
            )?;
        }
    }
    if out.truncated {
        writeln!(
            text,
            "(output truncated at {OUTPUT_LIMIT} bytes per stream)"
        )?;
    }
    for (path, _) in &changes {
        writeln!(text, "restored reserved path {}", path.display())?;
    }
    // What the command's network was refused, which the command itself cannot explain: it sees a
    // connection that failed, not the rule that refused it.
    for why in &denials {
        writeln!(text, "--- net ---\n{why}")?;
    }
    let ok = out.code == Some(0) && !out.timed_out;
    Ok(ToolOutput {
        ok,
        diff: None,
        images: Vec::new(),
        content: text.trim_end().to_owned(),
    })
}
