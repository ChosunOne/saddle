//! shell workflow and bounded presentation over the delegated exec SDK (D142).

use std::fmt::Write as _;

use saddle_sdk::ExecReply;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Command, Definition, Output, Tool, execute};
use serde::Deserialize;
#[cfg(target_arch = "wasm32")]
use serde_json::json;

pub struct Bash;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub command: String,
    pub timeout_secs: Option<u64>,
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
            let mut end = body.len().min(24 * 1024);
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            let _ = writeln!(text, "--- {label} ---\n{}", body[..end].trim_end());
            if end < body.len() {
                let _ = writeln!(text, "({label} presentation truncated at 24 KiB)");
            }
        }
    }
    if reply.truncated {
        text.push_str("(capture truncated at 256 KiB per stream)\n");
    }
    for path in reply.restored.iter().take(16) {
        let _ = writeln!(text, "restored reserved path {path}");
    }
    for why in reply.denials.iter().take(8) {
        let _ = writeln!(text, "--- net ---\n{why}");
    }
    let mut end = text.len().min(60 * 1024);
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
            description:"run sh -c in the workspace through the delegated exec sandbox. Filesystem/environment/net reach comes from this plugin's runtime grants and protected sandbox configuration, not the agent's broader grants. Network uses the policy proxy or falls back to none. Timeout is bounded by the grant and 300 seconds. Captures at most 256 KiB per stream; presents at most 24 KiB each. Cancellation kills the process group; reserved Git paths are restored.".into(),
            input_schema:json!({"type":"object","properties":{"command":{"type":"string"},"timeout_secs":{"type":"integer","minimum":1,"maximum":300}},"required":["command"],"additionalProperties":false}).to_string(),
        }
    }
    fn invoke(arguments: String) -> Output {
        let result = serde_json::from_str::<Arguments>(&arguments)
            .map_err(|error| format!("invalid arguments: {error}"))
            .and_then(|args| {
                execute(&Command {
                    argv: vec!["sh".into(), "-c".into(), args.command],
                    timeout_secs: args.timeout_secs.unwrap_or(300),
                })
            });
        match result {
            Ok(reply) => {
                let (text, ok) = format_reply(&reply);
                Output {
                    text,
                    is_error: !ok,
                }
            }
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
