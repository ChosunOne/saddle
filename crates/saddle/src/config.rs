//! User-level config (`$XDG_CONFIG_HOME/saddle/config.toml`). Extra roots and model providers live
//! here, never in the repository policy (D16).
//!
//! ```toml
//! # The provider `saddle run` uses without --provider. Built in: openai, synthetic; any other
//! # goes under [providers.<name>]. A table named after a built-in overrides that preset's fields.
//! provider = "local"
//!
//! [roots]
//! rustup = "~/.rustup"
//! cargo = "~/.cargo"
//!
//! # Roots every sandboxed command may use on top of the policy's grants (D19).
//! [sandbox]
//! read = ["rustup", "cargo"]
//! write = []
//!
//! [providers.local]
//! api = "openai"                       # or "anthropic"
//! base-url = "http://127.0.0.1:8080/v1"
//! model = "qwen3-coder"
//! # api-key-env = "LOCAL_KEY"          # omit when the server takes no key
//! ```

use crate::defaults;
use crate::mcp;
use crate::paths;
use crate::plugins::Entry as PluginEntry;
use crate::tools;
use crate::tui::theme::ThemeTables;
use saddle_cap::sandbox;
use saddle_cap::sandbox::Include;
use saddle_limits::Budgets;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::iter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Error as AnyError, Result, bail};
use saddle_cap::sandbox::SandboxExtras;
use saddle_cap::{PluginId, RootName, Roots};
use serde::Deserialize;

use crate::mcp::McpTable;
use crate::provider::ProviderEntry;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    #[serde(default, rename = "tool-limits")]
    pub tool_limits: Budgets,
    /// explicit plugin grants, separate from installed/enabled artifact configuration (D128).
    #[serde(default)]
    pub plugin: BTreeMap<PluginId, toml::Value>,
    #[serde(default)]
    pub plugins: BTreeMap<PluginId, PluginEntry>,
    /// The grants every workspace inherits (D63). This was `policy.toml`; it is the same TOML,
    /// under `[agent]` here.
    #[serde(default)]
    pub agent: Option<toml::Value>,
    /// Accepted and ignored: one version briefly kept workspace grants here as a table, and a
    /// config written then is split back into `policies/<key>.toml` by `migrate` (D63). Read by
    /// `migrate` through the raw document rather than through this field, which exists only so a
    /// file containing the table still parses.
    #[serde(default)]
    #[allow(dead_code)]
    pub workspaces: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub roots: BTreeMap<String, String>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// The provider used when `--provider` is not given.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model requests per prompt before the loop stops and waits. Unset means no limit (D37).
    #[serde(default, rename = "max-steps")]
    pub max_steps: Option<usize>,
    /// Whether a workspace that holds credentials may be entered without an allowance (D60). For a
    /// machine where the home directory has no secrets; off unless a person turns it on, and this
    /// file is outside every root, so nothing the agent runs can set it (D42).
    #[serde(default, rename = "allow-sensitive")]
    pub allow_sensitive: bool,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Named colour roles, `[themes.<name>]`, and `[ui] theme` selects one (D71).
    #[serde(default)]
    pub themes: ThemeTables,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    /// The MCP servers a person declared, `[mcp.<name>]` (D104). A grant (D103) still decides
    /// whether any of them runs.
    #[serde(default)]
    pub mcp: McpTable,
}

/// How the TUI starts; ctrl+t and ctrl+o change these for the session only.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct UiConfig {
    /// The theme to draw with: a built-in name, or one this file defines under `[themes.<name>]`
    /// (D71). Unset means `default`, which is what the interface looked like before themes.
    pub theme: Option<String>,
    pub show_reasoning: Option<bool>,
    pub expand_tools: Option<bool>,
    /// The status line, as a template of `{field}` placeholders (D36). Empty uses the default.
    pub status: Option<String>,
    /// Handle the mouse in saddle: wheel scrolling, and dragging to select text with the highlight
    /// saddle draws and the copy it sends (D35). On by default; `false` hands the mouse back to the
    /// terminal, which brings back its own selection and loses the wheel.
    pub mouse: Option<bool>,
    /// How an attached picture is drawn: `kitty`, `iterm2`, `none`, or unset to work it out from
    /// the environment (D59).
    pub images: Option<String>,
}

