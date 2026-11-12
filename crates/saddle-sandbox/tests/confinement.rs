//! Runs probes inside the real sandbox. The probe is this test binary itself, re-run with
//! `PROBE` set, so the tests depend on nothing but the kernel.

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use saddle_cap::sandbox::{SYSTEM_READ, SYSTEM_WRITE};
use saddle_sandbox::{SandboxSpec, availability, run};

const PROBE: &str = "SADDLE_SANDBOX_PROBE";
const MARK: &str = "probe-result:";
static NEVER: AtomicBool = AtomicBool::new(false);

/// The probe side. Does nothing unless started by one of the tests below.
#[test]
fn probe() {
    let Ok(action) = std::env::var(PROBE) else {
        return;
    };
    let (verb, arg) = action.split_once(':').unwrap_or((action.as_str(), ""));
    let result: std::io::Result<String> = match verb {
        "read" => std::fs::read(arg).map(|b| format!("{} bytes", b.len())),
        "write" => std::fs::write(arg, b"x").map(|_| "written".into()),
        "truncate" => std::fs::OpenOptions::new()
            .write(true)
            .open(arg)
            .and_then(|f| f.set_len(0))
            .map(|_| "truncated".into()),
        "tcp" => std::net::TcpStream::connect(arg).map(|_| "connected".into()),
        "unix" => std::os::unix::net::UnixStream::connect(arg).map(|_| "connected".into()),
        "env" => Ok(std::env::vars()
            .map(|(k, _)| k)
            .collect::<Vec<_>>()
            .join(",")),
        _ => panic!("unknown probe {verb}"),
    };
    let line = match result {
        Ok(s) => format!("{MARK}ok {s}"),
        Err(e) => format!("{MARK}err {}", e.raw_os_error().unwrap_or(-1)),
    };
    println!("{line}");
    std::io::stdout().flush().unwrap();
    std::process::exit(0);
}

