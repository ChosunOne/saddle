//! protected workspace sandbox list overrides; operators never become grants (D157, D158).

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use saddle_cap::roots::normalize;
use saddle_cap::{RelPath, Roots};
use saddle_policy::audit::sha256_hex;
use serde::Deserialize;

use super::{SandboxConfig, SandboxEntry, UserConfig};
use crate::fs_access;
use crate::paths::Paths;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Lists {
    read: Option<Vec<SandboxEntry>>,
    write: Option<Vec<SandboxEntry>>,
}

#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub sha256: Option<String>,
}

/// base configuration has no inherited list to modify; do not silently accept operators there.
pub fn validate_global(sandbox: &SandboxConfig) -> Result<()> {
    for (name, entries) in [("read", &sandbox.read), ("write", &sandbox.write)] {
        if entries.iter().any(|entry| operator(entry.path()).is_some()) {
            bail!(
                "invalid config: sandbox.{name} uses + or - in global config; list operators are only valid in workspace overrides"
            );
        }
    }
    Ok(())
}

/// read the existing workspace override beneath protected configuration without following links.
pub fn read(paths: &Paths, path: &Path) -> Result<Option<String>> {
    let pinned = match fs::canonicalize(&paths.config) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let root = fs_access::root(&pinned)?;
    let relative = RelPath::new(path.strip_prefix(&paths.config)?)?;
    let mut file = match fs_access::file(&root, &relative) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(Some(text))
}

/// sandbox lists share the protected workspace document with grants, never a second file.
pub fn apply(
    user: &mut UserConfig,
    paths: &Paths,
    roots: &Roots,
    workspace: &Path,
    path: &Path,
    text: &str,
    doc: &toml::Table,
) -> Result<Source> {
    for key in doc.keys() {
        if ![
            "version",
            "workspace",
            "agent",
            "plugin",
            "sandbox",
            "launch-profiles",
        ]
        .contains(&key.as_str())
        {
            bail!(
                "unsupported workspace override section `{key}` in {}",
                path.display()
            );
        }
    }
    if let Some(value) = doc.get("sandbox") {
        if doc.get("workspace").and_then(|value| value.as_str()) != workspace.to_str() {
            bail!("sandbox overrides require a matching canonical workspace line");
        }
        let lists: Lists = value
            .clone()
            .try_into()
            .with_context(|| format!("invalid workspace override {}", path.display()))?;
        for (name, base, override_list) in [
            ("read", &mut user.sandbox.read, lists.read),
            ("write", &mut user.sandbox.write, lists.write),
        ] {
            if let Some(entries) = override_list {
                *base = merge(base, entries, roots, &paths.protected()).with_context(|| {
                    format!(
                        "invalid workspace override {}: sandbox.{name}",
                        path.display()
                    )
                })?;
            }
        }
    }
    Ok(Source {
        path: path.to_path_buf(),
        sha256: Some(sha256_hex(text.as_bytes())),
    })
}

fn operator(path: &str) -> Option<char> {
    path.chars().next().filter(|c| matches!(c, '+' | '-'))
}

/// reuse the ordinary sandbox resolver and protected-path/tool-name checks, not a second allowlist.
fn key(entry: &SandboxEntry, roots: &Roots, protected: &[PathBuf]) -> Result<PathBuf> {
    let config = UserConfig {
        sandbox: SandboxConfig {
            read: vec![entry.clone()],
            include_defaults: false,
            tools_defaults: false,
            ..SandboxConfig::default()
        },
        ..UserConfig::default()
    };
    let (extras, _) = config.sandbox_extras(roots, protected)?;
    let path = extras
        .read
        .into_iter()
        .next()
        .context("missing resolved sandbox entry")?;
    let actual = resolve_missing(&normalize(&path))?;
    for secret in protected {
        let secret = resolve_missing(&normalize(secret))?;
        if actual.starts_with(&secret) || secret.starts_with(&actual) {
            bail!(
                "sandbox entry resolves to a protected config/state directory; no sandbox entry may reach it"
            );
        }
    }
    Ok(actual)
}