/// Instruction files (D32).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContextConfig {
    /// The global AGENTS.md; empty turns it off. Defaults to `$XDG_CONFIG_HOME/saddle/AGENTS.md`.
    pub global: Option<String>,
    /// Read CLAUDE.md where no AGENTS.md exists (and `~/.claude/CLAUDE.md` when there is no
    /// global AGENTS.md). Defaults to on.
    pub claude_fallback: Option<bool>,
}

/// Skills (D64). The locations are fixed and portable; this only says whether to look and how many
/// to advertise.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SkillsConfig {
    /// Whether skills are discovered at all. Defaults to on.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// How many skills are advertised in the prompt. Defaults to 32.
    #[serde(default = "skill_limit")]
    pub max: usize,
}

fn skill_limit() -> usize {
    32
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max: skill_limit(),
        }
    }
}

/// Compaction (D33).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CompactionConfig {
    /// Compact when the context nears the window, and once after a context-overflow error.
    pub auto: Option<bool>,
    /// Tokens kept free below the context window; compaction starts past this point.
    pub reserve_tokens: Option<u64>,
    /// Recent conversation kept word for word beside the summary.
    pub keep_recent_tokens: Option<u64>,
    /// Compact a session that has been left alone once it is half full, so coming back to it does
    /// not spend the whole window on a prompt typed before a break (D77).
    pub idle: Option<bool>,
    /// The share of the window at which an idle session is compacted, 0 to 1.
    pub idle_fraction: Option<f64>,
    /// How long a session is left alone before that happens.
    pub idle_seconds: Option<u64>,
    /// Summarize in the background while the turn carries on, rather than stopping it (D78).
    pub background: Option<bool>,
    /// The share of the window at which that starts, 0 to 1.
    pub background_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub read: Vec<SandboxEntry>,
    #[serde(default)]
    pub write: Vec<SandboxEntry>,
    /// Directories worked out by walking a base and excluding what matches, so a command gets what
    /// it needs without saddle knowing about each tool (D62).
    #[serde(default)]
    pub include: Vec<IncludeConfig>,
    /// Whether the rule below is applied. On by default, so an unconfigured machine works: the
    /// rules in this file are added to it rather than replacing it, which is what "add a
    /// directory to exclude" means. Set false to have only the rules written here (D62).
    #[serde(default = "yes", rename = "include-defaults")]
    pub include_defaults: bool,
    /// Whether the built-in tool path is added: `~/.agents`, readable by the tools that only read
    /// (D66). On by default, because the shared skills directory is the case the property exists
    /// for; set false to keep the tools to what is written in this file.
    #[serde(default = "yes", rename = "tools-defaults")]
    pub tools_defaults: bool,
}

/// One `[sandbox]` entry: a directory a sandboxed command may reach (D62), and, if it names tools,
/// a directory the file tools may reach too (D66).
///
/// A bare path is what every entry was before tool access existed: indirect reach only, through a
/// command. A table adds `tools`, the list of file tools that may name the directory directly.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SandboxEntry {
    Path(String),
    WithTools {
        path: String,
        /// Tool names, from `tools::NAMES`. Empty means indirect reach only, which is the default
        /// for an entry that does not say otherwise (D66).
        #[serde(default)]
        tools: Vec<String>,
    },
}

impl SandboxEntry {
    pub fn path(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::WithTools { path, .. } => path,
        }
    }

    /// The tools the file tools may use on this directory; empty for a bare path.
    pub fn tools(&self) -> &[String] {
        match self {
            Self::Path(_) => &[],
            Self::WithTools { tools, .. } => tools,
        }
    }
}

impl Default for SandboxConfig {
    /// The same values serde would use for a table that omits them, because `UserConfig.sandbox`
    /// is `#[serde(default)]`: without this a config with no `[sandbox]` table at all silently lost
    /// the built-in include rule (D62) and the built-in tool path (D66). Both are on by default, so
    /// "absent" must not mean "off".
    fn default() -> Self {
        Self {
            read: Vec::new(),
            write: Vec::new(),
            include: Vec::new(),
            include_defaults: true,
            tools_defaults: true,
        }
    }
}

/// The tools the built-in `~/.agents` path allows: the ones that only read. A skill is
/// instructions, and nothing needs to write the directory they are installed in (D66).
pub const DEFAULT_TOOL_TOOLS: &[&str] = &["read", "ls", "find", "grep"];

