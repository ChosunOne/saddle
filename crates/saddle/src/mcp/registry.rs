//! D108: the tools the model can see. A server's tools are not declared to the model until it asks
//! for them: `tool_search` matches a query against what the granted servers offer and declares the
//! matches, which stay declared for the session. Nothing here decides authority — the grant does
//! (D103), so a tool the grant does not name is never declared and never callable, however loudly
//! the server advertises it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::mcp::McpEntry;
use crate::mcp::stdio::{Client, Tool};
use crate::net::bridge::Bridge;
use crate::provider::ToolDef;
use crate::session::Session;
use crate::tools::ToolOutput;

/// What a call carried, for the audit log: how much and what, never what it said. A tool's
/// arguments may hold a secret the way a `fetch` query string may, so both are recorded the same
/// way — a length and a hash (D24, D112).
pub fn arguments_record(input: &Value) -> Value {
    let text = input.to_string();
    serde_json::json!({
        "bytes": text.len(),
        "sha256": saddle_policy::audit::sha256_hex(text.as_bytes()),
    })
}

/// One MCP tool call, allowed or refused, recorded against the server it was for (D112). One
/// function, because the model's call and a person's `saddle mcp call` are the same decision and
/// two record shapes would be two answers to what the audit log means.
pub fn record_call(
    session: &mut Session,
    server: &str,
    tool: &str,
    arguments: &Value,
    allow: Option<bool>,
    explanation: &str,
) -> Result<()> {
    let args = serde_json::json!({
        "via": "mcp",
        "server": server,
        "tool": tool,
        "arguments": arguments,
    });
    session
        .audit
        .event(
            saddle_policy::Kind::Decision,
            Some(&saddle_cap::Principal::Agent),
            Some(saddle_cap::Scope::Mcp),
            allow,
            args,
            explanation,
        )
        .context("writing the audit log")?;
    Ok(())
}

/// The prefix that keeps a server's tool from ever shadowing a built-in one.
pub const PREFIX: &str = "mcp__";

/// How many matches one search declares, so a broad query cannot fill the context.
const MATCHES: usize = 8;

