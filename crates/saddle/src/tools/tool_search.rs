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
    let mut registry = mem::take(&mut session.mcp);
    let failing = registry.discover(session);
    let declared = registry.search(&query);
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
