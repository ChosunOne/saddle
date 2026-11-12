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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use saddle_cap::sandbox::SandboxExtras;
use saddle_cap::{RootName, Roots};
use serde::Deserialize;

use crate::provider::ProviderEntry;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
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
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderEntry>,
}

/// How the TUI starts; ctrl+t and ctrl+o change these for the session only.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct UiConfig {
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
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ContextConfig {
    /// The global AGENTS.md; empty turns it off. Defaults to `$XDG_CONFIG_HOME/saddle/AGENTS.md`.
    pub global: Option<String>,
    /// Read CLAUDE.md where no AGENTS.md exists (and `~/.claude/CLAUDE.md` when there is no
    /// global AGENTS.md). Defaults to on.
    pub claude_fallback: Option<bool>,
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxConfig {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    /// Directories worked out by walking a base and excluding what matches, so a command gets what
    /// it needs without saddle knowing about each tool (D62).
    #[serde(default)]
    pub include: Vec<IncludeConfig>,
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

fn default_depth() -> usize {
    2
}

/// Applies `change` to config.toml, keeping its comments and layout, and writes the result only if
/// it still parses as a valid config.
pub fn edit(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
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
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(".config.toml.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
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
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("{}", path.display())),
        }
    }

    pub fn root_entries(&self) -> Result<Vec<(RootName, PathBuf)>> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
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

    /// The extras every child gets: named roots, plus the directories the include rules work out
    /// (D62). Enumeration happens here because the protected directories are known here, and the
    /// result is a plain list of paths, so the lowering itself stays a mapping.
    pub fn sandbox_extras(&self, roots: &Roots, protected: &[PathBuf]) -> Result<SandboxExtras> {
        let names = |list: &[String]| -> Result<Vec<RootName>> {
            list.iter()
                .map(|n| {
                    let name = RootName::new(n)?;
                    if !roots.contains(&name) {
                        bail!("config.toml: [sandbox] names unknown root `{name}`");
                    }
                    Ok(name)
                })
                .collect()
        };
        let home = std::env::var_os("HOME").map(PathBuf::from);
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
        let mut include = Vec::new();
        for rule in &self.sandbox.include {
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
            let resolved = saddle_cap::sandbox::enumerate(
                &saddle_cap::sandbox::Include {
                    base,
                    depth: rule.depth,
                    except,
                },
                protected,
            );
            include.extend(resolved);
        }
        include.sort();
        include.dedup();
        Ok(SandboxExtras {
            read: names(&self.sandbox.read)?,
            write: names(&self.sandbox.write)?,
            include,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        std::fs::write(
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
        let text = std::fs::read_to_string(&path).unwrap();
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
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
}
