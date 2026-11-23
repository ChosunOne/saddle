//! named launch confinement and pinned executables (D184, D186, D188, D189, D191, D193).

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::env::{self, consts};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::ptr;
use std::sync::mpsc;
use std::thread;

#[cfg(target_arch = "x86_64")]
use seccompiler::sock_filter as SockFilter;

use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope,
};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule, TargetArch,
};

use saddle_cap::sandbox::NamedLaunch;
use sha2::{Digest, Sha256};

use crate::linux::start_network;
use crate::{SandboxError, SandboxSpec, launch_availability};

/// system runtime paths; no ordinary command includes, checkout or general home access.
const LIB_DIRS: &[&str] = &[
    "/lib",
    "/lib32",
    "/lib64",
    "/usr/lib",
    "/usr/lib32",
    "/usr/lib64",
];
const RUNTIME_READ: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ssl/certs",
    "/etc/pki/tls/certs",
    "/etc/pki/ca-trust",
    "/usr/share/ca-certificates",
    "/usr/share/crypto-policies",
    "/dev/null",
    "/dev/urandom",
];

pub(crate) fn enter(spec: &SandboxSpec, guard: &NamedLaunch) -> Result<Infallible, SandboxError> {
    launch_availability().map_err(SandboxError::Unavailable)?;
    let path = Path::new(&spec.argv[0]);
    let executable = verified_executable(path, guard.executable_sha256.as_ref(), guard)?;
    let argv = strings(spec.argv.iter().map(String::as_str))?;
    let environment: Vec<_> = spec.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let envp = strings(environment.iter().map(String::as_str))?;
    let argv = pointers(&argv);
    let envp = pointers(&envp);

    // The fresh helper has consumed its spec and replaced stdin. Do not pass any other inherited
    // handle to the relay or executable. Do this before making sockets or starting threads.
    // SAFETY: close_range takes descriptor numbers and flags; preserve the pinned executable.
    let fd = executable.as_raw_fd();
    for (first, last) in [(3, fd as u32 - 1), (fd as u32 + 1, u32::MAX)] {
        if first <= last && unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
    }
    env::set_current_dir(&spec.cwd)?;
    let networked = start_network(spec);
    let helpers = guard
        .helpers
        .iter()
        .map(|(path, expected)| verified_executable(path, expected.as_ref(), guard))
        .collect::<Result<Vec<_>, _>>()?;
    confine_files(spec, guard, &executable, &helpers)?;
    confine_syscalls(networked)?;
    if !helpers.is_empty() {
        seal_filters()?;
        return exec(path, &executable, &argv, &envp);
    }
    let (send, receive) = mpsc::sync_channel::<File>(0);
    // Only trusted helper code is running. The monitor is started after Landlock, before the
    // exec-notification filter. It authorizes one syscall and drops the CLOEXEC listener. The
    // successful exec destroys this thread; all later exec attempts inherit the filter but have
    // no listener, and the kernel denies them. There is no monitor serving untrusted requests.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
    thread::Builder::new()
        .name("launch-once".into())
        .spawn(move || {
            if let Ok(listener) = receive.recv()
                && let Err(error) = allow_initial_exec(&listener, tid)
            {
                eprintln!("saddle-sandbox: cannot release initial exec: {error}");
            }
        })?;
    let listener = exec_listener()?;
    // A later filter with its own listener could supersede USER_NOTIF. Seal both Linux filter
    // installation paths before any untrusted code runs.
    seal_filters()?;
    send.send(listener)
        .map_err(|_| SandboxError::Spec("launch monitor stopped".into()))?;
    exec(path, &executable, &argv, &envp)
}

