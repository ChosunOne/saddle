//! protected launch-profile definitions and workspace overlays (D184); no invocation authority.

use std::collections::BTreeMap;
use std::env;
use std::num::NonZeroU32;
use std::path::{Component, PathBuf};

use anyhow::{Context, Result, bail};
use saddle_cap::policy::valid_launch_profile_name;
use serde::Deserialize;

use super::sandbox_overrides::resolve_missing;

/// definitions as written in one protected configuration source.
pub type Definitions = BTreeMap<String, Definition>;

/// Partial in a workspace overlay; a new definition must name an executable.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Definition {
    executable: Option<String>,
    read: Option<Vec<String>>,
    timeout_secs: Option<NonZeroU32>,
}

/// effective paths and field provenance for `saddle check`, not pinned runtime handles.
#[derive(Debug)]
pub struct Resolved {
    pub executable: PathBuf,
    pub read: Vec<PathBuf>,
    pub timeout_secs: Option<NonZeroU32>,
    pub workspace_fields: Vec<&'static str>,
}

/// reject incomplete defaults even if the current workspace does not use them.
pub fn validate_global(definitions: &Definitions) -> Result<()> {
    for (name, definition) in definitions {
        validate(name, definition)?;
        if definition.executable.is_none() {
            bail!("launch-profiles.{name} needs an executable");
        }
    }
    Ok(())
}

/// Compute the effective definitions without adding them to any of the ordinary reach lists.
pub fn resolve(
    mut base: Definitions,
    overlay: Definitions,
    protected: &[PathBuf],
) -> Result<BTreeMap<String, Resolved>> {
    let mut workspace_fields = BTreeMap::new();
    for (name, fields) in overlay {
        validate(&name, &fields)?;
        let mut source = Vec::new();
        if let Some(inherited) = base.get_mut(&name) {
            if fields.executable.is_some() {
                bail!(
                    "launch-profiles.{name} inherits its executable; use a new profile name to select an executable"
                );
            }
            if let Some(read) = fields.read {
                inherited.read = Some(read);
                source.push("read");
            }
            if let Some(timeout) = fields.timeout_secs {
                inherited.timeout_secs = Some(timeout);
                source.push("timeout-secs");
            }
        } else {
            source.push("executable");
            if fields.read.is_some() {
                source.push("read");
            }
            if fields.timeout_secs.is_some() {
                source.push("timeout-secs");
            }
            base.insert(name.clone(), fields);
        }
        workspace_fields.insert(name, source);
    }
    base.into_iter()
        .map(|(name, definition)| {
            let executable = definition
                .executable
                .with_context(|| format!("launch-profiles.{name} needs an executable"))?;
            let executable = check_path(&executable, protected)
                .with_context(|| format!("launch-profiles.{name}.executable"))?;
            let read = definition
                .read
                .unwrap_or_default()
                .iter()
                .map(|path| check_path(path, protected))
                .collect::<Result<_>>()
                .with_context(|| format!("launch-profiles.{name}.read"))?;
            let workspace_fields = workspace_fields.remove(&name).unwrap_or_default();
            Ok((
                name,
                Resolved {
                    executable,
                    read,
                    timeout_secs: definition.timeout_secs,
                    workspace_fields,
                },
            ))
        })
        .collect()
}

fn validate(name: &str, fields: &Definition) -> Result<()> {
    if !valid_launch_profile_name(name) {
        bail!("invalid launch profile name {name:?}: use 1-64 letters, digits, `-` or `_`");
    }
    for path in fields.executable.iter().chain(fields.read.iter().flatten()) {
        expand(path).with_context(|| format!("launch-profiles.{name}"))?;
    }
    Ok(())
}

fn expand(written: &str) -> Result<PathBuf> {
    let path = match written.strip_prefix("~/") {
        Some(rest) => PathBuf::from(env::var_os("HOME").context("`~/` needs HOME to be set")?)
            .join(rest.trim_start_matches('/')),
        None => PathBuf::from(written),
    };
    if !path.is_absolute()
        || written.contains('\0')
        || path.components().any(|c| c == Component::ParentDir)
    {
        bail!("expected an absolute path or ~/path without `..`, got {written:?}");
    }
    Ok(path)
}

fn check_path(written: &str, protected: &[PathBuf]) -> Result<PathBuf> {
    let path = expand(written)?;
    let resolved = resolve_missing(&path)?;
    for directory in protected {
        let actual = resolve_missing(directory)?;
        for candidate in [&path, &resolved] {
            if candidate.starts_with(directory)
                || directory.starts_with(candidate)
                || candidate.starts_with(&actual)
                || actual.starts_with(candidate)
            {
                bail!("launch profile path {written:?} overlaps protected config/state");
            }
        }
    }
    // This is a configuration-time check only. The launcher must validate and pin what it opens.
    Ok(resolved)
}
