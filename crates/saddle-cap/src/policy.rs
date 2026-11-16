//! The policy file (D22): grants only, every grant explained, everything resolved at load.

use serde::de::Error;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::time::Duration;
use toml::de::Error as DeError;
use toml::ser::Error as SerError;

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

use crate::roots::{RelPath, RootName, Roots, is_ident};
use crate::scope::Scope;

pub const POLICY_FILE: &str = "saddle.toml";
pub const POLICY_VERSION: u32 = 1;
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(id: &str) -> Option<Self> {
        is_ident(id).then(|| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PluginId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        PluginId::new(&s).ok_or_else(|| {
            Error::custom(format!("`{s}` is not a valid plugin id ([a-z][a-z0-9-]*)"))
        })
    }
}

/// Who is asking. The agent is a principal like any plugin (D17).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Principal {
    Agent,
    Plugin(PluginId),
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::Agent => f.write_str("agent"),
            Principal::Plugin(id) => write!(f, "plugin.{id}"),
        }
    }
}

/// The TOML path of a grant, such as `agent.fs-read[0]`. This is the rule recorded under D10.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RuleId(String);

impl RuleId {
    fn new(principal: &Principal, scope: Scope, index: Option<usize>) -> Self {
        match index {
            Some(i) => Self(format!("{principal}.{scope}[{i}]")),
            None => Self(format!("{principal}.{scope}")),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A rule inside another, such as `agent.mcp[0].fs-read[1]`: an audit record names the exact line
/// a person wrote, not just the grant it sits in.
impl RuleId {
    fn under(&self, part: &str, index: usize) -> Self {
        Self(format!("{}.{part}[{index}]", self.0))
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct FsGrant {
    pub rule: RuleId,
    pub why: String,
    pub root: RootName,
    pub exclude: Vec<String>,
    exclude_set: GlobSet,
}

impl FsGrant {
    /// keeps the compiled matcher in step with the written exclusions when delegating a grant.
    pub(crate) fn with_exclusions(&self, exclude: Vec<String>) -> Result<Self, String> {
        Ok(Self {
            exclude_set: exclude_set(&exclude)?,
            exclude,
            ..self.clone()
        })
    }

    /// Whether this grant's exclusions cover `rel`. Patterns without a `/` match at any depth, as
    /// in `.gitignore`, and a pattern that names a directory also covers everything beneath it.
    pub fn excludes(&self, rel: &RelPath) -> bool {
        !rel.is_root() && self.exclude_set.is_match(rel.as_slash())
    }
}

#[derive(Debug, Clone)]
pub struct ExecGrant {
    pub rule: RuleId,
    pub why: String,
    /// Environment variable names passed to the child on top of the base allowlist (D19).
    pub env: Vec<String>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    /// A hostname or an IP literal.
    Exact(String),
    /// `*.example.com`: any strict subdomain of `example.com`.
    Suffix(String),
    /// `*`: any host at all, which is the one grant whose `why` carries all the weight (D40).
    Any,
}

impl HostPattern {
    pub fn parse(s: &str) -> Result<Self, String> {
        let lower = s.to_ascii_lowercase();
        if lower == "*" {
            return Ok(HostPattern::Any);
        }
        if let Some(rest) = lower.strip_prefix("*.") {
            if valid_hostname(rest) {
                return Ok(HostPattern::Suffix(rest.to_owned()));
            }
        } else if lower.parse::<IpAddr>().is_ok() || valid_hostname(&lower) {
            return Ok(HostPattern::Exact(lower));
        }
        Err(format!(
            "`{s}` is not a hostname, `*.suffix`, `*` for any host, or an IP address"
        ))
    }

    pub fn matches(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        match self {
            HostPattern::Exact(h) => host == *h,
            HostPattern::Suffix(suffix) => host
                .strip_suffix(suffix.as_str())
                .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
            HostPattern::Any => true,
        }
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostPattern::Exact(h) => f.write_str(h),
            HostPattern::Suffix(s) => write!(f, "*.{s}"),
            HostPattern::Any => f.write_str("*"),
        }
    }
}

fn valid_hostname(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UrlScheme {
    Https,
    Http,
}

impl UrlScheme {
    pub fn as_str(self) -> &'static str {
        match self {
            UrlScheme::Https => "https",
            UrlScheme::Http => "http",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            UrlScheme::Https => 443,
            UrlScheme::Http => 80,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetGrant {
    pub rule: RuleId,
    pub why: String,
    pub host: HostPattern,
    pub scheme: UrlScheme,
    /// `None` allows only the scheme's default port.
    pub port: Option<u16>,
}

/// One list of net grants against one request. A server's reach is a narrowed list rather than a
/// principal the engine resolves, so the question is asked here and answered the same way (D110).
pub fn net_allows<'a>(
    grants: &'a [NetGrant],
    scheme: &str,
    host: &str,
    port: u16,
) -> Option<&'a NetGrant> {
    grants.iter().find(|g| g.allows(scheme, host, port))
}

/// Whether a list of grants spells the host out rather than covering it with a pattern. Only a
/// grant that names an address can reach loopback or private space (D40), and a server's narrowed
/// reach has to be asked the same question the agent's grants are (D110).
pub fn names_host(grants: &[NetGrant], host: &str) -> bool {
    grants
        .iter()
        .any(|g| matches!(&g.host, HostPattern::Exact(h) if h.eq_ignore_ascii_case(host)))
}

impl NetGrant {
    pub fn allows(&self, scheme: &str, host: &str, port: u16) -> bool {
        scheme == self.scheme.as_str()
            && self.host.matches(host)
            && port == self.port.unwrap_or(self.scheme.default_port())
    }
}

#[derive(Debug, Clone)]
pub struct BudgetGrant {
    pub rule: RuleId,
    pub why: String,
    pub tokens_per_session: u64,
    pub cost_per_session: Option<f64>,
}

/// One MCP server, and the tools of it the model may reach (D103). The server's own tool list is
/// never authority: it can only contain names written here.
#[derive(Debug, Clone)]
pub struct McpGrant {
    pub rule: RuleId,
    pub why: String,
    pub server: String,
    /// Every tool named, never a pattern: a server that grows a tool overnight does not grow it
    /// into the callable set.
    pub tools: Vec<String>,
    /// What the server asks to reach. Empty means it inherits the agent's own reach; anything
    /// here must be inside it, or the policy does not load (D105).
    pub reach: McpReach,
}

/// The files and hosts one MCP server asks for, before it is narrowed to what the agent holds
/// (D105).
#[derive(Debug, Clone, Default)]
pub struct McpReach {
    pub fs_read: Vec<FsGrant>,
    pub fs_write: Vec<FsGrant>,
    pub net: Vec<NetGrant>,
}

impl McpGrant {
    pub fn allows(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }
}

#[derive(Debug, Clone)]
pub struct PlainGrant {
    pub rule: RuleId,
    pub why: String,
}

impl PlainGrant {
    /// registration metadata compiled from an explicit protected host-tool selection (D147).
    pub fn registration(source: String, why: String) -> Self {
        Self {
            rule: RuleId(source),
            why,
        }
    }
}

/// read-only roles D66 can delegate through its SDK service (D138).
pub const TOOL_READ_ROLES: &[&str] = &["read", "ls", "find", "grep"];
pub const TOOL_WRITE_ROLES: &[&str] = &["write", "edit", "patch"];

/// delegates existing D66 tool-path reach, not new directories or policy roots (D138).
#[derive(Debug, Clone)]
pub struct ToolReadGrant {
    pub rule: RuleId,
    pub why: String,
    pub tools: Vec<String>,
}

impl ToolReadGrant {
    /// constructs the write ceiling from the same protected D66 declarations (D141).
    pub fn for_agent_write(tools: Vec<String>) -> Self {
        Self {
            rule: RuleId::new(&Principal::Agent, Scope::ToolWrite, None),
            why: "bounded by config.toml tool-path declarations".into(),
            tools,
        }
    }
    /// constructs the parent's ceiling from host-validated D66 configuration (D138).
    pub fn for_agent(tools: Vec<String>) -> Self {
        Self {
            rule: RuleId::new(&Principal::Agent, Scope::ToolRead, None),
            why: "bounded by config.toml tool-path declarations".into(),
            tools,
        }
    }
}

/// everything one principal holds.
#[derive(Debug, Clone, Default)]
pub struct Grants {
    pub tool_read: Option<ToolReadGrant>,
    pub tool_write: Option<ToolReadGrant>,
    pub fs_read: Vec<FsGrant>,
    pub fs_write: Vec<FsGrant>,
    pub exec: Option<ExecGrant>,
    pub net: Vec<NetGrant>,
    pub mcp: Vec<McpGrant>,
    pub model_invoke: Option<BudgetGrant>,
    /// Scopes whose grant carries no parameters: transcript-read, context-contribute, tool, veto,
    /// ui, persist.
    pub plain: BTreeMap<Scope, PlainGrant>,
}

impl Grants {
    /// Whether a grant names `host` exactly, which is what lets a private address be reached
    /// (D28): a wildcard grant is not a name, so it does not open loopback (D40).
    pub fn names_host(&self, host: &str) -> bool {
        names_host(&self.net, host)
    }

    pub fn scopes(&self) -> BTreeSet<Scope> {
        let mut out: BTreeSet<Scope> = self.plain.keys().copied().collect();
        if self.tool_read.as_ref().is_some_and(|g| !g.tools.is_empty()) {
            out.insert(Scope::ToolRead);
        }
        if self
            .tool_write
            .as_ref()
            .is_some_and(|g| !g.tools.is_empty())
        {
            out.insert(Scope::ToolWrite);
        }
        if !self.fs_read.is_empty() {
            out.insert(Scope::FsRead);
        }
        if !self.fs_write.is_empty() {
            out.insert(Scope::FsWrite);
        }
        if self.exec.is_some() {
            out.insert(Scope::Exec);
        }
        if !self.net.is_empty() {
            out.insert(Scope::Net);
        }
        if !self.mcp.is_empty() {
            out.insert(Scope::Mcp);
        }
        if self.model_invoke.is_some() {
            out.insert(Scope::ModelInvoke);
        }
        out
    }

    /// The grant for one server, which is what decides every call to it (D103).
    pub fn mcp_server(&self, server: &str) -> Option<&McpGrant> {
        self.mcp.iter().find(|g| g.server == server)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    pub principals: BTreeMap<Principal, Grants>,
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error(transparent)]
    Toml(#[from] DeError),
    #[error("could not combine the policies: {0}")]
    Combine(#[from] SerError),
    #[error("unsupported policy version {0}; this build reads version {POLICY_VERSION}")]
    Version(u32),
    #[error("{at}: {msg}")]
    Invalid { at: String, msg: String },
}

/// A TOML value as a document, which is what `Policy::parse` reads. `toml::to_string` on a `Value`
/// writes an inline `{...}`, which is not a document.
pub fn document(value: &toml::Value) -> Result<String, PolicyError> {
    let table = match value {
        toml::Value::Table(t) => t.clone(),
        other => {
            let mut t = toml::Table::new();
            t.insert("value".into(), other.clone());
            t
        }
    };
    Ok(toml::to_string(&toml::Value::Table(table))?)
}

/// `project` on top of `global`, as one TOML document (D43).
///
/// Tables merge, so a grant the project leaves alone keeps the global value. Grants are lists, and
/// a project's list replaces the global one for that scope — the whole point of overriding.
pub fn merge(global: toml::Value, project: toml::Value) -> toml::Value {
    match (global, project) {
        (toml::Value::Table(mut g), toml::Value::Table(p)) => {
            for (key, value) in p {
                let merged = match g.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                g.insert(key, merged);
            }
            toml::Value::Table(g)
        }
        // A project value replaces the global one unless the global is a table, above.
        (_, project) => project,
    }
}

fn invalid(at: impl fmt::Display, msg: impl Into<String>) -> PolicyError {
    PolicyError::Invalid {
        at: at.to_string(),
        msg: msg.into(),
    }
}

// Wire format. Unknown keys are errors everywhere.

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    version: u32,
    agent: Option<RawGrants>,
    #[serde(default)]
    plugin: BTreeMap<PluginId, RawGrants>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawGrants {
    tool_read: Option<RawToolRead>,
    tool_write: Option<RawToolRead>,
    #[serde(default)]
    fs_read: Vec<RawFs>,
    #[serde(default)]
    fs_write: Vec<RawFs>,
    exec: Option<RawExec>,
    #[serde(default)]
    net: Vec<RawNet>,
    #[serde(default)]
    mcp: Vec<RawMcp>,
    model_invoke: Option<RawBudget>,
    transcript_read: Option<RawPlain>,
    event_publish: Option<RawPlain>,
    event_subscribe: Option<RawPlain>,
    context_contribute: Option<RawPlain>,
    tool: Option<RawPlain>,
    veto: Option<RawPlain>,
    ui: Option<RawPlain>,
    persist: Option<RawPlain>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFs {
    root: String,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    why: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawExec {
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    env: Vec<String>,
    timeout_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNet {
    host: String,
    scheme: Option<UrlScheme>,
    port: Option<u16>,
    #[serde(default)]
    why: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "kebab-case")]
struct RawMcp {
    server: String,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    fs_read: Vec<RawFs>,
    #[serde(default)]
    fs_write: Vec<RawFs>,
    #[serde(default)]
    net: Vec<RawNet>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RawBudget {
    tokens_per_session: u64,
    cost_per_session: Option<f64>,
    #[serde(default)]
    why: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlain {
    #[serde(default)]
    why: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawToolRead {
    tools: Vec<String>,
    why: Option<String>,
}

impl Policy {
    /// Parses and validates a policy against the session's roots. Every root a grant names must
    /// exist in `roots`.
    /// The global policy with a workspace's own on top (D43). A grant the workspace file does not
    /// mention keeps the global value; one it does mention replaces it for that scope, whether that
    /// makes it narrower or wider. The workspace file is the authority for that directory: it lives
    /// in the config directory (D42), which nothing the agent can run is able to write, so it is a
    /// person's declaration and not something a project can smuggle in.
    pub fn layer(global: &str, project: &str, roots: &Roots) -> Result<Policy, PolicyError> {
        let global: toml::Value = toml::from_str(global)?;
        let project: toml::Value = toml::from_str(project)?;
        // Both sides must parse first, so a project cannot slip past the check by being invalid.
        // `toml::to_string` on a `Value` writes an inline `{...}`, which is not a document, so the
        // merged table is rendered as one first.
        let merged = merge(global, project);
        Policy::parse(&document(&merged)?, roots)
    }

    pub fn parse(text: &str, roots: &Roots) -> Result<Policy, PolicyError> {
        let raw: RawPolicy = toml::from_str(text)?;
        if raw.version != POLICY_VERSION {
            return Err(PolicyError::Version(raw.version));
        }
        let mut principals = BTreeMap::new();
        if let Some(agent) = raw.agent {
            principals.insert(Principal::Agent, lower(&Principal::Agent, agent, roots)?);
        }
        for (id, grants) in raw.plugin {
            let who = Principal::Plugin(id);
            let grants = lower(&who, grants, roots)?;
            principals.insert(who, grants);
        }
        Ok(Policy { principals })
    }

    pub fn grants(&self, who: &Principal) -> Option<&Grants> {
        self.principals.get(who)
    }

    /// One line per grant, stable across runs. `saddle check` prints them.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for g in self.principals.values() {
            for g in [&g.tool_read, &g.tool_write].into_iter().flatten() {
                lines.push(with_why(
                    format!("{} = tools [{}]", g.rule, g.tools.join(", ")),
                    &g.why,
                ));
            }
            for f in g.fs_read.iter().chain(&g.fs_write) {
                let exclude = if f.exclude.is_empty() {
                    String::new()
                } else {
                    format!(" exclude [{}]", f.exclude.join(", "))
                };
                lines.push(with_why(
                    format!("{} = {}{exclude}", f.rule, f.root),
                    &f.why,
                ));
            }
            if let Some(e) = &g.exec {
                let env = if e.env.is_empty() {
                    String::new()
                } else {
                    format!(" env [{}]", e.env.join(", "))
                };
                lines.push(with_why(
                    format!("{} = timeout {}s{env}", e.rule, e.timeout.as_secs()),
                    &e.why,
                ));
            }
            for n in &g.net {
                let port = n.port.unwrap_or(n.scheme.default_port());
                lines.push(with_why(
                    format!("{} = {}://{}:{port}", n.rule, n.scheme.as_str(), n.host),
                    &n.why,
                ));
            }
            for m in &g.mcp {
                let mut line = format!(
                    "{} = server {} tools [{}]",
                    m.rule,
                    m.server,
                    m.tools.join(", ")
                );
                if !m.reach.fs_read.is_empty() || !m.reach.fs_write.is_empty() {
                    let roots: Vec<&str> = m
                        .reach
                        .fs_read
                        .iter()
                        .chain(&m.reach.fs_write)
                        .map(|f| f.root.as_str())
                        .collect();
                    line.push_str(&format!(" fs [{}]", roots.join(", ")));
                }
                if !m.reach.net.is_empty() {
                    let hosts: Vec<String> =
                        m.reach.net.iter().map(|n| n.host.to_string()).collect();
                    line.push_str(&format!(" net [{}]", hosts.join(", ")));
                }
                lines.push(with_why(line, &m.why));
            }
            if let Some(b) = &g.model_invoke {
                lines.push(with_why(
                    format!("{} = {} tokens per session", b.rule, b.tokens_per_session),
                    &b.why,
                ));
            }
            for p in g.plain.values() {
                lines.push(with_why(p.rule.to_string(), &p.why));
            }
        }
        lines
    }
}

/// A grant's `why`, which a person reads in `saddle check` and in the audit log (D22). It is
/// optional (D41): an absent or blank one is an empty string, and everything that prints a grant
/// then prints no reason rather than an empty one.
fn why(why: Option<String>) -> String {
    why.unwrap_or_default().trim().to_owned()
}

/// A grant's line, with its reason only when there is one, so a grant without a `why` leaves no
/// dangling `# ` for a reviewer to wonder about (D41).
fn with_why(line: String, why: &str) -> String {
    if why.is_empty() {
        line
    } else {
        format!("{line}  # {why}")
    }
}

/// One filesystem entry of an MCP grant, checked against the roots exactly as an agent's is.
fn lower_fs(rule: RuleId, f: RawFs, roots: &Roots) -> Result<FsGrant, PolicyError> {
    let root = RootName::new(&f.root).map_err(|e| invalid(&rule, e.to_string()))?;
    if !roots.contains(&root) {
        let known: Vec<_> = roots.iter().map(|(n, _)| n.as_str()).collect();
        return Err(invalid(
            &rule,
            format!("unknown root `{root}` (known: {})", known.join(", ")),
        ));
    }
    let exclude_set = exclude_set(&f.exclude).map_err(|e| invalid(&rule, e))?;
    Ok(FsGrant {
        why: why(f.why),
        rule,
        root,
        exclude: f.exclude,
        exclude_set,
    })
}

fn lower(who: &Principal, raw: RawGrants, roots: &Roots) -> Result<Grants, PolicyError> {
    let fs = |scope: Scope, list: Vec<RawFs>| -> Result<Vec<FsGrant>, PolicyError> {
        list.into_iter()
            .enumerate()
            .map(|(i, f)| {
                let rule = RuleId::new(who, scope, Some(i));
                let root = RootName::new(&f.root).map_err(|e| invalid(&rule, e.to_string()))?;
                if !roots.contains(&root) {
                    let known: Vec<_> = roots.iter().map(|(n, _)| n.as_str()).collect();
                    return Err(invalid(
                        &rule,
                        format!("unknown root `{root}` (known: {})", known.join(", ")),
                    ));
                }
                let exclude_set = exclude_set(&f.exclude).map_err(|e| invalid(&rule, e))?;
                Ok(FsGrant {
                    why: why(f.why),
                    rule,
                    root,
                    exclude: f.exclude,
                    exclude_set,
                })
            })
            .collect()
    };

    let mut grants = Grants {
        fs_read: fs(Scope::FsRead, raw.fs_read)?,
        fs_write: fs(Scope::FsWrite, raw.fs_write)?,
        ..Grants::default()
    };

    for (input, output, scope, roles) in [
        (
            raw.tool_read,
            &mut grants.tool_read,
            Scope::ToolRead,
            TOOL_READ_ROLES,
        ),
        (
            raw.tool_write,
            &mut grants.tool_write,
            Scope::ToolWrite,
            TOOL_WRITE_ROLES,
        ),
    ] {
        let Some(g) = input else { continue };
        let rule = RuleId::new(who, scope, None);
        if let Some(tool) = g.tools.iter().find(|tool| !roles.contains(&tool.as_str())) {
            return Err(invalid(
                &rule,
                format!("unknown {scope} tool-path role `{tool}`"),
            ));
        }
        *output = Some(ToolReadGrant {
            rule,
            why: why(g.why),
            tools: g.tools,
        });
    }

    if let Some(e) = raw.exec {
        let rule = RuleId::new(who, Scope::Exec, None);
        for name in &e.env {
            if !valid_env_name(name) {
                return Err(invalid(
                    &rule,
                    format!("`{name}` is not an environment variable name"),
                ));
            }
        }
        let timeout = match e.timeout_secs {
            Some(0) => return Err(invalid(&rule, "timeout-secs must be positive")),
            Some(s) => Duration::from_secs(s),
            None => DEFAULT_EXEC_TIMEOUT,
        };
        grants.exec = Some(ExecGrant {
            why: why(e.why),
            rule,
            env: e.env,
            timeout,
        });
    }

    for (i, n) in raw.net.into_iter().enumerate() {
        let rule = RuleId::new(who, Scope::Net, Some(i));
        let host = HostPattern::parse(&n.host).map_err(|e| invalid(&rule, e))?;
        grants.net.push(NetGrant {
            why: why(n.why),
            rule,
            host,
            scheme: n.scheme.unwrap_or(UrlScheme::Https),
            port: n.port,
        });
    }

    if let Some(b) = raw.model_invoke {
        if b.cost_per_session
            .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
        {
            return Err(PolicyError::Invalid {
                at: "model-invoke.cost-per-session".into(),
                msg: "must be finite and nonnegative".into(),
            });
        }
        let rule = RuleId::new(who, Scope::ModelInvoke, None);
        grants.model_invoke = Some(BudgetGrant {
            why: why(b.why),
            rule,
            tokens_per_session: b.tokens_per_session,
            cost_per_session: b.cost_per_session,
        });
    }

    for (i, m) in raw.mcp.into_iter().enumerate() {
        let rule = RuleId::new(who, Scope::Mcp, Some(i));
        let server = m.server.trim();
        if server.is_empty() {
            return Err(invalid(&rule, "an mcp grant must name a server"));
        }
        if m.tools.is_empty() {
            return Err(invalid(
                &rule,
                format!("`{server}` grants no tools, so nothing could be called through it"),
            ));
        }
        let mut tools: Vec<String> = Vec::new();
        for tool in &m.tools {
            let tool = tool.trim();
            if tool.is_empty() || tool == "*" {
                return Err(invalid(
                    &rule,
                    format!("`{server}` must name each tool, never a pattern"),
                ));
            }
            if !tools.iter().any(|t| t == tool) {
                tools.push(tool.to_owned());
            }
        }
        let mut reach = McpReach::default();
        for (j, f) in m.fs_read.into_iter().enumerate() {
            reach
                .fs_read
                .push(lower_fs(rule.under("fs-read", j), f, roots)?);
        }
        for (j, f) in m.fs_write.into_iter().enumerate() {
            reach
                .fs_write
                .push(lower_fs(rule.under("fs-write", j), f, roots)?);
        }
        for (j, n) in m.net.into_iter().enumerate() {
            let rule = rule.under("net", j);
            let host = HostPattern::parse(&n.host).map_err(|e| invalid(&rule, e))?;
            reach.net.push(NetGrant {
                why: why(n.why),
                rule,
                host,
                scheme: n.scheme.unwrap_or(UrlScheme::Https),
                port: n.port,
            });
        }
        grants.mcp.push(McpGrant {
            why: why(m.why),
            rule,
            server: server.to_owned(),
            tools,
            reach,
        });
    }

    let plain = [
        (Scope::TranscriptRead, raw.transcript_read),
        (Scope::EventPublish, raw.event_publish),
        (Scope::EventSubscribe, raw.event_subscribe),
        (Scope::ContextContribute, raw.context_contribute),
        (Scope::Tool, raw.tool),
        (Scope::Veto, raw.veto),
        (Scope::Ui, raw.ui),
        (Scope::Persist, raw.persist),
    ];
    for (scope, raw) in plain {
        if let Some(p) = raw {
            let rule = RuleId::new(who, scope, None);
            grants.plain.insert(
                scope,
                PlainGrant {
                    why: why(p.why),
                    rule,
                },
            );
        }
    }
    Ok(grants)
}

fn valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Compiles exclusion patterns with `.gitignore`-like anchoring: a pattern without `/` matches at
/// any depth, a leading `/` anchors to the root, and every pattern also covers what lies beneath.
pub fn exclude_set(patterns: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let anchored = match pattern.strip_prefix('/') {
            Some(rest) => rest.to_owned(),
            None if pattern.contains('/') => pattern.clone(),
            None => format!("**/{pattern}"),
        };
        let anchored = anchored.trim_end_matches('/');
        for p in [anchored.to_owned(), format!("{anchored}/**")] {
            builder.add(glob(&p).map_err(|e| format!("exclude `{pattern}`: {e}"))?);
        }
    }
    builder.build().map_err(|e| e.to_string())
}

fn glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern).literal_separator(true).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> (tempfile::TempDir, Roots) {
        let dir = tempfile::tempdir().unwrap();
        let roots =
            Roots::resolve([(RootName::workspace(), dir.path().to_path_buf())], &[]).unwrap();
        (dir, roots)
    }

    const EXAMPLE: &str = r#"
version = 1

[agent]
fs-read  = [{ root = "workspace", why = "reads the repo" }]
fs-write = [{ root = "workspace", exclude = [".env*", "/target"], why = "edits the repo" }]
exec     = { why = "runs builds and tests", env = ["CARGO_TERM_COLOR"] }

[plugin.web-search]
net          = [{ host = "api.search.example", why = "search backend" }]
model-invoke = { tokens-per-session = 200000, why = "summarises results" }
"#;

    #[test]
    fn a_wildcard_grant_allows_any_host_but_names_none() {
        assert_eq!(HostPattern::parse("*").unwrap(), HostPattern::Any);
        let any = HostPattern::Any;
        for host in ["api.example.com", "evil.test", "127.0.0.1", "2001:db8::1"] {
            assert!(any.matches(host), "{host}");
        }
        assert_eq!(any.to_string(), "*");

        // A suffix is still a suffix, and a wildcard is not a name.
        let policy = Policy::parse(
            &format!("{EXAMPLE}\n[plugin.wild]\nnet = [{{ host = \"*\", why = \"anywhere\" }}]\n"),
            &roots().1,
        )
        .unwrap();
        let who = Principal::Plugin(PluginId::new("wild").unwrap());
        let wild = policy.grants(&who).unwrap();
        assert!(wild.net[0].allows("https", "anything.test", 443));
        assert!(
            !wild.net[0].allows("http", "anything.test", 80),
            "https only"
        );
        assert!(
            !wild.names_host("127.0.0.1"),
            "a wildcard does not open a private address (D40)"
        );

        let agent = policy.grants(&Principal::Agent).unwrap();
        assert!(agent.net.is_empty(), "the agent still has no net grant");
    }

    #[test]
    fn a_grant_that_spells_out_an_address_names_it() {
        let (dir, roots) = roots();
        let _ = dir;
        let text =
            "version = 1\n\n[agent]\nnet = [{ host = \"127.0.0.1\", why = \"a local server\" }]\n";
        let policy = Policy::parse(text, &roots).unwrap();
        let agent = policy.grants(&Principal::Agent).unwrap();
        assert!(agent.names_host("127.0.0.1"));
        assert!(!agent.names_host("127.0.0.2"));
    }

    #[test]
    fn parses_the_plan_example() {
        let (_d, roots) = roots();
        let policy = Policy::parse(EXAMPLE, &roots).unwrap();
        let agent = policy.grants(&Principal::Agent).unwrap();
        assert_eq!(
            agent.scopes().into_iter().collect::<Vec<_>>(),
            [Scope::Exec, Scope::FsRead, Scope::FsWrite]
        );
        assert_eq!(agent.fs_write[0].rule.as_str(), "agent.fs-write[0]");
        let web = Principal::Plugin(PluginId::new("web-search").unwrap());
        let web = policy.grants(&web).unwrap();
        assert_eq!(web.net[0].rule.as_str(), "plugin.web-search.net[0]");
        assert!(web.net[0].allows("https", "api.search.example", 443));
        assert!(!web.net[0].allows("http", "api.search.example", 80));
        assert!(!web.net[0].allows("https", "evil.example", 443));
    }

    #[test]
    fn exclusions_follow_gitignore_anchoring() {
        let (_d, roots) = roots();
        let policy = Policy::parse(EXAMPLE, &roots).unwrap();
        let g = &policy.grants(&Principal::Agent).unwrap().fs_write[0];
        let ex = |p: &str| g.excludes(&RelPath::new(p).unwrap());
        assert!(ex(".env"));
        assert!(ex("config/.env.local"));
        assert!(ex("target"));
        assert!(ex("target/debug/saddle"));
        assert!(!ex("crates/x/target"));
        assert!(!ex("src/main.rs"));
    }

    #[test]
    fn a_project_policy_is_layered_on_the_global_one() {
        let (_d, roots) = roots();
        let global = r#"
version = 1
[agent]
fs-read  = [{ root = "workspace", why = "reads the project" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the project" }]
exec     = { why = "runs tests" }
"#;
        // The project says nothing: it inherits everything.
        let same = Policy::layer(global, "version = 1\n", &roots).unwrap();
        let base = Policy::parse(global, &roots).unwrap();
        assert_eq!(same.describe(), base.describe());

        // The project narrows one scope and leaves the rest alone.
        let narrower = "version = 1\n[agent]\nfs-write = [{ root = \"workspace\", exclude = [\".env*\", \"/secrets\"] }]\n";
        let layered = Policy::layer(global, narrower, &roots).unwrap();
        let agent = layered.grants(&Principal::Agent).unwrap();
        assert!(agent.exec.is_some(), "the untouched grant is still there");
        assert_eq!(agent.fs_write[0].exclude, [".env*", "/secrets"]);
        assert_eq!(
            agent.fs_read[0].exclude,
            Vec::<String>::new(),
            "also untouched"
        );
    }

    #[test]
    fn a_workspace_policy_may_widen_because_it_is_the_authority_for_its_directory() {
        // D43: the workspace file is written by a person in the config directory, which nothing
        // the agent runs can write, so it may grant more as well as less.
        let (_d, roots) = roots();
        let global = "version = 1\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n";
        let wider = r#"
version = 1
[agent]
fs-read = [{ root = "workspace" }]
fs-write = [{ root = "workspace" }]
net = [{ host = "api.example.com" }]
exec = { timeout-secs = 60 }
"#;
        let layered = Policy::layer(global, wider, &roots).unwrap();
        let agent = layered.grants(&Principal::Agent).unwrap();
        assert!(
            agent.exec.is_some(),
            "a scope the global policy never granted"
        );
        assert_eq!(agent.net[0].host.to_string(), "api.example.com");
        assert_eq!(agent.fs_write[0].root.as_str(), "workspace");

        // A budget above the global ceiling is just a budget the person chose.
        let with_budget = "version = 1\n\n[agent]\nmodel-invoke = { tokens-per-session = 100 }\n";
        let raised = "version = 1\n\n[agent]\nmodel-invoke = { tokens-per-session = 500 }\n";
        let layered = Policy::layer(with_budget, raised, &roots).unwrap();
        assert_eq!(
            layered
                .grants(&Principal::Agent)
                .unwrap()
                .model_invoke
                .as_ref()
                .unwrap()
                .tokens_per_session,
            500
        );

        // What is still refused is a grant that makes no sense for the session: an unknown root,
        // exactly as in a single-file policy.
        let e = Policy::layer(
            global,
            "version = 1\n[agent]\nfs-read = [{ root = \"nope\" }]\n",
            &roots,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("unknown root `nope`"), "{e}");
    }

    #[test]
    fn a_grant_without_a_why_is_accepted_and_prints_without_one() {
        // D41: the reason is optional, so a policy can be written without one; nothing prints a
        // dangling `#` for a grant that has none.
        let (_d, roots) = roots();
        let text = "version = 1\n\n[agent]\nnet = [{ host = \"*\" }]\nexec = { why = \"  \" }\n";
        let policy = Policy::parse(text, &roots).unwrap();
        let agent = policy.grants(&Principal::Agent).unwrap();
        assert_eq!(agent.net[0].why, "");
        assert_eq!(agent.exec.as_ref().unwrap().why, "");
        let lines = policy.describe();
        assert!(
            lines.iter().any(|l| l == "agent.net[0] = https://*:443"),
            "{lines:?}"
        );
        assert!(lines.iter().all(|l| !l.contains('#')), "{lines:?}");

        // One that does say why still shows it, and an allow is explained by the rule alone when
        // there is nothing to quote.
        let text = "version = 1\n\n[agent]\nnet = [{ host = \"a.test\", why = \"a reason\" }]\n";
        let policy = Policy::parse(text, &roots).unwrap();
        assert_eq!(
            policy.grants(&Principal::Agent).unwrap().net[0].why,
            "a reason"
        );
        assert!(
            policy.describe().iter().any(|l| l.ends_with("# a reason")),
            "{:?}",
            policy.describe()
        );
    }

    #[test]
    fn rejects_unknown_keys_and_roots() {
        let (_d, roots) = roots();
        let err = |text: &str| Policy::parse(text, &roots).unwrap_err().to_string();

        assert!(err("version = 1\n[agent]\nintercept = { why = \"x\" }").contains("intercept"));
        assert!(
            err("version = 1\n[agent]\nfs-read = [{ root = \"home\", why = \"x\" }]")
                .contains("unknown root `home`")
        );
        assert!(err("version = 2").contains("version 2"));
        assert!(err("version = 1\n[plugin.Bad]\n").contains("plugin id"));
    }

    #[test]
    fn host_patterns() {
        let p = HostPattern::parse("*.github.com").unwrap();
        assert!(p.matches("api.github.com"));
        assert!(!p.matches("github.com"));
        assert!(!p.matches("evilgithub.com"));
        assert!(HostPattern::parse("127.0.0.1").is_ok());
        assert!(HostPattern::parse("not a host").is_err());
        assert!(HostPattern::parse("*.").is_err());
    }

    #[test]
    fn an_mcp_grant_names_a_server_and_each_of_its_tools() {
        // D103: the grant is what makes a server and its tools reachable, and both the prompt and
        // `saddle check` print it because both read these lines.
        let (_d, roots) = roots();
        let policy = Policy::parse(
            r#"
version = 1

[agent]
mcp = [{ server = "job-scout", tools = ["search", "fetch-posting", "search"], why = "scrapes boards" }]
"#,
            &roots,
        )
        .unwrap();
        let agent = policy.grants(&Principal::Agent).unwrap();
        assert!(agent.scopes().contains(&Scope::Mcp));
        let grant = agent.mcp_server("job-scout").unwrap();
        assert!(grant.allows("search"));
        assert!(grant.allows("fetch-posting"));
        assert!(!grant.allows("delete-everything"));
        assert_eq!(
            grant.tools,
            ["search", "fetch-posting"],
            "a repeat is one tool"
        );
        assert!(agent.mcp_server("other").is_none());
        assert_eq!(
            policy.describe(),
            ["agent.mcp[0] = server job-scout tools [search, fetch-posting]  # scrapes boards"]
        );
    }

    #[test]
    fn an_mcp_grant_must_name_its_tools_rather_than_a_pattern() {
        // D103: no wildcard, so a tool the server grows overnight is not callable on its own.
        let (_d, roots) = roots();
        for body in [
            r#"mcp = [{ server = "x" }]"#,
            r#"mcp = [{ server = "x", tools = ["*"] }]"#,
            r#"mcp = [{ server = "", tools = ["a"] }]"#,
        ] {
            let text = format!("version = 1\n\n[agent]\n{body}\n");
            let err = Policy::parse(&text, &roots).unwrap_err().to_string();
            assert!(err.contains("agent.mcp[0]"), "{err}");
        }
    }
}
