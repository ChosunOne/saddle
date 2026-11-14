use crate::namespaces;
use crate::relay;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::env;
use std::env::consts;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::ptr;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, Scope, path_beneath_rules,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use crate::{SandboxError, SandboxSpec, resolve_program};

const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;

pub(crate) fn landlock_abi() -> Option<i32> {
    // SAFETY: with a null attr, size 0 and the VERSION flag, the syscall only returns the ABI.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (abi > 0).then_some(abi as i32)
}

pub(crate) fn enter(spec: &SandboxSpec) -> Result<Infallible, SandboxError> {
    // The network comes first: it needs `/proc/self/uid_map` and the loopback interface, and
    // landlock refuses both once it is applied (D79).
    let networked = start_network(spec);
    env::set_current_dir(&spec.cwd)?;
    let program = resolve_program(&spec.argv[0], &spec.env)?;
    apply_landlock(spec)?;
    apply_seccomp(networked)?;
    let source = Command::new(&program)
        .arg0(&spec.argv[0])
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .exec();
    Err(SandboxError::Exec { program, source })
}

/// Gives the command its own network, or leaves it with none. True means sockets may be allowed
/// because everything they can reach is the command's own loopback (D79).
///
/// Every step can fail, and every failure means the answer is "no network": an environment without
/// user namespaces runs the command exactly as it ran before D79, with `socket(2)` refused. That is
/// why the answer is a `bool` rather than a result — the caller must not be able to treat a failure
/// as permission.
fn start_network(spec: &SandboxSpec) -> bool {
    let Some(net) = &spec.net else {
        return false;
    };
    // Read before landlock restricts /etc to reading, and before the command exists, so the
    // resolver list cannot be influenced by anything the command does.
    let resolvers = fs::read_to_string("/etc/resolv.conf")
        .map(|text| namespaces::resolvers(&text))
        .unwrap_or_default();
    if let Err(why) = namespaces::enter(&resolvers) {
        // Said out loud rather than swallowed: the command still runs, but with no network, and a
        // person needs to know which of the two they got and why.
        eprintln!("saddle-sandbox: no network for this command: {why}");
        return false;
    }
    if let Err(why) = relay::spawn(net, &resolvers) {
        eprintln!("saddle-sandbox: no network for this command: {why}");
        return false;
    }
    true
}

fn existing(paths: &[PathBuf]) -> impl Iterator<Item = &PathBuf> {
    paths.iter().filter(|p| p.exists())
}

fn apply_landlock(spec: &SandboxSpec) -> Result<(), SandboxError> {
    let ll = |e: landlock::RulesetError| SandboxError::Landlock(e.to_string());
    let abi = ABI::V3;
    let status = Ruleset::default()
        // The filesystem rules are the sandbox; refuse to run without all of them.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(ll)?
        // Signal scoping needs ABI 6 (Linux 6.12); use it where the kernel has it.
        .set_compatibility(CompatLevel::BestEffort)
        .scope(Scope::Signal)
        .map_err(ll)?
        .set_compatibility(CompatLevel::HardRequirement)
        .create()
        .map_err(ll)?
        .add_rules(path_beneath_rules(
            existing(&spec.read),
            AccessFs::from_read(abi),
        ))
        .map_err(ll)?
        .add_rules(path_beneath_rules(
            existing(&spec.write),
            AccessFs::from_all(abi),
        ))
        .map_err(ll)?
        .restrict_self()
        .map_err(ll)?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(SandboxError::Landlock("ruleset not enforced".into()));
    }
    if !status.no_new_privs {
        return Err(SandboxError::Landlock("no_new_privs not set".into()));
    }
    Ok(())
}

fn apply_seccomp(networked: bool) -> Result<(), SandboxError> {
    let sc = |e: seccompiler::BackendError| SandboxError::Seccomp(e.to_string());
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // socket(domain, ...) for any domain that reaches outside the sandbox. socketpair stays open.
    //
    // With a network namespace of its own (D79) the internet domains are no longer a way out —
    // there is nothing on the other side of them but the command's own loopback — so they are
    // allowed. Unix sockets stay refused: they are the way to `docker.sock`, `gpg-agent` and every
    // other local daemon, which a network namespace does not confine. Netlink stays allowed too,
    // because glibc uses it to ask about interfaces, and inside this namespace there is nothing to
    // learn but the loopback that was just raised.
    let domains: &[libc::c_int] = if networked {
        &[libc::AF_UNIX, libc::AF_PACKET]
    } else {
        &[
            libc::AF_INET,
            libc::AF_INET6,
            libc::AF_UNIX,
            libc::AF_NETLINK,
            libc::AF_PACKET,
        ]
    };
    let socket_rules = domains
        .iter()
        .map(|&domain| {
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                domain as u64,
            )?])
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(sc)?;
    rules.insert(libc::SYS_socket, socket_rules);

    // io_uring can open sockets without calling socket(2). An empty rule list matches always.
    // So can setns(2), which would let a command walk back out into another namespace.
    for nr in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_setns,
    ] {
        rules.insert(nr, Vec::new());
    }

    let arch = TargetArch::try_from(consts::ARCH).map_err(sc)?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(sc)?;
    let program: BpfProgram = filter.try_into().map_err(sc)?;
    seccompiler::apply_filter(&program).map_err(|e| SandboxError::Seccomp(e.to_string()))
}
