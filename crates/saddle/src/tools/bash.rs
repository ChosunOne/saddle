//! shell tool adapter; privileged execution lives in the shared service (D142).

use super::{ToolOutput, str_field};
use crate::execution::{self, OUTPUT_LIMIT};
use crate::session::Session;
use anyhow::Result;
use saddle_cap::Principal;
use serde_json::Value;
use std::fmt::Write as _;
use std::time::Duration;

pub fn run(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let command = match str_field(input, "command") {
        Ok(command) => command,
        Err(out) => return Ok(out),
    };
    let timeout = Duration::from_secs(
        input
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(u64::MAX),
    );
    let argv = vec!["sh".to_owned(), "-c".to_owned(), command.to_owned()];
    let out = match execution::execute(
        session,
        &Principal::Agent,
        "native-bash",
        argv,
        timeout,
        &mut false,
    )? {
        Ok(out) => out,
        Err(why) => return Ok(ToolOutput::err(why)),
    };
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
    for path in &out.restored {
        writeln!(text, "restored reserved path {}", path)?;
    }
    // What the command's network was refused, which the command itself cannot explain: it sees a
    // connection that failed, not the rule that refused it.
    for why in &out.denials {
        writeln!(text, "--- net ---\n{why}")?;
    }
    let ok = out.code == Some(0) && !out.timed_out;
    Ok(ToolOutput {
        provenance: None,
        ok,
        diff: None,
        images: Vec::new(),
        content: text.trim_end().to_owned(),
    })
}