/// A directory the file tools may reach outside every root, with the tools allowed on it (D66).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPath {
    pub dir: PathBuf,
    /// As written in `config.toml`, for `check` and the prompt.
    pub written: String,
    pub tools: Vec<String>,
}

/// A policy document from an `[agent]` table: the `version` line the parser requires, and the
/// grants as written. `None` is an empty `[agent]`, which inherits everything (D63).
pub fn policy_document_pub(agent: Option<toml::Value>) -> toml::Value {
    policy_document(agent)
}

fn policy_document(agent: Option<toml::Value>) -> toml::Value {
    let mut doc = toml::Table::new();
    doc.insert("version".into(), toml::Value::Integer(1));
    let agent = match agent {
        Some(toml::Value::Table(t)) => toml::Value::Table(t),
        Some(other) => other,
        None => toml::Value::Table(toml::Table::new()),
    };
    doc.insert("agent".into(), agent);
    toml::Value::Table(doc)
}

fn yes() -> bool {
    true
}

/// The credential stores every machine is assumed to have (D62). A pattern that never matches is
/// harmless, so this errs towards naming stores a particular machine may not have.
pub const DEFAULT_EXCEPTS: &[&str] = &[
    // keys and secret managers
    r"^\.ssh$",
    r"^\.gnupg$",
    r"^\.pki$",
    r"^\.password-store$",
    r"^\.local/share/keyrings$",
    r"^\.local/share/kwalletd$",
    r"^\.config/op$",
    r"^\.vault-token$",
    // cloud and containers: a provider token, or a kubeconfig that carries one
    r"^\.aws$",
    r"^\.azure$",
    r"^\.config/gcloud$",
    r"^\.kube$",
    r"^\.docker$",
    r"^\.config/containers$",
    r"^\.terraform\.d$",
    r"^\.oci$",
    r"^\.s3cfg$",
    r"^\.boto$",
    r"^\.config/rclone$",
    r"^\.config/doctl$",
    r"^\.config/hcloud$",
    // forges and git credential helpers
    r"^\.config/gh$",
    r"^\.config/glab-cli$",
    r"^\.config/hub$",
    r"^\.config/tea$",
    r"^\.netrc$",
    r"^\.git-credentials$",
    r"^\.config/git/credentials$",
    // package registries: a token for publishing, or for a private index
    r"^\.npmrc$",
    r"^\.yarnrc$",
    r"^\.yarnrc\.yml$",
    r"^\.pypirc$",
    r"^\.config/pip$",
    r"^\.pip$",
    r"^\.gem/credentials$",
    r"^\.nuget$",
    r"^\.m2/settings\.xml$",
    r"^\.gradle/gradle\.properties$",
    r"^\.composer$",
    r"^\.config/composer$",
    r"^\.hex$",
    r"^\.cargo/credentials$",
    r"^\.cargo/credentials\.toml$",
    // databases
    r"^\.pgpass$",
    r"^\.my\.cnf$",
    r"^\.mylogin\.cnf$",
    // mail
    r"^\.msmtprc$",
    r"^\.offlineimaprc$",
    r"^\.fetchmailrc$",
    r"^\.authinfo$",
    // shell and repl history: a secret pasted on a command line is kept here
    r"^\.bash_history$",
    r"^\.zsh_history$",
    r"^\.python_history$",
    r"^\.node_repl_history$",
    r"^\.psql_history$",
    r"^\.mysql_history$",
    // browser and mail profiles: a session cookie is a credential
    r"^\.mozilla$",
    r"^\.thunderbird$",
    r"^\.config/google-chrome$",
    r"^\.config/chromium$",
];

/// The rule applied when `include-defaults` is on: walk `~` three levels, withhold the usual
/// credential stores (D62). Not written into `config.toml`, because a copy in the file would be a
/// second source of truth that drifts from this one.
pub fn default_include() -> IncludeConfig {
    IncludeConfig {
        base: "~".to_owned(),
        depth: default_depth(),
        except: DEFAULT_EXCEPTS.iter().map(|p| (*p).to_owned()).collect(),
    }
}

