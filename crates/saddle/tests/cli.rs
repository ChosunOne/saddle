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

    /// The one config file, holding both the base grants and each workspace's own (D63).
    fn policy_path(&self) -> PathBuf {
        self.xdg.path().join("config/saddle/config.toml")
    }

    /// This workspace's own grant file, one per workspace (D63).
    fn override_path(&self) -> PathBuf {
        self.xdg
            .path()
            .join("config/saddle/policies")
            .join(format!("{}.toml", self.workspace_key()))
    }

    /// Writes this workspace's own grants, as a policy-shaped document. The `workspace` line is
    /// what the file is checked against, so it is written here (D63).
    fn write_override(&self, text: &str) {
        let path = self.override_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ws = self.ws.path().canonicalize().unwrap();
        std::fs::write(
            &path,
            format!("workspace = {:?}\n{text}", ws.display().to_string()),
        )
        .unwrap();
    }

    /// The base grants: `[agent]` in `config.toml` (D63).
    fn write_policy(&self, text: impl AsRef<str>) {
        let agent = agent_of(text.as_ref());
        let path = self.policy_path();
        let base = std::fs::read_to_string(&path).unwrap_or_default();
        let mut doc: toml::Table = base.parse().unwrap_or_default();
        doc.insert("agent".into(), agent);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
    }

    /// Changes the base grants in place, so a test does not have to spell the serialised TOML.
    fn edit_agent(&self, change: impl FnOnce(&mut toml::Table)) {
        let path = self.policy_path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut doc: toml::Table = text.parse().unwrap_or_default();
        let agent = doc
            .entry("agent")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        change(agent.as_table_mut().unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
    }

    fn workspace_key(&self) -> String {
        // pi's scheme, as `paths::workspace_key` (D63).
        let ws = self.ws.path().canonicalize().unwrap();
        let inner = ws
            .to_string_lossy()
            .trim_start_matches('/')
            .replace(['/', '\\', ':'], "-");
        format!("--{inner}--")
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
        // No run yet means no audit directory, which is not a failure.
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut logs: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
        logs.sort_by_key(|p| p.metadata().unwrap().modified().unwrap());
        logs
    }
}

