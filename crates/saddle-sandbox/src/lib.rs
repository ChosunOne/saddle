use std::convert::Infallible;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub use saddle_cap::sandbox::SandboxSpec;

mod dns;
#[cfg(target_os = "linux")]
mod launch;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
mod namespaces;
#[cfg(target_os = "linux")]
mod relay;

pub const REQUIRED_LANDLOCK_ABI: i32 = 3;

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

pub fn launch_availability() -> Result<(), String> {
    match landlock_abi() {
        Some(abi) if abi >= 6 => Ok(()),
        _ => Err("named launches require Linux with Landlock ABI 6 for signal and abstract socket isolation".into()),
    }
}

pub fn launch_digest(path: &Path) -> Result<[u8; 32], SandboxError> {
    #[cfg(target_os = "linux")]
    {
        launch::digest(path)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Err(SandboxError::Unavailable(
            "named launches require Linux".into(),
        ))
    }
}

pub fn network_availability() -> Result<String, String> {
    if !cfg!(target_os = "linux") {
        return Err("a command's network is Linux-only in v1".into());
    }
    if let Ok(value) = fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone")
        && value.trim() == "0"
    {
        return Err("unprivileged user namespaces are disabled".into());
    }
    let resolvers = fs::read_to_string("/etc/resolv.conf")
        .map(|text| namespaces::resolvers(&text))
        .unwrap_or_default();
    if resolvers.is_empty() {
        return Ok(
            "its own namespace; no resolver on loopback, so only the proxy variables work".into(),
        );
    }
    Ok(format!(
        "its own namespace, and DNS answered for {} resolver(s) on loopback",
        resolvers.len()
    ))
}

pub fn helper_main() -> ! {
    let err = match helper() {
        Ok(never) => match never {},
        Err(err) => err,
    };
    eprintln!("saddle-sandbox: {err}");
    process::exit(err.exit_code())
}

const SPEC_FD: i32 = 3;

fn helper() -> Result<Infallible, SandboxError> {
    let (spec, spec_was_on_stdin) = match spec_from_fd3() {
        Some(bytes) => (parse_spec(bytes)?, false),
        None => {
            let mut buf = Vec::new();
            io::stdin().read_to_end(&mut buf)?;
            (parse_spec(buf)?, true)
        }
    };
    if spec_was_on_stdin {
        reopen_stdin_null()?;
    }
    enter(&spec)
}

fn parse_spec(bytes: Vec<u8>) -> Result<SandboxSpec, SandboxError> {
    serde_json::from_slice(&bytes).map_err(|e| SandboxError::Spec(e.to_string()))
}

fn spec_from_fd3() -> Option<Vec<u8>> {
    let mut file = ManuallyDrop::new(unsafe { File::from_raw_fd(SPEC_FD) });
    let mut buf = Vec::new();
    match file.read_to_end(&mut buf) {
        Ok(_) if !buf.is_empty() => Some(buf),
        _ => None,
    }
}

pub fn spawn(helper: &Path, helper_args: &[&str], spec: &SandboxSpec) -> io::Result<Child> {
    let payload = serde_json::to_vec(spec).map_err(io::Error::other)?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let fd = theirs.as_raw_fd();
    let mut command = Command::new(helper);
    command
        .args(helper_args)
        .env_clear()
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, SPEC_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let _ = ours.write_all(&payload);
    drop(ours);
    drop(theirs);
    Ok(child)
}

fn reopen_stdin_null() -> io::Result<()> {
    let null = File::open("/dev/null")?;
    if unsafe { libc::dup2(null.as_raw_fd(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

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
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
}

pub fn run(
    helper: &Path,
    helper_args: &[&str],
    spec: &SandboxSpec,
    timeout: Duration,
    output_limit: usize,
    cancel: &AtomicBool,
    service: &mut dyn FnMut(),
) -> io::Result<Output> {
    run_observed(
        helper,
        helper_args,
        spec,
        timeout,
        output_limit,
        cancel,
        service,
        None,
    )
}

#[derive(Default, Clone, Debug)]
pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

#[allow(clippy::too_many_arguments)]
pub fn run_observed(
    helper: &Path,
    helper_args: &[&str],
    spec: &SandboxSpec,
    timeout: Duration,
    output_limit: usize,
    cancel: &AtomicBool,
    service: &mut dyn FnMut(),
    capture: Option<Arc<Mutex<Capture>>>,
) -> io::Result<Output> {
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
        let capture = capture.clone();
        thread::spawn(move || {
            let _ = tx.send((is_err, read_limited(stream, output_limit, capture, is_err)));
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
        service();
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
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

fn read_limited(
    mut stream: impl Read,
    limit: usize,
    capture: Option<Arc<Mutex<Capture>>>,
    is_err: bool,
) -> (Vec<u8>, bool) {
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
                if let Some(capture) = &capture
                    && let Ok(mut capture) = capture.lock()
                {
                    let bytes = if is_err {
                        &mut capture.stderr
                    } else {
                        &mut capture.stdout
                    };
                    let room = limit.saturating_sub(bytes.len());
                    bytes.extend_from_slice(&buf[..n.min(room)]);
                    capture.truncated |= n > room;
                }
            }
        }
    }
    (out, truncated)
}
