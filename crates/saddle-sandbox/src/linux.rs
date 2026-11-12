use std::collections::BTreeMap;
use std::convert::Infallible;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;

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
            std::ptr::null::<libc::c_void>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    (abi > 0).then_some(abi as i32)
}

pub(crate) fn enter(spec: &SandboxSpec) -> Result<Infallible, SandboxError> {
    std::env::set_current_dir(&spec.cwd)?;
    let program = resolve_program(&spec.argv[0], &spec.env)?;
    apply_landlock(spec)?;
    apply_seccomp()?;
    let source = Command::new(&program)
        .arg0(&spec.argv[0])
        .args(&spec.argv[1..])
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .exec();
    Err(SandboxError::Exec { program, source })
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

fn apply_seccomp() -> Result<(), SandboxError> {
    let sc = |e: seccompiler::BackendError| SandboxError::Seccomp(e.to_string());
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // socket(domain, ...) for any domain that reaches outside the sandbox. socketpair stays open.
    let domains = [
        libc::AF_INET,
        libc::AF_INET6,
        libc::AF_UNIX,
        libc::AF_NETLINK,
        libc::AF_PACKET,
    ];
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
    for nr in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        rules.insert(nr, Vec::new());
    }

    let arch = TargetArch::try_from(std::env::consts::ARCH).map_err(sc)?;
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
