//! shell workflow and bounded presentation over the delegated exec SDK (D142).

use saddle_limits as budgets;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::configure_limits;
use std::fmt::Write as _;

use saddle_sdk::ExecReply;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{
    Command, Definition, Output, Task, Tool, cancel_task, execute, inspect_task, list_tasks,
    start_task, wake_task,
};
use serde::Deserialize;
#[cfg(target_arch = "wasm32")]
use serde_json::json;

pub struct Bash;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub command: Option<String>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub background: bool,
    /// Whether finishing should resume the session, which is the default (D174). Only meaningful
    /// with `background`, and a task that declined can be armed later with `action: "wait"`.
    pub wake: Option<bool>,
    #[serde(default)]
    pub action: Action,
    pub task_id: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    #[default]
    Run,
    Status,
    List,
    Cancel,
    Wait,
}

#[cfg(target_arch = "wasm32")]
fn format_task(task: &Task) -> String {
    let status = if task.running { "running" } else { "finished" };
    // A running task has no exit status to report; its capture is what has arrived so far (D179).
    let output = render(&task.output, !task.running);
    if output.is_empty() {
        format!("task {}: {status}", task.id)
    } else {
        format!("task {}: {status}\n{output}", task.id)
    }
}

#[cfg(target_arch = "wasm32")]
fn invoke_task(args: Arguments) -> Result<(String, bool), String> {
    if !matches!(args.action, Action::Run)
        && (args.command.is_some() || args.background || args.wake.is_some())
    {
        return Err("command, background and wake are only valid for action run".into());
    }
    match args.action {
        Action::Run => {
            if args.task_id.is_some() {
                return Err("task_id is not valid for action run".into());
            }
            let command = args
                .command
                .filter(|command| !command.trim().is_empty())
                .ok_or("run requires a nonempty command")?;
            let request = Command {
                argv: vec!["sh".into(), "-c".into(), command],
                timeout_secs: args.timeout_secs.unwrap_or(budgets::exec_seconds() as u64),
            };
            if !args.background {
                if args.wake.is_some() {
                    return Err("wake is only valid with background: true".into());
                }
                return execute(&request).map(|reply| format_reply(&reply));
            }
            // The launch says what its completion should do about the session (D174), rather than
            // leaving the session to infer it from the call's own association.
            let wake = args.wake.unwrap_or(true);
            let started = start_task(&request)?;
            let task = wake_task(&started.id, wake)?;
            let mut text = format_task(&task);
            text.push_str(if wake {
                "\nthe session will be resumed when this finishes; read its output with action=status"
            } else {
                "\nthe session will not be resumed when this finishes; arm it with action=wait, or read it with action=status"
            });
            Ok((text, true))
        }
        // D174: arm a wake for a task that was started without one. It answers at once, because the
        // wait belongs to the session rather than to this call.
        Action::Wait => {
            let id = args.task_id.ok_or("wait requires task_id")?;
            let task = wake_task(&id, true)?;
            let mut text = format_task(&task);
            text.push_str(if task.running {
                "\narmed: the session will be resumed when this finishes; the notification carries no output, so read it with action=status"
            } else {
                "\nit has already finished, so there is nothing to wait for; its output is above"
            });
            Ok((text, true))
        }
        Action::Status | Action::Cancel => {
            let id = args.task_id.ok_or("status/cancel requires task_id")?;
            let task = if matches!(args.action, Action::Cancel) {
                cancel_task(&id)?
            } else {
                inspect_task(&id)?
            };
            let ok = task.running
                || (task.output.code == Some(0)
                    && !task.output.timed_out
                    && !task.output.cancelled)
                || matches!(args.action, Action::Cancel);
            Ok((format_task(&task), ok))
        }
        Action::List => {
            if args.task_id.is_some() {
                return Err("task_id is not valid for action list".into());
            }
            let tasks = list_tasks()?;
            let text = if tasks.is_empty() {
                "no background tasks".into()
            } else {
                tasks
                    .iter()
                    .map(|task| format!("{}: {}", task.id, state(task)))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, true))
        }
    }
}

pub fn format_reply(reply: &ExecReply) -> (String, bool) {
    let text = render(reply, true);
    (
        text,
        reply.code == Some(0) && !reply.timed_out && !reply.cancelled,
    )
}

