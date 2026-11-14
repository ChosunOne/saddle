//! MCP servers: the ones a person declared in `config.toml`, and the transport that reaches one
//! (D104). The grant in the policy (`mcp`, D103) still decides whether any of them runs and which
//! tools may be called; this module only reads what was declared and refuses a table that does not
//! describe exactly one server.

pub mod host;
pub mod http;
pub mod lower;
pub mod oauth;
pub mod protocol;
pub mod registry;
pub mod stdio;

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// A per-call deadline when the table does not set one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The longest description the prompt will show for a server, so one table cannot push the rest
/// of the prompt out of the way.
const MAX_DESCRIPTION: usize = 200;

/// The hosts a plain-http server may name. Everything else is https, because a plain-http server
/// to a remote host sends its traffic, and whatever token it carries, in the clear.
const LOOPBACK_HTTP: [&str; 3] = ["http://127.0.0.1", "http://localhost", "http://[::1]"];

/// One `[mcp.<name>]` table: what to run, or where to reach it (D104).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct McpEntry {
    /// The prompt's one line about this server. A person writes it, so unlike a server's own text
    /// it is not untrusted prose.
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Variables the child gets, on top of the allowlisted environment an `exec` inherits.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A directory under a root, in the form the file tools take; defaults to the workspace.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Seconds a single call may take.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// How this server's tools reach the model (D117). Presentation, not authority: the grant still
    /// decides what may be called.
    #[serde(default, deserialize_with = "exposure")]
    pub exposure: Exposure,
    /// Per-tool overrides, by exact tool name (D122). A pattern is refused: the grant names tools
    /// exactly, and a wildcard here would be the one pattern in a file a person writes by hand.
    #[serde(default, rename = "tool-exposure", deserialize_with = "tool_exposures")]
    pub tool_exposure: BTreeMap<String, Exposure>,
}

/// How a server's tools reach the model, in pi's vocabulary (D117).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Exposure {
    /// Not declared until `tool_search` matches one. The default, and the cheapest.
    #[default]
    Deferred,
    /// Declared in the prompt from the first request, like a built-in. The server is started when
    /// the session opens, because its schemas are needed before the model says anything.
    Direct,
    /// Registered and never reachable: the table can say "off" without the grant losing the tools
    /// it names, so turning a server back on cannot be mistaken for granting it (D117).
    Hidden,
}

impl Exposure {
    /// The word in the table, refusing pi's `codemode` by name: a person who asked for it asked
    /// for something saddle does not do, and a silent fall back to deferring would hide that.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "deferred" => Ok(Self::Deferred),
            "direct" => Ok(Self::Direct),
            "hidden" => Ok(Self::Hidden),
            "codemode" | "codemode-deferred" => Err(
                "saddle has no codemode: tools are called directly, so use `\"deferred\"` to \
                 declare them through tool_search or `\"direct\"` to declare them up front"
                    .to_owned(),
            ),
            other => Err(format!(
                "`{other}` is not an exposure: use `\"deferred\"`, `\"direct\"` or `\"hidden\"`"
            )),
        }
    }
}

/// An exposure written as a string, with the refusal above rather than serde's list of variants.
fn exposure<'de, D>(deserializer: D) -> Result<Exposure, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    Exposure::parse(&text).map_err(serde::de::Error::custom)
}

/// The per-tool map (D122): every value is an exposure, and every key is an exact tool name.
fn tool_exposures<'de, D>(deserializer: D) -> Result<BTreeMap<String, Exposure>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
    let mut out = BTreeMap::new();
    for (name, text) in raw {
        if name.contains('*') {
            return Err(serde::de::Error::custom(format!(
                "`{name}` is a pattern: tool-exposure names tools exactly, the way the grant does"
            )));
        }
        out.insert(
            name,
            Exposure::parse(&text).map_err(serde::de::Error::custom)?,
        );
    }
    Ok(out)
}

/// A description as one line of at most 100 characters, since a server may write a paragraph.
pub fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 100 {
        let cut: String = line.chars().take(99).collect();
        format!("{cut}\u{2026}")
    } else {
        line.to_owned()
    }
}

/// How a call reaches the server, which the table describes exactly one way (D104).
#[derive(Debug, PartialEq, Eq)]
pub enum Transport<'a> {
    /// A child process spoken to over stdin and stdout.
    Stdio {
        command: &'a str,
        args: &'a [String],
    },
    /// A streamable-HTTP endpoint, reached by the host as `fetch` is (D39).
    Http { url: &'a str },
}

impl McpEntry {
    /// How this server's tools reach the model, which is the table's own choice (D117).
    pub fn exposure(&self) -> Exposure {
        self.exposure
    }

