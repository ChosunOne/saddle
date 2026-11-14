//! D108: the tools the model can see. A server's tools are not declared to the model until it asks
//! for them: `tool_search` matches a query against what the granted servers offer and declares the
//! matches, which stay declared for the session. Nothing here decides authority — the grant does
//! (D103), so a tool the grant does not name is never declared and never callable, however loudly
//! the server advertises it.

use crate::mcp::Exposure;
use crate::mcp::host;
use crate::mcp::http::Client as HttpClient;
use crate::mcp::http::Outcome;
use crate::mcp::lower;
use saddle_policy::audit;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::mcp::stdio::{Client, Tool};
use crate::mcp::{McpEntry, first_line};
use crate::net::bridge::Bridge;
use crate::provider::ToolDef;
use crate::session::Session;
use crate::tools::ToolOutput;

/// What a call carried, for the audit log: how much and what, never what it said. A tool's
/// arguments may hold a secret the way a `fetch` query string may, so both are recorded the same
/// way — a length and a hash (D24, D112).
pub fn arguments_record(input: &Value) -> Value {
    let text = input.to_string();
    json!({
        "bytes": text.len(),
        "sha256": audit::sha256_hex(text.as_bytes()),
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
    let args = json!({
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
    Http(HttpClient),
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
    /// Servers a person turned off for this session only: not a config change, and forgotten when
    /// the session ends (D123).
    suspended: BTreeSet<String>,
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
            helper: env::current_exe().ok(),
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
            .filter(|name| !self.is_off(name))
            .collect();
        for name in pending.into_iter() {
            if let Err(e) = self.connect(&name, session) {
                let why = format!("{e:#}");
                notes.push(format!("{name}: {why}"));
                self.failures.insert(name, why);
            }
        }
        // A server that said its catalogue changed is read again before this search scores
        // anything, so the model is never offered a tool the server has since withdrawn (D114).
        let running: Vec<String> = self.servers.keys().cloned().collect();
        for name in running {
            if let Err(e) = self.refresh(&name, session) {
                notes.push(format!("{name}: {e:#}"));
            }
        }
        notes
    }

    /// Re-reads one server's catalogue if it said it changed. A failure here is not a failure of
    /// the session: the old catalogue is still what the grant decides over.
    fn refresh(&mut self, server: &str, session: &mut Session) -> Result<()> {
        let Some(Running { conn, bridge }) = self.servers.get_mut(server) else {
            return Ok(());
        };
        let tools = match conn {
            Connection::Stdio(client) => {
                if !client.stale() {
                    return Ok(());
                }
                let mut service = || {
                    if let Some(bridge) = bridge.as_mut() {
                        bridge.service(session);
                    }
                };
                let tools = client.tools(&mut service)?;
                client.refreshed();
                tools
            }
            Connection::Http(_) => return Ok(()),
        };
        self.catalogue.retain(|_, (owner, _)| owner != server);
        self.record(server, tools);
        Ok(())
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
                let mut spec = lower::spec(session, &grant, &entry, command, args, &self.host_env)?;
                let mut bridge = self.tmp.as_deref().and_then(|dir| Bridge::start(dir).ok());
                if let Some(bridge) = &bridge {
                    spec.net = Some(bridge.spec());
                }
                let roots = host::roots(session);
                let mut client = Client::start(server, &helper, &spec, entry.timeout(), roots)?;
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
                let reach = lower::net_reach(session, &grant)?;
                let mut client = HttpClient::start(server, url, reach, entry.timeout())?;
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

    /// What one tool's exposure is, from its table: the tool's own word, the server's, or `hidden`
    /// when there is no table to ask (D117, D122).
    fn exposure(&self, server: &str, tool: &str) -> Exposure {
        self.entries
            .get(server)
            .map_or(Exposure::Deferred, |entry| entry.exposure_of(tool))
    }

    /// Whether a server is off: `hidden` in its table, or turned off for this session (D117, D123).
    fn is_off(&self, server: &str) -> bool {
        self.is_hidden(server) || self.suspended.contains(server)
    }

    /// Turns one server off for this session, without touching the config a person would have to
    /// keep in step (D123). Dropping the client kills its child, so off means off.
    pub fn suspend(&mut self, server: &str) -> Result<String> {
        if !self.entries.contains_key(server) {
            bail!("`{server}` has no [mcp.{server}] table in config.toml");
        }
        self.suspended.insert(server.to_owned());
        self.servers.remove(server);
        let note = format!("{server} is off for this session");
        Ok(note)
    }

    /// Turns it back on. Not started again: it starts on demand, or on `/mcp restart`.
    pub fn resume(&mut self, server: &str) -> Result<String> {
        if !self.suspended.remove(server) {
            bail!("`{server}` is not off in this session");
        }
        self.failures.remove(server);
        Ok(format!(
            "{server} is available again; it starts when it is next needed"
        ))
    }

    /// Whether a server's table says `hidden`, in which case it is not started at all (D117).
    fn is_hidden(&self, server: &str) -> bool {
        self.entries
            .get(server)
            .is_some_and(|entry| entry.exposure() == Exposure::Hidden)
    }

    /// The servers whose table says `direct`, which are started when the session opens because
    /// their schemas are needed before the model's first request (D117).
    pub fn direct(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(name, entry)| {
                entry.exposure() == Exposure::Direct
                    && self.grants.iter().any(|g| &g.server == *name)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Starts the `direct` servers. Called where the model loop is set up rather than from a
    /// search, because there is nothing to search for: the table already said what to declare.
    pub fn start_direct(&mut self, session: &mut Session) -> Vec<String> {
        let mut notes = Vec::new();
        for name in self.direct() {
            if self.failures.contains_key(&name) {
                continue;
            }
            if let Err(e) = self.connect(&name, session) {
                let why = format!("{e:#}");
                notes.push(format!("{name}: {why}"));
                self.failures.insert(name, why);
            }
        }
        notes
    }

    /// The `mcp_servers` section of the prompt: one line per granted server, saying how its tools
    /// are reached and what the person wrote about it (D120). Only the person's own words appear —
    /// a server's instructions and tool descriptions are its text, and the prompt is not the place
    /// for prose a server controls (D104).
    pub fn prompt_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for grant in &self.grants {
            let Some(entry) = self.entries.get(&grant.server) else {
                out.push(format!(
                    "  {} — granted, but no [mcp.{}] table in config.toml, so it cannot start",
                    grant.server, grant.server
                ));
                continue;
            };
            if entry.exposure() == Exposure::Hidden {
                continue;
            }
            let how = match entry.exposure() {
                Exposure::Direct => "direct: its tools are in your list already",
                Exposure::Deferred | Exposure::Hidden => "deferred: search for one to declare it",
            };
            let description = entry
                .description
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(|d| format!(" — {}", first_line(d)))
                .unwrap_or_default();
            out.push(format!(
                "  {} — {how} — tools: {}{description}",
                grant.server,
                grant.tools.join(", ")
            ));
        }
        out
    }

    /// What `/mcp` shows: one line per granted server, its exposure, whether it is running, how
    /// much of what it offers is granted, and what went wrong if anything did (D118).
    pub fn report(&self) -> Vec<String> {
        let mut out = Vec::new();
        for grant in &self.grants {
            let entry = self.entries.get(&grant.server);
            let exposure = match entry.map(McpEntry::exposure) {
                Some(Exposure::Direct) => "direct",
                Some(Exposure::Hidden) => "hidden",
                Some(Exposure::Deferred) | None => "deferred",
            };
            let state = if self.servers.contains_key(&grant.server) {
                "running".to_owned()
            } else if let Some(why) = self.failures.get(&grant.server) {
                format!("failed: {}", first_line(why))
            } else if entry.is_none() {
                "no [mcp.{}] table".to_owned().replace("{}", &grant.server)
            } else if self.suspended.contains(&grant.server) {
                "off (this session)".to_owned()
            } else if entry.is_some_and(|e| e.exposure() == Exposure::Hidden) {
                "off".to_owned()
            } else {
                "not started".to_owned()
            };
            let offered = self
                .catalogue
                .keys()
                .filter(|key| key.starts_with(&format!("{PREFIX}{}__", grant.server)))
                .count();
            let granted = self
                .catalogue
                .iter()
                .filter(|(_, (owner, tool))| owner == &grant.server && grant.allows(&tool.name))
                .count();
            out.push(format!(
                "{:<16} {exposure:<9} {state:<24} {granted} of {offered} offered tool(s) granted, \
                 {} declared",
                grant.server,
                self.loaded
                    .iter()
                    .filter(|key| key.starts_with(&format!("{PREFIX}{}__", grant.server)))
                    .count(),
            ));
        }
        if out.is_empty() {
            out.push(
                "no mcp server is granted: add `mcp = [...]` to the agent's grants".to_owned(),
            );
        }
        out
    }

    /// Forgets a server and starts it again, which is what a person does when a server has been
    /// upgraded or its credentials have changed underneath a running session (D118). Dropping the
    /// client kills the child, so nothing of the old one survives into the new.
    pub fn restart(&mut self, server: &str, session: &mut Session) -> Result<String> {
        if !self.grants.iter().any(|g| g.server == server) {
            bail!("`{server}` is not granted");
        }
        if self.is_off(server) {
            bail!("`{server}` is off: `/mcp enable {server}` first");
        }
        self.servers.remove(server);
        self.failures.remove(server);
        self.catalogue
            .retain(|_, (owner, _)| owner.as_str() != server);
        self.connect(server, session)?;
        let tools = self
            .catalogue
            .iter()
            .filter(|(_, (owner, _))| owner == server)
            .count();
        Ok(format!("{server} restarted, offering {tools} tool(s)"))
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
            // A hidden server or tool is not searched, called or started: the switch is in the
            // table, and a catalogue that somehow holds the tool must not be a way past it
            // (D117, D122).
            if self.is_off(server)
                || self.exposure(server, &tool.name) == Exposure::Hidden
                || !self.granted(server, &tool.name)
            {
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

    /// The definitions to add to the tool list: what this session has declared by searching, plus
    /// every granted tool whose exposure is `direct` — whether the server said so or the tool's own
    /// entry did (D117, D122). A hidden server or tool contributes nothing, which is what makes the
    /// word a switch rather than a second grant.
    pub fn declared(&self) -> Vec<ToolDef> {
        let mut keys: BTreeSet<String> = self.loaded.clone();
        for (key, (server, tool)) in &self.catalogue {
            if self.is_off(server) {
                continue;
            }
            let direct = self.exposure(server, &tool.name) == Exposure::Direct;
            if direct && self.granted(server, &tool.name) {
                keys.insert(key.clone());
            }
        }
        keys.iter()
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
        let refusal = if self.suspended.contains(&server) {
            Some(format!(
                "`{server}` is off for this session: `/mcp enable {server}`"
            ))
        } else if self.exposure(&server, &tool.name) == Exposure::Hidden {
            Some(format!(
                "`{server}` is turned off in config.toml (`exposure = \"hidden\"`)"
            ))
        } else if !self.grants.iter().any(|g| g.server == server) {
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
                Outcome::Answered(text) => ToolOutput::ok(text),
                Outcome::Failed(text) => ToolOutput::err(text),
                Outcome::Denied(why) => ToolOutput::denied(&why),
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
    fn tool(name: &str) -> Tool {
        Tool {
            name: name.to_owned(),
            description: format!("{name} does things"),
            schema: json!({ "type": "object" }),
        }
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
    use crate::mcp::Exposure;
    use serde_json::json;

    #[test]
    fn a_server_can_be_turned_off_for_this_session_without_editing_the_config() {
        // D123: pi's `/mcp` toggle, made session-scoped. Off means off for search, declaration and
        // calls, and the report says which kind of off it is — the table's or this session's.
        let mut entries = BTreeMap::new();
        entries.insert(
            "scout".to_owned(),
            McpEntry {
                exposure: Exposure::Direct,
                ..McpEntry::default()
            },
        );
        let mut registry = Registry::new(
            vec![granted("scout", &["search"])],
            entries,
            Vec::new(),
            None,
        );
        registry.attach("scout", Registry::tool("search"));
        assert_eq!(registry.declared().len(), 1, "declared while it is on");

        registry.suspend("scout").unwrap();
        assert!(
            registry.declared().is_empty(),
            "a suspension must undeclare"
        );
        assert!(
            registry.search("").is_empty(),
            "and a search must not reach it"
        );
        assert!(
            registry.report().join("\n").contains("off (this session)"),
            "the report does not say why it is off: {:?}",
            registry.report()
        );

        registry.resume("scout").unwrap();
        assert_eq!(registry.declared().len(), 1, "and it comes back");
    }

    #[test]
    fn one_tool_can_be_declared_or_hidden_beside_its_server() {
        // D122: exposure per tool, on top of the server's own word. A `direct` tool of a deferred
        // server is in the prompt, a `hidden` tool of the same server is not reachable at all, and
        // neither can widen what the grant allows.
        let mut entries = BTreeMap::new();
        entries.insert(
            "scout".to_owned(),
            McpEntry {
                exposure: Exposure::Deferred,
                tool_exposure: BTreeMap::from([
                    ("search".to_owned(), Exposure::Direct),
                    ("rm_rf".to_owned(), Exposure::Hidden),
                ]),
                ..McpEntry::default()
            },
        );
        let mut registry = Registry::new(
            vec![granted("scout", &["search", "rm_rf"])],
            entries,
            Vec::new(),
            None,
        );
        registry.attach("scout", Registry::tool("search"));
        registry.attach("scout", Registry::tool("rm_rf"));

        let declared: Vec<String> = registry
            .declared()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            declared,
            [Registry::name("scout", "search")],
            "only the tool the table said was direct is declared"
        );
        let searched = registry.search("");
        assert!(
            !searched.iter().any(|key| key.contains("rm_rf")),
            "a hidden tool was declared by a search: {searched:?}"
        );
    }

    #[test]
    fn the_report_says_what_each_server_is_doing() {
        // D118: `/mcp` is one line per granted server — exposure, state, how much of what it
        // offers is granted — so a server that never started is visible before it is needed.
        let mut entries = BTreeMap::new();
        entries.insert(
            "front".to_owned(),
            McpEntry {
                exposure: Exposure::Direct,
                ..McpEntry::default()
            },
        );
        entries.insert(
            "off".to_owned(),
            McpEntry {
                exposure: Exposure::Hidden,
                ..McpEntry::default()
            },
        );
        let mut registry = Registry::new(
            vec![
                granted("front", &["search"]),
                granted("off", &["search"]),
                granted("ghost", &["search"]),
            ],
            entries,
            Vec::new(),
            None,
        );
        registry.attach("front", Registry::tool("search"));
        registry.attach("front", Registry::tool("rm_rf"));
        registry
            .failures
            .insert("off".to_owned(), "boom".to_owned());

        let report = registry.report().join("\n");
        assert!(report.contains("front"), "{report}");
        assert!(
            report.contains("direct"),
            "the exposure is not shown: {report}"
        );
        assert!(
            report.contains("1 of 2 offered tool(s) granted"),
            "the granted count is wrong or missing: {report}"
        );
        assert!(report.contains("hidden"), "{report}");
        assert!(
            report.contains("no [mcp.ghost] table"),
            "a granted server with no table is not called out: {report}"
        );
        assert!(
            report.contains("failed: boom"),
            "the last error is not shown: {report}"
        );
    }

    #[test]
    fn a_direct_server_is_declared_without_a_search_and_a_hidden_one_never_is() {
        // D117: exposure is presentation, so it decides what reaches the prompt and nothing about
        // what may be called. `direct` declares the granted tools up front; `hidden` declares none
        // however loudly the server advertises them.
        let tool = |name: &str| Tool {
            name: name.to_owned(),
            description: format!("{name} does things"),
            schema: json!({ "type": "object" }),
        };
        let mut entries = BTreeMap::new();
        entries.insert(
            "front".to_owned(),
            McpEntry {
                exposure: Exposure::Direct,
                ..McpEntry::default()
            },
        );
        entries.insert(
            "off".to_owned(),
            McpEntry {
                exposure: Exposure::Hidden,
                ..McpEntry::default()
            },
        );
        let mut registry = Registry::new(
            vec![
                granted("front", &["search", "fetch"]),
                granted("off", &["search"]),
            ],
            entries,
            Vec::new(),
            None,
        );
        registry.attach("front", tool("search"));
        registry.attach("front", tool("fetch"));
        registry.attach("front", tool("rm_rf"));
        registry.attach("off", tool("search"));

        let names: Vec<String> = registry
            .declared()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(
            names,
            [
                Registry::name("front", "fetch"),
                Registry::name("front", "search")
            ],
            "a direct server declares what is granted, and only that, in name order"
        );
        assert!(registry.loaded().is_empty(), "and nothing was searched for");

        // The server that was told to stay off does not even answer a search for everything.
        let declared = registry.search("");
        assert!(
            !declared.iter().any(|key| key.starts_with("mcp__off")),
            "a hidden server was declared: {declared:?}"
        );
    }

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
