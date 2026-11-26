use saddle_limits as budgets;
use saddle_sdk::{
    Definition, ExecReply, Launch, Output, Tool, configure_limits, run_binary_with_options,
};
use serde::Deserialize;
use serde_json::json;

use crate::bash::format_reply;

pub struct RunBinary;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    profile: String,
    argv: Vec<String>,
    timeout_secs: Option<u64>,
    allow_children: Option<bool>,
}

impl Tool for RunBinary {
    fn describe() -> Definition {
        Definition {
            name: "run_binary".into(),
            description: "run an authorized named launch profile with literal arguments, not a shell. The profile fixes the binary, filesystem and network bounds; argv does not include the executable. Configured helpers are allowed by default; set allow_children to false to block further program execution. Foreground only. Available names are listed in the session grants.".into(),
            input_schema: json!({"type":"object","properties":{"profile":{"type":"string"},"argv":{"type":"array","items":{"type":"string"}},"timeout_secs":{"type":"integer","minimum":1},"allow_children":{"type":"boolean","default":true}},"required":["profile","argv"],"additionalProperties":false}).to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
        let result = configure_limits().and_then(|()| {
            let args: Arguments = serde_json::from_str(&arguments)
                .map_err(|error| format!("invalid arguments: {error}"))?;
            run_binary_with_options(
                &Launch {
                    profile: args.profile,
                    argv: args.argv,
                    timeout_secs: args.timeout_secs.unwrap_or(budgets::exec_seconds() as u64),
                },
                args.allow_children.unwrap_or(true),
            )
            .map(|reply| {
                format_reply(&ExecReply {
                    code: reply.code,
                    signal: reply.signal,
                    stdout: reply.stdout,
                    stderr: reply.stderr,
                    timed_out: reply.timed_out,
                    cancelled: reply.cancelled,
                    truncated: reply.truncated,
                    denials: reply.denials,
                })
            })
        });
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
