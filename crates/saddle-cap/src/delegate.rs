//! the initial plugin's operational reach is bounded by the agent (D126, D127).

use crate::{Grants, RootName, Scope};

#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    #[error("scope `{0}` is not implemented by the read-only plugin runtime")]
    Unsupported(Scope),
    #[error("plugin asks to read root `{0}`, which the agent does not hold")]
    Root(RootName),
    #[error("could not combine grant exclusions: {0}")]
    Exclusions(String),
}

/// bounds an explicitly granted read-only tool without inheriting omitted grants.
///
/// `tool` registers an export, not operational authority. Additional adapters must extend this
/// check before their scopes can be linked; unsupported grants are never silently ignored.
pub fn read_tool(agent: &Grants, plugin: &Grants) -> Result<Grants, DelegationError> {
    for scope in plugin.scopes() {
        if !matches!(scope, Scope::FsRead | Scope::Tool) {
            return Err(DelegationError::Unsupported(scope));
        }
    }
    let mut out = Grants {
        plain: plugin.plain.clone(),
        ..Grants::default()
    };
    for want in &plugin.fs_read {
        let held: Vec<_> = agent
            .fs_read
            .iter()
            .filter(|g| g.root == want.root)
            .collect();
        if held.is_empty() {
            return Err(DelegationError::Root(want.root.clone()));
        }
        // Grants are unions. Pairwise intersections preserve an agent's multiple entries for a
        // root rather than accidentally making one entry's exclusions bind an unrelated entry.
        for grant in held {
            let mut exclude = want.exclude.clone();
            for pattern in &grant.exclude {
                if !exclude.contains(pattern) {
                    exclude.push(pattern.clone());
                }
            }
            out.fs_read.push(
                want.with_exclusions(exclude)
                    .map_err(DelegationError::Exclusions)?,
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PluginId, Policy, Principal, RelPath, Roots};

    fn grants(text: &str) -> (Grants, Grants) {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse(&format!("version = 1\n{text}"), &roots).unwrap();
        let agent = policy
            .grants(&Principal::Agent)
            .cloned()
            .unwrap_or_default();
        let plugin = policy
            .grants(&Principal::Plugin(PluginId::new("reader").unwrap()))
            .cloned()
            .unwrap();
        (agent, plugin)
    }

    #[test]
    fn an_omitted_grant_does_not_inherit_the_agents_files() {
        let (agent, plugin) =
            grants("[agent]\nfs-read = [{root='workspace'}]\n[plugin.reader]\ntool={}\n");
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert!(bounded.fs_read.is_empty());
        assert_eq!(
            bounded.scopes().into_iter().collect::<Vec<_>>(),
            [Scope::Tool]
        );
    }

    #[test]
    fn a_sibling_cannot_supply_the_missing_agents_grant() {
        let (agent, plugin) = grants(
            "[plugin.sibling]\nfs-read=[{root='workspace'}]\n[plugin.reader]\nfs-read=[{root='workspace'}]\n",
        );
        assert!(matches!(
            read_tool(&agent, &plugin),
            Err(DelegationError::Root(_))
        ));
    }

    #[test]
    fn both_exclusions_are_enforced_by_the_compiled_matcher() {
        let (agent, plugin) = grants(
            "[agent]\nfs-read=[{root='workspace',exclude=['.env*']}]\n[plugin.reader]\nfs-read=[{root='workspace',exclude=['private']}]\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        let g = &bounded.fs_read[0];
        for path in [".env", "nested/.env.local", "private/key"] {
            assert!(g.excludes(&RelPath::new(path).unwrap()), "{path}");
        }
        assert!(!g.excludes(&RelPath::new("src/main.rs").unwrap()));
        assert_eq!(g.rule.as_str(), "plugin.reader.fs-read[0]");
    }

    #[test]
    fn multiple_agent_entries_keep_their_union_semantics() {
        let (agent, plugin) = grants(
            "[agent]\nfs-read=[{root='workspace',exclude=['a']},{root='workspace',exclude=['b']}]\n[plugin.reader]\nfs-read=[{root='workspace',exclude=['private']}]\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        for path in ["a", "b"] {
            let rel = RelPath::new(path).unwrap();
            assert!(bounded.fs_read.iter().any(|g| !g.excludes(&rel)));
        }
        assert!(
            bounded
                .fs_read
                .iter()
                .all(|g| g.excludes(&RelPath::new("private/key").unwrap()))
        );
    }

    #[test]
    fn an_unimplemented_scope_is_refused_instead_of_inherited_or_ignored() {
        let (agent, plugin) = grants("[agent]\nexec={}\n[plugin.reader]\nexec={}\n");
        assert!(matches!(
            read_tool(&agent, &plugin),
            Err(DelegationError::Unsupported(Scope::Exec))
        ));
    }
}