fn exec(
    path: &Path,
    executable: &File,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> Result<Infallible, SandboxError> {
    // SAFETY: the pinned ELF fd stays owned and argv/envp consist of live NUL-terminated strings
    // and a terminating null pointer. AT_EMPTY_PATH executes the opened inode, not a reopened path.
    unsafe {
        libc::syscall(
            libc::SYS_execveat,
            executable.as_raw_fd(),
            c"".as_ptr(),
            argv.as_ptr(),
            envp.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    Err(SandboxError::Exec {
        program: path.to_path_buf(),
        source: io::Error::last_os_error(),
    })
}

fn strings<'a>(items: impl Iterator<Item = &'a str>) -> Result<Vec<CString>, SandboxError> {
    items
        .map(|s| {
            CString::new(s)
                .map_err(|_| SandboxError::Spec("NUL in launch arguments or environment".into()))
        })
        .collect()
}

fn pointers(items: &[CString]) -> Vec<*const libc::c_char> {
    items
        .iter()
        .map(|s| s.as_ptr())
        .chain([ptr::null()])
        .collect()
}

fn verified_executable(
    path: &Path,
    expected: Option<&[u8; 32]>,
    guard: &NamedLaunch,
) -> Result<File, SandboxError> {
    reject_overlap(path, &guard.protected)?;
    reject_overlap(path, &guard.agent_write)?;
    let mut executable = open_pinned(path, true)?;
    if let Some(expected) = expected {
        if hash_elf(path, &mut executable)? != *expected {
            return Err(SandboxError::Spec(format!(
                "launch binary SHA-256 does not match the session baseline: {} (restart Saddle after verifying the update)",
                path.display()
            )));
        }
    } else {
        check_elf(path, &mut executable)?;
    }
    Ok(executable)
}

pub(crate) fn digest(path: &Path) -> Result<[u8; 32], SandboxError> {
    hash_elf(path, &mut open_pinned(path, true)?)
}

fn check_elf(path: &Path, executable: &mut File) -> Result<[u8; 4], SandboxError> {
    let mut magic = [0; 4];
    executable.read_exact(&mut magic)?;
    if &magic != b"\x7fELF" {
        return Err(SandboxError::Spec(format!(
            "launch executable must be a native ELF binary, not a script: {}",
            path.display()
        )));
    }
    Ok(magic)
}

fn hash_elf(path: &Path, executable: &mut File) -> Result<[u8; 32], SandboxError> {
    let before = executable.metadata()?;
    let magic = check_elf(path, executable)?;
    let mut digest = Sha256::new();
    digest.update(magic);
    let remaining = before.len().saturating_sub(magic.len() as u64);
    let copied = io::copy(&mut executable.take(remaining), &mut digest)?;
    let after = executable.metadata()?;
    if before.len() < magic.len() as u64
        || copied != remaining
        || (before.len(), before.ctime(), before.ctime_nsec())
            != (after.len(), after.ctime(), after.ctime_nsec())
    {
        return Err(SandboxError::Spec(format!(
            "launch binary changed while hashing: {}",
            path.display()
        )));
    }
    Ok(digest.finalize().into())
}

/// Open an absolute path without following any links, using pinned parents. An explicit
/// executable must be a regular file; its trust comes from the protected launch profile.
fn open_pinned(path: &Path, executable: bool) -> Result<File, SandboxError> {
    let mut parts = path.components().peekable();
    if parts.next() != Some(Component::RootDir) || parts.peek().is_none() {
        return Err(SandboxError::Spec(format!(
            "invalid launch path: {}",
            path.display()
        )));
    }
    let mut file = open_at(libc::AT_FDCWD, c"/", libc::O_PATH | libc::O_DIRECTORY)?;
    while let Some(Component::Normal(part)) = parts.next() {
        let part = CString::new(part.as_bytes())
            .map_err(|_| SandboxError::Spec("NUL in launch path".into()))?;
        let flags = if parts.peek().is_some() {
            libc::O_PATH | libc::O_DIRECTORY
        } else if executable {
            libc::O_RDONLY | libc::O_NONBLOCK
        } else {
            libc::O_PATH
        };
        file = open_at(file.as_raw_fd(), &part, flags)?;
        let typ = file.metadata()?.file_type();
        if typ.is_symlink() || (parts.peek().is_none() && executable && !typ.is_file()) {
            return Err(SandboxError::Spec(format!(
                "invalid launch path (expected no symlinks and a regular executable file): {}",
                path.display()
            )));
        }
        if parts.peek().is_none() {
            return Ok(file);
        }
    }
    Err(SandboxError::Spec(format!(
        "invalid launch path: {}",
        path.display()
    )))
}

fn open_at(dir: libc::c_int, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    // SAFETY: name is live and NUL-terminated, no creation mode is needed, and the fd is owned.
    let fd = unsafe {
        libc::openat(
            dir,
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn system_owned(file: &File, owner: u32) -> io::Result<bool> {
    let metadata = file.metadata()?;
    Ok(metadata.uid() == owner && metadata.mode() & 0o022 == 0)
}

fn confine_files(
    spec: &SandboxSpec,
    guard: &NamedLaunch,
    executable: &File,
    helpers: &[File],
) -> Result<(), SandboxError> {
    let ll = |error: landlock::RulesetError| SandboxError::Landlock(error.to_string());
    let abi = ABI::V6;
    let mut rules = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(abi))
        .map_err(ll)?
        .scope(Scope::Signal | Scope::AbstractUnixSocket)
        .map_err(ll)?
        .create()
        .map_err(ll)?
        .add_rule(PathBeneath::new(
            executable.try_clone()?,
            AccessFs::ReadFile | AccessFs::Execute,
        ))
        .map_err(ll)?;
    for helper in helpers {
        rules = rules
            .add_rule(PathBeneath::new(
                helper.try_clone()?,
                AccessFs::ReadFile | AccessFs::Execute,
            ))
            .map_err(ll)?;
    }
    let owner = fs::metadata("/")?.uid();
    for (written, access) in LIB_DIRS
        .iter()
        .map(|p| (p, AccessFs::from_read(abi)))
        .chain(
            RUNTIME_READ
                .iter()
                .map(|p| (p, AccessFs::ReadFile | AccessFs::ReadDir)),
        )
    {
        let path = match fs::canonicalize(written) {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        reject_overlap(&path, &guard.protected)?;
        reject_overlap(&path, &guard.agent_write)?;
        let file = open_pinned(&path, false)?;
        let device = matches!(*written, "/dev/null" | "/dev/urandom");
        let metadata = file.metadata()?;
        if if device {
            !metadata.file_type().is_char_device()
        } else {
            !system_owned(&file, owner)?
        } {
            return Err(SandboxError::Spec(format!(
                "untrusted system runtime path: {}",
                path.display()
            )));
        }
        let mut access = if metadata.is_dir() {
            access
        } else {
            access & !AccessFs::ReadDir
        };
        if *written == "/dev/null" {
            // Git opens this sink O_RDWR to fill any missing standard descriptors (D191).
            // Check the pinned device identity before granting write, not just the pathname.
            if metadata.rdev() != libc::makedev(1, 3) {
                return Err(SandboxError::Spec("unexpected /dev/null device".into()));
            }
            access |= AccessFs::WriteFile;
        }
        rules = rules.add_rule(PathBeneath::new(file, access)).map_err(ll)?;
    }
    for path in &spec.read {
        reject_overlap(path, &guard.protected)?;
        let file = open_pinned(path, false).map_err(|error| {
            SandboxError::Spec(format!(
                "opening launch read path {}: {error}",
                path.display()
            ))
        })?;
        let metadata = file.metadata()?;
        let access = if metadata.is_dir() {
            AccessFs::ReadFile | AccessFs::ReadDir
        } else if metadata.is_file() {
            AccessFs::ReadFile.into()
        } else {
            return Err(SandboxError::Spec(format!(
                "launch read path must be a regular file or directory: {}",
                path.display()
            )));
        };
        rules = rules.add_rule(PathBeneath::new(file, access)).map_err(ll)?;
    }
    for path in &spec.write {
        reject_overlap(path, &guard.protected)?;
        let dir = open_pinned(path, false).map_err(|error| {
            SandboxError::Spec(format!(
                "opening launch write path {}: {error}",
                path.display()
            ))
        })?;
        if !dir.metadata()?.is_dir() {
            return Err(SandboxError::Spec(format!(
                "launch write path must be a directory: {}",
                path.display()
            )));
        }
        // Everything but Execute: a writable tree must never become a place to run a new
        // program, which is why approved executables are checked against this reach (D188).
        let write = AccessFs::from_all(abi) & !AccessFs::Execute;
        rules = rules.add_rule(PathBeneath::new(dir, write)).map_err(ll)?;
    }
    let status = rules.restrict_self().map_err(ll)?;
    if status.ruleset != RulesetStatus::FullyEnforced || !status.no_new_privs {
        return Err(SandboxError::Landlock(
            "named launch rules were not fully enforced".into(),
        ));
    }
    Ok(())
}

fn reject_overlap(path: &Path, forbidden: &[PathBuf]) -> Result<(), SandboxError> {
    if forbidden
        .iter()
        .any(|claim| path.starts_with(claim) || claim.starts_with(path))
    {
        return Err(SandboxError::Spec(format!(
            "launch path overlaps protected or agent-writable paths: {}",
            path.display()
        )));
    }
    Ok(())
}

fn confine_syscalls(networked: bool) -> Result<(), SandboxError> {
    let sc = |error: seccompiler::BackendError| SandboxError::Seccomp(error.to_string());
    let mut rules = BTreeMap::new();
    let socket = if networked {
        vec![
            SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_INET as u64,
                )
                .map_err(sc)?,
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_INET6 as u64,
                )
                .map_err(sc)?,
            ])
            .map_err(sc)?,
        ]
    } else {
        Vec::new()
    };
    rules.insert(libc::SYS_socket, socket);
    rules.insert(
        libc::SYS_socketpair,
        vec![
            SeccompRule::new(vec![
                SeccompCondition::new(
                    0,
                    SeccompCmpArgLen::Dword,
                    SeccompCmpOp::Ne,
                    libc::AF_UNIX as u64,
                )
                .map_err(sc)?,
            ])
            .map_err(sc)?,
        ],
    );
    for syscall in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_setns,
        libc::SYS_unshare,
        libc::SYS_setsid,
        libc::SYS_setpgid,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_ptrace,
        libc::SYS_pidfd_getfd,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        rules.insert(syscall, Vec::new());
    }
    let arch = TargetArch::try_from(consts::ARCH).map_err(sc)?;
    let program: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(sc)?
    .try_into()
    .map_err(sc)?;
    // The x32 ABI uses the same audit architecture but aliases native syscall numbers. Reject it
    // in both launch modes so it cannot evade restrictions such as socket and process-group rules.
    #[cfg(target_arch = "x86_64")]
    let program = {
        let mut program = program;
        program.splice(
            0..0,
            [
                SockFilter {
                    code: 0x20,
                    jt: 0,
                    jf: 0,
                    k: 0,
                }, // load seccomp_data.nr
                SockFilter {
                    code: 0x35,
                    jt: 0,
                    jf: 1,
                    k: 0x4000_0000,
                }, // nr >= x32 bit
                SockFilter {
                    code: 0x06,
                    jt: 0,
                    jf: 0,
                    k: libc::SECCOMP_RET_KILL_PROCESS,
                },
            ],
        );
        program
    };
    seccompiler::apply_filter(&program).map_err(|error| SandboxError::Seccomp(error.to_string()))
}

