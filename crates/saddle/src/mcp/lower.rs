//! D105, D107: one server's sandbox spec, lowered from the intersection of its grant with the
//! agent's, so a server reaches no more than the principal it acts for. Shared by `saddle mcp list`
//! and the session's registry, so there is one lowering and not two.

use anyhow::{Context, Result, anyhow};
use saddle_cap::narrow::narrow;
use saddle_cap::sandbox::{ExecRequest, SandboxSpec, lower_exec};
use saddle_cap::{Location, Principal, RelPath, RootName};

use crate::mcp::{McpEntry, Transport};
use crate::session::Session;

pub fn spec(
    session: &Session,
    grant: &saddle_cap::McpGrant,
    entry: &McpEntry,
    command: &str,
    args: &[String],
    host_env: &[(String, String)],
) -> Result<SandboxSpec> {
    let agent = session
        .engine
        .policy()
        .grants(&Principal::Agent)
        .context("the policy gives the agent no grants")?;
    let narrowed = narrow(agent, &grant.reach, grant.rule.as_str()).map_err(|e| anyhow!("{e}"))?;
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
    // The server's own variables are added after the allowlist: they are its configuration, and a
    // token for it has nowhere else to live (D107).
    if let Transport::Stdio { .. } = entry.transport()? {
        for (name, value) in &entry.env {
            lowered.spec.env.push((name.clone(), value.clone()));
        }
        lowered.spec.env.sort();
    }
    Ok(lowered.spec)
}
