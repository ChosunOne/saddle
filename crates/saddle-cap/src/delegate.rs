//! the initial plugin's operational reach is bounded by the agent (D126, D127, D150).

use crate::component::runtime_supports_scope;
use crate::narrow::covers;
use crate::policy::Sessions;
use crate::{Grants, RootName, Scope};

#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    #[error("scope `{0}` is not implemented by the plugin runtime")]
    Unsupported(Scope),
    #[error("plugin asks for scope `{0}`, which the agent does not hold")]
    Scope(Scope),
    #[error("plugin asks for tool-path role `{0}`, which the agent does not hold")]
    ToolRole(String),
    #[error("plugin asks for a filesystem root `{0}`, which the agent does not hold")]
    Root(RootName),
    #[error("could not combine grant exclusions: {0}")]
    Exclusions(String),
    #[error("plugin delegation refused: {0}")]
    Reach(String),
}

/// bounds an explicitly granted tool without inheriting omitted grants.
///
/// `tool` registers an export, not operational authority. Additional adapters must extend the interface
/// catalogue before their scopes can be linked; unsupported grants are never silently ignored.
pub fn read_tool(agent: &Grants, plugin: &Grants) -> Result<Grants, DelegationError> {
    for scope in plugin.scopes() {
        if !runtime_supports_scope(scope) {
            return Err(DelegationError::Unsupported(scope));
        }
        if matches!(
            scope,
            Scope::TranscriptRead
                | Scope::EventPublish
                | Scope::EventSubscribe
                | Scope::Veto
                | Scope::ContextContribute
                | Scope::Ui
                | Scope::Persist
        ) && !agent.scopes().contains(&scope)
        {
            return Err(DelegationError::Scope(scope));
        }
    }
    let mut out = Grants {
        plain: plugin.plain.clone(),
        tool_read: plugin.tool_read.clone(),
        tool_write: plugin.tool_write.clone(),
        ..Grants::default()
    };
    // D163: another conversation of this workspace is the agent's reach to lend, so a plugin reads
    // beyond its own only when the agent's grant does too.
    if let Some(want) = plugin.plain.get(&Scope::TranscriptRead) {
        let parent = agent
            .plain
            .get(&Scope::TranscriptRead)
            .ok_or(DelegationError::Scope(Scope::TranscriptRead))?;
        let mut grant = want.clone();
        grant.sessions = match (want.sessions, parent.sessions) {
            (Sessions::Workspace, Sessions::Workspace) => Sessions::Workspace,
            _ => Sessions::SelfOnly,
        };
        out.plain.insert(Scope::TranscriptRead, grant);
    }
    if let Some(want) = &plugin.model_invoke {
        let parent = agent
            .model_invoke
            .as_ref()
            .ok_or(DelegationError::Scope(Scope::ModelInvoke))?;
        let mut grant = want.clone();
        // A delegated grant is never wider than the one it comes from: no token budget in the
        // parent leaves the child's own choice, and a child that asks for none still inherits the
        // parent's cap rather than escaping it.
        grant.tokens_per_session = match (want.tokens_per_session, parent.tokens_per_session) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
        grant.cost_per_session = match (want.cost_per_session, parent.cost_per_session) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        out.model_invoke = Some(grant);
    }
    if let Some(want) = &plugin.exec {
        let parent = agent
            .exec
            .as_ref()
            .ok_or(DelegationError::Scope(Scope::Exec))?;
        if want.env.iter().any(|name| !parent.env.contains(name)) {
            return Err(DelegationError::Reach(
                "exec environment exceeds parent grant".into(),
            ));
        }
        let mut grant = want.clone();
        grant.timeout = want.timeout.min(parent.timeout);
        out.exec = Some(grant);
    }
    for want in &plugin.run_binary {
        if agent.launch_profile(&want.profile).is_none() {
            return Err(DelegationError::Reach(format!(
                "run-binary profile {:?} exceeds parent reach",
                want.profile
            )));
        }
        out.run_binary.push(want.clone());
    }
    for want in &plugin.net {
        if !agent.net.iter().any(|parent| covers(parent, want)) {
            return Err(DelegationError::Reach(format!(
                "net grant for {} exceeds parent reach",
                want.host
            )));
        }
        out.net.push(want.clone());
    }
    for (want, parent) in [
        (&plugin.tool_read, &agent.tool_read),
        (&plugin.tool_write, &agent.tool_write),
    ] {
        let Some(want) = want else { continue };
        for role in &want.tools {
            if !parent.as_ref().is_some_and(|g| g.tools.contains(role)) {
                return Err(DelegationError::ToolRole(role.clone()));
            }
        }
    }
    for (wants, parent, output) in [
        (&plugin.fs_read, &agent.fs_read, &mut out.fs_read),
        (&plugin.fs_write, &agent.fs_write, &mut out.fs_write),
    ] {
        for want in wants {
            let held: Vec<_> = parent.iter().filter(|g| g.root == want.root).collect();
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
                output.push(
                    want.with_exclusions(exclude)
                        .map_err(DelegationError::Exclusions)?,
                );
            }
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
    fn exec_and_network_delegation_never_inherit_or_exceed_parent_parameters() {
        let (agent, plugin) = grants(
            "[agent]\nexec={env=['ALLOWED'],timeout-secs=3}\nnet=[{host='*.example.com'}]\n[plugin.reader]\ntool={}\nexec={env=['ALLOWED'],timeout-secs=30}\nnet=[{host='api.example.com'}]\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert_eq!(bounded.exec.unwrap().timeout.as_secs(), 3);
        assert_eq!(bounded.net.len(), 1);
        for text in [
            "[agent]\nexec={env=[]}\n[plugin.reader]\nexec={env=['SECRET']}\n",
            "[agent]\nnet=[{host='api.example.com'}]\n[plugin.reader]\nnet=[{host='*'}]\n",
            "[plugin.sibling]\nexec={}\n[plugin.reader]\nexec={}\n",
        ] {
            let (agent, plugin) = grants(text);
            assert!(read_tool(&agent, &plugin).is_err());
        }
        let (agent, plugin) =
            grants("[agent]\nexec={}\nnet=[{host='*'}]\n[plugin.reader]\ntool={}\n");
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert!(bounded.exec.is_none() && bounded.net.is_empty());
    }

    #[test]
    fn extension_capabilities_are_supported_but_require_the_parent_grant() {
        for scope in [
            Scope::Persist,
            Scope::Veto,
            Scope::Ui,
            Scope::ModelInvoke,
            Scope::ContextContribute,
        ] {
            let value = if scope == Scope::ModelInvoke {
                "{tokens-per-session=100}"
            } else {
                "{}"
            };
            let (agent, plugin) = grants(&format!(
                "[agent]\n{scope}={value}\n[plugin.reader]\ntool={{}}\n{scope}={value}\n"
            ));
            assert!(
                read_tool(&agent, &plugin)
                    .unwrap()
                    .scopes()
                    .contains(&scope)
            );
            let (agent, plugin) = grants(&format!(
                "[plugin.sibling]\n{scope}={value}\n[plugin.reader]\ntool={{}}\n{scope}={value}\n"
            ));
            assert!(
                matches!(read_tool(&agent, &plugin), Err(DelegationError::Scope(refused)) if refused == scope)
            );
        }
        assert!(!runtime_supports_scope(Scope::Mcp));
    }

    #[test]
    fn writes_are_bounded_by_parent_write_grants_not_read_or_sibling_grants() {
        let (agent, plugin) = grants(
            "[agent]\nfs-write=[{root='workspace',exclude=['.env*']}]\n[plugin.reader]\ntool={}\nfs-write=[{root='workspace',exclude=['private']}]\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert!(bounded.fs_read.is_empty());
        for value in [".env", "private/key"] {
            assert!(bounded.fs_write[0].excludes(&RelPath::new(value).unwrap()));
        }
        assert!(!bounded.fs_write[0].excludes(&RelPath::new("file").unwrap()));
        let (agent, plugin) = grants(
            "[agent]\nfs-read=[{root='workspace'}]\n[plugin.sibling]\nfs-write=[{root='workspace'}]\n[plugin.reader]\ntool={}\nfs-write=[{root='workspace'}]\n",
        );
        assert!(matches!(
            read_tool(&agent, &plugin),
            Err(DelegationError::Root(_))
        ));
        let (agent, plugin) =
            grants("[agent]\nfs-write=[{root='workspace'}]\n[plugin.reader]\ntool={}\n");
        assert!(read_tool(&agent, &plugin).unwrap().fs_write.is_empty());
    }

    #[test]
    fn transcript_reach_is_explicit_and_cannot_come_from_a_sibling() {
        let (agent, plugin) = grants("[agent]\ntranscript-read={}\n[plugin.reader]\ntool={}\n");
        assert!(
            !read_tool(&agent, &plugin)
                .unwrap()
                .scopes()
                .contains(&Scope::TranscriptRead)
        );
        let (agent, plugin) =
            grants("[agent]\ntranscript-read={}\n[plugin.reader]\ntool={}\ntranscript-read={}\n");
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert!(bounded.scopes().contains(&Scope::TranscriptRead));
        assert!(bounded.fs_read.is_empty());
        let (agent, plugin) = grants(
            "[plugin.sibling]\ntranscript-read={}\n[plugin.reader]\ntool={}\ntranscript-read={}\n",
        );
        assert!(matches!(
            read_tool(&agent, &plugin),
            Err(DelegationError::Scope(Scope::TranscriptRead))
        ));
    }

    #[test]
    fn a_plugin_reads_other_conversations_only_when_the_agent_does() {
        // D163: the parent's own mode bounds what it can lend, and an omitted mode is the narrow
        // one for a plugin, so delegation cannot widen the reach by leaving it out.
        let (agent, plugin) = grants(
            "[agent]\ntranscript-read={sessions='workspace'}\n[plugin.reader]\ntool={}\ntranscript-read={sessions='workspace'}\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert_eq!(
            bounded.plain[&Scope::TranscriptRead].sessions,
            Sessions::Workspace
        );
        let (agent, plugin) = grants(
            "[agent]\ntranscript-read={sessions='self'}\n[plugin.reader]\ntool={}\ntranscript-read={sessions='workspace'}\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert_eq!(
            bounded.plain[&Scope::TranscriptRead].sessions,
            Sessions::SelfOnly,
            "the agent reads one conversation, so it cannot lend more"
        );
        let (agent, plugin) =
            grants("[agent]\ntranscript-read={}\n[plugin.reader]\ntool={}\ntranscript-read={}\n");
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert_eq!(
            bounded.plain[&Scope::TranscriptRead].sessions,
            Sessions::Workspace,
            "a bare grant is the workspace on both sides"
        );
    }

    #[test]
    fn event_channels_need_their_own_explicit_parent_and_plugin_grants() {
        for scope in [Scope::EventPublish, Scope::EventSubscribe] {
            let (agent, plugin) = grants(&format!(
                "[agent]\n{scope}={{}}\n[plugin.reader]\ntool={{}}\n"
            ));
            assert!(
                !read_tool(&agent, &plugin)
                    .unwrap()
                    .scopes()
                    .contains(&scope)
            );
            let (agent, plugin) = grants(&format!(
                "[plugin.sibling]\n{scope}={{}}\n[plugin.reader]\n{scope}={{}}\n"
            ));
            assert!(
                matches!(read_tool(&agent, &plugin), Err(DelegationError::Scope(missing)) if missing == scope)
            );
            let (agent, plugin) = grants(&format!(
                "[agent]\n{scope}={{}}\n[plugin.reader]\n{scope}={{}}\n"
            ));
            assert_eq!(
                read_tool(&agent, &plugin)
                    .unwrap()
                    .scopes()
                    .into_iter()
                    .collect::<Vec<_>>(),
                [scope]
            );
        }
    }

    #[test]
    fn tool_path_roles_are_explicit_and_bounded_without_inheriting_roots() {
        let (agent, plugin) = grants(
            "[agent]\ntool-read={tools=['read','ls']}\n[plugin.reader]\ntool={}\ntool-read={tools=['read']}\n",
        );
        let bounded = read_tool(&agent, &plugin).unwrap();
        assert!(bounded.fs_read.is_empty());
        assert_eq!(bounded.tool_read.unwrap().tools, ["read"]);
        let (agent, plugin) = grants(
            "[agent]\ntool-read={tools=['ls']}\n[plugin.reader]\ntool={}\ntool-read={tools=['read']}\n",
        );
        assert!(matches!(
            read_tool(&agent, &plugin),
            Err(DelegationError::ToolRole(_))
        ));
        let (agent, plugin) =
            grants("[agent]\ntool-read={tools=['read']}\n[plugin.reader]\ntool={}\n");
        assert!(read_tool(&agent, &plugin).unwrap().tool_read.is_none());
        for role in ["write", "*", "Read", "plugin__sibling__read"] {
            let dir = tempfile::tempdir().unwrap();
            let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
            assert!(
                Policy::parse(
                    &format!("version=1\n[plugin.reader]\ntool-read={{tools=['{role}']}}"),
                    &roots
                )
                .is_err()
            );
        }
    }
}
