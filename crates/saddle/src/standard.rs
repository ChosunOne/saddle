use anyhow::{Result, bail};
use saddle_cap::manifest::Manifest;
use saddle_cap::policy::PlainGrant;
use saddle_cap::{Grants, Scope};
use saddle_standard::artifact;
use std::collections::{BTreeMap, BTreeSet};

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
    if scopes.contains(&Scope::ModelInvoke) {
        child.model_invoke = parent.model_invoke.clone();
    }
    if scopes.contains(&Scope::Exec) {
        child.exec = parent.exec.clone();
    }
    if scopes.contains(&Scope::RunBinary) {
        child.run_binary = parent.run_binary.clone();
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

pub fn default_entry(name: &str) -> Option<&'static str> {
    DEFAULT_PROFILES.lines().find_map(|line| {
        let (key, _) = line.split_once('=')?;
        (key.trim() == name).then(|| line.trim())
    })
}

#[cfg(test)]
mod worker_delegation_tests {
    use super::*;
    use saddle_cap::Principal;
    use saddle_cap::Roots;
    use saddle_cap::policy::Policy;

    #[test]
    fn the_team_tools_can_invoke_models_only_when_the_agent_can() {
        let policy = Policy::parse(
            "version = 1\n[agent]\nmodel-invoke = { tokens-per-session = 100 }\n[plugin.standard-agent]\n[plugin.standard-swarm]\n",
            &Roots::default(),
        )
        .unwrap();
        let agent = policy.grants(&Principal::Agent).cloned().unwrap();
        let scopes: BTreeSet<Scope> = [Scope::ModelInvoke, Scope::TranscriptRead].into();
        for name in ["agent", "swarm"] {
            let child = delegate(&agent, name, &scopes);
            assert!(
                child
                    .model_invoke
                    .as_ref()
                    .is_some_and(|g| g.tokens_per_session == Some(100)),
                "{name} inherits the budget"
            );
        }
        let none = Policy::parse("version = 1\n[agent]\n", &Roots::default()).unwrap();
        let plain = none.grants(&Principal::Agent).cloned().unwrap();
        assert!(delegate(&plain, "agent", &scopes).model_invoke.is_none());
    }
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
