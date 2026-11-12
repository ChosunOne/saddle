//! End to end through the binary: the policy in force, decisions, reserved paths, the exec sandbox,
//! network grants and the audit log, in a throwaway workspace with its own XDG directories.

use std::path::PathBuf;
use std::process::{Command, Output};

const POLICY: &str = r#"
version = 1

[agent]
fs-read  = [{ root = "workspace", why = "reads the repo" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the repo" }]
exec     = { why = "runs builds and tests", timeout-secs = 20 }
"#;

struct Env {
    ws: tempfile::TempDir,
    xdg: tempfile::TempDir,
    outside: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            ws: tempfile::tempdir().unwrap(),
            xdg: tempfile::tempdir().unwrap(),
            outside: tempfile::tempdir().unwrap(),
        };
        let ws = env.ws.path();
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        env.write_policy(POLICY);
        std::fs::write(ws.join("README"), "hello\n").unwrap();
        std::fs::write(ws.join(".env"), "TOKEN=1\n").unwrap();
        std::fs::write(env.outside.path().join("secret"), "secret\n").unwrap();
        env
    }

    /// The global policy: the base every workspace inherits (D43). Tests write here when they want
    /// a grant that applies everywhere.
    fn policy_path(&self) -> PathBuf {
        self.xdg.path().join("config/saddle/policy.toml")
    }

    /// This workspace's own file, which starts empty and is layered on the global policy.
    fn override_path(&self) -> PathBuf {
        let ws = self.ws.path().canonicalize().unwrap();
        let key: String = ws
            .to_string_lossy()
            .trim_start_matches('/')
            .chars()
            .map(|c| match c {
                '/' => '%',
                c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '%') => c,
                _ => '_',
            })
            .collect();
        self.xdg
            .path()
            .join("config/saddle/policies")
            .join(format!("{key}.toml"))
    }

    fn write_override(&self, text: &str) {
        let path = self.override_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn write_policy(&self, text: impl AsRef<str>) {
        let text = text.as_ref();
        let path = self.policy_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn policy(&self) -> String {
        std::fs::read_to_string(self.policy_path()).unwrap()
    }

    fn saddle(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(self.ws.path())
            .args(args)
            .env("XDG_STATE_HOME", self.xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", self.xdg.path().join("config"))
            .output()
            .unwrap()
    }

    fn tool(&self, name: &str, input: serde_json::Value) -> (bool, String) {
        let out = self.saddle(&["tool", name, &input.to_string()]);
        let text = if out.status.success() {
            &out.stdout
        } else {
            &out.stderr
        };
        let text = String::from_utf8_lossy(text).into_owned();
        assert_ne!(out.status.code(), Some(2), "host error: {text}");
        (out.status.success(), text)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.ws.path().join(rel)
    }

    fn audit_logs(&self) -> Vec<PathBuf> {
        let dir = self.xdg.path().join("state/saddle/audit");
        let mut logs: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        logs.sort_by_key(|p| p.metadata().unwrap().modified().unwrap());
        logs
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ready() -> Env {
    Env::new()
}

#[test]
fn a_workspace_policy_is_layered_on_the_global_one_not_copied_from_it() {
    // D43: the global policy is the base, the workspace file holds only what differs, and a
    // change to the global file reaches a workspace that does not override that key.
    let env = Env::new();
    std::fs::remove_file(env.override_path()).ok();

    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(env.override_path().exists(), "first use creates it: {text}");
    assert!(text.contains("policy     layered: "), "{text}");
    // The new file is a header, not a copy, so the global grants show up without being duplicated.
    let own = std::fs::read_to_string(env.override_path()).unwrap();
    assert!(
        own.lines()
            .all(|l| l.trim_start().starts_with('#') || l.trim().is_empty()),
        "the workspace file starts empty: {own}"
    );
    assert!(text.contains("agent.exec = timeout 20s"), "{text}");

    // Editing only the global policy changes the effective grants here.
    env.write_policy(POLICY.replace("timeout-secs = 20", "timeout-secs = 45"));
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("agent.exec = timeout 45s"),
        "the global change reached this workspace: {text}"
    );

    // A workspace file that narrows one scope leaves the others inherited.
    env.write_override(
        "version = 1\n[agent]\nfs-write = [{ root = \"workspace\", exclude = [\".env*\", \"/secrets\"] }]\n",
    );
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("exclude [.env*, /secrets]"), "{text}");
    assert!(
        text.contains("agent.exec = timeout 45s"),
        "still inherited: {text}"
    );

    // A workspace file may widen: it is the authority for this directory, and it lives where
    // nothing the agent runs can write it (D42, D43).
    env.write_override(
        "version = 1\n[agent]\nnet = [{ host = \"api.example.com\" }]\nexec = { timeout-secs = 5 }\n",
    );
    let out = env.saddle(&["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("agent.net[0] = https://api.example.com:443"),
        "{text}"
    );
    assert!(text.contains("agent.exec = timeout 5s"), "{text}");

    // A tree-level file is reported and not read.
    env.write_override("");
    std::fs::write(
        env.path("saddle.toml"),
        "version = 1\n\n[agent]\nnet = [{ host = \"*\" }]\n",
    )
    .unwrap();
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("is not read"), "{text}");
    assert!(
        !text.contains("net["),
        "the tree file grants nothing: {text}"
    );
}

#[test]
fn a_command_cannot_write_the_policy_that_governs_it() {
    // The whole point of D42: the file is outside every root, so the kernel refuses the write and
    // no snapshot has to put anything back.
    let env = ready();
    let before = env.policy();
    let path = env.policy_path();
    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": format!("echo PWNED > {}", path.display())}),
    );
    assert!(!ok, "the write should fail: {text}");
    assert!(
        text.contains("exit code 1") || text.contains("Permission denied"),
        "{text}"
    );
    assert_eq!(env.policy(), before, "the policy is untouched");

    // And through the tools it is not even reachable, since no root contains it.
    let (ok, text) = env.tool(
        "write",
        serde_json::json!({"path": path.to_string_lossy(), "content": "version = 1\n"}),
    );
    assert!(!ok, "{text}");
    assert!(text.contains("denied:"), "{text}");
}

#[test]
fn file_tools_follow_the_policy() {
    let env = ready();
    assert_eq!(
        env.tool("read", serde_json::json!({"path": "README"})),
        (true, "hello\n\n".into())
    );

    let (ok, text) = env.tool(
        "write",
        serde_json::json!({"path": "src/new.rs", "content": "fn main() {}"}),
    );
    assert!(ok, "{text}");
    assert_eq!(
        std::fs::read_to_string(env.path("src/new.rs")).unwrap(),
        "fn main() {}"
    );

    let (ok, text) = env.tool(
        "edit",
        serde_json::json!({"path": "README", "old": "hello", "new": "goodbye"}),
    );
    assert!(ok, "{text}");
    assert_eq!(
        std::fs::read_to_string(env.path("README")).unwrap(),
        "goodbye\n"
    );

    let denied = |name: &str, input: serde_json::Value, expect: &str| {
        let (ok, text) = env.tool(name, input);
        assert!(
            !ok && text.contains(expect),
            "{name}: expected `{expect}` in {text}"
        );
    };
    denied(
        "write",
        serde_json::json!({"path": ".env", "content": "x"}),
        "excluded by agent.fs-write[0]",
    );
    denied(
        "write",
        serde_json::json!({"path": ".git/hooks/pre-commit", "content": "x"}),
        "reserved",
    );
    denied(
        "write",
        serde_json::json!({"path": "saddle.toml", "content": "x"}),
        "reserved",
    );
    let secret = env.outside.path().join("secret");
    denied(
        "read",
        serde_json::json!({"path": secret}),
        "outside every root",
    );
    denied(
        "read",
        serde_json::json!({"path": "../x"}),
        "outside every root",
    );

    // A symlink inside the workspace that points at a reserved path is decided on its target.
    std::os::unix::fs::symlink(".git/hooks/pre-commit", env.path("innocent")).unwrap();
    denied(
        "write",
        serde_json::json!({"path": "innocent", "content": "x"}),
        "reserved",
    );
    assert!(!env.path(".git/hooks/pre-commit").exists());
}

#[test]
fn bash_runs_sandboxed_and_reserved_paths_are_restored() {
    if let Err(reason) = saddle_sandbox::availability() {
        eprintln!("skipping: {reason}");
        return;
    }
    let env = ready();
    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": "cat README && echo built > out.txt"}),
    );
    assert!(ok, "{text}");
    assert!(text.contains("hello"));
    assert_eq!(
        std::fs::read_to_string(env.path("out.txt")).unwrap(),
        "built\n"
    );

    let secret = env.outside.path().join("secret");
    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": format!("cat {}", secret.display())}),
    );
    assert!(!ok && text.contains("Permission denied"), "{text}");

    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": "echo evil > .git/hooks/pre-commit && echo '[core] fsmonitor = evil' >> .git/config"}),
    );
    assert!(ok, "{text}");
    assert!(
        text.contains("restored reserved path .git/hooks/pre-commit"),
        "{text}"
    );
    assert!(!env.path(".git/hooks/pre-commit").exists());
    assert_eq!(
        std::fs::read_to_string(env.path(".git/config")).unwrap(),
        "[core]\n"
    );

    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": "sleep 30", "timeout_secs": 1}),
    );
    assert!(!ok && text.contains("timed out"), "{text}");
}

