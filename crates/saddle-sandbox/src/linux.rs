use crate::launch;
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
    if let Some(guard) = &spec.named_launch {
        return launch::enter(spec, guard);
    }
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

pub(crate) fn start_network(spec: &SandboxSpec) -> bool {
    let Some(net) = &spec.net else {
        return false;
    };
    let resolvers = fs::read_to_string("/etc/resolv.conf")
        .map(|text| namespaces::resolvers(&text))
        .unwrap_or_default();
    if let Err(why) = namespaces::enter(&resolvers) {
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
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(ll)?
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
