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

/// The prefix that keeps a server's tool from ever shadowing a built-in one.
pub const PREFIX: &str = "mcp__";

/// How many matches one search declares, so a broad query cannot fill the context.
const MATCHES: usize = 8;

/// One running server and its relay.
pub struct Running {
    client: Client,
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
        let (command, args) = match entry.transport()? {
            crate::mcp::Transport::Stdio { command, args } => (command.to_owned(), args.to_vec()),
            crate::mcp::Transport::Http { url } => {
                bail!("{url} is a streamable-HTTP server, which is not built yet")
            }
        };
        let helper = self.helper.clone().context("locating the saddle binary")?;
        let mut spec =
            crate::mcp::lower::spec(session, &grant, &entry, &command, &args, &self.host_env)?;
        let mut bridge = self.tmp.as_deref().and_then(|dir| Bridge::start(dir).ok());
        if let Some(bridge) = &bridge {
            spec.net = Some(bridge.spec());
        }
        let mut client = Client::start(server, &helper, &spec)?;
        let tools = {
            let mut service = || {
                if let Some(bridge) = bridge.as_mut() {
                    bridge.service(session);
                }
            };
            client.initialize(&mut service)?;
            client.tools(&mut service)?
        };
        for tool in tools {
            let key = Self::name(server, &tool.name);
            self.catalogue.insert(key, (server.to_owned(), tool));
        }
        self.servers
            .insert(server.to_owned(), Running { client, bridge });
        Ok(())
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

    /// Calls one declared tool. A granted tool that was never declared is a host error rather than
    /// a denial: the model named something it cannot see, which is a protocol mistake, not a
    /// permission one.
    pub fn call(&mut self, key: &str, input: &Value, session: &mut Session) -> Result<ToolOutput> {
        let (server, tool) = self
            .catalogue
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("unknown tool `{key}`"))?;
        if !self.grants.iter().any(|g| g.server == server) {
            return Ok(ToolOutput::denied(&format!("`{server}` is not granted")));
        }
        if !self.granted(&server, &tool.name) {
            return Ok(ToolOutput::denied(&format!(
                "`{}` is not one of the tools granted for `{server}`",
                tool.name
            )));
        }
        if !self.loaded.contains(key) {
            bail!("`{key}` has not been declared: search for it first");
        }
        let mut denials = Vec::new();
        let running = self
            .servers
            .get_mut(&server)
            .ok_or_else(|| anyhow!("`{server}` is not running"))?;
        let outcome = {
            // Borrowed as two fields, so the relay can be serviced while the client is being
            // waited on: they are different things that happen to live together.
            let Running { client, bridge } = running;
            let mut service = || {
                if let Some(bridge) = bridge.as_mut() {
                    denials.extend(bridge.service(session));
                }
            };
            client.call(&tool.name, input, &mut service)
        }?;
        let mut out = match outcome {
            (true, text) => ToolOutput::ok(text),
            (false, text) => ToolOutput::err(text),
        };
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