/// resolve an existing ancestor too, so a missing leaf cannot hide a protected symlink target.
pub(crate) fn resolve_missing(path: &Path) -> Result<PathBuf> {
    let mut base = path;
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(base) {
            Ok(mut resolved) => {
                for part in suffix.into_iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if fs::symlink_metadata(base)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    bail!(
                        "sandbox entry has an unresolved symlink: {}",
                        base.display()
                    );
                }
                suffix.push(
                    base.file_name()
                        .context("sandbox path has no existing ancestor")?,
                );
                base = base.parent().context("sandbox path has no parent")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn merge(
    base: &[SandboxEntry],
    entries: Vec<SandboxEntry>,
    roots: &Roots,
    protected: &[PathBuf],
) -> Result<Vec<SandboxEntry>> {
    let prefixed = entries
        .iter()
        .filter(|entry| operator(entry.path()).is_some())
        .count();
    if prefixed == 0 {
        // An explicit empty list also replaces; omission is how a list inherits.
        for entry in &entries {
            key(entry, roots, protected)?;
        }
        return Ok(entries);
    }
    if prefixed != entries.len() {
        bail!(
            "cannot mix prefixed and unprefixed paths; use all + or - entries to modify, or no prefixes to replace"
        );
    }
    let mut merged = base.to_vec();
    for entry in entries {
        let op = operator(entry.path()).context("missing sandbox list operator")?;
        let entry = match entry {
            SandboxEntry::Path(path) => SandboxEntry::Path(path[1..].into()),
            SandboxEntry::WithTools { path, tools } => SandboxEntry::WithTools {
                path: path[1..].into(),
                tools,
            },
        };
        if entry.path().is_empty() {
            bail!("sandbox list operator needs a path");
        }
        let target = key(&entry, roots, protected)?;
        if op == '+' {
            // Keep each claim's tool roles; the ordinary resolver unions duplicate roles.
            if !merged.contains(&entry) {
                merged.push(entry);
            }
        } else {
            let mut kept = Vec::new();
            for previous in merged {
                if key(&previous, roots, protected)? != target {
                    kept.push(previous);
                }
            }
            merged = kept;
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    use saddle_cap::{RootName, Roots};

    use super::{merge, validate_global};
    use crate::config::{SandboxConfig, SandboxEntry};

    fn paths(values: &[&str]) -> Vec<SandboxEntry> {
        values
            .iter()
            .map(|path| SandboxEntry::Path((*path).into()))
            .collect()
    }

    #[test]
    fn missing_leaves_cannot_hide_a_protected_alias() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let protected = temp.path().join("protected");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&protected).unwrap();
        let alias = temp.path().join("alias");
        symlink(&protected, &alias).unwrap();
        let roots = Roots::resolve([(RootName::workspace(), workspace)], &[]).unwrap();
        let entry = format!("+{}/missing", alias.display());
        assert!(merge(&[], paths(&[&entry]), &roots, &[protected]).is_err());
    }

    #[test]
    fn list_operators_modify_but_plain_or_empty_lists_replace() {
        let temp = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), temp.path().into())], &[]).unwrap();
        let base = paths(&["/one", "/two"]);
        assert_eq!(
            merge(&base, paths(&["+/three", "-/one"]), &roots, &[]).unwrap(),
            paths(&["/two", "/three"])
        );
        assert_eq!(
            merge(&base, paths(&["/three"]), &roots, &[]).unwrap(),
            paths(&["/three"])
        );
        assert!(merge(&base, Vec::new(), &roots, &[]).unwrap().is_empty());
        assert!(merge(&base, paths(&["+/three", "/four"]), &roots, &[]).is_err());
        assert!(merge(&base, paths(&["+"]), &roots, &[]).is_err());
        assert!(merge(&base, paths(&["+relative"]), &roots, &[]).is_err());
        assert!(
            merge(
                &base,
                paths(&["+/protected"]),
                &roots,
                &[PathBuf::from("/protected")]
            )
            .is_err()
        );
        let config = SandboxConfig {
            write: paths(&["+/three"]),
            ..SandboxConfig::default()
        };
        assert!(validate_global(&config).is_err());
        let config = SandboxConfig {
            read: paths(&["-/three"]),
            ..SandboxConfig::default()
        };
        assert!(validate_global(&config).is_err());
    }
}
