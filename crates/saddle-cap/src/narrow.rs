//! D105: an MCP server is a delegate of the agent, so its reach is the intersection of what its
//! grant asks for with what the agent holds. It is never a widening, and a grant asking for more
//! than the agent has is an error rather than a silent trim: a narrowing that quietly halved a
//! server's reach would look like a server that mysteriously cannot see a file, which is a much
//! worse thing to debug than a policy that will not load.

use std::collections::BTreeSet;

use crate::policy::{FsGrant, Grants, HostPattern, McpReach, NetGrant, RuleId};
use crate::roots::RootName;

/// The grants one server runs under: the agent's own, narrowed by what its grant asks for.
///
/// `what` names the grant in the error, so a policy with several servers says which one asked for
/// too much.
pub fn narrow(agent: &Grants, reach: &McpReach, what: &str) -> Result<Grants, String> {
    let mut out = Grants {
        // A server is a command: it needs the agent's `exec`, and its timeout, to run at all.
        exec: agent.exec.clone(),
        ..Grants::default()
    };
    out.fs_read = narrow_fs(&agent.fs_read, &reach.fs_read, what, "read")?;
    out.fs_write = narrow_fs(&agent.fs_write, &reach.fs_write, what, "write")?;
    out.net = narrow_net(&agent.net, &reach.net, what)?;
    // Nothing else crosses: a server is not a plugin principal, so it holds no plain scope, no
    // model budget and no roots of its own.
    Ok(out)
}

/// Filesystem entries: one per root the server asked for, with the exclusions of both grants, so
/// the result is never wider than either.
fn narrow_fs(
    agent: &[FsGrant],
    asked: &[FsGrant],
    what: &str,
    verb: &str,
) -> Result<Vec<FsGrant>, String> {
    if asked.is_empty() {
        return Ok(agent.to_vec());
    }
    let mut out = Vec::new();
    for want in asked {
        let held: Vec<&FsGrant> = agent.iter().filter(|g| g.root == want.root).collect();
        if held.is_empty() {
            return Err(format!(
                "{what} asks to {verb} root `{}`, which the agent does not hold",
                want.root
            ));
        }
        let mut exclude: Vec<String> = want.exclude.clone();
        for grant in held {
            for pattern in &grant.exclude {
                if !exclude.iter().any(|e| e == pattern) {
                    exclude.push(pattern.clone());
                }
            }
        }
        let mut narrowed = want.clone();
        narrowed.exclude = exclude;
        // The rule names the grant that asked, so a denial in the audit points at a line in the
        // policy rather than at the agent's own grant.
        narrowed.rule = want.rule.clone();
        out.push(narrowed);
    }
    Ok(out)
}

/// Network entries: each must be covered by an agent grant. An exact host is kept as it was
/// written; a wildcard is kept only where the agent already holds one at least as wide.
fn narrow_net(agent: &[NetGrant], asked: &[NetGrant], what: &str) -> Result<Vec<NetGrant>, String> {
    if asked.is_empty() {
        return Ok(agent.to_vec());
    }
    let mut out = Vec::new();
    for want in asked {
        let covered = agent.iter().find(|g| covers(g, want));
        if covered.is_none() {
            return Err(format!(
                "{what} asks to reach {}, which the agent's net grants do not cover",
                describe_net(want)
            ));
        }
        out.push(want.clone());
    }
    Ok(out)
}

/// Whether `outer` allows everything `inner` does, on the same scheme and port.
fn covers(outer: &NetGrant, inner: &NetGrant) -> bool {
    outer.scheme == inner.scheme
        && outer.port.unwrap_or(outer.scheme.default_port())
            == inner.port.unwrap_or(inner.scheme.default_port())
        && covers_host(&outer.host, &inner.host)
}

fn covers_host(outer: &HostPattern, inner: &HostPattern) -> bool {
    match (outer, inner) {
        (HostPattern::Any, _) => true,
        (HostPattern::Exact(a), HostPattern::Exact(b)) => a.eq_ignore_ascii_case(b),
        (HostPattern::Exact(_), _) => false,
        // A suffix covers itself and anything below it: `*.example.com` covers
        // `*.api.example.com` and `api.example.com`.
        (HostPattern::Suffix(suffix), HostPattern::Suffix(inner)) => {
            suffix.eq_ignore_ascii_case(inner) || inner.ends_with(&format!(".{suffix}"))
        }
        // A suffix covers strict subdomains only, as `matches` does: `*.example.com` says nothing
        // about `example.com` itself.
        (HostPattern::Suffix(suffix), HostPattern::Exact(host)) => {
            host.ends_with(&format!(".{suffix}"))
        }
        // A suffix never covers a wildcard: `*.example.com` says nothing about every host.
        (HostPattern::Suffix(_), HostPattern::Any) => false,
    }
}

fn describe_net(grant: &NetGrant) -> String {
    format!(
        "{}://{}:{}",
        grant.scheme.as_str(),
        grant.host,
        grant.port.unwrap_or(grant.scheme.default_port())
    )
}

/// The roots a narrowed grant reaches, for a caller that wants to say so.
pub fn roots_of(grants: &Grants) -> BTreeSet<RootName> {
    grants
        .fs_read
        .iter()
        .chain(&grants.fs_write)
        .map(|g| g.root.clone())
        .collect()
}

