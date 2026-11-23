//! explicit protected delegation profiles for shipped standard components (D147).

use anyhow::{Result, bail};
use saddle_cap::manifest::Manifest;
use saddle_cap::policy::PlainGrant;
use saddle_cap::{Grants, Scope};
use saddle_standard::artifact;
use std::collections::{BTreeMap, BTreeSet};

/// presence registers a shipped tool; listed scopes select only the parent's current reach.
pub type Profiles = BTreeMap<String, BTreeSet<Scope>>;

pub use saddle_standard::DEFAULT_PROFILES;

pub fn validate(profiles: &Profiles) -> Result<()> {
    for (name, scopes) in profiles {
        let Some(artifact) = artifact(name) else {
            bail!("unknown standard tool {name:?}");
        };
        let manifest = Manifest::parse(artifact.manifest)?;
        for scope in scopes {
            if *scope == Scope::Tool || !manifest.scopes.contains(scope) {
                bail!("standard tool {name} cannot delegate {scope}");
            }
        }
    }
    Ok(())
}

/// a profile is an explicit scope selector, never an independent resource list or authority.
pub fn delegate(parent: &Grants, name: &str, scopes: &BTreeSet<Scope>) -> Grants {
    let mut child = Grants::default();
    child.plain.insert(
        Scope::Tool,
        PlainGrant::registration(
            format!("standard-tools.{name}"),
            format!("explicit standard-tools.{name} registration"),
        ),
    );
    if scopes.contains(&Scope::FsRead) {
        child.fs_read = parent.fs_read.clone();
    }
    if scopes.contains(&Scope::FsWrite) {
        child.fs_write = parent.fs_write.clone();
    }
    if scopes.contains(&Scope::Exec) {
        child.exec = parent.exec.clone();
    }
    if scopes.contains(&Scope::Net) {
        child.net = parent.net.clone();
    }
    if scopes.contains(&Scope::TranscriptRead)
        && let Some(grant) = parent.plain.get(&Scope::TranscriptRead)
    {
        child.plain.insert(Scope::TranscriptRead, grant.clone());
    }
    if scopes.contains(&Scope::ToolRead) {
        child.tool_read = parent.tool_read.clone();
        let role = if matches!(name, "ls" | "find" | "grep") {
            name
        } else {
            "read"
        };
        if let Some(grant) = child.tool_read.as_mut() {
            grant.tools.retain(|held| held == role);
            if grant.tools.is_empty() {
                child.tool_read = None;
            }
        }
    }
    if scopes.contains(&Scope::ToolWrite) {
        child.tool_write = parent.tool_write.clone();
        if let Some(grant) = child.tool_write.as_mut() {
            grant.tools.retain(|held| held == name);
            if grant.tools.is_empty() {
                child.tool_write = None;
            }
        }
    }
    child
}

/// the `[standard-tools]` line the shipped defaults carry for one tool (D182), read out of the
/// defaults' own text so it matches what `saddle tools --config` prints. A refusal uses it to show
/// what to add rather than only which mechanism refused.
pub fn default_entry(name: &str) -> Option<&'static str> {
    DEFAULT_PROFILES.lines().find_map(|line| {
        let (key, _) = line.split_once('=')?;
        (key.trim() == name).then(|| line.trim())
    })
}

#[cfg(test)]
mod tests {
    use super::default_entry;

    #[test]
    fn a_default_entry_is_named_exactly_as_the_config_prints_it() {
        assert_eq!(default_entry("tasks"), Some(r#"tasks = ["exec"]"#));
        assert_eq!(default_entry("config_help"), Some("config_help = []"));
        assert_eq!(
            default_entry("bash"),
            Some(r#"bash = ["exec", "fs-read", "fs-write", "net"]"#)
        );
        assert_eq!(default_entry("nope"), None);
    }
}