/// One `[[sandbox.include]]` rule, before it is resolved.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct IncludeConfig {
    /// The directory to walk: a root name, or a path, `~` allowed.
    pub base: String,
    /// How deep to look for exclusions. A landlock rule is recursive, so this bounds the search,
    /// not what is granted (D62).
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Regular expressions matched against each directory's path relative to the base. A match
    /// excludes that directory and everything under it.
    #[serde(default)]
    pub except: Vec<String>,
}

/// How far below the base to enumerate. Three, because the credential stores that matter are not
/// all at the top: `.local/share/keyrings` and `.config/git/credentials` are three levels down, and
/// a walk that stops short of them leaves the pattern matching nothing (D62).
fn default_depth() -> usize {
    3
}

/// Applies `change` to config.toml, keeping its comments and layout, and writes the result only if
/// it still parses as a valid config.
pub fn edit(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{}", path.display()))?;
    change(&mut doc)?;
    let out = doc.to_string();
    toml::from_str::<UserConfig>(&out)
        .with_context(|| format!("the change would leave {} invalid", path.display()))?;
    let dir = path.parent().context("config path has no parent")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(".config.toml.tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// A top-level `[name]` table, created if missing.
pub fn table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    name: &str,
) -> Result<&'a mut toml_edit::Table> {
    doc.entry(name)
        .or_insert(toml_edit::table())
        .as_table_mut()
        .with_context(|| format!("`{name}` in config.toml must be a table"))
}

/// The `[providers.<name>]` table, created if missing.
pub fn provider_table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    name: &str,
) -> Result<&'a mut toml_edit::Table> {
    let providers = doc
        .entry("providers")
        .or_insert(toml_edit::table())
        .as_table_mut()
        .context("`providers` in config.toml must be a table of [providers.<name>] tables")?;
    providers.set_implicit(true);
    providers
        .entry(name)
        .or_insert(toml_edit::table())
        .as_table_mut()
        .with_context(|| format!("`providers.{name}` in config.toml must be a table"))
}