/// The rules a narrowed grant was resolved from, so `check` and the audit can name them.
pub fn rules_of(grants: &Grants) -> Vec<&RuleId> {
    let files = grants
        .fs_read
        .iter()
        .chain(&grants.fs_write)
        .map(|g| &g.rule);
    let hosts = grants.net.iter().map(|g| &g.rule);
    files.chain(hosts).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use crate::roots::Roots;
    use std::fs;
    use std::path::Path;

    fn roots(dir: &Path) -> Roots {
        fs::create_dir_all(dir.join("tmp")).unwrap();
        Roots::resolve(
            [
                (RootName::workspace(), dir.to_path_buf()),
                (RootName::new("tmp").unwrap(), dir.join("tmp")),
            ],
            &[],
        )
        .unwrap()
    }

    fn parsed(text: &str, roots: &Roots) -> Policy {
        Policy::parse(text, roots).unwrap()
    }

    fn agent_grants(policy: &Policy) -> &Grants {
        policy.grants(&crate::Principal::Agent).unwrap()
    }

    const BASE: &str = r#"
version = 1

[agent]
fs-read  = [{ root = "workspace", exclude = [".env*"] }]
fs-write = [{ root = "workspace" }]
exec     = { timeout-secs = 30 }
net      = [{ host = "*.example.com" }, { host = "api.other.test" }]
"#;

    #[test]
    fn a_server_that_asks_for_nothing_inherits_the_agents_reach() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let policy = parsed(
            &format!("{BASE}\nmcp = [{{ server = \"scout\", tools = [\"search\"] }}]\n"),
            &roots,
        );
        let agent = agent_grants(&policy);
        let grant = &agent.mcp[0];
        let narrowed = narrow(agent, &grant.reach, grant.rule.as_str()).unwrap();
        assert_eq!(narrowed.fs_read.len(), agent.fs_read.len());
        assert_eq!(narrowed.fs_write.len(), agent.fs_write.len());
        assert_eq!(narrowed.net.len(), agent.net.len());
        assert!(narrowed.exec.is_some());
        assert!(narrowed.plain.is_empty(), "a server holds no plain scope");
        assert!(narrowed.model_invoke.is_none());
    }

    #[test]
    fn a_server_gets_only_the_root_it_asks_for_with_both_grants_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let policy = parsed(
            &format!(
                "{BASE}\nmcp = [{{ server = \"scout\", tools = [\"search\"], \
                 fs-read = [{{ root = \"workspace\", exclude = [\"secrets\"] }}] }}]\n"
            ),
            &roots,
        );
        let agent = agent_grants(&policy);
        let grant = &agent.mcp[0];
        let narrowed = narrow(agent, &grant.reach, grant.rule.as_str()).unwrap();
        assert_eq!(narrowed.fs_read.len(), 1);
        assert_eq!(narrowed.fs_read[0].root.as_str(), "workspace");
        let mut exclude = narrowed.fs_read[0].exclude.clone();
        exclude.sort();
        assert_eq!(
            exclude,
            [".env*", "secrets"],
            "both grants' exclusions hold"
        );
        assert_eq!(
            narrowed.fs_read[0].rule.as_str(),
            "agent.mcp[0].fs-read[0]",
            "the audit names the line the server's grant was written on"
        );
        // It asked for nothing writable, so it inherits the agent's writes.
        assert_eq!(narrowed.fs_write.len(), agent.fs_write.len());
    }

    #[test]
    fn a_server_cannot_ask_for_more_than_the_agent_holds() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        for (body, message) in [
            (
                r#"fs-read = [{ root = "tmp" }]"#,
                "which the agent does not hold",
            ),
            (r#"net = [{ host = "api.nowhere.test" }]"#, "do not cover"),
            (r#"net = [{ host = "example.com" }]"#, "do not cover"),
            (
                r#"net = [{ host = "*.example.com", scheme = "http" }]"#,
                "do not cover",
            ),
        ] {
            let text =
                format!("{BASE}\nmcp = [{{ server = \"scout\", tools = [\"search\"], {body} }}]\n");
            let policy = parsed(&text, &roots);
            let agent = agent_grants(&policy);
            let grant = agent.mcp[0].clone();
            let err = narrow(agent, &grant.reach, grant.rule.as_str()).unwrap_err();
            assert!(err.contains(message), "{body}: {err}");
            assert!(err.contains("agent.mcp[0]"), "{body}: {err}");
        }
    }

    #[test]
    fn a_suffix_grant_covers_what_lies_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let roots = roots(dir.path());
        let policy = parsed(
            &format!(
                "{BASE}\nmcp = [{{ server = \"scout\", tools = [\"s\"], \
                 net = [{{ host = \"api.example.com\" }}, {{ host = \"*.api.example.com\" }}] }}]\n"
            ),
            &roots,
        );
        let agent = agent_grants(&policy);
        let grant = &agent.mcp[0];
        let narrowed = narrow(agent, &grant.reach, grant.rule.as_str()).unwrap();
        assert_eq!(narrowed.net.len(), 2);
        // A host just outside the suffix is refused, which no wildcard may paper over.
        let text = format!(
            "{BASE}\nmcp = [{{ server = \"s\", tools = [\"t\"], net = [{{ host = \"badexample.com\" }}] }}]\n"
        );
        let policy = parsed(&text, &roots);
        let agent = agent_grants(&policy);
        assert!(narrow(agent, &agent.mcp[0].reach, "agent.mcp[0]").is_err());
    }
}