struct Fixture {
    granted: tempfile::TempDir,
    writable: tempfile::TempDir,
    secret: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let f = Self {
            granted: tempfile::tempdir().unwrap(),
            writable: tempfile::tempdir().unwrap(),
            secret: tempfile::tempdir().unwrap(),
        };
        std::fs::write(f.granted.path().join("file"), b"granted").unwrap();
        std::fs::write(f.secret.path().join("file"), b"secret").unwrap();
        f
    }

    fn spec(&self, action: &str) -> SandboxSpec {
        let exe = std::env::current_exe().unwrap();
        let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
        read.push(exe.parent().unwrap().to_path_buf());
        read.push(self.granted.path().to_path_buf());
        let mut write: Vec<PathBuf> = SYSTEM_WRITE.iter().map(PathBuf::from).collect();
        write.push(self.writable.path().to_path_buf());
        SandboxSpec {
            argv: vec![
                exe.to_string_lossy().into_owned(),
                "--exact".into(),
                "probe".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            cwd: self.granted.path().to_path_buf(),
            env: vec![
                (PROBE.into(), action.into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            read,
            write,
        }
    }

    fn probe(&self, action: &str) -> String {
        let helper = Path::new(env!("CARGO_BIN_EXE_saddle-sandbox"));
        let out = run(
            helper,
            &[],
            &self.spec(action),
            Duration::from_secs(30),
            1 << 20,
            &NEVER,
        )
        .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        stdout
            .lines()
            .find_map(|l| l.split_once(MARK).map(|(_, r)| r))
            .unwrap_or_else(|| {
                panic!(
                    "no probe result for {action}: {out:?}\nstderr: {}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
            .to_owned()
    }
}

fn sandbox_available() -> bool {
    match availability() {
        Ok(_) => true,
        Err(reason) => {
            eprintln!("skipping: {reason}");
            false
        }
    }
}

const EACCES: &str = "err 13";
const EPERM: &str = "err 1";

#[test]
fn reads_only_granted_paths() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    let p = |p: &Path| format!("read:{}", p.join("file").display());
    assert_eq!(f.probe(&p(f.granted.path())), "ok 7 bytes");
    assert_eq!(f.probe(&p(f.secret.path())), EACCES);
}

#[test]
fn writes_only_writable_paths() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    assert_eq!(
        f.probe(&format!(
            "write:{}",
            f.writable.path().join("new").display()
        )),
        "ok written"
    );
    assert_eq!(
        f.probe(&format!("write:{}", f.granted.path().join("new").display())),
        EACCES
    );
    // ABI 3: truncation of a read-only file is refused as well.
    assert_eq!(
        f.probe(&format!(
            "truncate:{}",
            f.granted.path().join("file").display()
        )),
        EACCES
    );
    assert_eq!(
        std::fs::read(f.granted.path().join("file")).unwrap(),
        b"granted"
    );
}

#[test]
fn no_network_of_any_family() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    let sock = f.writable.path().join("agent.sock");
    // Inside another saddle sandbox (the agent running this suite) the fixture itself cannot
    // create a Unix socket; the TCP probes still run.
    let listener = match UnixListener::bind(&sock) {
        Ok(l) => Some(l),
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => None,
        Err(e) => panic!("binding {}: {e}", sock.display()),
    };
    assert_eq!(f.probe("tcp:127.0.0.1:9"), EPERM);
    assert_eq!(f.probe("tcp:[::1]:9"), EPERM);
    if listener.is_some() {
        assert_eq!(f.probe(&format!("unix:{}", sock.display())), EPERM);
    }
}

#[test]
fn environment_is_exactly_the_spec() {
    if !sandbox_available() {
        return;
    }
    // SAFETY: tests in this binary do not read this variable concurrently.
    unsafe { std::env::set_var("SADDLE_TEST_SECRET", "hunter2") };
    let f = Fixture::new();
    let mut names: Vec<String> = f
        .probe("env")
        .trim_start_matches("ok ")
        .split(',')
        .map(str::to_owned)
        .collect();
    names.sort();
    // libtest may add RUST_* variables of its own; nothing from the host may appear.
    names.retain(|n| !n.starts_with("RUST_"));
    assert_eq!(names, ["PATH", PROBE]);
}

#[test]
fn timeout_kills_the_command() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    let mut spec = f.spec("unused");
    spec.argv = vec!["sh".into(), "-c".into(), "sleep 30".into()];
    let helper = Path::new(env!("CARGO_BIN_EXE_saddle-sandbox"));
    let start = std::time::Instant::now();
    let out = run(helper, &[], &spec, Duration::from_millis(300), 1024, &NEVER).unwrap();
    assert!(out.timed_out);
    assert_eq!(out.signal, Some(libc::SIGKILL));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn missing_program_exits_127() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    let mut spec = f.spec("unused");
    spec.argv = vec!["definitely-not-a-program".into()];
    let helper = Path::new(env!("CARGO_BIN_EXE_saddle-sandbox"));
    let out = run(helper, &[], &spec, Duration::from_secs(5), 1024, &NEVER).unwrap();
    assert_eq!(
        out.code,
        Some(127),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn cancel_kills_the_command_and_its_children() {
    if !sandbox_available() {
        return;
    }
    let f = Fixture::new();
    let mut spec = f.spec("unused");
    spec.argv = vec!["sh".into(), "-c".into(), "sleep 30 & sleep 30".into()];
    let helper = Path::new(env!("CARGO_BIN_EXE_saddle-sandbox"));
    static CANCEL: AtomicBool = AtomicBool::new(false);
    let setter = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(200));
        CANCEL.store(true, Ordering::SeqCst);
    });
    let start = std::time::Instant::now();
    let out = run(helper, &[], &spec, Duration::from_secs(30), 1024, &CANCEL).unwrap();
    setter.join().unwrap();
    assert!(out.cancelled && !out.timed_out);
    assert_eq!(out.signal, Some(libc::SIGKILL));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}