/// The server and tool a declared name stands for. A name that does not split is not a tool of
/// ours; one that splits wrongly can only ever be refused, because the grant lookup that follows
/// is exact — so a server whose own name holds `__` loses its declarations rather than widening.
fn parse(key: &str) -> Option<(&str, &str)> {
    let (server, tool) = key.strip_prefix(PREFIX)?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// How one running server is spoken to: a child's pipes, or the host's own requests (D107, D110).
pub enum Connection {
    Stdio(Client),
    Http(crate::mcp::http::Client),
}

/// One running server and its relay.
pub struct Running {
    conn: Connection,
    bridge: Option<Bridge>,
}

/// What one session knows about the servers its policy grants.
#[derive(Default)]
pub struct Registry {
    /// The servers a grant names (D103), and the tables that declare them (D104).
    grants: Vec<saddle_cap::McpGrant>,
    entries: BTreeMap<String, McpEntry>,
    /// What each running server offers, keyed `mcp__<server>__<tool>`.
    catalogue: BTreeMap<String, (String, Tool)>,
    loaded: BTreeSet<String>,
    servers: BTreeMap<String, Running>,
    /// Servers that could not be started, and why: remembered, so a search does not retry one that
    /// is broken or repeat the same failure.
    failures: BTreeMap<String, String>,
    host_env: Vec<(String, String)>,
    tmp: Option<PathBuf>,
    helper: Option<PathBuf>,
}

impl Registry {
    pub fn new(
        grants: Vec<saddle_cap::McpGrant>,
        entries: BTreeMap<String, McpEntry>,
        host_env: Vec<(String, String)>,
        tmp: Option<PathBuf>,
    ) -> Self {
        Self {
            grants,
            entries,
            host_env,
            tmp,
            helper: std::env::current_exe().ok(),
            ..Self::default()
        }
    }

    /// The name the model sees for one tool of one server.
    pub fn name(server: &str, tool: &str) -> String {
        format!("{PREFIX}{server}__{tool}")
    }

    /// Starts every granted server that is not running, and records what each offers. Failures are
    /// returned as notes for the model, once.
    pub fn discover(&mut self, session: &mut Session) -> Vec<String> {
        let mut notes = Vec::new();
        let pending: Vec<String> = self
            .grants
            .iter()
            .map(|g| g.server.clone())
            .filter(|name| !self.servers.contains_key(name) && !self.failures.contains_key(name))
            .collect();
        for name in pending.into_iter() {
            if let Err(e) = self.connect(&name, session) {
                let why = format!("{e:#}");
                notes.push(format!("{name}: {why}"));
                self.failures.insert(name, why);
            }
        }
        notes
    }

    /// Starts one server and records the tools it offers. The grant is consulted when a tool is
    /// declared or called, not here: what a server offers and what the model may use are different
    /// questions, and only the second is authority.
    fn connect(&mut self, server: &str, session: &mut Session) -> Result<()> {
        let grant = self
            .grants
            .iter()
            .find(|g| g.server == server)
            .cloned()
            .ok_or_else(|| anyhow!("`{server}` is not granted"))?;
        let entry = self
            .entries
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow!("`{server}` has no [mcp.{server}] table in config.toml"))?;
        match entry.transport()? {
            crate::mcp::Transport::Stdio { command, args } => {
                let helper = self.helper.clone().context("locating the saddle binary")?;
                let mut spec = crate::mcp::lower::spec(
                    session,
                    &grant,
                    &entry,
                    command,
                    args,
                    &self.host_env,
                )?;
                let mut bridge = self.tmp.as_deref().and_then(|dir| Bridge::start(dir).ok());
                if let Some(bridge) = &bridge {
                    spec.net = Some(bridge.spec());
                }
                let mut client = Client::start(server, &helper, &spec, entry.timeout())?;
                let tools = {
                    let mut service = || {
                        if let Some(bridge) = bridge.as_mut() {
                            bridge.service(session);
                        }
                    };
                    client.initialize(&mut service)?;
                    client.tools(&mut service)?
                };
                self.record(server, tools);
                self.servers.insert(
                    server.to_owned(),
                    Running {
                        conn: Connection::Stdio(client),
                        bridge,
                    },
                );
            }
            crate::mcp::Transport::Http { url } => {
                // A server with no child still has a reach: its net grants, narrowed from the
                // agent's, are what every request of its own is decided against (D105, D110).
                let reach = crate::mcp::lower::net_reach(session, &grant)?;
                let mut client =
                    crate::mcp::http::Client::start(server, url, reach, entry.timeout())?;
                client.initialize(session)?;
                let tools = client.tools(session)?;
                self.record(server, tools);
                self.servers.insert(
                    server.to_owned(),
                    Running {
                        conn: Connection::Http(client),
                        bridge: None,
                    },
                );
            }
        }
        Ok(())
    }

    /// Files what a server offers under namespaced keys (D108).
    fn record(&mut self, server: &str, tools: Vec<Tool>) {
        for tool in tools {
            let key = Self::name(server, &tool.name);
            self.catalogue.insert(key, (server.to_owned(), tool));
        }
    }

    /// Starts one server by name, so `saddle mcp call` reaches the one it was given rather than
    /// every granted server. The discovery path starts them all at once; this is the same
    /// `connect`, reached by name (D111).
    pub fn start(&mut self, server: &str, session: &mut Session) -> Result<()> {
        self.connect(server, session)
    }

    /// Declares every tool of one server the grant names, without a search (D111). A by-hand call
    /// has no conversation to declare into, so this is the model's "search for everything" with
    /// nothing recorded: the session it belongs to is about to end.
    pub fn declare_server(&mut self, server: &str) {
        for (key, (owner, tool)) in &self.catalogue {
            if owner == server && self.granted(server, &tool.name) {
                self.loaded.insert(key.clone());
            }
        }
    }

    /// Declares what matches `query`. An empty query declares everything the granted servers offer,
    /// which is what a model with no idea what is available should ask for.
    pub fn search(&mut self, query: &str) -> Vec<String> {
        let wanted = query.trim().to_lowercase();
        let words: Vec<&str> = wanted.split_whitespace().collect();
        let mut scored: Vec<(usize, String)> = Vec::new();
        for (key, (server, tool)) in &self.catalogue {
            if !self.granted(server, &tool.name) {
                continue;
            }
            let haystack = format!("{key} {}", tool.description).to_lowercase();
            let score = if words.is_empty() {
                1
            } else if haystack.contains(&wanted) {
                10 + words.iter().filter(|w| haystack.contains(**w)).count()
            } else {
                let hits = words.iter().filter(|w| haystack.contains(**w)).count();
                if hits == words.len() { hits } else { 0 }
            };
            if score > 0 {
                scored.push((score, key.clone()));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let mut declared = Vec::new();
        for (_, key) in scored.into_iter().take(MATCHES) {
            self.loaded.insert(key.clone());
            if let Some((_, tool)) = self.catalogue.get(&key) {
                let first = tool.description.lines().next().unwrap_or_default().trim();
                declared.push(format!("{key}: {first}"));
            }
        }
        declared
    }

    fn granted(&self, server: &str, tool: &str) -> bool {
        self.grants
            .iter()
            .find(|g| g.server == server)
            .is_some_and(|g| g.allows(tool))
    }

    /// The definitions to add to the tool list: what this session has declared, and nothing else.
    pub fn declared(&self) -> Vec<ToolDef> {
        self.loaded
            .iter()
            .filter_map(|key| {
                let (server, tool) = self.catalogue.get(key)?;
                if !self.granted(server, &tool.name) {
                    return None;
                }
                Some(ToolDef {
                    name: key.clone().into(),
                    description: format!(
                        "{}\n\n(the tool `{}` of the `{}` server)",
                        tool.description, tool.name, server
                    )
                    .into(),
                    schema: tool.schema.clone(),
                })
            })
            .collect()
    }

    /// What this session has declared, in name order, for the store to record (D109).
    pub fn loaded(&self) -> Vec<String> {
        self.loaded.iter().cloned().collect()
    }

    /// Puts back the declarations a branch was saved with, keeping only those the grant in force
    /// still names (D109). A branch recorded under a wider policy therefore confers nothing under
    /// a narrower one, which is why this is a filter and not an assignment.
    pub fn restore(&mut self, names: &[String]) {
        let kept: BTreeSet<String> = names
            .iter()
            .filter(|name| match parse(name) {
                Some((server, tool)) => self.granted(server, tool),
                None => false,
            })
            .cloned()
            .collect();
        self.loaded = kept;
    }

    /// Calls one declared tool. A granted tool that was never declared is a host error rather than
    /// a denial: the model named something it cannot see, which is a protocol mistake, not a
    /// permission one.
    pub fn call(&mut self, key: &str, input: &Value, session: &mut Session) -> Result<ToolOutput> {
        let (server, tool) = self
            .catalogue
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("unknown tool `{key}`"))?;
        // D112: a call is a decision wherever it is answered, so a refusal is recorded here and a
        // grant is recorded below — the same shape every other tool's allow carries, with the
        // arguments hashed rather than quoted (D24).
        let arguments = arguments_record(input);
        let refusal = if !self.grants.iter().any(|g| g.server == server) {
            Some(format!("`{server}` is not granted"))
        } else if !self.granted(&server, &tool.name) {
            Some(format!(
                "`{}` is not one of the tools granted for `{server}`",
                tool.name
            ))
        } else {
            None
        };
        let grant = self.grants.iter().find(|g| g.server == server);
        match &refusal {
            Some(why) => {
                record_call(session, &server, &tool.name, &arguments, None, why)?;
                return Ok(ToolOutput::denied(why));
            }
            None => {
                let (rule, why) = grant
                    .map(|g| (g.rule.to_string(), g.why.clone()))
                    .unwrap_or_default();
                let explanation = if why.is_empty() {
                    format!("granted by {rule}")
                } else {
                    format!("granted by {rule}: {why}")
                };
                record_call(
                    session,
                    &server,
                    &tool.name,
                    &arguments,
                    Some(true),
                    &explanation,
                )?;
            }
        }
        if !self.loaded.contains(key) {
            bail!("`{key}` has not been declared: search for it first");
        }
        let mut denials = Vec::new();
        let running = self
            .servers
            .get_mut(&server)
            .ok_or_else(|| anyhow!("`{server}` is not running"))?;
        let Running { conn, bridge } = running;
        let outcome = match conn {
            Connection::Stdio(client) => {
                let mut service = || {
                    if let Some(bridge) = bridge.as_mut() {
                        denials.extend(bridge.service(session));
                    }
                };
                let (ok, text) = client.call(&tool.name, input, &mut service)?;
                if ok {
                    ToolOutput::ok(text)
                } else {
                    ToolOutput::err(text)
                }
            }
            Connection::Http(client) => match client.call(session, &tool.name, input)? {
                crate::mcp::http::Outcome::Answered(text) => ToolOutput::ok(text),
                crate::mcp::http::Outcome::Failed(text) => ToolOutput::err(text),
                crate::mcp::http::Outcome::Denied(why) => ToolOutput::denied(&why),
            },
        };
        let mut out = outcome;
        for denial in denials {
            out.content.push_str(&format!("\n{denial}"));
        }
        Ok(out)
    }

    /// Stops every server, so a session leaves none behind.
    pub fn close(&mut self) {
        self.servers.clear();
    }

    /// The tools the servers were seen offering that no grant names: what a server grew overnight
    /// is visible without being callable (D103).
    pub fn ungranted(&self) -> Vec<String> {
        self.catalogue
            .iter()
            .filter(|(_, (server, tool))| !self.granted(server, &tool.name))
            .map(|(key, _)| key.clone())
            .collect()
    }

    #[cfg(test)]
    fn attach(&mut self, server: &str, tool: Tool) {
        let key = Self::name(server, &tool.name);
        self.catalogue.insert(key, (server.to_owned(), tool));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_restored_declaration_is_checked_against_the_grant_again() {
        // D109: a branch carries names, not authority. Restoring them under a narrower policy —
        // or under a name that is not a tool of ours at all — keeps only what is still granted.
        let mut registry = Registry::new(
            vec![granted("fake", &["search"])],
            BTreeMap::new(),
            Vec::new(),
            None,
        );
        registry.restore(&[
            Registry::name("fake", "search"),
            Registry::name("fake", "rm_rf"),
            Registry::name("other", "search"),
            "not a tool".to_owned(),
        ]);
        assert_eq!(registry.loaded(), [Registry::name("fake", "search")]);
    }

    fn granted(server: &str, tools: &[&str]) -> saddle_cap::McpGrant {
        let list: String = tools.iter().map(|t| format!("\"{t}\",")).collect();
        let text = format!(
            "version = 1\n\n[agent]\nmcp = [{{ server = \"{server}\", tools = [{list}] }}]\n"
        );
        let empty: [(saddle_cap::RootName, PathBuf); 0] = [];
        let roots = saddle_cap::Roots::resolve(empty, &[]).unwrap();
        saddle_cap::Policy::parse(&text, &roots)
            .unwrap()
            .grants(&saddle_cap::Principal::Agent)
            .unwrap()
            .mcp[0]
            .clone()
    }

    fn registry() -> Registry {
        Registry::new(
            vec![granted("scout", &["search"]), granted("db", &["query"])],
            BTreeMap::new(),
            Vec::new(),
            None,
        )
    }

    fn tool(name: &str, description: &str) -> Tool {
        Tool {
            name: name.to_owned(),
            description: description.to_owned(),
            schema: json!({"type": "object"}),
        }
    }

    #[test]
    fn a_search_declares_what_it_matches_and_never_what_the_grant_does_not_name() {
        let mut registry = registry();
        registry.attach("scout", tool("search", "finds job postings"));
        registry.attach("scout", tool("delete_everything", "removes every posting"));
        registry.attach("db", tool("query", "runs one sql query"));
        let declared = registry.search("job");
        assert_eq!(declared.len(), 1, "{declared:?}");
        assert!(
            declared[0].starts_with("mcp__scout__search"),
            "{declared:?}"
        );
        assert_eq!(
            registry
                .declared()
                .iter()
                .map(|d| d.name.clone())
                .collect::<Vec<_>>(),
            ["mcp__scout__search"]
        );
        assert_eq!(
            registry.ungranted(),
            ["mcp__scout__delete_everything"],
            "a tool the grant does not name is seen and not callable"
        );
    }

    #[test]
    fn an_empty_search_declares_every_granted_tool_and_says_where_each_came_from() {
        let mut registry = registry();
        registry.attach("scout", tool("search", "finds job postings"));
        registry.attach("db", tool("query", "runs one sql query"));
        assert_eq!(registry.search("").len(), 2);
        let definitions = registry.declared();
        assert_eq!(definitions.len(), 2);
        let db = definitions
            .iter()
            .find(|d| d.name == "mcp__db__query")
            .unwrap();
        assert!(
            db.description.contains("`query` of the `db` server"),
            "{}",
            db.description
        );
    }

    #[test]
    fn a_search_that_matches_nothing_declares_nothing() {
        let mut registry = registry();
        registry.attach("scout", tool("search", "finds job postings"));
        assert!(registry.search("kubernetes").is_empty());
        assert!(registry.declared().is_empty());
    }
}