impl UserConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("{}", path.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
        };
        config.tool_limits.validate().map_err(AnyError::msg)?;
        mcp::validate(&config.mcp).with_context(|| format!("{}", path.display()))?;
        if config.plugins.len() > config.tool_limits.plugin_count {
            bail!("configured plugin count limit exceeded");
        }
        Ok(config)
    }

    pub fn root_entries(&self) -> Result<Vec<(RootName, PathBuf)>> {
        let home = env::var_os("HOME").map(PathBuf::from);
        self.roots
            .iter()
            .map(|(name, path)| {
                let name = RootName::new(name)?;
                if name.is_builtin() {
                    bail!("config.toml: `{name}` is a built-in root and cannot be redefined");
                }
                let path = match (path.strip_prefix("~/"), &home) {
                    (Some(rest), Some(home)) => home.join(rest),
                    (Some(_), None) => {
                        bail!("config.toml: root `{name}` uses ~ but HOME is not set")
                    }
                    (None, _) => PathBuf::from(path),
                };
                Ok((name, path))
            })
            .collect()
    }

    /// The grant document every workspace inherits, as the policy parser reads it (D63).
    pub fn global_policy(&self) -> toml::Value {
        // No `[agent]` at all is "not configured yet", so the built-in grants apply. An `[agent]`
        // table that is present and empty is a decision, and is taken as written (D63).
        let agent = match self.agent.clone() {
            Some(agent) => Some(agent),
            None => toml::from_str::<toml::Value>(defaults::DEFAULT_AGENT)
                .ok()
                .map(|v| toml::Value::Table(v.as_table().cloned().unwrap_or_default())),
        };
        let mut document = policy_document(agent);
        if let toml::Value::Table(table) = &mut document {
            table.insert(
                "plugin".into(),
                toml::Value::Table(
                    self.plugin
                        .iter()
                        .map(|(id, grants)| (id.to_string(), grants.clone()))
                        .collect(),
                ),
            );
        }
        document
    }

    /// The extras every child gets: named roots, plus the directories the include rules work out
    /// (D62). Enumeration happens here because the protected directories are known here, and the
    /// result is a plain list of paths, so the lowering itself stays a mapping.
    pub fn sandbox_extras(
        &self,
        roots: &Roots,
        protected: &[PathBuf],
    ) -> Result<(SandboxExtras, Vec<ToolPath>)> {
        let home = env::var_os("HOME").map(PathBuf::from);
        let expand = |path: &str| -> Result<PathBuf> {
            // `~` is home itself, and `~/x` is a path in it; anything else is taken as written.
            if path == "~" {
                return match &home {
                    Some(home) => Ok(home.clone()),
                    None => bail!("config.toml: `~` needs HOME to be set"),
                };
            }
            match (path.strip_prefix("~/"), &home) {
                (Some(rest), Some(home)) => Ok(home.join(rest)),
                (Some(_), None) => bail!("config.toml: `{path}` uses ~ but HOME is not set"),
                (None, _) => Ok(PathBuf::from(path)),
            }
        };
        // An entry is a root name, or a path. A path is what a toolchain directory should be:
        // making one a root makes it nameable by every policy in the tree, and a workspace policy
        // could then grant `fs-write` on `~/.cargo` and plant a program in `~/.cargo/bin` that runs
        // outside the sandbox later (D62).
        let resolve = |n: &str| -> Result<PathBuf> {
            match RootName::new(n) {
                Ok(name) if roots.contains(&name) => roots
                    .get(&name)
                    .map(Path::to_path_buf)
                    .ok_or_else(|| anyhow::anyhow!("config.toml: unknown root `{name}`")),
                _ => {
                    let path = expand(n)?;
                    if !path.is_absolute() {
                        bail!(
                            "config.toml: [sandbox] entry `{n}` is neither a root nor an \
                             absolute path (use ~/... for a path in your home directory)"
                        );
                    }
                    Ok(path)
                }
            }
        };
        // An entry may not name, contain, or sit inside the state or config directory: those hold
        // the policy, the audit log and the stored keys, and D16 says no root contains them. A
        // sandbox entry is the same claim by another route, and it used to be unchecked, so
        // `read = ["~/.config/saddle"]` handed a command the policy file. A rule over `~` prunes
        // these instead, because it is a pattern rather than a claim.
        let check_protected = |path: &Path, written: &str| -> Result<()> {
            for protected in protected {
                if path.starts_with(protected) || protected.starts_with(path) {
                    bail!(
                        "config.toml: [sandbox] entry `{written}` names, contains or sits inside \
                         {}, which holds the policy, the audit log and the stored keys; no \
                         sandbox entry may reach it (D16). A default include rule over ~ leaves \
                         it out on its own.",
                        protected.display()
                    );
                }
            }
            Ok(())
        };
        let names = |list: &[SandboxEntry]| -> Result<Vec<PathBuf>> {
            list.iter()
                .map(|entry| {
                    let path = resolve(entry.path())?;
                    check_protected(&path, entry.path())?;
                    Ok(path)
                })
                .collect()
        };
        // Entries that name tools are also readable by the file tools (D66). An unknown name is an
        // error rather than a silent no-op: the list is the authority for these directories, and
        // there is no kernel check behind a host-side tool, so a typo would otherwise read as a
        // permission that silently does nothing.
        let mut tool_paths: Vec<ToolPath> = Vec::new();
        if self.sandbox.tools_defaults
            && let Some(home) = &home
        {
            // The shared skills directory is the case this property exists for, so it is a default
            // rather than something each machine writes by hand (D66). Read-only: a skill is
            // instructions, and nothing needs to write this one.
            let dir = home.join(".agents");
            if dir.is_dir() {
                tool_paths.push(ToolPath {
                    dir,
                    written: "~/.agents".to_owned(),
                    tools: DEFAULT_TOOL_TOOLS.iter().map(|t| (*t).to_owned()).collect(),
                });
            }
        }
        for entry in self.sandbox.read.iter().chain(self.sandbox.write.iter()) {
            let tools = entry.tools();
            if tools.is_empty() {
                continue;
            }
            for tool in tools {
                if !tools::NAMES.contains(&tool.as_str()) {
                    bail!(
                        "config.toml: [sandbox] entry `{}` lists unknown tool `{tool}`; the \
                         tools are {}",
                        entry.path(),
                        tools::NAMES.join(", ")
                    );
                }
            }
            let dir = resolve(entry.path())?;
            check_protected(&dir, entry.path())?;
            // Two entries over one directory are one claim: the tools add up, so writing an entry
            // for `~/.agents` adds to the default rather than replacing it. Subtracting would need
            // a second syntax for "none of these", which is what leaving it out already means.
            let key = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            match tool_paths
                .iter_mut()
                .find(|p| p.dir.canonicalize().unwrap_or_else(|_| p.dir.clone()) == key)
            {
                Some(existing) => {
                    for tool in tools {
                        if !existing.tools.contains(tool) {
                            existing.tools.push(tool.clone());
                        }
                    }
                    // The written spelling is what a person recognises, so it wins over the
                    // built-in `~/.agents`.
                    existing.written = entry.path().to_owned();
                }
                None => tool_paths.push(ToolPath {
                    dir,
                    written: entry.path().to_owned(),
                    tools: tools.to_vec(),
                }),
            }
        }
        // The built-in rule first, so a machine with nothing configured still works (D62).
        let built_in = default_include();
        let rules: Vec<&IncludeConfig> = if self.sandbox.include_defaults {
            iter::once(&built_in)
                .chain(self.sandbox.include.iter())
                .collect()
        } else {
            self.sandbox.include.iter().collect()
        };
        // Rules are combined per base rather than enumerated separately, and their `except` lists
        // are merged. Two rules over the same base would otherwise *union*: the broader one grants
        // a directory the narrower one excluded, so a person's extra pattern would do nothing.
        // Adding an exclusion has to narrow (D62).
        let mut by_base: BTreeMap<PathBuf, (usize, Vec<regex::Regex>)> = BTreeMap::new();
        for rule in rules {
            // A root name is accepted so a rule can follow the same names as everything else.
            let base = match RootName::new(&rule.base) {
                Ok(name) if roots.contains(&name) => roots.get(&name).map(Path::to_path_buf),
                _ => None,
            };
            let base = match base {
                Some(path) => path,
                None => expand(&rule.base)?,
            };
            if !base.is_dir() {
                // Nothing to walk. A missing base grants nothing rather than failing the session,
                // so a rule for a toolchain on another machine is harmless (D62).
                continue;
            }
            let except = rule
                .except
                .iter()
                .map(|p| {
                    regex::Regex::new(p)
                        .with_context(|| format!("config.toml: [[sandbox.include]] except `{p}`"))
                })
                .collect::<Result<Vec<_>>>()?;
            // Canonical, so `~` and `/home/me` are one base rather than two that overlap.
            let key = base.canonicalize().unwrap_or_else(|_| base.clone());
            let slot = by_base.entry(key).or_insert((0, Vec::new()));
            slot.0 = slot.0.max(rule.depth);
            slot.1.extend(except);
        }
        let mut include = Vec::new();
        for (base, (depth, except)) in by_base {
            include.extend(sandbox::enumerate(
                &Include {
                    base,
                    depth,
                    except,
                },
                protected,
            ));
        }
        include.sort();
        include.dedup();
        Ok((
            SandboxExtras {
                read: names(&self.sandbox.read)?,
                write: names(&self.sandbox.write)?,
                include,
            },
            tool_paths,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resource_overrides_are_strict_validated_and_do_not_create_grants() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[agent]\n[tool-limits]\nplugin-input-bytes=1048576\nplugin-fuel=10000000000\nhttp-redirects=0\n").unwrap();
        let config = UserConfig::load(&path).unwrap();
        assert_eq!(config.tool_limits.plugin_input_bytes, 1048576);
        assert_eq!(config.tool_limits.plugin_fuel, 10_000_000_000);
        assert_eq!(config.tool_limits.http_redirects, 0);
        assert_eq!(
            config.tool_limits.mutation_bytes,
            Budgets::default().mutation_bytes
        );
        assert!(
            config.global_policy()["agent"]
                .as_table()
                .unwrap()
                .is_empty()
        );
        for bad in [
            "plugin-input-bytes=0",
            "plugin-input-bytes=-1",
            "plugin-input-bytes=4294967296",
            "plugin-inpt-bytes=1024",
        ] {
            fs::write(&path, format!("[tool-limits]\n{bad}\n")).unwrap();
            assert!(UserConfig::load(&path).is_err(), "{bad}");
        }
    }

    #[test]
    fn an_old_percent_grants_file_is_renamed_and_stamped_in_one_pass() {
        // D63: the `%` name is the only one that decodes exactly, so the rename and the
        // `workspace` line happen together. Doing them as two passes left the line behind, and it
        // arrived only on the next run.
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "provider = \"synthetic\"\n").unwrap();
        let policies = dir.path().join("policies");
        fs::create_dir_all(&policies).unwrap();
        let old = policies.join("home%me%my-proj.toml");
        fs::write(&old, "version = 1\n[agent]\nexec = { why = \"x\" }\n").unwrap();

        let touched = migrate(&config, &policies).unwrap();
        assert_eq!(touched.len(), 1, "{touched:?}");
        assert!(!old.exists(), "the old name is gone on this run");
        let new = policies.join("--home-me-my-proj--.toml");
        let text = fs::read_to_string(&new).unwrap();
        assert!(text.contains("workspace = \"/home/me/my-proj\""), "{text}");
        assert!(text.contains("exec"), "the grants survive: {text}");

        // Running again changes nothing.
        assert!(migrate(&config, &policies).unwrap().is_empty());
        assert_eq!(fs::read_to_string(&new).unwrap(), text);
    }

    #[test]
    fn a_config_without_max_steps_means_no_limit() {
        let cfg: UserConfig = toml::from_str("provider = \"local\"\n").unwrap();
        assert_eq!(cfg.max_steps, None, "unset is unlimited, not 50 (D37)");
        let cfg: UserConfig = toml::from_str("max-steps = 5\n").unwrap();
        assert_eq!(cfg.max_steps, Some(5));
    }

    #[test]
    fn edits_keep_comments_and_refuse_invalid_results() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# top comment\nprovider = \"anthropic\"\n\n[sandbox]\nread = [] # inline\n",
        )
        .unwrap();
        edit(&path, |doc| {
            doc["provider"] = toml_edit::value("local");
            let t = provider_table(doc, "local")?;
            t["api"] = toml_edit::value("openai");
            t["base-url"] = toml_edit::value("http://127.0.0.1:8080/v1");
            t["model"] = toml_edit::value("qwen");
            Ok(())
        })
        .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# top comment") && text.contains("# inline"),
            "{text}"
        );
        assert!(
            text.contains("[providers.local]") && !text.contains("[providers]\n"),
            "{text}"
        );
        let cfg = UserConfig::load(&path).unwrap();
        assert_eq!(cfg.provider.as_deref(), Some("local"));
        assert_eq!(cfg.providers["local"].model.as_deref(), Some("qwen"));

        let bad = edit(&path, |doc| {
            doc["nonsense"] = toml_edit::value(1);
            Ok(())
        });
        assert!(bad.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }
}

