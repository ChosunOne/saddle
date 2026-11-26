pub mod launch_profiles;
pub mod sandbox_overrides;

use crate::defaults;
use crate::mcp;
use crate::paths;
use crate::plugins::Entry as PluginEntry;
use crate::standard::{self, Profiles};
use crate::tools;
use crate::tui::keys::Keys;
use crate::tui::notify::Notify;
use crate::tui::theme::ThemeTables;
use saddle_cap::sandbox;
use saddle_cap::sandbox::Include;
use saddle_limits::Budgets;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Error as AnyError, Result, bail};
use saddle_cap::sandbox::SandboxExtras;
use saddle_cap::{PluginId, RootName, Roots, policy};
use serde::Deserialize;

use crate::mcp::McpTable;
use crate::provider::ProviderEntry;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    #[serde(default, rename = "standard-tools")]
    pub standard_tools: Profiles,
    #[serde(default, rename = "tool-limits")]
    pub tool_limits: Budgets,
    #[serde(default)]
    pub plugin: BTreeMap<PluginId, toml::Value>,
    #[serde(default)]
    pub plugins: BTreeMap<PluginId, PluginEntry>,
    #[serde(default)]
    pub agent: Option<toml::Value>,
    #[serde(default)]
    pub profiles: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub workspaces: BTreeMap<String, toml::Value>,
    #[serde(default)]
    pub roots: BTreeMap<String, String>,
    #[serde(default)]
    pub sandbox: SandboxConfig,
    #[serde(default, rename = "launch-profiles")]
    pub launch_profiles: launch_profiles::Definitions,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default, rename = "max-steps")]
    pub max_steps: Option<usize>,
    #[serde(default, rename = "allow-sensitive")]
    pub allow_sensitive: bool,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub themes: ThemeTables,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
    #[serde(default)]
    pub mcp: McpTable,
    #[serde(default)]
    pub keys: Keys,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct UiConfig {
    pub theme: Option<String>,
    pub show_reasoning: Option<bool>,
    pub expand_tools: Option<bool>,
    pub status: Option<String>,
    pub mouse: Option<bool>,
    pub images: Option<String>,
    pub notify: Option<Notify>,
    pub math_scale: Option<f32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContextConfig {
    pub global: Option<String>,
    pub claude_fallback: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct SkillsConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct CompactionConfig {
    pub auto: Option<bool>,
    pub reserve_tokens: Option<u64>,
    pub keep_recent_tokens: Option<u64>,
    pub idle: Option<bool>,
    pub idle_fraction: Option<f64>,
    pub idle_seconds: Option<u64>,
    pub background: Option<bool>,
    pub background_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub read: Vec<SandboxEntry>,
    #[serde(default)]
    pub write: Vec<SandboxEntry>,
    #[serde(default)]
    pub include: Vec<IncludeConfig>,
    #[serde(default = "yes", rename = "include-defaults")]
    pub include_defaults: bool,
    #[serde(default = "yes", rename = "tools-defaults")]
    pub tools_defaults: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SandboxEntry {
    Path(String),
    WithTools {
        path: String,
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

    pub fn tools(&self) -> &[String] {
        match self {
            Self::Path(_) => &[],
            Self::WithTools { tools, .. } => tools,
        }
    }
}

impl Default for SandboxConfig {
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

pub const DEFAULT_TOOL_TOOLS: &[&str] = &["read", "ls", "find", "grep"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPath {
    pub dir: PathBuf,
    pub written: String,
    pub tools: Vec<String>,
}

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

pub const DEFAULT_EXCEPTS: &[&str] = &[
    r"^\.ssh$",
    r"^\.gnupg$",
    r"^\.pki$",
    r"^\.password-store$",
    r"^\.local/share/keyrings$",
    r"^\.local/share/kwalletd$",
    r"^\.config/op$",
    r"^\.vault-token$",
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
    r"^\.config/gh$",
    r"^\.config/glab-cli$",
    r"^\.config/hub$",
    r"^\.config/tea$",
    r"^\.netrc$",
    r"^\.git-credentials$",
    r"^\.config/git/credentials$",
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
    r"^\.pgpass$",
    r"^\.my\.cnf$",
    r"^\.mylogin\.cnf$",
    r"^\.msmtprc$",
    r"^\.offlineimaprc$",
    r"^\.fetchmailrc$",
    r"^\.authinfo$",
    r"^\.bash_history$",
    r"^\.zsh_history$",
    r"^\.python_history$",
    r"^\.node_repl_history$",
    r"^\.psql_history$",
    r"^\.mysql_history$",
    r"^\.mozilla$",
    r"^\.thunderbird$",
    r"^\.config/google-chrome$",
    r"^\.config/chromium$",
];

pub fn default_includes() -> Vec<IncludeConfig> {
    vec![
        IncludeConfig {
            base: "~/.cargo".to_owned(),
            depth: default_depth(),
            except: vec![
                r"^credentials$".to_owned(),
                r"^credentials\.toml$".to_owned(),
            ],
        },
        IncludeConfig {
            base: "~/.rustup".to_owned(),
            depth: default_depth(),
            except: Vec::new(),
        },
        IncludeConfig {
            base: "~/.config".to_owned(),
            depth: default_depth(),
            except: DEFAULT_EXCEPTS
                .iter()
                .filter_map(|pattern| pattern.strip_prefix(r"^\.config/"))
                .map(|pattern| format!("^{pattern}"))
                .collect(),
        },
    ]
}

pub fn explicit_home_defaults() -> IncludeConfig {
    IncludeConfig {
        base: "~".to_owned(),
        depth: default_depth(),
        except: DEFAULT_EXCEPTS.iter().map(|p| (*p).to_owned()).collect(),
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct IncludeConfig {
    pub base: String,
    #[serde(default = "default_depth")]
    pub depth: usize,
    #[serde(default)]
    pub except: Vec<String>,
}

fn default_depth() -> usize {
    3
}

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
    let config = toml::from_str::<UserConfig>(&out)
        .with_context(|| format!("the change would leave {} invalid", path.display()))?;
    sandbox_overrides::validate_global(&config.sandbox)?;
    launch_profiles::validate_global(&config.launch_profiles)?;
    let dir = path.parent().context("config path has no parent")?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(".config.toml.tmp");
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

pub fn table<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    name: &str,
) -> Result<&'a mut toml_edit::Table> {
    doc.entry(name)
        .or_insert(toml_edit::table())
        .as_table_mut()
        .with_context(|| format!("`{name}` in config.toml must be a table"))
}

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

pub const NONE_PROFILE: &str = "none";

fn validate_profile_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("a profile name is 1-64 letters, digits, `-` or `_`; `{name}` is not one");
    }
    if name == NONE_PROFILE {
        bail!("`{NONE_PROFILE}` cannot be a profile name: it means [agent] as written");
    }
    Ok(())
}

impl UserConfig {
    pub fn profile_in_force(&self, chosen: Option<&str>) -> Result<Option<String>> {
        let Some(name) = chosen.or(self.profile.as_deref()) else {
            return Ok(None);
        };
        if name == NONE_PROFILE {
            return Ok(None);
        }
        validate_profile_name(name)?;
        if !self.profiles.contains_key(name) {
            bail!(
                "no profile named `{name}` in config.toml{}; `{NONE_PROFILE}` is [agent] as written",
                match self.profiles.keys().map(String::as_str).collect::<Vec<_>>() {
                    names if names.is_empty() => String::new(),
                    names => format!(", which defines `{}`", names.join("`, `")),
                }
            );
        }
        Ok(Some(name.to_owned()))
    }

    fn validate_profiles(&self) -> Result<()> {
        for (name, grants) in &self.profiles {
            validate_profile_name(name)?;
            if !grants.is_table() {
                bail!("profile `{name}` must be a table of grants, the way [agent] is");
            }
        }
        if let Some(name) = &self.profile
            && name != NONE_PROFILE
        {
            validate_profile_name(name)?;
            if !self.profiles.contains_key(name) {
                bail!(
                    "`profile = \"{name}\"` names no [profiles.{name}]; `{NONE_PROFILE}` is [agent] as written"
                );
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self> {
        let config: Self = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("{}", path.display()))?,
            Err(e) if e.kind() == ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("{}", path.display())),
        };
        sandbox_overrides::validate_global(&config.sandbox)
            .with_context(|| format!("{}", path.display()))?;
        launch_profiles::validate_global(&config.launch_profiles)
            .with_context(|| format!("{}", path.display()))?;
        config
            .validate_profiles()
            .with_context(|| format!("{}", path.display()))?;
        standard::validate(&config.standard_tools)?;
        if config
            .plugins
            .keys()
            .chain(config.plugin.keys())
            .any(|id| id.as_str().starts_with("standard-"))
        {
            bail!("standard-* plugin ids are reserved for shipped tools; use [standard-tools]");
        }
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

    pub fn global_policy(&self, profile: Option<&str>) -> Result<toml::Value> {
        let agent = self.agent.clone().or_else(|| {
            toml::from_str::<toml::Value>(defaults::DEFAULT_AGENT)
                .ok()
                .map(|v| toml::Value::Table(v.as_table().cloned().unwrap_or_default()))
        });
        let mut document = policy_document(agent);
        policy::check_base(&document).context("config.toml")?;
        if let Some(name) = profile {
            let Some(grants) = self.profiles.get(name).cloned() else {
                return Ok(policy_document(None));
            };
            document = policy::merge(document, policy_document(Some(grants)))
                .with_context(|| format!("profiles.{name}"))?;
        }
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
        Ok(document)
    }

    pub fn sandbox_extras(
        &self,
        roots: &Roots,
        protected: &[PathBuf],
    ) -> Result<(SandboxExtras, Vec<ToolPath>)> {
        let home = env::var_os("HOME").map(PathBuf::from);
        let expand = |path: &str| -> Result<PathBuf> {
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
        let check_protected = |path: &Path, written: &str| -> Result<()> {
            for protected in protected {
                if path.starts_with(protected) || protected.starts_with(path) {
                    bail!(
                        "config.toml: [sandbox] entry `{written}` names, contains or sits inside \
                         {}, which holds the policy, the audit log and the stored keys; no \
                         sandbox entry may reach it (D16). Include rules leave it out on their own.",
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
        let mut tool_paths: Vec<ToolPath> = Vec::new();
        if self.sandbox.tools_defaults
            && let Some(home) = &home
        {
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
                    existing.written = entry.path().to_owned();
                }
                None => tool_paths.push(ToolPath {
                    dir,
                    written: entry.path().to_owned(),
                    tools: tools.to_vec(),
                }),
            }
        }
        let include_base = |rule: &IncludeConfig| -> Result<PathBuf> {
            if let Ok(name) = RootName::new(&rule.base)
                && let Some(path) = roots.get(&name)
            {
                return Ok(path.to_path_buf());
            }
            expand(&rule.base)
        };
        let canonical_home = home
            .as_ref()
            .map(|home| home.canonicalize().unwrap_or_else(|_| home.clone()));
        let mut explicit_home = false;
        for rule in &self.sandbox.include {
            let base = include_base(rule)?;
            let base = base.canonicalize().unwrap_or(base);
            explicit_home |= canonical_home.as_ref() == Some(&base);
        }
        let built_in = if !self.sandbox.include_defaults {
            Vec::new()
        } else if explicit_home {
            vec![explicit_home_defaults()]
        } else {
            default_includes()
        };
        let rules = built_in
            .iter()
            .map(|rule| (rule, true))
            .chain(self.sandbox.include.iter().map(|rule| (rule, false)));
        let mut by_base: BTreeMap<PathBuf, (usize, Vec<regex::Regex>)> = BTreeMap::new();
        for (rule, implicit) in rules {
            let base = include_base(rule)?;
            if implicit
                && !explicit_home
                && fs::symlink_metadata(&base)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                continue;
            }
            if !base.is_dir() {
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
    use crate::tui::keys::Keys;
    use std::fs;

    #[test]
    fn a_profile_is_layered_on_the_base_and_an_unknown_one_grants_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            concat!(
                "[agent]\n",
                "exec = { timeout-secs = 20 }\n",
                "net = [{ host = \"api.example.com\" }]\n\n",
                "[profiles.build]\nexec = { timeout-secs = 300 }\n\n",
                "[profiles.offline]\nnet = []\n\n",
                "[profiles.empty]\n",
            ),
        )
        .unwrap();
        let config = UserConfig::load(&path).unwrap();
        let agent = |profile: Option<&str>| {
            config.global_policy(profile).unwrap()["agent"]
                .as_table()
                .cloned()
                .unwrap()
        };
        assert_eq!(
            agent(None)["exec"]["timeout-secs"].as_integer(),
            Some(20),
            "the base as written"
        );
        let build = agent(Some("build"));
        assert_eq!(build["exec"]["timeout-secs"].as_integer(), Some(300));
        assert_eq!(
            build["net"][0]["host"].as_str(),
            Some("api.example.com"),
            "a profile inherits what it does not name: {build:?}"
        );
        let offline = agent(Some("offline"));
        assert!(offline["net"].as_array().unwrap().is_empty());
        assert!(
            offline.contains_key("exec"),
            "a scope written as a table cannot be removed by a layer: {offline:?}"
        );
        assert_eq!(agent(Some("empty")), agent(None));
        assert!(agent(Some("missing")).is_empty());
    }

    #[test]
    fn the_profile_in_force_is_the_runs_choice_else_the_files_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "profile = \"locked\"\n\n[profiles.locked]\n\n[profiles.open]\n",
        )
        .unwrap();
        let config = UserConfig::load(&path).unwrap();
        assert_eq!(
            config.profile_in_force(None).unwrap().as_deref(),
            Some("locked")
        );
        assert_eq!(
            config.profile_in_force(Some("open")).unwrap().as_deref(),
            Some("open"),
            "a run or a session may choose another"
        );
        assert_eq!(
            config.profile_in_force(Some(NONE_PROFILE)).unwrap(),
            None,
            "`none` is [agent] as written"
        );
        let refused = config
            .profile_in_force(Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(refused.contains("no profile named `nope`"), "{refused}");
        assert!(
            refused.contains("locked"),
            "the alternatives are named: {refused}"
        );
    }

    #[test]
    fn a_profile_name_and_body_are_validated_where_they_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        for (text, expected) in [
            ("[profiles.\"two words\"]\n", "1-64 letters"),
            ("[profiles.none]\n", "cannot be a profile name"),
            ("profiles = { build = true }\n", "must be a table of grants"),
            ("profile = \"missing\"\n", "names no [profiles.missing]"),
        ] {
            fs::write(&path, text).unwrap();
            let error = format!("{:#}", UserConfig::load(&path).unwrap_err());
            assert!(error.contains(expected), "{text}: {error}");
        }
    }

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
            config.global_policy(None).unwrap()["agent"]
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
    fn the_maths_scale_is_read_and_a_misspelling_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[ui]\nmath-scale = 1.25\n").unwrap();
        assert_eq!(
            UserConfig::load(&path).unwrap().ui.math_scale,
            Some(1.25_f32)
        );
        fs::write(&path, "[ui]\nmath_scale = 1.25\n").unwrap();
        assert!(UserConfig::load(&path).is_err());
        fs::write(&path, "[ui]\nmath-scale = \"big\"\n").unwrap();
        assert!(UserConfig::load(&path).is_err());
    }

    #[test]
    fn a_bound_key_is_read_from_the_file_and_a_misspelt_one_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[keys]\ntree-next-end = \"ctrl+n\"\ntree-previous-end = \"alt+j\"\n",
        )
        .unwrap();
        let bound = UserConfig::load(&path).unwrap();
        assert_eq!(
            bound.keys.ends().as_deref(),
            Some("ctrl+n/alt+j branch ends")
        );
        fs::write(&path, "").unwrap();
        let none = UserConfig::load(&path).unwrap();
        assert_eq!(none.keys, Keys::default());
        assert_eq!(none.keys.ends(), None);
        fs::write(&path, "[keys]\ntree-next-end = \"ctrln\"\n").unwrap();
        let error = format!("{:#}", UserConfig::load(&path).unwrap_err());
        assert!(error.contains("ctrln"), "{error}");
        assert!(error.contains("not a key"), "{error}");
        fs::write(&path, "[keys]\ntree-nxt-end = \"ctrl+n\"\n").unwrap();
        assert!(UserConfig::load(&path).is_err());
    }

    #[test]
    fn an_old_percent_grants_file_is_renamed_and_stamped_in_one_pass() {
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

#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub grant: Vec<String>,
    pub revoke: Vec<String>,
    pub profile: Option<String>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.grant.is_empty() && self.revoke.is_empty()
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for g in &self.grant {
            parts.push(format!("+{g}"));
        }
        for r in &self.revoke {
            parts.push(format!("-{r}"));
        }
        if let Some(profile) = &self.profile {
            parts.push(format!("profile {profile}"));
        }
        parts.join(" ")
    }

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
                    if matches!(
                        scope,
                        "fs-read" | "fs-write" | "net" | "tool" | "tool-read" | "run-binary"
                    ) {
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
                        "run-binary" => {
                            grant.insert("profile".into(), toml::Value::String(rest.to_owned()));
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

pub fn migrate(path: &Path, policies: &Path) -> Result<Vec<PathBuf>> {
    let mut touched = Vec::new();
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
            let _ = fs::rename(&global, global.with_extension("toml.imported"));
            touched.push(global);
        }
    }
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
