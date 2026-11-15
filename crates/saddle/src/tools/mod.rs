//! standard/third-party component dispatch and host MCP discovery, never native tool fallbacks (D17, D147).

use crate::mcp::registry;
use crate::plugins;
use crate::provider::Image;
use saddle_cap::{Principal, Scope};
use saddle_limits as budgets;
use saddle_policy::Kind;
use saddle_policy::audit::sha256_hex;
use std::mem;
pub(crate) mod timeout;
mod tool_search;

pub(crate) use crate::execution::HELPER_ARG;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::provider::{Provenance, ToolDef, visible_tool_text};
use crate::session::Session;

/// What the model sees. A denial is an ordinary result with `ok: false`, not a host error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub provenance: Option<Box<Provenance>>,
    pub ok: bool,
    pub content: String,
    /// For the transcript only: a line diff of the file the call changed.
    pub diff: Option<String>,
    /// Pictures the call produced, such as a screenshot `read` was asked for (D58).
    pub images: Vec<Image>,
}

impl ToolOutput {
    /// human rendering only; providers/store always retain the original content and provenance.
    pub fn visible_content(&self) -> &str {
        visible_tool_text(&self.content, self.provenance.as_deref())
    }

    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            provenance: None,
            ok: true,
            content: content.into(),
            diff: None,
            images: Vec::new(),
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
            provenance: None,
            ok: false,
            content: content.into(),
            diff: None,
            images: Vec::new(),
        }
    }

    pub fn denied(explanation: &str) -> Self {
        Self::err(format!("denied: {explanation}"))
    }
}

pub const NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "patch",
    "ls",
    "find",
    "grep",
    "bash",
    "fetch",
    "websearch",
    "session_query",
    "tool_search",
];

/// The tools as the model sees them. The descriptions say when to use each one.
pub fn definitions() -> Vec<ToolDef> {
    vec![timeout::definition(ToolDef {
        name: "tool_search".into(),
        description: "Find and declare a tool an MCP server offers. A server's tools are not \
                          declared until this matches them, so use it before calling one: an \
                          empty query declares everything the granted servers offer. A declared \
                          tool is called by the name this returns, and stays declared for the \
                          session."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words to match against a tool's name and description; empty for all",
                },
            },
            "required": ["query"],
            "additionalProperties": false,
        }),
    })]
}

/// Runs one tool as the agent. `Err` means the host itself failed (for example, the audit log
/// could not be written) and the session must stop.
pub fn call(session: &mut Session, name: &str, input: &Value) -> Result<ToolOutput> {
    let limits = session.setup.tool_limits.clone();
    let deadline = match timeout::parse(input) {
        Ok(deadline) => deadline,
        Err(why) => {
            let arguments = input.to_string();
            session.audit.event(Kind::Decision, Some(&Principal::Agent), Some(Scope::Tool), Some(false),
                json!({"operation":"invalid-tool-arguments", "tool":name, "arguments":{"bytes":arguments.len(), "sha256":sha256_hex(arguments.as_bytes())}}), &why)?;
            return Ok(ToolOutput::err(why));
        }
    };
    timeout::using(deadline, || {
        budgets::using(&limits, || {
            let output = call_inner(session, name, input)?;
            // Never downgrade a fatal host/audit error to a timeout.
            if timeout::expired() && output.provenance.is_none() {
                Ok(ToolOutput {
                    ok: false,
                    content: format!(
                        "{}\ntool call timed out; effects already completed are not rolled back",
                        output.content
                    ),
                    ..output
                })
            } else {
                Ok(output)
            }
        })
    })
}

fn call_inner(session: &mut Session, name: &str, input: &Value) -> Result<ToolOutput> {
    match name {
        name if NAMES.contains(&name) && name != "tool_search" => {
            let mut registry = mem::take(&mut session.plugins);
            let out = registry.call(session, name, input);
            session.plugins = registry;
            out
        }
        "tool_search" => tool_search::search(session, input),
        _ if name.starts_with(plugins::PREFIX) => {
            let mut registry = mem::take(&mut session.plugins);
            let out = registry.call(session, name, input);
            session.plugins = registry;
            out
        }
        _ if name.starts_with(registry::PREFIX) => {
            // Same reason as `tool_search`: the registry is a field of the session it has to ask.
            let mut registry = mem::take(&mut session.mcp);
            let out = registry.call(name, input, session);
            session.mcp = registry;
            out
        }
        _ => bail!("unknown tool `{name}` (known: {})", NAMES.join(", ")),
    }
}
