//! Built-in tools (D17). Each one is called with the JSON input the model would send, passes the
//! policy engine for every access, and returns the text the model would see.

use crate::mcp::registry;
use crate::provider::Image;
use std::mem;
mod bash;
mod diff;
pub(crate) mod fetch;
pub mod fs;
mod patch;
pub mod search;
pub(crate) mod session_query;
mod tool_search;
pub(crate) mod websearch;

pub(crate) use bash::HELPER_ARG;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use crate::provider::ToolDef;
use crate::session::Session;

/// What the model sees. A denial is an ordinary result with `ok: false`, not a host error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub ok: bool,
    pub content: String,
    /// For the transcript only: a line diff of the file the call changed.
    pub diff: Option<String>,
    /// Pictures the call produced, such as a screenshot `read` was asked for (D58).
    pub images: Vec<Image>,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            ok: true,
            content: content.into(),
            diff: None,
            images: Vec::new(),
        }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self {
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
    let path = json!({
        "type": "string",
        "description": "Absolute, or relative to the workspace root",
    });
    vec![
        ToolDef {
            name: "session_query".into(),
            description: "Read-only SQL over three views of the current conversation branch, \
                including pre-compaction history. \
                messages(ordinal INTEGER, id INTEGER, role TEXT, text TEXT): role is user, \
                assistant, tool or summary. \
                requests(ordinal INTEGER, node INTEGER, kind TEXT, provider TEXT, model TEXT, \
                input INTEGER, output INTEGER, cache_read INTEGER, cache_write INTEGER, \
                cached INTEGER, cost REAL, reasoning_dropped INTEGER, \
                reasoning_tokens INTEGER, at TEXT, after INTEGER): \
                one row per model request, kind reply or summary, cached 1 when the prompt was \
                served from the provider's cache, reasoning_dropped the reasoning items the \
                provider said it removed from the prompt, reasoning_tokens how much of output was \
                thinking, after the ordinal of the last message before it. \
                reasoning(ordinal INTEGER, node INTEGER, part INTEGER, text TEXT): one row per \
                reasoning block, ordinal the message it belongs to, part its position among that \
                message's parts, text what it says when the block is prose at all (an encrypted \
                or redacted block has no row). \
                No other conversations, images, internal tables or files are accessible. Requires \
                transcript-read. Results are capped at 100 rows and 50 KiB; use WHERE, ORDER BY \
                ordinal and LIMIT/OFFSET to search and page. Examples: \
                SELECT ordinal, role, text FROM messages WHERE text LIKE '%decision%' LIMIT 10; \
                SELECT provider, model, sum(cache_read), sum(input + cache_read) FROM requests \
                GROUP BY provider, model; SELECT m.ordinal, r.text FROM messages m \
                JOIN reasoning r USING (ordinal) WHERE r.text LIKE '%cache%'"
                .into(),
            schema: json!({
                "type": "object",
                "properties": {"sql": {"type": "string", "description": "A read-only SQLite query"}},
                "required": ["sql"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
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
        },
        ToolDef {
            name: "read".into(),
            description: "Read a UTF-8 text file, or a PNG, JPEG, GIF or WebP image. Text comes \
                          back at most 2000 lines and 50 KiB at a time, and says so when there is \
                          more, so a large file cannot fill the context; an image is handed over \
                          as a picture for you to look at. Use this rather than `cat` in bash."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "offset": {
                        "type": "integer",
                        "description": "Line to start at, 1-based; defaults to 1",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Lines to return at most; the page cap still applies",
                    },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "write".into(),
            description: "Create a file, or replace a file's entire contents. Missing parent \
                          directories are created. To change part of an existing file, use edit."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "content": { "type": "string", "description": "The complete new contents" },
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "edit".into(),
            description: "Replace one occurrence of `old` with `new` in a file. `old` must match \
                          the file exactly, including whitespace and indentation, and must occur \
                          exactly once; include surrounding lines until it is unique. Read the \
                          file before editing it."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": path,
                    "old": { "type": "string", "description": "Exact text to replace" },
                    "new": { "type": "string", "description": "Replacement text" },
                },
                "required": ["path", "old", "new"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "patch".into(),
            description: "Change several files in one call, in the apply-patch format: \
                          `*** Begin Patch`, then `*** Add File: path`, `*** Update File: path` \
                          (with optional `*** Move to: path`), `*** Delete File: path`, then \
                          `*** End Patch`. In an update, `@@` starts a hunk, `-` lines must match \
                          the file exactly and uniquely, `+` lines replace them, and a leading \
                          space is a context line kept on both sides. Use this for a rename or a \
                          change that touches several files; use edit for one change in one file. \
                          Every path is checked against the policy, and one denial refuses the \
                          whole patch."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "The whole patch text, including the begin and end markers",
                    },
                },
                "required": ["patch"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "ls".into(),
            description: "List a directory: one entry per line, directories ending in `/`. Use \
                          this rather than `ls` in bash."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory; defaults to the workspace root",
                    },
                },
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "find".into(),
            description: "Find files by name. A glob without a slash, such as `*.rs`, matches \
                          file names at any depth; one with a slash, such as `src/**/mod.rs`, \
                          matches paths from the search directory. Skips .git and files \
                          .gitignore lists. Use this rather than `find` in bash."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob" },
                    "path": {
                        "type": "string",
                        "description": "Directory to search; defaults to the workspace root",
                    },
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "grep".into(),
            description: "Search file contents with a regular expression (Rust regex syntax). \
                          Returns `path:line: text` for each matching line, up to 200. Skips \
                          binary files, .git and files .gitignore lists. Use this rather than \
                          grep or rg in bash."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regular expression" },
                    "path": {
                        "type": "string",
                        "description": "File or directory; defaults to the workspace root",
                    },
                    "glob": {
                        "type": "string",
                        "description": "Only files matching this glob, such as `*.rs`",
                    },
                    "ignore_case": { "type": "boolean" },
                },
                "required": ["pattern"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "bash".into(),
            description: "Run a shell command with `sh -c` in the workspace root and return its \
                          exit code, stdout and stderr. Use it for builds, tests and git. The \
                          command runs in a sandbox: no network, \
                          files limited to the granted roots, a time limit, and background \
                          processes are killed when it exits. Output past 256 KiB per stream is \
                          cut."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Lower time limit in seconds; the policy sets the maximum",
                    },
                },
                "required": ["command"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "fetch".into(),
            description: "Fetch one http or https URL and return its status, content type and \
                          body as text. The host makes the request, so it needs a net grant for \
                          that host; `bash` never has network access, so use this rather than \
                          curl. Only GET, and the body is cut at 100 KiB. A key or other secret \
                          belongs in `headers`; it is never written to the audit log or shown \
                          back to you."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The whole URL, including the scheme",
                    },
                    "headers": {
                        "type": "object",
                        "description": "Header names and values, for a keyed endpoint",
                        "additionalProperties": { "type": "string" },
                    },
                },
                "required": ["url"],
                "additionalProperties": false,
            }),
        },
        ToolDef {
            name: "websearch".into(),
            description:
                "Search the web and return the most relevant pages with the passages that \
                          answer the query. Use it when the answer is not in the workspace and you \
                          do not already know the address; use `fetch` when you do. It needs a net \
                          grant for the search host, and it names the host it used in the result. \
                          Five results come back unless `num_results` says otherwise, and each \
                          passage is an excerpt: pass `result` with a result's number to read that \
                          one in full, which reuses the search already made rather than asking \
                          again."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What to search for, as a person would type it",
                    },
                    "num_results": {
                        "type": "integer",
                        "description": "How many results to return; 5 by default, 100 at most",
                    },
                    "result": {
                        "type": "integer",
                        "description": "The number of one result to read in full, as the \
                                        previous search numbered it; omit `query` to use the \
                                        search this session already made",
                    },
                },
                "required": [],
                "additionalProperties": false,
            }),
        },
    ]
}

/// Runs one tool as the agent. `Err` means the host itself failed (for example, the audit log
/// could not be written) and the session must stop.
pub fn call(session: &mut Session, name: &str, input: &Value) -> Result<ToolOutput> {
    match name {
        "read" => fs::read(session, input),
        "write" => fs::write(session, input),
        "edit" => fs::edit(session, input),
        "ls" => search::ls(session, input),
        "find" => search::find(session, input),
        "grep" => search::grep(session, input),
        "bash" => bash::run(session, input),
        "fetch" => fetch::fetch(session, input),
        "patch" => patch::patch(session, input),
        "websearch" => websearch::search(session, input),
        "session_query" => session_query::run(session, input),
        "tool_search" => tool_search::search(session, input),
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

fn str_field<'a>(input: &'a Value, field: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutput::err(format!("missing string field `{field}`")))
}

/// An optional count, which must be a positive whole number when it is there.
fn num_field(input: &Value, field: &str) -> Result<Option<u64>, ToolOutput> {
    match input.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n > 0 => Ok(Some(n)),
            Some(_) => Err(ToolOutput::err(format!(
                "`{field}` must be a positive whole number"
            ))),
            None => Err(ToolOutput::err(format!("`{field}` must be a whole number"))),
        },
    }
}
