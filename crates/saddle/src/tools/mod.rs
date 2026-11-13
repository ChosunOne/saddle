//! Built-in tools (D17). Each one is called with the JSON input the model would send, passes the
//! policy engine for every access, and returns the text the model would see.

mod bash;
mod diff;
pub(crate) mod fetch;
pub mod fs;
mod patch;
pub mod search;
pub(crate) mod websearch;

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
    pub images: Vec<crate::provider::Image>,
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
];

/// The tools as the model sees them. The descriptions say when to use each one.
pub fn definitions() -> Vec<ToolDef> {
    let path = json!({
        "type": "string",
        "description": "Absolute, or relative to the workspace root",
    });
    vec![
        ToolDef {
            name: "read",
            description: "Read a UTF-8 text file, or a PNG, JPEG, GIF or WebP image. Text comes \
                          back at most 2000 lines and 50 KiB at a time, and says so when there is \
                          more, so a large file cannot fill the context; an image is handed over \
                          as a picture for you to look at. Use this rather than `cat` in bash.",
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
            name: "write",
            description: "Create a file, or replace a file's entire contents. Missing parent \
                          directories are created. To change part of an existing file, use edit.",
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
            name: "edit",
            description: "Replace one occurrence of `old` with `new` in a file. `old` must match \
                          the file exactly, including whitespace and indentation, and must occur \
                          exactly once; include surrounding lines until it is unique. Read the \
                          file before editing it.",
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
            name: "patch",
            description: "Change several files in one call, in the apply-patch format: \
                          `*** Begin Patch`, then `*** Add File: path`, `*** Update File: path` \
                          (with optional `*** Move to: path`), `*** Delete File: path`, then \
                          `*** End Patch`. In an update, `@@` starts a hunk, `-` lines must match \
                          the file exactly and uniquely, `+` lines replace them, and a leading \
                          space is a context line kept on both sides. Use this for a rename or a \
                          change that touches several files; use edit for one change in one file. \
                          Every path is checked against the policy, and one denial refuses the \
                          whole patch.",
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
            name: "ls",
            description: "List a directory: one entry per line, directories ending in `/`. Use \
                          this rather than `ls` in bash.",
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
            name: "find",
            description: "Find files by name. A glob without a slash, such as `*.rs`, matches \
                          file names at any depth; one with a slash, such as `src/**/mod.rs`, \
                          matches paths from the search directory. Skips .git and files \
                          .gitignore lists. Use this rather than `find` in bash.",
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
            name: "grep",
            description: "Search file contents with a regular expression (Rust regex syntax). \
                          Returns `path:line: text` for each matching line, up to 200. Skips \
                          binary files, .git and files .gitignore lists. Use this rather than \
                          grep or rg in bash.",
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
            name: "bash",
            description: "Run a shell command with `sh -c` in the workspace root and return its \
                          exit code, stdout and stderr. Use it for builds, tests and git. The \
                          command runs in a sandbox: no network, \
                          files limited to the granted roots, a time limit, and background \
                          processes are killed when it exits. Output past 256 KiB per stream is \
                          cut.",
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
            name: "fetch",
            description: "Fetch one http or https URL and return its status, content type and \
                          body as text. The host makes the request, so it needs a net grant for \
                          that host; `bash` never has network access, so use this rather than \
                          curl. Only GET, and the body is cut at 100 KiB. A key or other secret \
                          belongs in `headers`; it is never written to the audit log or shown \
                          back to you.",
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
            name: "websearch",
            description: "Search the web and return the most relevant pages with the passages that \
                          answer the query. Use it when the answer is not in the workspace and you \
                          do not already know the address; use `fetch` when you do. It needs a net \
                          grant for the search host, and it names the host it used in the result. \
                          Five results come back unless `num_results` says otherwise, and each \
                          passage is an excerpt: pass `result` with a result's number to read that \
                          one in full, which reuses the search already made rather than asking \
                          again.",
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
