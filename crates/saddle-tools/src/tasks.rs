#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, Output, Tool, cancel_task, inspect_task, list_tasks, wake_task};
use serde::Deserialize;
#[cfg(target_arch = "wasm32")]
use serde_json::json;

#[cfg(target_arch = "wasm32")]
use crate::bash::{format_task, state};

pub struct Tasks;

#[derive(Deserialize)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum Action {
    Status,
    List,
    Cancel,
    Wait,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub action: Action,
    pub task_id: Option<String>,
}

#[cfg(target_arch = "wasm32")]
fn invoke_task(args: Arguments) -> Result<(String, bool), String> {
    match args.action {
        Action::List => {
            if args.task_id.is_some() {
                return Err("task_id is not valid for action list".into());
            }
            let tasks = list_tasks()?;
            let text = if tasks.is_empty() {
                "no background tasks".to_owned()
            } else {
                tasks
                    .iter()
                    .map(|task| format!("{}: {}", task.id, state(task)))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, true))
        }
        Action::Wait => {
            let id = args.task_id.ok_or("wait requires task_id")?;
            let task = wake_task(&id, true)?;
            let mut text = format_task(&task);
            text.push_str(if task.running {
                "\narmed: the session will be resumed when this finishes; the notification carries no output, so read it with action: status"
            } else {
                "\nit has already finished, so there is nothing to wait for; its output is above"
            });
            Ok((text, true))
        }
        Action::Status | Action::Cancel => {
            let id = args.task_id.ok_or("status/cancel requires task_id")?;
            let cancel = matches!(args.action, Action::Cancel);
            let task = if cancel {
                cancel_task(&id)?
            } else {
                inspect_task(&id)?
            };
            let ok = task.running
                || (task.output.code == Some(0)
                    && !task.output.timed_out
                    && !task.output.cancelled)
                || cancel;
            Ok((format_task(&task), ok))
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Tool for Tasks {
    fn describe() -> Definition {
        Definition {
            name: "tasks".into(),
            description: "report on, stop or wait for a background task that bash started with \
                          background=true. Every task id is the session's, and it is held by the \
                          principal that started it, so another plugin cannot read or cancel it. \
                          action=status says how it stands (running, \
                          or finished with its exit code) and shows the bounded output so far; a \
                          finished task keeps answering until the session drops the record. \
                          action=list names every task of this session with how each stands, and \
                          carries no output. action=cancel stops the process group and answers with \
                          the ending, waiting a moment for it to be reaped. action=wait arms the \
                          session to be resumed when the task finishes, or says it already has. \
                          Anything this reports is bounded by the exec budgets."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["status", "list", "cancel", "wait"],
                        "description": "what to do; status, cancel and wait need task_id, list does not",
                    },
                    "task_id": {
                        "type": "string",
                        "description": "the id bash returned when it started the task",
                    },
                },
                "required": ["action"],
                "additionalProperties": false,
            })
            .to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
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