/// Grants changed for one run, from `--grant` and `--revoke` (D63).
///
/// They are folded into the workspace's own document before it is parsed, so a session override
/// goes through exactly the same validation and the same audit record as a grant written in the
/// file. Nothing is stored: the session ends and they are gone.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// `scope` or `scope:root`, as in `--grant fs-read:scratch` or `--grant exec`.
    pub grant: Vec<String>,
    /// A scope whose grants are dropped for this session, as in `--revoke net`.
    pub revoke: Vec<String>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.grant.is_empty() && self.revoke.is_empty()
    }

    /// A phrase for the session intro, so what was changed is visible rather than remembered.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for g in &self.grant {
            parts.push(format!("+{g}"));
        }
        for r in &self.revoke {
            parts.push(format!("-{r}"));
        }
        parts.join(" ")
    }

    /// Applies the overrides to an `[agent]` table, in place.
    pub fn apply(&self, agent: &mut toml::Table) -> Result<()> {
        for scope in &self.revoke {
            let (scope, rest) = split_scope(scope);
            if !rest.is_empty() {
                bail!("--revoke takes a scope, as in `--revoke net`");
            }
            agent.remove(scope);
        }
        for spec in &self.grant {
            let (scope, rest) = split_scope(spec);
            let table = agent
                .entry(scope.to_owned())
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            match rest {
                "" => {
                    // A scope with no parameters: `exec`, `net` is not one of these.
                    if matches!(scope, "fs-read" | "fs-write" | "net" | "tool" | "tool-read") {
                        bail!(
                            "--grant {scope} needs what it grants, as in `--grant {scope}:scratch`"
                        );
                    }
                    let mut a = toml::Table::new();
                    a.insert(
                        "why".into(),
                        toml::Value::String("given on the command line".into()),
                    );
                    *table = toml::Value::Table(a);
                }
                rest => {
                    if scope == "tool-read" {
                        let mut grant = toml::Table::new();
                        grant.insert(
                            "tools".into(),
                            toml::Value::Array(
                                rest.split(',')
                                    .map(|role| toml::Value::String(role.trim().into()))
                                    .collect(),
                            ),
                        );
                        *table = toml::Value::Table(grant);
                        continue;
                    }
                    let mut grant = toml::Table::new();
                    let (root, exclude) = match rest.split_once(':') {
                        Some((root, pats)) => (
                            root,
                            pats.split(',')
                                .map(|p| toml::Value::String(p.trim().to_owned()))
                                .collect::<Vec<_>>(),
                        ),
                        None => (rest, Vec::new()),
                    };
                    let value = match scope {
                        "fs-read" | "fs-write" => {
                            grant.insert("root".into(), toml::Value::String(root.to_owned()));
                            if !exclude.is_empty() {
                                grant.insert("exclude".into(), toml::Value::Array(exclude));
                            }
                            toml::Value::Table(grant)
                        }
                        "net" => {
                            grant.insert("host".into(), toml::Value::String(root.to_owned()));
                            toml::Value::Table(grant)
                        }
                        other => bail!("--grant does not know how to build a `{other}` grant"),
                    };
                    let entry = table
                        .as_array_mut()
                        .with_context(|| format!("`{scope}` in config.toml is not a list"))?;
                    entry.push(value);
                }
            }
        }
        Ok(())
    }
}

