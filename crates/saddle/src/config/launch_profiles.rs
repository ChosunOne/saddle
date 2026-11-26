use std::collections::BTreeMap;
use std::env;
use std::num::NonZeroU32;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use saddle_cap::policy::{HostPattern, valid_launch_profile_name};
use saddle_sandbox::launch_digest;
use serde::Deserialize;

use super::sandbox_overrides::resolve_missing;

pub type Definitions = BTreeMap<String, Definition>;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Definition {
    executable: Option<String>,
    read: Option<Vec<String>>,
    read_workspace: Option<bool>,
    write_workspace: Option<bool>,
    helpers: Option<Vec<String>>,
    verify_sha256: Option<bool>,
    net: Option<Vec<String>>,
    ssh_hosts: Option<Vec<String>>,
    timeout_secs: Option<NonZeroU32>,
}

#[derive(Debug)]
pub struct Resolved {
    pub executable: PathBuf,
    pub read: Vec<PathBuf>,
    pub read_workspace: bool,
    pub write_workspace: bool,
    pub helpers: Vec<PathBuf>,
    pub verify_sha256: bool,
    pub net: Vec<String>,
    pub ssh_hosts: Vec<String>,
    pub timeout_secs: Option<NonZeroU32>,
    pub workspace_fields: Vec<&'static str>,
}

pub fn validate_global(definitions: &Definitions) -> Result<()> {
    for (name, definition) in definitions {
        validate(name, definition)?;
        if definition.executable.is_none() {
            bail!("launch-profiles.{name} needs an executable");
        }
    }
    Ok(())
}

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
            for (value, name) in [
                (fields.read_workspace, "read-workspace"),
                (fields.write_workspace, "write-workspace"),
            ] {
                if let Some(value) = value {
                    let target = if name == "read-workspace" {
                        &mut inherited.read_workspace
                    } else {
                        &mut inherited.write_workspace
                    };
                    *target = Some(value);
                    source.push(name);
                }
            }
            if let Some(helpers) = fields.helpers {
                inherited.helpers = Some(helpers);
                source.push("helpers");
            }
            if let Some(verify) = fields.verify_sha256 {
                inherited.verify_sha256 = Some(verify);
                source.push("verify-sha256");
            }
            if let Some(net) = fields.net {
                inherited.net = Some(net);
                source.push("net");
            }
            if let Some(hosts) = fields.ssh_hosts {
                inherited.ssh_hosts = Some(hosts);
                source.push("ssh-hosts");
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
            if fields.read_workspace.is_some() {
                source.push("read-workspace");
            }
            if fields.write_workspace.is_some() {
                source.push("write-workspace");
            }
            if fields.helpers.is_some() {
                source.push("helpers");
            }
            if fields.verify_sha256.is_some() {
                source.push("verify-sha256");
            }
            if fields.net.is_some() {
                source.push("net");
            }
            if fields.ssh_hosts.is_some() {
                source.push("ssh-hosts");
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
            let helpers = definition
                .helpers
                .unwrap_or_default()
                .iter()
                .map(|path| check_path(path, protected))
                .collect::<Result<_>>()
                .with_context(|| format!("launch-profiles.{name}.helpers"))?;
            let workspace_fields = workspace_fields.remove(&name).unwrap_or_default();
            Ok((
                name,
                Resolved {
                    executable,
                    read,
                    read_workspace: definition.read_workspace.unwrap_or(true),
                    write_workspace: definition.write_workspace.unwrap_or(true),
                    helpers,
                    verify_sha256: definition.verify_sha256.unwrap_or(false),
                    net: definition
                        .net
                        .unwrap_or_default()
                        .into_iter()
                        .map(|host| host.to_ascii_lowercase())
                        .collect(),
                    ssh_hosts: definition.ssh_hosts.unwrap_or_default(),
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
    for host in fields.net.iter().flatten() {
        if !matches!(HostPattern::parse(host), Ok(HostPattern::Exact(_))) {
            bail!(
                "launch-profiles.{name}.net needs an exact hostname or IP for HTTPS on port 443, got {host:?}"
            );
        }
    }
    for host in fields.ssh_hosts.iter().flatten() {
        if !matches!(HostPattern::parse(host), Ok(HostPattern::Exact(_))) {
            bail!(
                "launch-profiles.{name}.ssh-hosts needs an exact hostname or IP for SSH on port 22, got {host:?}"
            );
        }
    }
    for path in fields
        .executable
        .iter()
        .chain(fields.read.iter().flatten())
        .chain(fields.helpers.iter().flatten())
    {
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
    Ok(path)
}

#[derive(Default)]
pub struct Baselines(BTreeMap<PathBuf, Result<[u8; 32], String>>);

impl Baselines {
    pub fn enroll(&mut self, profiles: &BTreeMap<String, Resolved>) {
        for profile in profiles.values().filter(|profile| profile.verify_sha256) {
            for path in [&profile.executable].into_iter().chain(&profile.helpers) {
                self.0
                    .entry(path.clone())
                    .or_insert_with(|| launch_digest(path).map_err(|error| error.to_string()));
            }
        }
    }

    pub fn expected(&self, profile: &Resolved, path: &Path) -> Result<Option<[u8; 32]>, String> {
        if !profile.verify_sha256 {
            return Ok(None);
        }
        match self.0.get(path) {
            Some(Ok(digest)) => Ok(Some(*digest)),
            result => {
                let why = match result {
                    Some(Err(why)) => why.as_str(),
                    _ => "baseline not recorded",
                };
                Err(format!(
                    "cannot verify launch binary {}: {why}; restart Saddle after checking the path",
                    path.display()
                ))
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{Baselines, resolve};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn launch_digest_baselines_are_opt_in_and_survive_reload() {
        let dir = tempdir().unwrap();
        let executable = dir.path().join("binary");
        fs::write(&executable, b"\x7fELF-original").unwrap();
        let base = format!("[demo]\nexecutable = {:?}\n", executable.to_str().unwrap());
        let profiles = |overlay: &str| {
            resolve(
                toml::from_str(&base).unwrap(),
                toml::from_str(overlay).unwrap(),
                &[],
            )
            .unwrap()
        };
        let mut baselines = Baselines::default();
        let disabled = profiles("");
        baselines.enroll(&disabled);
        assert_eq!(
            baselines.expected(&disabled["demo"], &executable).unwrap(),
            None
        );
        let enabled = profiles("[demo]\nverify-sha256 = true\n");
        assert_eq!(enabled["demo"].workspace_fields, ["verify-sha256"]);
        baselines.enroll(&enabled);
        let original = baselines.expected(&enabled["demo"], &executable).unwrap();
        assert!(original.is_some());
        fs::write(&executable, b"\x7fELF-modified").unwrap();
        baselines.enroll(&profiles("[demo]\nverify-sha256 = false\n"));
        baselines.enroll(&enabled);
        assert_eq!(
            baselines.expected(&enabled["demo"], &executable).unwrap(),
            original
        );
        let mut restarted = Baselines::default();
        restarted.enroll(&enabled);
        assert_ne!(
            restarted.expected(&enabled["demo"], &executable).unwrap(),
            original
        );
    }
}
