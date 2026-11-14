//! D108: `tool_search` is how a server's tools reach the model. Nothing an MCP server offers is
//! declared until a search matches it, so a session with ten configured servers costs ten lines in
//! the prompt rather than ten tool schemas. A match is declared for the rest of the session, and
//! what a search can never do is declare a tool the grant does not name.

use anyhow::Result;
use serde_json::Value;
use std::mem;

use crate::session::Session;
use crate::tools::ToolOutput;

pub fn search(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    // The registry lives on the session it has to ask, so it is taken out for the call and put
    // back afterwards: starting a server needs the engine and the audit log, which are the
    // session's too.
    let mut registry = mem::take(&mut session.mcp);
    let failing = registry.discover(session);
    let declared = registry.search(&query);
    // Named rather than listed: a tool the grant does not name is not the model's to reach, but
    // knowing it exists is not a secret either (D103).
    let withheld = registry.ungranted().len();
    session.mcp = registry;

    let mut lines: Vec<String> = Vec::new();
    for note in failing {
        lines.push(format!("a server could not be started — {note}"));
    }
    if declared.is_empty() {
        lines.push(if query.trim().is_empty() {
            "no MCP tool is available to declare".to_owned()
        } else {
            format!("nothing matches `{}`", query.trim())
        });
        if !lines.is_empty() {
            lines.push("try an empty query to see everything the granted servers offer".to_owned());
        }
    } else {
        lines.push("these are now declared, and callable by name:".to_owned());
        lines.extend(declared);
    }
    if withheld > 0 {
        lines.push(format!(
            "{withheld} further tool(s) of these servers are not granted, and cannot be called"
        ));
    }
    Ok(ToolOutput::ok(lines.join("\n")))
}