fn split_scope(spec: &str) -> (&str, &str) {
    match spec.split_once(':') {
        Some((scope, rest)) => (scope.trim(), rest.trim()),
        None => (spec.trim(), ""),
    }
}

/// Folds `policy.toml` and `policies/*.toml` into `config.toml`, once (D63).
///
/// **Temporary.** The split between a policy file and a config file is gone, so an older install
/// has grants in files that are no longer read. This moves them into the one file and renames the
/// originals `.imported`, the same way the JSON Lines transcripts were imported (D53). It is
/// written to be deleted in the next version, when only the merged layout exists.
pub fn migrate(path: &Path, policies: &Path) -> Result<Vec<PathBuf>> {
    let mut touched = Vec::new();
    // 1. The base grants, if they are still in `policy.toml`.
    let global = path.with_file_name("policy.toml");
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
    };
    if let Ok(policy) = fs::read_to_string(&global) {
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("{}", path.display()))?;
        let policy: toml_edit::DocumentMut = policy
            .parse()
            .with_context(|| format!("{}", global.display()))?;
        if doc.get("agent").is_none()
            && let Some(agent) = policy.get("agent")
        {
            doc.insert("agent", agent.clone());
            let out = doc.to_string();
            toml::from_str::<UserConfig>(&out)
                .with_context(|| format!("the migration would leave {} invalid", path.display()))?;
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            fs::write(path, &out).with_context(|| format!("{}", path.display()))?;
            // Kept under a name nothing reads, so a second run is a no-op.
            let _ = fs::rename(&global, global.with_extension("toml.imported"));
            touched.push(global);
        }
    }
    // 2. Workspace grants files named with the old `%` separator: renamed, and the `workspace`
    // line stamped in the same step. The `%` form is the only name that decodes exactly, and doing
    // the two as separate passes is what made the line arrive one run late (D63).
    if let Ok(entries) = fs::read_dir(policies) {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        found.sort();
        for file in found {
            let Some(stem) = file.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            if !stem.contains('%') {
                continue;
            }
            let claimed = format!("/{}", stem.trim_start_matches('%').replace('%', "/"));
            let target = policies.join(format!(
                "{}.toml",
                paths::workspace_key(Path::new(&claimed))
            ));
            if target.exists() {
                continue;
            }
            let Ok(text) = fs::read_to_string(&file) else {
                continue;
            };
            let Ok(mut own) = text.parse::<toml_edit::DocumentMut>() else {
                continue;
            };
            if own.get("workspace").is_none() {
                own.insert("workspace", toml_edit::value(claimed));
            }
            fs::write(&target, own.to_string()).with_context(|| format!("{}", target.display()))?;
            fs::remove_file(&file).with_context(|| format!("{}", file.display()))?;
            touched.push(target);
        }
    }
    Ok(touched)
}
