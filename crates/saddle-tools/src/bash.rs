//! shell workflow and bounded presentation over the delegated exec SDK (D142).

use saddle_limits as budgets;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::configure_limits;
use std::fmt::Write as _;

use saddle_sdk::ExecReply;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{
    Command, Definition, Output, Task, Tool, cancel_task, execute, inspect_task, list_tasks,
    start_task,
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
}

#[cfg(target_arch = "wasm32")]
fn format_task(task: &Task) -> String {
    let status = if task.running { "running" } else { "finished" };
    let output = format_reply(&task.output).0;
    format!(
        "task {}: {status}\n{}",
        task.id,
        if task.running {
            output.trim_start_matches("exited\n")
        } else {
            &output
        }
    )
}

#[cfg(target_arch = "wasm32")]
fn invoke_task(args: Arguments) -> Result<(String, bool), String> {
    if !matches!(args.action, Action::Run) && (args.command.is_some() || args.background) {
        return Err("command and background are only valid for action run".into());
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
            if args.background {
                start_task(&request).map(|task| (format_task(&task), true))
            } else {
                execute(&request).map(|reply| format_reply(&reply))
            }
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
                    .map(|task| {
                        format!(
                            "{}: {}",
                            task.id,
                            if task.running { "running" } else { "finished" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, true))
        }
    }
}

pub fn format_reply(reply: &ExecReply) -> (String, bool) {
    let mut text = match (reply.code, reply.signal) {
        _ if reply.cancelled => "cancelled by the user\n".into(),
        _ if reply.timed_out => "command timed out\n".into(),
        (Some(code), _) => format!("exit code {code}\n"),
        (_, Some(signal)) => format!("killed by signal {signal}\n"),
        _ => "exited\n".into(),
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
    for path in reply.restored.iter().take(budgets::exec_report_paths()) {
        let _ = writeln!(text, "restored reserved path {path}");
    }
    for why in reply.denials.iter().take(budgets::exec_report_denials()) {
        let _ = writeln!(text, "--- net ---\n{why}");
    }
    let mut end = text.len().min(budgets::presentation_bytes());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (
        text.trim_end().into(),
        reply.code == Some(0) && !reply.timed_out && !reply.cancelled,
    )
}

#[cfg(target_arch = "wasm32")]
impl Tool for Bash {
    fn describe() -> Definition {
        Definition {
            name:"bash".into(),
            description:"run sh -c in the workspace through the delegated exec sandbox. Filesystem/environment/net reach comes from this plugin's runtime grants and protected sandbox configuration, not the agent's broader grants. Network uses the policy proxy or falls back to none. Set background=true to start a session-owned task and return its id promptly. Use action=status/list/cancel and task_id to inspect bounded live output or stop it. Only one shell command per workspace may run at a time; cancel or finish it before another. Tasks stop on session exit/reload. Timeout bounds the task lifetime, never increases grants or configured limits. Captures at most the configured per-stream capture budget; presentation is separately host-configurable. Cancellation kills the process group; reserved Git paths are restored.".into(),
            input_schema:json!({"type":"object","properties":{"command":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1},"background":{"type":"boolean"},"action":{"type":"string","enum":["run","status","list","cancel"]},"task_id":{"type":"string"}},"additionalProperties":false}).to_string(),
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
            restored: vec![".git/config".into()],
            denials: vec![],
        };
        let (text, ok) = format_reply(&reply);
        assert!(
            ok && text.contains("exit code 0")
                && text.contains("hello")
                && text.contains("restored reserved path")
        );
        reply.cancelled = true;
        assert!(!format_reply(&reply).1);
        reply.stdout = "é".repeat(256000).into_bytes();
        assert!(format_reply(&reply).0.len() < 65536);
    }
}