/// What `list` says about one task: how it stands, and how it ended (D179). A record outlives its
/// command, so a poll after the fact still answers what the answer was.
#[cfg(target_arch = "wasm32")]
fn state(task: &Task) -> String {
    if task.running {
        return "running".into();
    }
    match (
        task.output.code,
        task.output.timed_out,
        task.output.cancelled,
    ) {
        (_, true, _) => "timed out".into(),
        (_, _, true) => "cancelled".into(),
        (Some(code), _, _) => format!("finished (exit {code})"),
        _ => "finished (no exit status)".into(),
    }
}

/// The reply as text: the status line when there is one, then the bounded streams (D179). A task
/// still running is asked for the streams alone, because "exited" is the one thing it has not done.
pub fn render(reply: &ExecReply, finished: bool) -> String {
    let mut text = match (reply.code, reply.signal) {
        _ if reply.cancelled => "cancelled by the user\n".into(),
        _ if reply.timed_out => "command timed out\n".into(),
        (Some(code), _) => format!("exit code {code}\n"),
        (_, Some(signal)) => format!("killed by signal {signal}\n"),
        _ if finished => "exited\n".into(),
        _ => String::new(),
    };
    for (label, bytes) in [("stdout", &reply.stdout), ("stderr", &reply.stderr)] {
        if !bytes.is_empty() {
            let body = String::from_utf8_lossy(bytes);
            let mut end = body.len().min(budgets::exec_present_bytes());
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            let _ = writeln!(text, "--- {label} ---\n{}", body[..end].trim_end());
            if end < body.len() {
                let _ = writeln!(text, "({label} presentation truncated at configured limit)");
            }
        }
    }
    if reply.truncated {
        text.push_str("(capture truncated at configured per-stream limit)\n");
    }
    for why in reply.denials.iter().take(budgets::exec_report_denials()) {
        let _ = writeln!(text, "--- net ---\n{why}");
    }
    let mut end = text.len().min(budgets::presentation_bytes());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.trim_end().to_owned()
}

#[cfg(target_arch = "wasm32")]
impl Tool for Bash {
    fn describe() -> Definition {
        Definition {
            name:"bash".into(),
            description:"run sh -c in the workspace through the delegated exec sandbox. Filesystem/environment/net reach comes from this plugin's runtime grants and protected sandbox configuration, not the agent's broader grants; network uses the policy proxy or falls back to none. background=true starts a session-owned task and returns its id promptly, and the session resumes you when it finishes unless wake=false, so you can do other work meanwhile; action=wait arms a task started with wake=false, answers at once and never blocks the call. action=status/list/cancel with task_id inspect bounded live output or stop it. Only one shell command per workspace may run at a time. Tasks stop on session exit/reload. Timeout bounds the task lifetime, never raises grants or configured limits. Capture is bounded per stream; presentation is host-configurable. Cancellation kills the process group.".into(),
            input_schema:json!({"type":"object","properties":{"command":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1},"background":{"type":"boolean"},"wake":{"type":"boolean","description":"with background: whether the session resumes you when the task finishes (default true)"},"action":{"type":"string","enum":["run","status","list","cancel","wait"]},"task_id":{"type":"string"}},"additionalProperties":false}).to_string(),
        }
    }
    fn invoke(arguments: String) -> Output {
        if let Err(text) = configure_limits() {
            return Output {
                text,
                is_error: true,
            };
        }
        let result = serde_json::from_str::<Arguments>(&arguments)
            .map_err(|error| format!("invalid arguments: {error}"))
            .and_then(invoke_task);
        match result {
            Ok((text, ok)) => Output {
                text,
                is_error: !ok,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_and_failure_semantics_are_guest_owned_and_bounded() {
        let mut reply = ExecReply {
            code: Some(0),
            signal: None,
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            timed_out: false,
            cancelled: false,
            truncated: false,
            denials: vec![],
        };
        let (text, ok) = format_reply(&reply);
        assert!(ok && text.contains("exit code 0") && text.contains("hello"));
        // A task still running reports its streams and no status line: it has not exited (D179).
        let running = ExecReply {
            code: None,
            signal: None,
            truncated: false,
            ..reply.clone()
        };
        let streams = render(&running, false);
        assert!(!streams.contains("exited"), "{streams}");
        assert!(streams.contains("hello"), "{streams}");
        assert!(render(&running, true).starts_with("exited\n"));
        reply.cancelled = true;
        assert!(!format_reply(&reply).1);
        reply.stdout = "é".repeat(256000).into_bytes();
        assert!(format_reply(&reply).0.len() < 65536);
    }
}