#[test]
fn every_decision_is_logged_and_the_chain_verifies() {
    let env = ready();
    env.tool("read", serde_json::json!({"path": "README"}));
    env.tool("write", serde_json::json!({"path": ".env", "content": "x"}));
    let logs = env.audit_logs();
    assert_eq!(logs.len(), 2);

    let records: Vec<serde_json::Value> = std::fs::read_to_string(&logs[1])
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let kinds: Vec<&str> = records
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["session-start", "decision", "session-end"]);
    let d = &records[1];
    assert_eq!(d["allow"], false);
    assert_eq!(d["scope"], "fs-write");
    assert_eq!(d["args"]["path"], "workspace:.env");
    assert_eq!(
        d["explanation"],
        "workspace:.env is excluded by agent.fs-write[0]"
    );

    let out = env.saddle(&["audit", "verify", logs[1].to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("head matches"),
        "{stdout}{}",
        stderr(&out)
    );

    // Dropping the last line is caught by the index even though the remaining chain is intact.
    let text = std::fs::read_to_string(&logs[1]).unwrap();
    let kept: Vec<&str> = text.lines().take(2).collect();
    std::fs::write(&logs[1], kept.join("\n") + "\n").unwrap();
    let out = env.saddle(&["audit", "verify", logs[1].to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn fetch_needs_a_net_grant_and_says_which_one() {
    // No net grant at all: the call is denied as an ordinary tool result, and audited.
    let env = Env::new();
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({"url": "https://api.example.com/v1/models"}),
    );
    assert!(!ok, "a workspace with no net grant must refuse: {text}");
    assert!(text.contains("denied:"), "{text}");
    assert!(
        text.contains("no net grant covering https://api.example.com:443"),
        "{text}"
    );
    // Nothing was sent: the decision is the whole answer, and the query is in the record.
    let logs = env.audit_logs();
    let record = std::fs::read_to_string(logs.last().unwrap()).unwrap();
    assert!(
        record.contains("https://api.example.com:443/v1/models"),
        "{record}"
    );

    // A grant for one host does not cover another.
    let policy = env.policy();
    env.write_policy(
        // A net grant is a list on the agent table, the same shape as the other scopes (D28).
        policy.replace(
            "exec     = { why = \"runs builds and tests\", timeout-secs = 20 }",
            "exec     = { why = \"runs builds and tests\", timeout-secs = 20 }\n\
             net      = [{ host = \"api.example.com\", why = \"reads the model list\" }]",
        ),
    );
    let (_, text) = env.tool("fetch", serde_json::json!({"url": "https://evil.test/x"}));
    assert!(
        text.contains("no net grant covering https://evil.test:443"),
        "{text}"
    );
    // http is not covered by an https grant either (D28).
    let (_, text) = env.tool(
        "fetch",
        serde_json::json!({"url": "http://api.example.com/x"}),
    );
    assert!(
        text.contains("no net grant covering http://api.example.com:80"),
        "{text}"
    );
}

#[test]
fn a_wildcard_net_grant_allows_any_host_but_not_the_local_machine() {
    // `*` covers every public host without naming one, which is the point of it (D40).
    let env = Env::new();
    let policy = env.policy();
    env.write_policy(policy.replace(
        "exec     = { why = \"runs builds and tests\", timeout-secs = 20 }",
        "exec     = { why = \"runs builds and tests\", timeout-secs = 20 }\n\
             net      = [{ host = \"*\", why = \"reaches whatever the task needs\" }]",
    ));

    // A public host passes the gate. It cannot be reached here (this sandbox has no sockets), so
    // the failure is the lookup, not a denial: that is what proves the grant allowed it.
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({"url": "https://example.test/x"}),
    );
    assert!(!ok);
    assert!(
        !text.contains("denied:"),
        "the wildcard grant allowed it: {text}"
    );

    // Loopback is not opened by a wildcard: only a grant that spells the address out may (D28).
    let policy = env.policy();
    env.write_policy(
        policy.replace(
            "net      = [{ host = \"*\", why = \"reaches whatever the task needs\" }]",
            "net      = [{ host = \"*\", scheme = \"http\", why = \"reaches whatever the task needs\" }]",
        ),
    );
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({"url": "http://127.0.0.1:80/v1/models"}),
    );
    assert!(!ok, "{text}");
    // The refusal names the grant to write, so a local server is not a mystery (D40).
    assert!(
        text.contains(r#"net = [{ host = "127.0.0.1", scheme = "http""#),
        "it should say which grant: {text}"
    );
    assert!(text.contains("policy file"), "{text}");

    // Naming the address does open it, which is the escape hatch the rule is built around.
    let policy = env.policy();
    env.write_policy(
        policy.replace(
            "net      = [{ host = \"*\", scheme = \"http\", why = \"reaches whatever the task needs\" }]",
            "net      = [{ host = \"*\", scheme = \"http\", why = \"anything\" }, { host = \"127.0.0.1\", scheme = \"http\", why = \"a local server\" }]",
        ),
    );
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({"url": "http://127.0.0.1:80/v1/models"}),
    );
    assert!(!ok);
    assert!(
        !text.contains("loopback"),
        "a named address is reachable: {text}"
    );
}