fn seal_filters() -> Result<(), SandboxError> {
    let sc = |error: seccompiler::BackendError| SandboxError::Seccomp(error.to_string());
    let set_filter = SeccompRule::new(vec![
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::PR_SET_SECCOMP as u64,
        )
        .map_err(sc)?,
    ])
    .map_err(sc)?;
    let rules = BTreeMap::from([
        (libc::SYS_seccomp, Vec::new()),
        (libc::SYS_prctl, vec![set_filter]),
    ]);
    let arch = TargetArch::try_from(consts::ARCH).map_err(sc)?;
    let program: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        arch,
    )
    .map_err(sc)?
    .try_into()
    .map_err(sc)?;
    seccompiler::apply_filter(&program).map_err(|error| SandboxError::Seccomp(error.to_string()))
}

fn exec_listener() -> Result<File, SandboxError> {
    let sc = |error: seccompiler::BackendError| SandboxError::Seccomp(error.to_string());
    let arch = TargetArch::try_from(consts::ARCH).map_err(sc)?;
    let rules = BTreeMap::from([
        (libc::SYS_execve, Vec::new()),
        (libc::SYS_execveat, Vec::new()),
    ]);
    let mut program: BpfProgram =
        SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Trace(0), arch)
            .map_err(sc)?
            .try_into()
            .map_err(sc)?;
    // seccompiler does not yet expose USER_NOTIF. Rewrite only the constant TRACE return it
    // emitted above; reject a compiler change that does not produce that return instruction.
    let mut rewritten = false;
    for op in &mut program {
        if op.code == 0x06 && op.k == libc::SECCOMP_RET_TRACE {
            op.k = libc::SECCOMP_RET_USER_NOTIF;
            rewritten = true;
        }
    }
    if !rewritten {
        return Err(SandboxError::Seccomp(
            "missing launch notification action".into(),
        ));
    }
    let prog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr().cast(),
    };
    // SAFETY: the kernel copies this live BPF program. This attaches to the calling thread only;
    // its future executable and descendants inherit it. The returned descriptor is CLOEXEC.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
            &prog,
        )
    };
    if fd < 0 {
        return Err(SandboxError::Seccomp(
            io::Error::last_os_error().to_string(),
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
}

fn allow_initial_exec(listener: &File, tid: u32) -> io::Result<()> {
    // SAFETY: ioctl writes into a zero-initialized kernel ABI record owned by this thread.
    let mut request: libc::seccomp_notif = unsafe { mem::zeroed() };
    loop {
        if unsafe {
            libc::ioctl(
                listener.as_raw_fd(),
                libc::SECCOMP_IOCTL_NOTIF_RECV,
                &mut request,
            )
        } == 0
        {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != ErrorKind::Interrupted {
            return Err(error);
        }
    }
    if request.pid != tid || i64::from(request.data.nr) != libc::SYS_execveat {
        return Err(io::Error::other("unexpected launch notification"));
    }
    let response = libc::seccomp_notif_resp {
        id: request.id,
        val: 0,
        error: 0,
        flags: libc::SECCOMP_USER_NOTIF_FLAG_CONTINUE as u32,
    };
    // SAFETY: the kernel reads the live response for the single trusted helper syscall.
    if unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            libc::SECCOMP_IOCTL_NOTIF_SEND,
            &response,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{NamedLaunch, digest, fs, verified_executable};
    use tempfile::tempdir;

    #[test]
    fn launch_digest_verification_rejects_changed_bytes_only_when_enabled() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("binary");
        fs::write(&path, b"\x7fELF-original").unwrap();
        let expected = digest(&path).unwrap();
        let guard = NamedLaunch {
            protected: vec![],
            agent_write: vec![],
            executable_sha256: Some(expected),
            helpers: vec![],
        };
        verified_executable(&path, Some(&expected), &guard).unwrap();
        // Same inode and length: a pathname or file size check alone would miss this.
        fs::write(&path, b"\x7fELF-modified").unwrap();
        let err = verified_executable(&path, Some(&expected), &guard).unwrap_err();
        assert!(
            err.to_string()
                .contains("does not match the session baseline")
        );
        verified_executable(&path, None, &guard).unwrap();
    }
}