    /// What one tool's exposure is: its own override, or the server's (D122).
    pub fn exposure_of(&self, tool: &str) -> Exposure {
        self.tool_exposure
            .get(tool)
            .copied()
            .unwrap_or(self.exposure)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs)
    }

    /// Which transport the table describes, refusing a table that describes both or neither.
    pub fn transport(&self) -> Result<Transport<'_>> {
        match (&self.command, &self.url) {
            (Some(command), None) => Ok(Transport::Stdio {
                command,
                args: &self.args,
            }),
            (None, Some(url)) => Ok(Transport::Http { url }),
            (Some(_), Some(_)) => bail!("names both a `command` and a `url`"),
            (None, None) => bail!("names neither a `command` nor a `url`"),
        }
    }

    /// Checks what the table says by itself, so a mistake is reported when the session opens
    /// rather than when the model first reaches for the server.
    fn validate(&self) -> Result<()> {
        match self.transport()? {
            Transport::Stdio { command, .. } => {
                if command.trim().is_empty() {
                    bail!("`command` is empty");
                }
            }
            Transport::Http { url } => {
                check_url(url)?;
                if !self.args.is_empty() {
                    bail!("`args` belongs to a `command`, not a `url`");
                }
                if !self.env.is_empty() {
                    bail!("`env` belongs to a `command`, not a `url`");
                }
                if self.cwd.is_some() {
                    bail!("`cwd` belongs to a `command`, not a `url`");
                }
            }
        }
        if let Some(description) = &self.description
            && description.chars().count() > MAX_DESCRIPTION
        {
            bail!("`description` is longer than {MAX_DESCRIPTION} characters");
        }
        if self.timeout == Some(0) {
            bail!("`timeout` must be positive");
        }
        Ok(())
    }
}

/// A `url` is https, or plain http to this machine only.
fn check_url(url: &str) -> Result<()> {
    if let Some(rest) = url.strip_prefix("https://") {
        return check_host(rest);
    }
    let loopback = LOOPBACK_HTTP.iter().find(|prefix| {
        url.strip_prefix(*prefix)
            .is_some_and(|rest| rest.starts_with(':') || rest.starts_with('/'))
    });
    if loopback.is_none() {
        bail!("`url` must be https:// (plain http only for this machine)");
    }
    Ok(())
}

fn check_host(rest: &str) -> Result<()> {
    let host = rest.split(['/', ':']).next().unwrap_or_default();
    if host.is_empty() {
        bail!("`url` names no host");
    }
    Ok(())
}

/// The `[mcp]` tables, keyed by the name a grant uses.
pub type McpTable = BTreeMap<String, McpEntry>;

/// Checks every table, naming the one at fault so a config with ten servers says which is wrong.
pub fn validate(table: &McpTable) -> Result<()> {
    for (name, entry) in table {
        entry
            .validate()
            .with_context(|| format!("`mcp.{name}` in config.toml"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_table(text: &str) -> Result<McpTable> {
        let table: McpTable = toml::from_str(text)?;
        validate(&table)?;
        Ok(table)
    }

    #[test]
    fn a_stdio_server_is_a_command_with_its_arguments() {
        let table = valid_table(
            r#"
[scout]
description = "scrapes boards"
command = "npx"
args = ["-y", "job-scout"]
env = { BOARD = "jobs" }
cwd = "."
"#,
        )
        .unwrap();
        let entry = &table["scout"];
        assert_eq!(entry.timeout(), DEFAULT_TIMEOUT);
        assert_eq!(
            entry.transport().unwrap(),
            Transport::Stdio {
                command: "npx",
                args: &["-y".to_owned(), "job-scout".to_owned()],
            }
        );
        assert_eq!(entry.env["BOARD"], "jobs");
    }

    #[test]
    fn a_remote_server_is_a_url_and_carries_nothing_else() {
        let table = valid_table(
            r#"[remote]
url = "https://mcp.example.com/mcp"
timeout = 10
"#,
        )
        .unwrap();
        assert_eq!(table["remote"].timeout(), Duration::from_secs(10));
        assert_eq!(
            table["remote"].transport().unwrap(),
            Transport::Http {
                url: "https://mcp.example.com/mcp"
            }
        );
        for body in [
            "url = \"https://x.example/mcp\"\nargs = [\"a\"]",
            "url = \"https://x.example/mcp\"\nenv = { A = \"b\" }",
            "url = \"https://x.example/mcp\"\ncwd = \".\"",
        ] {
            let text = format!("[remote]\n{body}\n");
            let err = format!("{:#}", valid_table(&text).unwrap_err());
            assert!(err.contains("mcp.remote"), "{err}");
        }
    }

    #[test]
    fn a_table_describes_exactly_one_transport_and_never_plain_http_off_this_machine() {
        for body in [
            "command = \"a\"\nurl = \"https://x.example/mcp\"",
            "description = \"neither\"",
            "command = \"  \"",
            "url = \"http://x.example/mcp\"",
            "url = \"ftp://x.example/mcp\"",
            "url = \"https://\"",
        ] {
            let text = format!("[s]\n{body}\n");
            assert!(valid_table(&text).is_err(), "{body} was accepted");
        }
        // Plain http is for this machine, where there is no wire to listen on.
        for url in ["http://127.0.0.1:8080/mcp", "http://localhost/mcp"] {
            let text = format!("[s]\nurl = \"{url}\"\n");
            assert!(valid_table(&text).is_ok(), "{url} was refused");
        }
    }

    #[test]
    fn a_description_is_one_line_worth_of_text() {
        let long = "x".repeat(MAX_DESCRIPTION + 1);
        let text = format!("[s]\ncommand = \"a\"\ndescription = \"{long}\"\n");
        let err = format!("{:#}", valid_table(&text).unwrap_err());
        assert!(err.contains("description"), "{err}");
        let ok = "x".repeat(MAX_DESCRIPTION);
        valid_table(&format!("[s]\ncommand = \"a\"\ndescription = \"{ok}\"\n")).unwrap();
    }
}
