use serde::de::Error;
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

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

const MAX_DESCRIPTION: usize = 200;

const LOOPBACK_HTTP: [&str; 3] = ["http://127.0.0.1", "http://localhost", "http://[::1]"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct McpEntry {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default, deserialize_with = "exposure")]
    pub exposure: Exposure,
    #[serde(default, rename = "tool-exposure", deserialize_with = "tool_exposures")]
    pub tool_exposure: BTreeMap<String, Exposure>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Exposure {
    #[default]
    Deferred,
    Direct,
    Hidden,
}

impl Exposure {
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

fn exposure<'de, D>(deserializer: D) -> Result<Exposure, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    Exposure::parse(&text).map_err(Error::custom)
}

fn tool_exposures<'de, D>(deserializer: D) -> Result<BTreeMap<String, Exposure>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, String>::deserialize(deserializer)?;
    let mut out = BTreeMap::new();
    for (name, text) in raw {
        if name.contains('*') {
            return Err(Error::custom(format!(
                "`{name}` is a pattern: tool-exposure names tools exactly, the way the grant does"
            )));
        }
        out.insert(name, Exposure::parse(&text).map_err(Error::custom)?);
    }
    Ok(out)
}

pub fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() > 100 {
        let cut: String = line.chars().take(99).collect();
        format!("{cut}\u{2026}")
    } else {
        line.to_owned()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Transport<'a> {
    Stdio {
        command: &'a str,
        args: &'a [String],
    },
    Http {
        url: &'a str,
    },
}

impl McpEntry {
    pub fn exposure(&self) -> Exposure {
        self.exposure
    }

    pub fn exposure_of(&self, tool: &str) -> Exposure {
        self.tool_exposure
            .get(tool)
            .copied()
            .unwrap_or(self.exposure)
    }

    pub fn timeout(&self) -> Duration {
        self.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs)
    }

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

pub type McpTable = BTreeMap<String, McpEntry>;

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