/// The `[agent]` table of a policy-shaped document, which is what the merged file stores.
fn agent_of(text: &str) -> toml::Value {
    let doc: toml::Table = text.parse().unwrap_or_default();
    doc.get("agent")
        .cloned()
        .unwrap_or_else(|| toml::Value::Table(toml::Table::new()))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn ready() -> Env {
    Env::new()
}

#[test]
fn a_workspace_layers_its_own_file_on_the_base() {
    // D63: one `config.toml`. `[agent]` is the base every workspace inherits, and
    // `[workspaces."<key>"]` is this workspace's own, replacing a key it lists and inheriting the
    // rest.
    let env = Env::new();
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("grants     "), "{text}");
    assert!(
        text.contains("has no grants for this directory: it inherits the base"),
        "a fresh workspace inherits everything: {text}"
    );
    assert!(text.contains("agent.exec = timeout 20s"), "{text}");

    // Editing the base changes the effective grants here.
    env.write_policy(POLICY.replace("timeout-secs = 20", "timeout-secs = 45"));
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("agent.exec = timeout 45s"),
        "the base change reached this workspace: {text}"
    );

    // An entry that narrows one scope leaves the others inherited.
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

    // It may widen too: it is the authority for this directory, and it lives where nothing the
    // agent runs can write it (D42, D43).
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
    env.write_override("version = 1\n");
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
    // D64: discovery is a record of its own, before any call is decided, so what was advertised
    // is in the log even when nothing loads a skill.
    assert_eq!(kinds, ["session-start", "skill", "decision", "session-end"]);
    let d = &records[2];
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
    env.edit_agent(|agent| {
        agent.insert(
            "net".into(),
            toml::from_str::<toml::Value>(
                "net = [{ host = \"*\", why = \"reaches whatever the task needs\" }]",
            )
            .unwrap()
            .get("net")
            .cloned()
            .unwrap(),
        );
    });

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
    env.edit_agent(|agent| {
        agent.insert(
            "net".into(),
            toml::from_str::<toml::Value>(
                "net = [{ host = \"*\", scheme = \"http\", why = \"reaches whatever the task needs\" }]",
            )
            .unwrap()
            .get("net")
            .cloned()
            .unwrap(),
        );
    });
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
    env.edit_agent(|agent| {
        agent.insert(
            "net".into(),
            toml::from_str::<toml::Value>(
                "net = [{ host = \"*\", scheme = \"http\", why = \"anything\" }, \
                 { host = \"127.0.0.1\", scheme = \"http\", why = \"a local server\" }]",
            )
            .unwrap()
            .get("net")
            .cloned()
            .unwrap(),
        );
    });
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
fn a_session_in_a_workspace_holding_credentials_needs_an_allowance_for_its_id() {
    // D60. Nothing is refused from the warning itself: the refusal names the session id and a
    // command a person runs in their own terminal, and the allowance is written outside every root.
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    let sub = home.path().join("src/proj");
    std::fs::create_dir_all(sub.join(".git")).unwrap();
    let saddle = |dir: &std::path::Path, args: &[&str]| {
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

    // A project inside home exposes nothing, so it needs no allowance at all.
    let project = saddle(&sub, &["check"]);
    assert!(project.status.success(), "{}", stderr(&project));
    assert!(
        !String::from_utf8_lossy(&project.stdout).contains("in reach"),
        "{}",
        String::from_utf8_lossy(&project.stdout)
    );

    // `check` reports and succeeds: it is the review tool, not the gate.
    let checked = saddle(home.path(), &["check"]);
    assert!(checked.status.success(), "{}", stderr(&checked));
    let text = String::from_utf8_lossy(&checked.stdout).into_owned();
    assert!(
        text.contains("in reach: your home directory itself"),
        "{text}"
    );

    // A session there is refused, and says which session id to allow.
    let refused = saddle(home.path(), &["tool", "read", r#"{"path":"README"}"#]);
    assert_eq!(refused.status.code(), Some(2), "{}", stderr(&refused));
    let text = stderr(&refused);
    assert!(text.contains("would be rooted at"), "{text}");
    assert!(text.contains("session allow"), "{text}");
    let id = text
        .split("this session (")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("the message names the session id")
        .to_owned();
    assert!(!id.is_empty(), "{text}");

    // The id is findable afterwards, which is the whole reason the request is recorded.
    let pending = saddle(home.path(), &["session", "pending"]);
    assert!(pending.status.success(), "{}", stderr(&pending));
    let listed = String::from_utf8_lossy(&pending.stdout).into_owned();
    assert!(listed.contains(&id), "{listed}");
    assert!(
        listed.contains("ago"),
        "the age tells a person which message it was: {listed}"
    );

    // Allowing another workspace does not allow this one.
    let elsewhere = tempfile::tempdir().unwrap();
    let wrong = saddle(
        home.path(),
        &[
            "session",
            "allow",
            &id,
            "--workspace",
            &elsewhere.path().display().to_string(),
        ],
    );
    assert!(wrong.status.success(), "{}", stderr(&wrong));
    let still = saddle(
        home.path(),
        &["--session", &id, "tool", "read", r#"{"path":"README"}"#],
    );
    assert_eq!(
        still.status.code(),
        Some(2),
        "the allowance names another workspace"
    );

    // Allowing this workspace for this id lets that session, and only that session, in.
    let allow = saddle(home.path(), &["session", "allow", &id]);
    assert!(allow.status.success(), "{}", stderr(&allow));
    std::fs::write(home.path().join("README"), "hello\n").unwrap();
    let other_id = "0123456789abcdef";
    let wrong_session = saddle(
        home.path(),
        &[
            "--session",
            other_id,
            "tool",
            "read",
            r#"{"path":"README"}"#,
        ],
    );
    assert_eq!(
        wrong_session.status.code(),
        Some(2),
        "an allowance belongs to one session id"
    );
    let allowed = saddle(
        home.path(),
        &["--session", &id, "tool", "read", r#"{"path":"README"}"#],
    );
    assert!(allowed.status.success(), "{}", stderr(&allowed));
    assert!(
        String::from_utf8_lossy(&allowed.stdout).contains("hello"),
        "{}",
        String::from_utf8_lossy(&allowed.stdout)
    );

    // The allowance outlives the session so it can be picked again without copying the id (D61),
    // which makes it something to revoke rather than something that expires by itself.
    let listed = saddle(home.path(), &["session", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let text = String::from_utf8_lossy(&listed.stdout).into_owned();
    assert!(text.contains(&id), "{text}");
    assert!(
        text.contains("README") || text.contains(home.path().to_str().unwrap()),
        "{text}"
    );

    // A prefix is enough, and a later session reuses the allowance instead of being refused.
    let prefix = &id[..8];
    let reused = saddle(
        home.path(),
        &["--session", prefix, "tool", "read", r#"{"path":"README"}"#],
    );
    assert!(reused.status.success(), "{}", stderr(&reused));

    // Revoking is how an allowance goes away for a capability that was only needed briefly, and it
    // takes the same short forms as `--session` (D61).
    let revoked = saddle(home.path(), &["session", "revoke", prefix]);
    assert!(revoked.status.success(), "{}", stderr(&revoked));
    let after = saddle(
        home.path(),
        &["--session", &id, "tool", "read", r#"{"path":"README"}"#],
    );
    assert_eq!(after.status.code(), Some(2), "{}", stderr(&after));
}

#[test]
fn allow_sensitive_in_the_users_config_skips_the_question() {
    // The escape hatch for a machine where home holds no secrets. It is the user's file, outside
    // every root, so nothing the agent runs can set it (D42, D60).
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    std::fs::write(home.path().join("README"), "hello\n").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(home.path())
            .args(["tool", "read", r#"{"path":"README"}"#])
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", xdg.path().join("config"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    assert_eq!(
        run().status.code(),
        Some(2),
        "refused while the flag is off"
    );
    std::fs::write(
        xdg.path().join("config/saddle/config.toml"),
        "allow-sensitive = true\n",
    )
    .unwrap();
    let out = run();
    assert!(out.status.success(), "{}", stderr(&out));
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

#[test]
fn the_default_include_rule_gives_a_child_what_it_needs_and_withholds_credentials() {
    // D62: no rule names cargo, rustup or git. The default walks home, keeps what does not match
    // the except patterns, and hands that list to the kernel — so a toolchain works on a machine
    // saddle has never seen, while the credentials beside it stay out of reach.
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    let h = home.path();
    std::fs::create_dir_all(h.join(".cargo/registry")).unwrap();
    std::fs::create_dir_all(h.join(".config/git")).unwrap();
    std::fs::create_dir_all(h.join(".ssh")).unwrap();
    std::fs::create_dir_all(h.join("proj/.git")).unwrap();
    std::fs::write(h.join(".config/git/config"), "gitconfig\n").unwrap();
    std::fs::write(h.join(".ssh/id_rsa"), "sk-live-private\n").unwrap();
    let auth = xdg.path().join("state/saddle/auth.json");
    std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
    // The shape `AuthStore` reads, so the file is a real store rather than a parse error.
    std::fs::write(&auth, r#"{"synthetic":{"api-key":"sk-live-api"}}"#).unwrap();
    let saddle = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(h.join("proj"))
            .args(args)
            .env("HOME", h)
            .env("XDG_STATE_HOME", xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", xdg.path().join("config"))
            .output()
            .unwrap()
    };
    // `check` names the directories a command may read, because the list is the authority.
    let checked = saddle(&["check"]);
    assert!(checked.status.success(), "{}", stderr(&checked));
    let text = String::from_utf8_lossy(&checked.stdout).into_owned();
    assert!(text.contains("exec reads"), "{text}");
    assert!(text.contains(".cargo"), "{text}");
    assert!(
        !text.contains(".ssh"),
        "the exclusion is not in the list: {text}"
    );

    let read = |path: &std::path::Path| -> bool {
        let out = saddle(&[
            "tool",
            "bash",
            &serde_json::json!({
                "command": format!("cat {} >/dev/null 2>&1", path.display())
            })
            .to_string(),
        ]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("exit code 0")
    };
    assert!(
        read(&h.join(".config/git/config")),
        "a tool config is readable"
    );
    assert!(!read(&h.join(".ssh/id_rsa")), "the key is not");
    assert!(!read(&auth), "saddle's own key is never in reach");
}

#[test]
fn an_extra_except_line_takes_one_more_directory_out_of_reach() {
    // The point of the shape: a person adds one pattern and the exclusion is real for children,
    // because the directory never reaches the kernel in the first place (D62).
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let h = home.path();
    std::fs::create_dir_all(h.join("proj/.git")).unwrap();
    std::fs::create_dir_all(h.join(".config/git")).unwrap();
    std::fs::create_dir_all(h.join("notes/private")).unwrap();
    std::fs::write(h.join("notes/private/a.txt"), "secret\n").unwrap();
    std::fs::write(h.join("notes/public.txt"), "fine\n").unwrap();
    std::fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    std::fs::write(
        xdg.path().join("config/saddle/config.toml"),
        r#"
[[sandbox.include]]
base = "~"
depth = 2
except = ["^notes/private$"]
"#,
    )
    .unwrap();
    let read = |path: &std::path::Path| -> bool {
        let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(h.join("proj"))
            .args([
                "tool",
                "bash",
                &serde_json::json!({
                    "command": format!("cat {} >/dev/null 2>&1", path.display())
                })
                .to_string(),
            ])
            .env("HOME", h)
            .env("XDG_STATE_HOME", xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", xdg.path().join("config"))
            .output()
            .unwrap();
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("exit code 0")
    };
    assert!(
        read(&h.join("notes/public.txt")),
        "the sibling stays readable"
    );
    assert!(
        !read(&h.join("notes/private/a.txt")),
        "the excluded one does not"
    );
}

#[test]
fn a_session_override_changes_the_grants_for_one_run_and_writes_nothing() {
    // D63: `--grant` and `--revoke` are the per-session half of one config file. They are folded
    // into the merged grants before parsing, so they are validated and audited like any other.
    let env = Env::new();
    let before = env.policy();

    // A write is allowed normally, and refused when the scope is revoked for the run.
    let (ok, _) = env.tool(
        "write",
        serde_json::json!({"path": "a.txt", "content": "x"}),
    );
    assert!(ok, "the base grants write");
    let out = env.saddle(&[
        "--revoke",
        "fs-write",
        "tool",
        "write",
        r#"{"path":"b.txt","content":"x"}"#,
    ]);
    let text = stderr(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("no fs-write grant"), "{text}");

    // The listing shows the grants of the run, not the file's.
    let out = env.saddle(&["--revoke", "fs-write", "check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !text.contains("agent.fs-write"),
        "revoked in the listing: {text}"
    );
    assert!(
        text.contains("grants changed for this session only: -fs-write"),
        "the note says what changed: {text}"
    );

    // Nothing was written: the file is byte for byte what it was.
    assert_eq!(env.policy(), before, "an override is not stored");

    // A scope the file does not have is still revoked cleanly, and an unknown form is refused.
    let out = env.saddle(&["--grant", "nonsense:thing", "check"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("does not know how to build"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_old_policy_file_is_migrated_once() {
    // D63, temporary: `policy.toml` held the base grants and is no longer read. They are moved
    // into `config.toml` and the original renamed, so a second run does nothing.
    let env = Env::new();
    let config = env.policy_path();
    // The old layout: a config with machine settings only, and a `policy.toml` beside it.
    std::fs::write(&config, "provider = \"synthetic\"\n").unwrap();
    let dir = config.parent().unwrap();
    std::fs::write(
        dir.join("policy.toml"),
        "version = 1\n[agent]\nexec = { why = \"old base\", timeout-secs = 42 }\n",
    )
    .unwrap();

    let out = env.saddle(&["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // The "moved" line is a first-run notice, so it goes to stderr like the others.
    let notice = stderr(&out);
    assert!(notice.contains("moved 1 grants file"), "{notice}");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("timeout 42s"),
        "the old base was taken: {text}"
    );

    // The original is kept under a name nothing reads, and a second run is a no-op.
    assert!(dir.join("policy.toml.imported").exists());
    let out = env.saddle(&["check"]);
    assert!(
        !stderr(&out).contains("moved"),
        "migration runs once: {}",
        stderr(&out)
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("timeout 42s"), "and the grants stay: {text}");
}

#[test]
fn a_grants_file_that_names_another_directory_is_refused() {
    // D63: the file says which directory it governs. The name is a readable encoding and is lossy,
    // so `~/a-b` and `~/a/b` collide — and applying one directory's grants to another is how a
    // `net *` written for one project would silently cover a second.
    let env = Env::new();
    let key = env.workspace_key();
    let dir = env.policy_path().parent().unwrap().join("policies");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{key}.toml"));
    std::fs::write(
        &file,
        "workspace = \"/somewhere/else\"\nversion = 1\n[agent]\nnet = [{ host = \"*\" }]\n",
    )
    .unwrap();
    let out = env.saddle(&["check"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("is for /somewhere/else"), "{text}");
    assert!(text.contains("two directories cannot share"), "{text}");

    // The same file naming this directory is used, which is the ordinary case.
    let ws = env.ws.path().canonicalize().unwrap();
    std::fs::write(
        &file,
        format!(
            "workspace = {:?}\nversion = 1\n[agent]\nnet = [{{ host = \"*\" }}]\n",
            ws.display().to_string()
        ),
    )
    .unwrap();
    let out = env.saddle(&["check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("agent.net[0]"), "{text}");
}

#[test]
fn a_skill_is_advertised_by_name_and_loaded_on_demand() {
    // D64: the prompt carries the name, the description and the directory; the body is a tool
    // call, so a directory of skills costs a few lines rather than all of their text.
    let env = Env::new();
    std::fs::create_dir_all(env.path(".agents/skills/house")).unwrap();
    std::fs::write(
        env.path(".agents/skills/house/SKILL.md"),
        "---\nname: house\ndescription: House style. Use when editing Rust here.\n---\n\n\
         lowercase doc comments.\n",
    )
    .unwrap();

    // A throwaway HOME throughout: the user tier would otherwise be the developer's own skills.
    let home = tempfile::tempdir().unwrap();
    let skill = |input: &str| {
        let out = saddle_in(&env, home.path(), &["tool", "skill", input]);
        let text = if out.status.success() {
            String::from_utf8_lossy(&out.stdout).into_owned()
        } else {
            stderr(&out)
        };
        (out.status.success(), text)
    };

    let (ok, text) = skill("{}");
    assert!(ok, "{text}");
    assert!(text.contains("house"), "advertised, not loaded: {text}");
    assert!(text.contains("House style."), "{text}");
    assert!(
        !text.contains("lowercase doc comments"),
        "the body is not the list"
    );

    let (ok, text) = skill(r#"{"name":"house"}"#);
    assert!(ok, "{text}");
    assert!(text.contains("lowercase doc comments"), "{text}");

    // A name is an identifier, so there is no path to traverse.
    let (ok, text) = skill(r#"{"name":"../../../etc/passwd"}"#);
    assert!(!ok, "{text}");
    assert!(text.contains("no skill `../../../etc/passwd`"), "{text}");
}

#[test]
fn a_skill_the_policy_refuses_is_not_advertised() {
    // D64: a project skill is read through the policy as the agent, so an excluded one is refused
    // and noted rather than offered — the same rule as an instruction file (D32).
    let env = Env::new();
    std::fs::create_dir_all(env.path(".agents/skills/house")).unwrap();
    std::fs::write(
        env.path(".agents/skills/house/SKILL.md"),
        "---\nname: house\ndescription: House style.\n---\nlowercase doc comments.\n",
    )
    .unwrap();
    env.edit_agent(|agent| {
        agent.insert(
            "fs-read".into(),
            toml::Value::Array(vec![toml::Value::Table(
                r#"root = "workspace"
                   exclude = [".agents/skills/house/**"]"#
                    .parse()
                    .unwrap(),
            )]),
        );
    });
    // A throwaway HOME, so the developer's own ~/.agents/skills cannot leak into the assertion.
    let home = tempfile::tempdir().unwrap();
    let out = saddle_in(&env, home.path(), &["tool", "skill", "{}"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("there are no skills"), "{text}");
}

#[test]
fn a_skill_under_the_config_directory_loads_although_nothing_can_read_it() {
    // D64: the user's own skills are read by the host like the global instruction file, so the tool
    // works for a skill the file tools can never reach. `~/.agents` became tool-readable by default
    // (D66), but the other user tier -- the config directory -- is protected from every root and
    // from every sandbox entry alike (D16), so this is where the reason for a tool still lives.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let dir = env.xdg.path().join("config/saddle/skills/pdf-tools");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    std::fs::write(
        &file,
        "---\nname: pdf-tools\ndescription: Extract text from PDFs.\n---\nRead formats.md.\n",
    )
    .unwrap();

    let out = saddle_in(
        &env,
        home.path(),
        &["tool", "skill", r#"{"name":"pdf-tools"}"#],
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("Read formats.md."), "{text}");

    // Nothing else reaches it: not a root, not a sandbox entry, not the tools.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, file.display()),
        ],
    );
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("outside every root"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_sandbox_path_reaches_commands_and_not_the_file_tools() {
    // D65: the two sets are different on purpose. `[sandbox] read` is one allow-list per child
    // process, with no per-call decision and no record per access; wiring it into the file tools
    // would let a directory be read with no grant, no exclusion and no audit record. So a path
    // there makes `bash` succeed and leaves `read` refusing, and this test exists to keep it that
    // way if someone later finds the asymmetry surprising.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let notes = home.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("a.md"), "hi\n").unwrap();

    // A path in [sandbox] read, not a root and not a grant.
    let config = env.policy_path();
    let mut doc: toml::Table = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    let sandbox = doc
        .entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    sandbox
        .as_table_mut()
        .unwrap()
        .insert("read".into(), toml::Value::Array(vec!["~/notes".into()]));

    let run = |args: &[&str]| {
        let text = toml::to_string(&doc).unwrap();
        std::fs::write(&config, &text).unwrap();
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(env.ws.path())
            .args(args)
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", env.xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
            .output()
            .unwrap()
    };

    let out = run(&[
        "tool",
        "read",
        &format!(r#"{{"path":"{}"}}"#, notes.join("a.md").display()),
    ]);
    let text = stderr(&out);
    assert_ne!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("outside every root"), "{text}");

    let out = run(&["tool", "bash", r#"{"command":"head -1 ~/notes/a.md"}"#]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("hi"), "{text}");

    // And `check` names it under exec reads, never under roots.
    let out = run(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let notes = notes.display().to_string();
    let roots = text
        .split("grants")
        .next()
        .filter(|before| before.contains("roots"))
        .unwrap_or_default();
    assert!(text.contains("exec reads"), "{text}");
    assert!(
        !roots.contains(&notes),
        "a sandbox path is not a root: {text}"
    );
    assert!(text.contains(&notes), "it is listed as exec reach: {text}");
}

/// Runs the binary with `HOME` pointed at a throwaway directory, so a `[sandbox]` entry can name
/// something under it.
fn saddle_in(env: &Env, home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(env.ws.path())
        .args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .output()
        .unwrap()
}

/// Writes `[sandbox] read` as the given TOML value, keeping the rest of the config.
fn set_sandbox_read(env: &Env, value: toml::Value) {
    let path = env.policy_path();
    let mut doc: toml::Table = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .parse()
        .unwrap();
    let sandbox = doc
        .entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    sandbox.as_table_mut().unwrap().insert("read".into(), value);
    std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
}

#[test]
fn a_sandbox_entry_naming_tools_lets_those_tools_reach_it() {
    // D66: a directory outside the workspace, reachable by the tools named on its `[sandbox]`
    // entry. It is not a root, so no policy can name it, and it is not part of the session.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join(".agents");
    std::fs::create_dir_all(agents.join("skills/a")).unwrap();
    std::fs::write(agents.join("skills/a/SKILL.md"), "---\nname: a\n---\nx\n").unwrap();
    let notes = home.path().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("n.md"), "notes\n").unwrap();

    let entry = |path: &str, tools: &[&str]| {
        let mut table = toml::Table::new();
        table.insert("path".into(), path.into());
        table.insert(
            "tools".into(),
            toml::Value::Array(tools.iter().map(|t| toml::Value::from(*t)).collect()),
        );
        toml::Value::Table(table)
    };
    set_sandbox_read(
        &env,
        toml::Value::Array(vec!["~/notes".into(), entry("~/.agents", &["read", "ls"])]),
    );

    // A named tool reaches it.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(
                r#"{{"path":"{}"}}"#,
                agents.join("skills/a/SKILL.md").display()
            ),
        ],
    );
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("name: a"), "{text}");

    // A tool the entry does not name is refused, and says which are allowed.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "write",
            &format!(
                r#"{{"path":"{}","content":"x"}}"#,
                agents.join("new.txt").display()
            ),
        ],
    );
    let text = stderr(&out);
    assert!(
        text.contains("not one of the tools config.toml allows"),
        "{text}"
    );
    assert!(text.contains("read, ls"), "{text}");

    // A bare path is indirect reach only: the entry above keeps ~/notes out of the tools' way.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, notes.join("n.md").display()),
        ],
    );
    assert!(
        stderr(&out).contains("outside every root"),
        "{}",
        stderr(&out)
    );

    // `check` shows it under its own heading, never under roots.
    let out = saddle_in(&env, home.path(), &["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let roots = text.split("grants").next().unwrap_or_default();
    assert!(!roots.contains(".agents"), "not a root: {text}");
    assert!(text.contains("tool paths"), "{text}");
    assert!(text.contains("~/.agents"), "{text}");
}

#[test]
fn a_sandbox_entry_naming_tools_can_write_but_never_a_reserved_path() {
    // The tools run in saddle, so the list is what allows a write there; the skeleton is the
    // kernel. D23 still holds, because a hook runs as the user later whatever granted the write.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let scratch = home.path().join("scratch");
    std::fs::create_dir_all(scratch.join(".git/hooks")).unwrap();
    let mut table = toml::Table::new();
    table.insert("path".into(), "~/scratch".into());
    table.insert(
        "tools".into(),
        toml::Value::Array(vec!["write".into(), "read".into()]),
    );
    set_sandbox_read(&env, toml::Value::Array(vec![toml::Value::Table(table)]));

    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "write",
            &format!(
                r#"{{"path":"{}","content":"fine"}}"#,
                scratch.join("ok.txt").display()
            ),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "write",
            &format!(
                r##"{{"path":"{}","content":"#!/bin/sh"}}"##,
                scratch.join(".git/hooks/pre-commit").display()
            ),
        ],
    );
    assert!(
        stderr(&out).contains("reserved by the host"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_sandbox_entry_may_not_reach_the_policy_or_the_audit_log() {
    // D16: no sandbox entry may reach the state or config directory. It used to be unchecked, so
    // `read = ["~/.config/saddle"]` handed a command the policy file.
    let env = Env::new();
    let config = env.policy_path();
    let dir = config.parent().unwrap().to_path_buf();
    set_sandbox_read(
        &env,
        toml::Value::Array(vec![toml::Value::from(dir.display().to_string())]),
    );
    let out = env.saddle(&["check"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    let text = stderr(&out);
    assert!(text.contains("no sandbox entry may reach it"), "{text}");

    // Naming something above it is refused too, since a rule covers everything beneath it.
    let parent = dir.parent().unwrap().display().to_string();
    set_sandbox_read(&env, toml::Value::Array(vec![toml::Value::from(parent)]));
    let out = env.saddle(&["check"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

#[test]
fn a_misspelt_tool_name_in_a_sandbox_entry_is_an_error() {
    // The list is the authority for that directory, and there is no kernel check behind a
    // host-side tool, so a typo must not read as a permission that silently does nothing.
    let env = Env::new();
    let mut table = toml::Table::new();
    table.insert("path".into(), "/tmp".into());
    table.insert("tools".into(), toml::Value::Array(vec!["raed".into()]));
    set_sandbox_read(&env, toml::Value::Array(vec![toml::Value::Table(table)]));
    let out = env.saddle(&["check"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("lists unknown tool `raed`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn one_access_records_one_decision() {
    // D10: one access is one decision. The read tool used to gate twice — once to sniff whether the
    // file is a picture, once to read it — so every read logged the same allow twice, and "how many
    // times was this file read" was unanswerable from the log.
    let env = ready();
    let decisions = |name: &str, input: &str| -> usize {
        let before = env.audit_logs().len();
        env.tool(name, serde_json::from_str(input).unwrap());
        let logs = env.audit_logs();
        assert!(logs.len() > before, "{name} wrote no log");
        std::fs::read_to_string(logs.last().unwrap())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|r| r["kind"] == "decision")
            .count()
    };

    assert_eq!(decisions("read", r#"{"path":"README"}"#), 1, "a text file");
    assert_eq!(decisions("read", r#"{"path":"nope"}"#), 1, "a missing file");
    assert_eq!(
        decisions("read", r#"{"path":"../outside"}"#),
        1,
        "a refusal"
    );
    assert_eq!(decisions("ls", r#"{"path":"."}"#), 1);
    // edit legitimately makes two: it reads the old text and writes the new one.
    assert_eq!(
        decisions("edit", r#"{"path":"README","old":"hello","new":"bye"}"#),
        2
    );
}

#[test]
fn the_shared_skills_directory_is_readable_by_default() {
    // D66: ~/.agents is built in and read-only, because that is where skills are installed and a
    // skill's own references have to be readable. Nothing is written in the config for this.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let skill = home.path().join(".agents/skills/s");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: s\ndescription: d\n---\nRead references/r.md.\n",
    )
    .unwrap();
    std::fs::write(skill.join("references/r.md"), "the reference\n").unwrap();

    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, skill.join("SKILL.md").display()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("name: s"));

    // The bundled file too: that is the point of the directory being tool-readable.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(
                r#"{{"path":"{}"}}"#,
                skill.join("references/r.md").display()
            ),
        ],
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("the reference"));

    // Read-only: the default names no tool that writes.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "write",
            &format!(
                r#"{{"path":"{}","content":"x"}}"#,
                skill.join("new").display()
            ),
        ],
    );
    assert!(
        stderr(&out).contains("not one of the tools config.toml allows"),
        "{}",
        stderr(&out)
    );

    // Shown under tool paths, and named as the built-in it is.
    let out = saddle_in(&env, home.path(), &["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("tool paths"), "{text}");
    assert!(text.contains("~/.agents"), "{text}");

    // `tools-defaults = false` turns it off.
    let path = env.policy_path();
    let mut doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    doc.entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap()
        .insert("tools-defaults".into(), toml::Value::Boolean(false));
    std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, skill.join("SKILL.md").display()),
        ],
    );
    assert!(
        stderr(&out).contains("outside every root"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_entry_for_a_default_path_adds_to_it_rather_than_replacing_it() {
    // Two entries over one directory are one claim, so writing an entry for ~/.agents widens it
    // instead of silently dropping the default tools.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join(".agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(agents.join("a.txt"), "text\n").unwrap();
    set_sandbox_read(
        &env,
        toml::Value::Array(vec![toml::Value::Table({
            let mut t = toml::Table::new();
            t.insert("path".into(), "~/.agents".into());
            t.insert("tools".into(), toml::Value::Array(vec!["write".into()]));
            t
        })]),
    );

    let out = saddle_in(&env, home.path(), &["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    for tool in ["read", "ls", "find", "grep", "write"] {
        assert!(
            text.contains(tool),
            "{tool} missing from the merged entry: {text}"
        );
    }
    // One line, not two: the union is a single claim.
    assert_eq!(text.matches("~/.agents").count(), 1, "{text}");

    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "write",
            &format!(
                r#"{{"path":"{}","content":"ok"}}"#,
                agents.join("new").display()
            ),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn a_config_with_no_sandbox_table_keeps_the_built_in_defaults() {
    // `UserConfig.sandbox` is `#[serde(default)]`, so a config with no `[sandbox]` table got
    // `SandboxConfig`'s *derived* Default — where both flags were false. A hand-written config then
    // silently lost the built-in include rule (D62) and, later, the built-in tool path (D66), and
    // "absent" has to mean "on" for both.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let skill = home.path().join(".agents/skills/s");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: s\ndescription: d\n---\nx\n",
    )
    .unwrap();

    // Env::new wrote a config with `[agent]` and nothing else; make sure there is no [sandbox].
    let path = env.policy_path();
    let mut doc: toml::Table = std::fs::read_to_string(&path).unwrap().parse().unwrap();
    doc.remove("sandbox");
    std::fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();

    let out = saddle_in(&env, home.path(), &["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("exec reads"),
        "the include rule survived: {text}"
    );
    assert!(
        text.contains("tool paths"),
        "the tool path survived: {text}"
    );
    assert!(text.contains("~/.agents"), "{text}");
}

#[test]
fn a_tool_path_may_follow_a_link_to_another_declared_path() {
    // D66: skills are installed by linking them in, so a link out of the directory is normal. The
    // target must be another directory the user declared: that is what stops a link pointing at the
    // policy, and it means the fix for a linked skill is one more entry, not a looser rule.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let real = home.path().join("elsewhere/skills/prose-lint");
    std::fs::create_dir_all(real.join("references")).unwrap();
    std::fs::write(real.join("SKILL.md"), "---\nname: prose-lint\n---\nbody\n").unwrap();
    std::fs::write(real.join("references/tells.md"), "# Tells\n").unwrap();
    let agents = home.path().join(".agents/skills");
    std::fs::create_dir_all(&agents).unwrap();
    std::os::unix::fs::symlink(&real, agents.join("prose-lint")).unwrap();

    // Undeclared: refused, and the message says what to do about it.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(
                r#"{{"path":"{}"}}"#,
                agents.join("prose-lint/SKILL.md").display()
            ),
        ],
    );
    let text = stderr(&out);
    assert!(
        text.contains("not inside any directory a [sandbox] entry names"),
        "{text}"
    );

    // Declared: the link resolves, and so does a file beneath it.
    set_sandbox_read(
        &env,
        toml::Value::Array(vec![toml::Value::Table({
            let mut t = toml::Table::new();
            t.insert("path".into(), "~/elsewhere/skills".into());
            t.insert(
                "tools".into(),
                toml::Value::Array(vec!["read".into(), "ls".into()]),
            );
            t
        })]),
    );
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(
                r#"{{"path":"{}"}}"#,
                agents.join("prose-lint/SKILL.md").display()
            ),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("body"));

    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(
                r#"{{"path":"{}"}}"#,
                agents.join("prose-lint/references/tells.md").display()
            ),
        ],
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("# Tells"),
        "{}",
        stderr(&out)
    );

    // A link may not reach the policy: the config directory is protected for every entry (D16).
    let policy = env.policy_path();
    let escape = agents.join("escape.md");
    std::os::unix::fs::symlink(&policy, &escape).unwrap();
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, escape.display()),
        ],
    );
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
}
