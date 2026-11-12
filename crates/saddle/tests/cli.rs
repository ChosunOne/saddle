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

    /// Writes this workspace's own grants, as a policy-shaped document.
    fn write_override(&self, text: &str) {
        let path = self.override_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
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
        let ws = self.ws.path().canonicalize().unwrap();
        ws.to_string_lossy()
            .trim_start_matches('/')
            .chars()
            .map(|c| match c {
                '/' => '%',
                c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '%') => c,
                _ => '_',
            })
            .collect()
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
