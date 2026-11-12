//! The exec sandbox (D18, D19).
//!
//! Two halves. [`run`] is the host side: it starts the helper with an empty environment in its own
//! process group, writes the spec to its stdin, enforces the timeout, and kills the group when the
//! command ends. [`helper_main`] is the helper side: it reads the spec, confines itself with
//! landlock and seccomp, and execs the target, which inherits the confinement.

use std::convert::Infallible;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub use saddle_cap::sandbox::SandboxSpec;

#[cfg(target_os = "linux")]
mod linux;

/// Landlock ABI below which exec is unavailable: ABI 3 adds control over `truncate(2)`.
pub const REQUIRED_LANDLOCK_ABI: i32 = 3;

/// Exit code when the helper fails before exec.
pub const EXIT_SETUP_FAILED: i32 = 125;

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("exec is unavailable: {0}")]
    Unavailable(String),
    #[error("landlock: {0}")]
    Landlock(String),
    #[error("seccomp: {0}")]
    Seccomp(String),
    #[error("bad sandbox spec: {0}")]
    Spec(String),
    #[error("{0}: not found in PATH")]
    NotFound(String),
    #[error("{program}: {source}")]
    Exec { program: PathBuf, source: io::Error },
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl SandboxError {
    fn exit_code(&self) -> i32 {
        match self {
            SandboxError::NotFound(_) => 127,
            SandboxError::Exec { source, .. } if source.kind() == io::ErrorKind::NotFound => 127,
            SandboxError::Exec { .. } => 126,
            _ => EXIT_SETUP_FAILED,
        }
    }
}

/// The running kernel's landlock ABI version, or `None` if landlock is missing or disabled.
pub fn landlock_abi() -> Option<i32> {
    #[cfg(target_os = "linux")]
    {
        linux::landlock_abi()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `Ok(abi)` when exec can run sandboxed here, otherwise the reason it cannot.
pub fn availability() -> Result<i32, String> {
    if !cfg!(target_os = "linux") {
        return Err("the exec sandbox is Linux-only in v1".into());
    }
    match landlock_abi() {
        Some(abi) if abi >= REQUIRED_LANDLOCK_ABI => Ok(abi),
        Some(abi) => Err(format!(
            "landlock ABI {abi} is below the required {REQUIRED_LANDLOCK_ABI} (Linux 6.2)"
        )),
        None => Err("landlock is not enabled in this kernel".into()),
    }
}

/// Entry point for the helper process. Never returns: it execs the target or exits with 125
/// (setup failed), 126 (target not executable) or 127 (target not found).
pub fn helper_main() -> ! {
    let err = match helper() {
        Ok(never) => match never {},
        Err(err) => err,
    };
    eprintln!("saddle-sandbox: {err}");
    std::process::exit(err.exit_code())
}

fn helper() -> Result<Infallible, SandboxError> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    let spec: SandboxSpec =
        serde_json::from_str(&buf).map_err(|e| SandboxError::Spec(e.to_string()))?;
    reopen_stdin_null()?;
    enter(&spec)
}

fn reopen_stdin_null() -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let null = std::fs::File::open("/dev/null")?;
    // SAFETY: dup2 on two valid descriptors; fd 0 is replaced atomically.
    if unsafe { libc::dup2(null.as_raw_fd(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Confines the current process according to `spec` and execs `spec.argv`.
pub fn enter(spec: &SandboxSpec) -> Result<Infallible, SandboxError> {
    if spec.argv.is_empty() {
        return Err(SandboxError::Spec("empty argv".into()));
    }
    #[cfg(target_os = "linux")]
    {
        linux::enter(spec)
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(SandboxError::Unavailable(
            "the exec sandbox is Linux-only in v1".into(),
        ))
    }
}

/// Finds `program` in the spec's `PATH`, the way `execvp` would, before the sandbox applies.
fn resolve_program(program: &str, env: &[(String, String)]) -> Result<PathBuf, SandboxError> {
    if program.contains('/') {
        return Ok(PathBuf::from(program));
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or("");
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = Path::new(dir).join(program);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(SandboxError::NotFound(program.to_owned()))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Exit code, or `None` if the command was killed by a signal.
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    /// The caller set the cancel flag and the command was killed.
    pub cancelled: bool,
    /// Output beyond the limit was discarded.
    pub truncated: bool,
}

/// Host side: runs `helper helper_args...` with an empty environment in a new process group,
/// feeds it `spec`, and waits at most `timeout`, or until `cancel` is set. Afterwards the whole
/// group is killed, so background processes the command started do not outlive it (unless they
/// called `setsid`).
pub fn run(
    helper: &Path,
    helper_args: &[&str],
    spec: &SandboxSpec,
    timeout: Duration,
    output_limit: usize,
    cancel: &AtomicBool,
) -> io::Result<Output> {
    use std::os::unix::process::{CommandExt, ExitStatusExt};

    let mut child = Command::new(helper)
        .args(helper_args)
        .env_clear()
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pgid = child.id() as libc::pid_t;

    let payload = serde_json::to_vec(spec).map_err(io::Error::other)?;
    if let Some(mut stdin) = child.stdin.take() {
        // A helper that died early closes the pipe; its exit status explains why.
        let _ = stdin.write_all(&payload);
    }

    let (tx, rx) = mpsc::channel();
    for (stream, is_err) in [
        (
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            false,
        ),
        (
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn Read + Send>),
            true,
        ),
    ] {
        let Some(stream) = stream else { continue };
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = tx.send((is_err, read_limited(stream, output_limit)));
        });
    }
    drop(tx);

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let mut cancelled = false;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            kill_group(pgid);
            break child.wait()?;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pgid);
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(10));
    };
    kill_group(pgid);

    let mut out = Output {
        code: status.code(),
        signal: status.signal(),
        stdout: Vec::new(),
        stderr: Vec::new(),
        timed_out,
        cancelled,
        truncated: false,
    };
    // A process that escaped the group can hold the pipes open; stop waiting after a second.
    let grace = Instant::now() + Duration::from_secs(1);
    for _ in 0..2 {
        let left = grace.saturating_duration_since(Instant::now());
        let Ok((is_err, (bytes, truncated))) = rx.recv_timeout(left) else {
            break;
        };
        out.truncated |= truncated;
        if is_err {
            out.stderr = bytes
        } else {
            out.stdout = bytes
        }
    }
    Ok(out)
}

fn kill_group(pgid: libc::pid_t) {
    // SAFETY: kill(2) with a negative pid signals the process group; ESRCH is fine.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

fn read_limited(mut stream: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
                truncated |= n > room;
            }
        }
    }
    (out, truncated)
}
