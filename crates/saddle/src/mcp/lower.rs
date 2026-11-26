use anyhow::{Context, Result, anyhow};
use saddle_cap::narrow::narrow;
use saddle_cap::policy::{Grants, McpGrant, NetGrant};
use saddle_cap::sandbox::{ExecRequest, SandboxSpec, lower_exec};
use saddle_cap::{Location, Principal, RelPath, RootName};

use crate::mcp::{McpEntry, Transport};
use crate::session::Session;

pub fn spec(
    session: &Session,
    grant: &McpGrant,
    entry: &McpEntry,
    command: &str,
    args: &[String],
    host_env: &[(String, String)],
) -> Result<SandboxSpec> {
    let narrowed = narrowed(session, grant)?;
    let mut argv = vec![command.to_owned()];
    argv.extend(args.iter().cloned());
    let cwd = Location::new(RootName::workspace(), RelPath::new("")?);
    let mut lowered = lower_exec(
        &narrowed,
        session.roots(),
        &session.setup.extras,
        ExecRequest {
            argv,
            cwd: &cwd,
            host_env,
        },
    )
    .map_err(|e| anyhow!("{e}"))?;
    if let Transport::Stdio { .. } = entry.transport()? {
        for (name, value) in &entry.env {
            lowered.spec.env.push((name.clone(), value.clone()));
        }
        lowered.spec.env.sort();
    }
    Ok(lowered.spec)
}

pub fn narrowed(session: &Session, grant: &McpGrant) -> Result<Grants> {
    let agent = session
        .engine
        .policy()
        .grants(&Principal::Agent)
        .context("the policy gives the agent no grants")?;
    narrow(agent, &grant.reach, grant.rule.as_str()).map_err(|e| anyhow!("{e}"))
}

pub fn net_reach(session: &Session, grant: &McpGrant) -> Result<Vec<NetGrant>> {
    Ok(narrowed(session, grant)?.net)
}