#[test]
fn fetch_refuses_a_url_it_should_not_follow() {
    let env = Env::new();
    assert!(env.saddle(&["check"]).status.success());
    for url in [
        "file:///etc/passwd",
        "https://user@api.example.com/",
        "nonsense",
    ] {
        let (ok, text) = env.tool("fetch", serde_json::json!({ "url": url }));
        assert!(!ok, "{url} should be refused: {text}");
    }
}

#[test]
fn workspace_containing_the_state_dir_is_refused() {
    let env = Env::new();
    let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(env.ws.path())
        .arg("check")
        .env("XDG_STATE_HOME", env.ws.path().join(".state"))
        .env("XDG_CONFIG_HOME", env.xdg.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("which no root may contain"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_workspace_holding_credentials_is_reported_and_needs_approval() {
    // D60: not a rule about what a workspace may be, and not a dismissible warning either.
    // `check` reports what is in reach and still succeeds; a session has to be approved by typing
    // the path, so with no terminal it refuses and leaves nothing behind.
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    let sub = home.path().join("src/proj");
    std::fs::create_dir_all(sub.join(".git")).unwrap();
    let run = |dir: &std::path::Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", xdg.path().join("config"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };

    // A project inside home exposes nothing, so it is not reported at all.
    let project = run(&sub, &["check"]);
    assert!(project.status.success(), "{}", stderr(&project));
    assert!(
        !String::from_utf8_lossy(&project.stdout).contains("in reach"),
        "{}",
        String::from_utf8_lossy(&project.stdout)
    );

    // Home itself: `check` succeeds and says what is in reach.
    let checked = run(home.path(), &["check"]);
    assert!(
        checked.status.success(),
        "a report, not a refusal: {}",
        stderr(&checked)
    );
    let text = String::from_utf8_lossy(&checked.stdout).into_owned();
    assert!(
        text.contains("in reach: your home directory itself"),
        "{text}"
    );

    // A session there is not started, because there is no terminal to ask on.
    let refused = run(home.path(), &["run", "--json", "-m", "hello"]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    let text = stderr(&refused);
    assert!(text.contains("not approved"), "{text}");
    assert!(text.contains("would be rooted at"), "{text}");
    // Nothing was created in the state directory: no session ran, so no audit log either.
    assert!(
        !xdg.path().join("state/saddle/audit").exists(),
        "a refused approval must not leave an audit log"
    );
}

#[test]
fn the_directory_above_home_is_reported_too() {
    // A root above home contains home, and therefore every credential under it, even though the
    // path looks different (D60).
    let above = tempfile::tempdir().unwrap();
    let home = above.path().join("me");
    std::fs::create_dir_all(&home).unwrap();
    let xdg = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(above.path())
        .arg("check")
        .env("HOME", &home)
        .env("XDG_STATE_HOME", xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", xdg.path().join("config"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("in reach: your home directory, and it is inside"),
        "{text}"
    );
}
