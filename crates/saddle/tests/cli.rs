//! End to end through the binary: the policy in force, decisions, the exec sandbox, network
//! grants and the audit log, in a throwaway workspace with its own XDG directories.

#[path = "support/launch_profiles.rs"]
mod launch_profile_tests;
#[path = "support/sandbox_overrides.rs"]
mod sandbox_override_tests;

use serde_json::json;
use std::fs;
use std::fs::Permissions;
use std::io::BufRead as _;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

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
    /// A throwaway home for every call, so the suite never reads the developer's own `~/.agents`,
    /// `~/.cargo` or config. A test that needs a home of its own passes one to `saddle_in`.
    home: tempfile::TempDir,
}

impl Env {
    fn new() -> Self {
        let env = Self {
            ws: tempfile::tempdir().unwrap(),
            xdg: tempfile::tempdir().unwrap(),
            outside: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        let ws = env.ws.path();
        fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        env.write_policy(POLICY);
        fs::write(ws.join("README"), "hello\n").unwrap();
        fs::write(ws.join(".env"), "TOKEN=1\n").unwrap();
        fs::write(env.outside.path().join("secret"), "secret\n").unwrap();
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
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ws = self.ws.path().canonicalize().unwrap();
        fs::write(
            &path,
            format!("workspace = {:?}\n{text}", ws.display().to_string()),
        )
        .unwrap();
    }

    /// The provider line at the top of the same file, for a test that needs the session to know
    /// which provider it is talking to (D82).
    fn write_config(&self, text: &str) {
        let path = self.policy_path();
        let base = fs::read_to_string(&path).unwrap_or_default();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{text}{base}")).unwrap();
    }

    /// The base grants: `[agent]` in `config.toml` (D63).
    fn write_policy(&self, text: impl AsRef<str>) {
        let agent = agent_of(text.as_ref());
        let path = self.policy_path();
        let base = fs::read_to_string(&path).unwrap_or_default();
        let mut doc: toml::Table = base.parse().unwrap_or_default();
        doc.insert("agent".into(), agent);
        let defaults: toml::Table = saddle_standard::DEFAULT_PROFILES.parse().unwrap();
        doc.entry("standard-tools")
            .or_insert_with(|| defaults["standard-tools"].clone());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
    }

    /// Changes the base grants in place, so a test does not have to spell the serialised TOML.
    fn edit_agent(&self, change: impl FnOnce(&mut toml::Table)) {
        let path = self.policy_path();
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut doc: toml::Table = text.parse().unwrap_or_default();
        let agent = doc
            .entry("agent")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        change(agent.as_table_mut().unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
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
        fs::read_to_string(self.policy_path()).unwrap()
    }

    fn saddle(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(self.ws.path())
            .args(args)
            .env("HOME", self.home.path())
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

    /// The skills a run advertised: discovery writes one `skill` record, which is what the prompt
    /// listing is built from, so this is the observable form of "the model was told about it".
    fn advertised(&self, home: &Path) -> Vec<String> {
        let out = saddle_in(self, home, &["tool", "ls", r#"{"path":"."}"#]);
        assert!(out.status.success(), "{}", stderr(&out));
        let log = self
            .audit_logs()
            .last()
            .cloned()
            .expect("a log was written");
        fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|r| r["kind"] == "skill")
            .flat_map(|r| {
                r["args"]["discovered"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s["name"].as_str().map(str::to_owned))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// Everything the runs so far recorded, as text. Every run opens its own log, so a test that
    /// makes two runs has to read both.
    fn audit(&self) -> String {
        self.audit_logs()
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn audit_logs(&self) -> Vec<PathBuf> {
        let dir = self.xdg.path().join("state/saddle/audit");
        // No run yet means no audit directory, which is not a failure.
        let Ok(entries) = fs::read_dir(dir) else {
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
fn every_shipped_tool_accepts_the_optional_host_timeout_without_guest_schema_changes() {
    let env = ready();
    env.edit_agent(|agent| {
        agent.insert(
            "transcript-read".into(),
            toml::Value::Table(toml::Table::new()),
        );
    });
    for (name, mut input, success) in [
        ("read", serde_json::json!({"path":"README"}), true),
        ("ls", serde_json::json!({"path":"."}), true),
        ("find", serde_json::json!({"pattern":"README"}), true),
        ("grep", serde_json::json!({"pattern":"hello"}), true),
        (
            "write",
            serde_json::json!({"path":"new","content":"one"}),
            true,
        ),
        (
            "edit",
            serde_json::json!({"path":"new","old":"one","new":"two"}),
            true,
        ),
        (
            "patch",
            serde_json::json!({"patch":"*** Begin Patch\n*** Add File: added\n+hello\n*** End Patch"}),
            true,
        ),
        ("bash", serde_json::json!({"command":"printf hello"}), true),
        (
            "fetch",
            serde_json::json!({"url":"https://example.test/"}),
            false,
        ),
        ("websearch", serde_json::json!({"query":"rust"}), false),
        (
            "session_query",
            serde_json::json!({"sql":"SELECT 1"}),
            false,
        ),
        ("tool_search", serde_json::json!({"query":""}), true),
        ("config_help", serde_json::json!({}), true),
    ] {
        input["timeout_secs"] = serde_json::json!(5);
        let (ok, text) = env.tool(name, input);
        assert_eq!(ok, success, "{name}: {text}");
        assert!(
            !text.contains("unknown field") && !text.contains("invalid arguments"),
            "{name}: {text}"
        );
    }
    // Invalid deadlines are rejected before any effects, and omitted timeout remains compatible.
    let (ok, text) = env.tool(
        "write",
        serde_json::json!({"path":"never-created", "content":"x", "timeout_secs":0}),
    );
    assert!(!ok && text.contains("positive integer"), "{text}");
    assert!(!env.path("never-created").exists());
    assert!(env.tool("read", serde_json::json!({"path":"README"})).0);
}

/// D171: the configuration reference is a shipped component with no reach at all, so it answers in a
/// workspace whose grants allow nothing — which is also the evidence that it is not a host tool.
#[test]
fn config_help_answers_from_a_component_that_holds_no_authority() {
    let env = ready();
    let (ok, text) = env.tool("config_help", serde_json::json!({"section": "agent"}));
    assert!(ok, "{text}");
    assert!(
        text.contains("fs-read") && text.contains("transcript-read"),
        "{text}"
    );
    let (ok, text) = env.tool("config_help", serde_json::json!({"section": "nonsense"}));
    assert!(!ok && text.contains("the sections are:"), "{text}");

    // Nothing granted, and it still answers: the guest imports the budgets snapshot and nothing
    // that reaches a file, a host or another conversation.
    env.edit_agent(toml::Table::clear);
    let (ok, text) = env.tool("config_help", serde_json::json!({}));
    assert!(ok, "{text}");
    assert!(
        text.contains("## tool-limits") && text.contains("query-rows ="),
        "{text}"
    );
    assert!(
        text.contains("in force for this session"),
        "the budgets it was given: {text}"
    );
    let (ok, text) = env.tool("read", serde_json::json!({"path": "README"}));
    assert!(!ok && text.contains("denied:"), "{text}");
}

#[test]
fn fetch_total_timeout_cuts_off_host_io_and_keeps_the_audit_chain_valid() {
    let env = ready();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let done = stop.clone();
    let seen = Arc::new(AtomicBool::new(false));
    let accepted = seen.clone();
    let server = thread::spawn(move || {
        let mut streams = Vec::new();
        while !done.load(Ordering::Relaxed) {
            if let Ok((stream, _)) = listener.accept() {
                streams.push(stream); // Accept, but deliberately send no response.
                accepted.store(true, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(5));
        }
    });
    env.write_policy(format!(
        "{POLICY}\nnet=[{{host='127.0.0.1',scheme='http',port={port}}}]\n"
    ));
    let start = Instant::now();
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({"url":format!("http://127.0.0.1:{port}/"), "timeout_secs":1}),
    );
    let elapsed = start.elapsed();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    assert!(
        seen.load(Ordering::Relaxed),
        "request did not reach the mock: {text}"
    );
    assert!(!ok && text.contains("timed out"), "{text}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert!(env.tool("read", serde_json::json!({"path":"README"})).0);
    for log in env.audit_logs() {
        let out = env.saddle(&["audit", "verify", log.to_str().unwrap()]);
        assert!(out.status.success(), "{}", stderr(&out));
    }
}

#[test]
fn shipped_components_honor_runtime_envelopes_without_rebuilding_code() {
    let env = Env::new();
    let mut config: toml::Table = env.policy().parse().unwrap();
    let limits: toml::Table = "plugin-memory-bytes=67108864\nplugin-fuel=200000000\nplugin-timeout-ms=5000\nplugin-stack-bytes=1048576\nrange-bytes=2\n".parse().unwrap();
    config.insert("tool-limits".into(), limits.into());
    fs::write(env.policy_path(), toml::to_string(&config).unwrap()).unwrap();
    let (ok, text) = env.tool("read", serde_json::json!({"path":"README"}));
    assert!(ok && text.contains("hello"), "{text}");
}

#[test]
fn existing_configs_need_explicit_standard_profiles_and_omitted_scopes_never_inherit() {
    let env = Env::new();
    let mut doc: toml::Table = env.policy().parse().unwrap();
    doc.remove("standard-tools");
    fs::write(env.policy_path(), toml::to_string(&doc).unwrap()).unwrap();
    let (ok, text) = env.tool("read", serde_json::json!({"path":"README"}));
    assert!(!ok && text.contains("no native fallback"), "{text}");
    // the refusal names the row to add, not only the mechanism that refused (D182)
    assert!(
        text.contains(r#"read = ["fs-read", "tool-read"]"#),
        "{text}"
    );
    let mut profiles = toml::Table::new();
    profiles.insert(
        "write".into(),
        serde_json::from_value::<toml::Value>(serde_json::json!(["fs-write"])).unwrap(),
    );
    doc.insert("standard-tools".into(), profiles.into());
    fs::write(env.policy_path(), toml::to_string(&doc).unwrap()).unwrap();
    let (ok, text) = env.tool(
        "write",
        serde_json::json!({"path":"README","content":"must not overwrite"}),
    );
    assert!(!ok && text.contains("denied:"), "{text}");
    assert_eq!(
        fs::read_to_string(env.ws.path().join("README")).unwrap(),
        "hello\n"
    );
    let (ok, _) = env.tool(
        "write",
        serde_json::json!({"path":"new","content":"must not turn refusal into absence"}),
    );
    assert!(!ok);
    assert!(!env.ws.path().join("new").exists());
    profiles =
        toml::from_str::<toml::Table>(saddle_standard::DEFAULT_PROFILES).unwrap()["standard-tools"]
            .as_table()
            .unwrap()
            .clone();
    doc.insert("standard-tools".into(), profiles.into());
    fs::write(env.policy_path(), toml::to_string(&doc).unwrap()).unwrap();
    assert!(env.tool("read", serde_json::json!({"path":"README"})).0);
}

#[test]
fn first_run_includes_and_enables_shipped_tools_without_manual_configuration() {
    let env = Env::new();
    // Remove the helper's explicit configuration so the real first-run path supplies everything.
    fs::remove_file(env.policy_path()).unwrap();
    let (ok, text) = env.tool("read", serde_json::json!({"path":"README"}));
    assert!(ok && text.trim_end() == "hello", "{text}");

    let config: toml::Table = env.policy().parse().unwrap();
    let recommended: toml::Table = saddle_standard::DEFAULT_PROFILES.parse().unwrap();
    assert_eq!(config["standard-tools"], recommended["standard-tools"]);
    assert_eq!(
        config["standard-tools"].as_table().unwrap().len(),
        saddle_standard::ARTIFACTS.len()
    );
    assert!(config["agent"].get("net").is_none());

    let listing = env.saddle(&["plugin", "list"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    let listing = String::from_utf8_lossy(&listing.stdout);
    for artifact in saddle_standard::ARTIFACTS {
        assert!(config["standard-tools"].get(artifact.name).is_some());
        let status = if matches!(artifact.name, "fetch" | "websearch" | "run_binary") {
            // Enabled tools never invent network or named launch grants missing from defaults.
            "standard tool unavailable:"
        } else {
            "ready: shipped standard tool"
        };
        assert!(
            listing.contains(&format!("{} · {status}", artifact.id)),
            "{listing}"
        );
    }
}

#[test]
fn shipped_tool_inventory_and_delegation_template_do_not_modify_existing_config() {
    let env = Env::new();
    let before = fs::read(env.policy_path()).unwrap();
    let listing = env.saddle(&["tools"]);
    assert!(listing.status.success(), "{}", stderr(&listing));
    let stdout = String::from_utf8_lossy(&listing.stdout);
    for artifact in saddle_standard::ARTIFACTS {
        assert!(stdout.contains(artifact.sha256));
    }
    let template = env.saddle(&["tools", "--config"]);
    assert!(template.status.success(), "{}", stderr(&template));
    let config: toml::Table = String::from_utf8_lossy(&template.stdout).parse().unwrap();
    assert_eq!(
        config["standard-tools"].as_table().unwrap().len(),
        saddle_standard::ARTIFACTS.len()
    );
    assert_eq!(fs::read(env.policy_path()).unwrap(), before);
}

#[test]
fn a_profile_is_layered_between_the_base_grants_and_the_project() {
    // D176: the order is `[agent]`, then the profile, then this workspace's own file. A profile
    // names what it changes and inherits the rest, so switching sets do not mean copying the base.
    let env = Env::new();
    env.write_policy(POLICY);
    env.edit_agent(|agent| {
        let mut grant = toml::Table::new();
        grant.insert("host".into(), toml::Value::String("api.example.com".into()));
        agent.insert(
            "net".into(),
            toml::Value::Array(vec![toml::Value::Table(grant)]),
        );
    });
    let path = env.policy_path();
    let mut doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    doc.insert(
        "profiles".into(),
        toml::Value::Table(
            concat!(
                "[build]\n",
                "exec = { timeout-secs = 60 }\n\n",
                "[offline]\nnet = []\n\n",
                "[narrow]\nnet = [\"-api.example.com\"]\n\n",
                "[typo]\nnet = [\"-nope.example\"]\n\n",
                "[review]\n",
            )
            .parse::<toml::Table>()
            .unwrap(),
        ),
    );
    let write = |doc: &toml::Table| fs::write(&path, toml::to_string(doc).unwrap()).unwrap();
    write(&doc);

    // `[agent]` as written, with the alternatives named.
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("profile    none ([agent] as written)"),
        "{text}"
    );
    assert!(
        text.contains("defined: build, narrow, offline, review, typo"),
        "{text}"
    );
    assert!(text.contains("agent.exec = timeout 20s"), "{text}");

    // The profile's own scope replaces the base's, and everything else is inherited.
    let out = env.saddle(&["--profile", "build", "check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.contains("profile    build"), "{text}");
    assert!(text.contains("agent.exec = timeout 60s"), "{text}");
    assert!(
        text.contains("agent.fs-read[0] = workspace"),
        "the base is inherited: {text}"
    );
    assert!(
        text.contains("agent.net[0] = https://api.example.com:443"),
        "inherited from the base too: {text}"
    );

    // A list scope can be emptied by the profile, which is how one narrows; a table scope cannot.
    let out = env.saddle(&["--profile", "offline", "check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !text.contains("agent.net["),
        "the profile emptied it: {text}"
    );
    assert!(text.contains("agent.exec = timeout 20s"), "{text}");

    // A `-name` entry takes one grant away without restating the ones it keeps (D177), and what
    // the base gave is still there.
    let out = env.saddle(&["--profile", "narrow", "check"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !text.contains("agent.net["),
        "the one host is subtracted: {text}"
    );
    assert!(
        text.contains("agent.exec = timeout 20s"),
        "and the scopes it does not name are inherited: {text}"
    );

    // A name that nothing inherits is a mistake in that profile, not a no-op, and it says so.
    let out = env.saddle(&["--profile", "typo", "check"]);
    assert!(!out.status.success());
    let e = stderr(&out);
    assert!(e.contains("profiles.typo"), "{e}");
    assert!(e.contains("nothing inherited matches -nope.example"), "{e}");
    assert!(e.contains("api.example.com"), "it names what is there: {e}");

    // This workspace's own file is layered on top of both (D63).
    env.write_override("version = 1\n[agent]\nexec = { timeout-secs = 7 }\n");
    let out = env.saddle(&["--profile", "build", "check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("agent.exec = timeout 7s"),
        "the project's file is last: {text}"
    );
    assert!(
        text.contains("agent.net[0] = https://api.example.com:443"),
        "and it leaves what it does not name alone: {text}"
    );

    // A name that is not defined is refused, never quietly replaced by `[agent]`.
    let out = env.saddle(&["--profile", "nope", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no profile named `nope`"),
        "{}",
        stderr(&out)
    );

    // The file's own default is what a session starts on, and `none` goes back to `[agent]`.
    doc.insert("profile".into(), toml::Value::String("build".into()));
    write(&doc);
    let out = env.saddle(&["check"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("profile    build"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = env.saddle(&["--profile", "none", "check"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("profile    none ([agent] as written)"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The project's file subtracts in the same form, from what the profile left (D177).
    env.write_override("version = 1\n[agent]\nfs-read = [\"-workspace\"]\n");
    let out = env.saddle(&["--profile", "build", "check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        !text.contains("agent.fs-read["),
        "the project took the root away: {text}"
    );
    assert!(
        text.contains("agent.net[0] = https://api.example.com:443"),
        "and left the rest of the base alone: {text}"
    );
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
    fs::write(
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
        fs::read_to_string(env.path("src/new.rs")).unwrap(),
        "fn main() {}"
    );

    let (ok, text) = env.tool(
        "edit",
        serde_json::json!({"path": "README", "old": "hello", "new": "goodbye"}),
    );
    assert!(ok, "{text}");
    assert_eq!(fs::read_to_string(env.path("README")).unwrap(), "goodbye\n");

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
    // D178: nothing inside the workspace is reserved, `.git` and `saddle.toml` included; the grant
    // on the root decides, as it does for every other path.
    for path in [".git/hooks/pre-commit", "saddle.toml"] {
        let (ok, text) = env.tool("write", serde_json::json!({"path": path, "content": "x"}));
        assert!(ok, "{path}: {text}");
        assert_eq!(fs::read_to_string(env.path(path)).unwrap(), "x");
    }
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

    // A grant is a grant (D178): a link into `.git` is decided on its target, and the target is
    // inside the workspace write grant, so it is written.
    unix_fs::symlink(".git/hooks/pre-commit", env.path("innocent")).unwrap();
    let (ok, text) = env.tool(
        "write",
        serde_json::json!({"path": "innocent", "content": "x"}),
    );
    assert!(ok, "{text}");
    assert_eq!(
        fs::read_to_string(env.path(".git/hooks/pre-commit")).unwrap(),
        "x"
    );
}

#[test]
fn bash_runs_sandboxed_and_vcs_metadata_is_the_workspace_s_own() {
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
    assert_eq!(fs::read_to_string(env.path("out.txt")).unwrap(), "built\n");

    let secret = env.outside.path().join("secret");
    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": format!("cat {}", secret.display())}),
    );
    assert!(!ok && text.contains("Permission denied"), "{text}");

    // D178: `.git` is inside the workspace grant, so what a command writes there is its own —
    // `git remote add` and an installer write exactly these paths as ordinary work.
    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": "echo hook > .git/hooks/pre-commit && echo 'fsmonitor = cargo-watch' >> .git/config"}),
    );
    assert!(ok, "{text}");
    assert_eq!(
        fs::read_to_string(env.path(".git/hooks/pre-commit")).unwrap(),
        "hook\n"
    );
    assert_eq!(
        fs::read_to_string(env.path(".git/config")).unwrap(),
        "[core]\nfsmonitor = cargo-watch\n"
    );

    let (ok, text) = env.tool(
        "bash",
        serde_json::json!({"command": "sleep 30", "timeout_secs": 1}),
    );
    assert!(!ok && text.contains("timed out"), "{text}");
}

#[test]
fn a_provider_key_never_reaches_a_command_even_when_the_exec_grant_names_it() {
    if let Err(reason) = saddle_sandbox::availability() {
        eprintln!("skipping: {reason}");
        return;
    }
    let env = ready();
    env.edit_agent(|agent| {
        let exec = agent["exec"].as_table_mut().unwrap();
        exec.insert(
            "env".into(),
            toml::Value::Array(vec!["ANTHROPIC_API_KEY".into(), "SADDLE_VISIBLE".into()]),
        );
    });
    // `saddle tool` opens its session without the interactive start-up, which is where the strip
    // used to be, so it is the path that would show the key (D19).
    let input = serde_json::json!({
        "command": "printf '%s|%s' \"${ANTHROPIC_API_KEY-unset}\" \"${SADDLE_VISIBLE-unset}\""
    });
    let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(env.ws.path())
        .args(["tool", "bash", &input.to_string()])
        .env("HOME", env.home.path())
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .env("ANTHROPIC_API_KEY", "sk-never-shown")
        .env("SADDLE_VISIBLE", "shown")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("unset|shown"), "{text}");
    assert!(!text.contains("sk-never-shown"), "{text}");
}

#[test]
fn every_decision_is_logged_and_the_chain_verifies() {
    let env = ready();
    env.tool("read", serde_json::json!({"path": "README"}));
    env.tool("write", serde_json::json!({"path": ".env", "content": "x"}));
    let logs = env.audit_logs();
    assert_eq!(logs.len(), 2);

    let records: Vec<serde_json::Value> = fs::read_to_string(&logs[1])
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(records.first().unwrap()["kind"], "session-start");
    assert_eq!(records.last().unwrap()["kind"], "session-end");
    let d = records
        .iter()
        .find(|r| r["scope"] == "fs-write" && r["allow"] == false)
        .expect("the excluded write is denied");
    assert_eq!(d["principal"], "plugin.standard-write");
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
    let text = fs::read_to_string(&logs[1]).unwrap();
    let kept: Vec<&str> = text.lines().take(2).collect();
    fs::write(&logs[1], kept.join("\n") + "\n").unwrap();
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
    assert!(text.contains("needs scope `net`"), "{text}");
    // No adapter is initialized without net; the attempted tool call is audited, not a fictitious GET.
    let logs = env.audit_logs();
    let record = fs::read_to_string(logs.last().unwrap()).unwrap();
    assert!(record.contains("\"allow\":false"));
    // A grant for one host does not cover another.
    env.edit_agent(|agent| {
        agent.insert(
            "net".into(),
            toml::from_str::<toml::Value>("net=[{host='api.example.com'}]").unwrap()["net"].clone(),
        );
    });
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
fn a_private_fetch_refusal_is_audited_without_header_values_or_query_secrets() {
    let env = Env::new();
    env.edit_agent(|agent| {
        agent.insert(
            "net".into(),
            toml::from_str::<toml::Value>("net = [{ host = \"*\" }]")
                .unwrap()
                .get("net")
                .cloned()
                .unwrap(),
        );
    });
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({
            "url": "https://127.0.0.1/private?token=query-secret",
            "headers": {"authorization": "Bearer header-secret"},
        }),
    );
    assert!(!ok, "{text}");
    assert!(text.starts_with("denied:"), "{text}");
    let logs = env.audit_logs();
    let log = fs::read_to_string(logs.last().unwrap()).unwrap();
    let decisions: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|r| r["kind"] == "decision" && r["scope"] == "net")
        .collect();
    assert_eq!(decisions.len(), 1, "{log}");
    let decision = &decisions[0];
    assert_eq!(decision["allow"], false);
    assert_eq!(decision["principal"], "plugin.standard-fetch");
    assert_eq!(decision["args"]["method"], "GET");
    assert_eq!(
        decision["args"]["url"],
        "https://127.0.0.1:443/private?token=…"
    );
    assert_eq!(
        decision["args"]["headers"],
        serde_json::json!(["authorization"])
    );
    assert!(text.starts_with(&format!(
        "denied: {}",
        decision["explanation"].as_str().unwrap()
    )));
    assert!(!log.contains("query-secret"), "{log}");
    assert!(!log.contains("header-secret"), "{log}");
    let out = env.saddle(&["audit", "verify", logs.last().unwrap().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
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

    // Naming the address does open it, which is the escape hatch the rule is built around. The
    // port is one nothing listens on, on purpose: a test that used 80 would be asking whether
    // something happens to be bound there, and inside a command's network namespace something is
    // (D79).
    env.edit_agent(|agent| {
        agent.insert(
            "net".into(),
            toml::from_str::<toml::Value>(
                "net = [{ host = \"*\", scheme = \"http\", why = \"anything\" }, \
                 { host = \"127.0.0.1\", scheme = \"http\", port = 59999, \
                   why = \"a local server\" }]",
            )
            .unwrap()
            .get("net")
            .cloned()
            .unwrap(),
        );
    });
    let (ok, text) = env.tool(
        "fetch",
        serde_json::json!({"url": "http://127.0.0.1:59999/v1/models"}),
    );
    assert!(!ok, "{text}");
    assert!(
        !text.contains("loopback"),
        "a named address is reachable: {text}"
    );
}

#[test]
fn websearch_is_a_tool_and_the_gate_decides_it() {
    // D82: the search host is decided like any other request. With no provider configured, the tool
    // falls back to Exa — and that path needs Exa's host, not a vendor's name.
    let env = Env::new();
    let (ok, text) = env.tool("websearch", serde_json::json!({ "query": "anything" }));
    assert!(!ok, "{text}");
    assert!(text.contains("needs scope `net`"), "{text}");
    // A grant for a different Exa host is not a grant for this one.
    env.write_policy(POLICY.replace(
        "[agent]",
        "[agent]\nnet = [{ host = \"api.exa.ai\", why = \"the keyed search\" }]",
    ));
    let (ok, text) = env.tool("websearch", serde_json::json!({ "query": "anything" }));
    assert!(!ok, "{text}");
    assert!(text.contains("mcp.exa.ai"), "{text}");
    assert!(!text.contains("api.exa.ai"), "{text}");

    // The second step of a search needs a search to expand (D82). No network is involved in the
    // refusal, which is what makes it testable here.
    let (ok, text) = env.tool("websearch", serde_json::json!({ "result": 1 }));
    assert!(!ok, "{text}");
    assert!(text.contains("no search to expand"), "{text}");
    // Neither field is a refusal that says what to pass.
    let (ok, text) = env.tool("websearch", serde_json::json!({}));
    assert!(!ok, "{text}");
    assert!(text.contains("`query` is empty"), "{text}");
    // A result number is a number, and 1-based.
    let (ok, text) = env.tool(
        "websearch",
        serde_json::json!({ "query": "x", "result": 0 }),
    );
    assert!(!ok, "{text}");
    assert!(text.contains("positive whole number"), "{text}");

    // An unknown tool names the ones that exist, so the catalogue is readable from the binary.
    let refused = env.saddle(&["tool", "nonesuch", "{}"]);
    let listed = String::from_utf8_lossy(&refused.stderr);
    assert!(listed.contains("websearch"), "{listed}");
}

#[test]
fn a_provider_that_searches_is_asked_before_exa() {
    // D82: Synthetic serves `/v2/search` beside `/v2/quotas`, so a session whose provider is
    // Synthetic searches there, with the provider's own key, and never reaches Exa.
    let env = Env::new();
    env.write_config("provider = \"synthetic\"\n");
    // A grant for Exa's hosts alone must not let the provider's search through, which is what shows
    // the request goes to the provider and is decided on the host it actually names.
    env.write_policy(POLICY.replace(
        "[agent]",
        "[agent]\nnet = [{ host = \"mcp.exa.ai\" }, { host = \"api.exa.ai\" }]",
    ));
    let (ok, text) = env.tool("websearch", serde_json::json!({ "query": "anything" }));
    assert!(!ok, "{text}");
    assert!(text.contains("api.synthetic.new"), "{text}");
    assert!(!text.contains("exa.ai"), "{text}");

    // The grant the search actually needs is the one its refusal named.
    env.write_policy(POLICY.replace(
        "[agent]",
        "[agent]\nnet = [{ host = \"api.synthetic.new\", why = \"the provider's search\" }]",
    ));
    let (ok, text) = env.tool("websearch", serde_json::json!({ "query": "anything" }));
    // No key is stored in a throwaway state directory, so the request is refused before it is made
    // — and the refusal says which variable to set.
    assert!(!ok, "{text}");
    assert!(text.contains("SYNTHETIC_API_KEY"), "{text}");
    assert!(text.contains("saddle login synthetic"), "{text}");
}

#[test]
fn check_says_what_network_a_command_will_get() {
    // D79: a command runs in its own namespace with one door to the proxy, or with no network at
    // all when a namespace cannot be made. Which one it will be is not something a person should
    // have to run a command to find out.
    let env = Env::new();
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("net        a command gets "), "{text}");
}

#[test]
fn check_says_which_mcp_servers_are_declared_and_granted() {
    // D104: a server that is declared and not granted, or granted and not declared, would
    // otherwise show up as a server that silently never starts.
    let env = Env::new();
    env.write_config(
        r#"
[mcp.scout]
description = "scrapes boards"
command = "npx"
args = ["-y", "job-scout"]

[mcp.remote]
url = "https://mcp.example.com/mcp"
"#,
    );
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
mcp     = [{ server = "scout", tools = ["search"] }, { server = "ghost", tools = ["x"] }]
"#,
    );
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("mcp        scout · stdio npx · granted [search]"),
        "{text}"
    );
    assert!(
        text.contains("mcp        remote · http https://mcp.example.com/mcp · not granted"),
        "{text}"
    );
    assert!(
        text.contains("ghost · granted, but no [mcp.ghost] table"),
        "{text}"
    );
}

/// A loopback streamable-HTTP MCP server: JSON for `initialize`, an event stream for `tools/list`,
/// so both framings a server may answer with are exercised (D110). It stops when the test says so,
/// because a refused request never reaches it and would otherwise leave a thread waiting forever.
fn mock_http_mcp() -> (String, Arc<AtomicUsize>, Arc<AtomicBool>, JoinHandle<()>) {
    mock_http_mcp_with_slow_call(false)
}

fn mock_http_mcp_with_slow_call(
    slow: bool,
) -> (String, Arc<AtomicUsize>, Arc<AtomicBool>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    let seen = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::clone(&seen);
    let done = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
            };
            count.fetch_add(1, Ordering::SeqCst);
            let mut stream = stream;
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // Headers first, then exactly as many bytes of body as they announced.
            loop {
                match Read::read(&mut stream, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(head) = text.find("\r\n\r\n") {
                    let length = text[..head]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if raw.len() >= head + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let body = text
                .find("\r\n\r\n")
                .map(|at| &text[at + 4..])
                .unwrap_or_default();
            if slow && body.contains("tools/call") {
                assert!(
                    !body.contains("timeout_secs"),
                    "host option leaked to MCP server: {body}"
                );
                while !done.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                break;
            }
            let answer = if body.contains("tools/list") {
                // An event stream, which is what a server may answer any request with.
                let event = r#"data: {"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"says it back","inputSchema":{}}]}}

"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{event}",
                    event.len()
                )
            } else if body.contains("notifications/initialized") {
                "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
            } else {
                let reply = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"mock"}}}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: mock-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
            };
            let _ = Write::write_all(&mut stream, answer.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), seen, stop, handle)
}

#[test]
fn remote_mcp_tool_timeout_is_shared_across_startup_and_the_call() {
    let env = Env::new();
    let (url, seen, stop, handle) = mock_http_mcp_with_slow_call(true);
    let port = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .rsplit(':')
        .next()
        .unwrap();
    env.write_config(&format!("\n[mcp.remote]\nurl={url:?}\ntimeout=30\n"));
    env.write_policy(format!(
        r#"[agent]
net=[{{host='127.0.0.1',scheme='http',port={port}}}]
mcp=[{{server='remote',tools=['echo'],net=[{{host='127.0.0.1',scheme='http',port={port}}}]}}]
"#
    ));
    let start = Instant::now();
    let out = env.saddle(&["mcp", "call", "remote", "echo", r#"{"timeout_secs":1}"#]);
    let elapsed = start.elapsed();
    stop.store(true, Ordering::SeqCst);
    handle.join().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success() && text.contains("timed out"),
        "{text} {}",
        stderr(&out)
    );
    assert_eq!(seen.load(Ordering::SeqCst), 4);
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}

#[test]
fn a_request_from_the_server_is_answered_so_the_listing_can_finish() {
    // D114: a server may ask the client something of its own. This one asks for the roots before
    // it will hand over its catalogue, so a listing that completes at all proves the answer went
    // back down the pipe — and that it carried the workspace root rather than an empty list.
    let env = Env::new();
    let script = env.ws.path().join("asking-mcp.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
asked=0
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}\n' ;;
    *'"tools/list"'*)
      if [ "$asked" = 1 ]; then
        printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}}]}}\n'
      else
        asked=1
        printf '{"jsonrpc":"2.0","id":99,"method":"roots/list","params":{}}\n'
      fi ;;
    *'"id":99'*)
      case "$line" in
        *file://*) printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}}]}}\n' ;;
        *) printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n' ;;
      esac ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    env.write_config(&format!(
        "\n[mcp.fake]\ncommand = {:?}\n",
        script.display().to_string()
    ));
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
exec    = { timeout-secs = 20 }
mcp     = [{ server = "fake", tools = ["search"] }]
"#,
    );
    let out = env.saddle(&["mcp", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("fake  1 tool(s)"),
        "the server's own request was not answered with the roots: {text}"
    );
}

#[test]
fn a_server_that_never_answers_is_cut_off_at_the_deadline_its_table_names() {
    // D113: `[mcp.<name>] timeout` is a promise the config makes, so it is the deadline actually
    // enforced rather than a constant. Two seconds, and the message says which deadline it was.
    let env = Env::new();
    let script = env.ws.path().join("mute-mcp.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}\n' ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}}]}}\n' ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    env.write_config(&format!(
        "\n[mcp.fake]\ncommand = {:?}\ntimeout = 2\n",
        script.display().to_string()
    ));
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
exec    = { timeout-secs = 20 }
mcp     = [{ server = "fake", tools = ["search"] }]
"#,
    );
    let began = Instant::now();
    let out = env.saddle(&["mcp", "call", "fake", "search", "{}"]);
    let took = began.elapsed();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("within 2s"),
        "the refusal does not name the deadline: {text}"
    );
    assert!(
        took < Duration::from_secs(20),
        "it waited past the deadline its table names: {took:?}"
    );
    let began = Instant::now();
    let out = env.saddle(&["mcp", "call", "fake", "search", r#"{"timeout_secs":1}"#]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success() && text.contains("timed out"),
        "{text}"
    );
    assert!(began.elapsed() < Duration::from_secs(5));
}

/// A loopback authorization server: metadata, registration and a token endpoint, each echoing the
/// `client_id` it was given so the exchange can be checked end to end.
fn mock_auth_server() -> (String, Arc<AtomicBool>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let base = format!("http://127.0.0.1:{port}");
        while !done.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
            };
            let mut stream = stream;
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match Read::read(&mut stream, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(head) = text.find("\r\n\r\n") {
                    let length = text[..head]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if raw.len() >= head + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let request_line = text.lines().next().unwrap_or_default().to_owned();
            let body = text
                .find("\r\n\r\n")
                .map(|at| text[at + 4..].to_owned())
                .unwrap_or_default();
            let answer = if request_line.contains("/.well-known/oauth-protected-resource") {
                format!(r#"{{"resource":"{base}/mcp","authorization_servers":["{base}"]}}"#)
            } else if request_line.contains("/.well-known/oauth-authorization-server") {
                format!(
                    r#"{{"issuer":"{base}","authorization_endpoint":"{base}/authorize","token_endpoint":"{base}/token","registration_endpoint":"{base}/register"}}"#
                )
            } else if request_line.contains("/register") {
                r#"{"client_id":"saddle-test-client"}"#.to_owned()
            } else if request_line.contains("/token") {
                // The code the test sends back is echoed in the access token, so the assertion can
                // see the whole round trip and not just that something was stored.
                let code = body
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("code="))
                    .unwrap_or("none")
                    .to_owned();
                format!(
                    r#"{{"access_token":"access-{code}","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}}"#
                )
            } else {
                r#"{"error":"not found"}"#.to_owned()
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            let _ = Write::write_all(&mut stream, header.as_bytes());
            let _ = Write::write_all(&mut stream, answer.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), stop, handle)
}

/// The headers a mock server was sent, kept for the assertion.
type SeenHeaders = Arc<Mutex<Vec<String>>>;

/// An MCP server that wants a bearer token: 401 without one, and the value it saw kept for the
/// assertion (D116).
fn mock_token_mcp() -> (String, SeenHeaders, Arc<AtomicBool>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
    let port = listener.local_addr().expect("the bound address").port();
    listener
        .set_nonblocking(true)
        .expect("a non-blocking listener");
    let seen: SeenHeaders = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let record = Arc::clone(&seen);
    let done = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !done.load(Ordering::SeqCst) {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(_) => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
            };
            let mut stream = stream;
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match Read::read(&mut stream, &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some(head) = text.find("\r\n\r\n") {
                    let length = text[..head]
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if raw.len() >= head + 4 + length {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            let body = text
                .find("\r\n\r\n")
                .map(|at| text[at + 4..].to_owned())
                .unwrap_or_default();
            let token = text.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("authorization")
                    .then(|| value.trim().to_owned())
            });
            if let Some(token) = &token
                && let Ok(mut held) = record.lock()
            {
                held.push(token.clone());
            }
            let (status, answer) = if token.as_deref() == Some("Bearer planted-token") {
                let reply = if body.contains("tools/list") {
                    r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"says it back","inputSchema":{}}]}}"#
                } else if body.contains("tools/call") {
                    r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"it works"}]}}"#
                } else if body.contains("notifications/initialized") {
                    ""
                } else {
                    r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}"#
                };
                ("200 OK", reply.to_owned())
            } else {
                (
                    "401 Unauthorized",
                    r#"{"error":"token required"}"#.to_owned(),
                )
            };
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            let _ = Write::write_all(&mut stream, head.as_bytes());
            let _ = Write::write_all(&mut stream, answer.as_bytes());
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), seen, stop, handle)
}

#[test]
fn mcp_logout_forgets_the_token_so_the_next_call_says_how_to_sign_in() {
    // D121: signing out is removing the one entry a token lives in. Afterwards the call is refused
    // with the command that fixes it, rather than retried without a token.
    let env = Env::new();
    let auth_path = env.xdg.path().join("state/saddle/auth.json");
    fs::create_dir_all(auth_path.parent().expect("the state directory")).unwrap();
    fs::write(
        &auth_path,
        r#"{"mcp:remote":{"oauth":{"access":"a-token","refresh":"r","expires-at":0}},"openai":{"api-key":"sk-keep"}}"#,
    )
    .unwrap();

    let out = env.saddle(&["mcp", "logout", "remote"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("signed out"), "{text}");

    let stored = fs::read_to_string(&auth_path).unwrap();
    assert!(!stored.contains("mcp:remote"), "{stored}");
    assert!(
        stored.contains("sk-keep"),
        "another provider's credential was removed too: {stored}"
    );

    // Nothing to forget the second time, and that is not a failure.
    let out = env.saddle(&["mcp", "logout", "remote"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("nothing to forget"), "{text}");
}

#[test]
fn the_transport_sends_the_stored_token_and_says_how_to_get_one() {
    // D116: a token the person obtained is sent as a bearer, and a server that refuses without one
    // is told nothing by a retry — the error has to name the command that fixes it.
    let env = Env::new();
    let (url, seen, stop, handle) = mock_token_mcp();
    env.write_config(&format!("\n[mcp.remote]\nurl = {url:?}\n"));
    let port = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .and_then(|authority| authority.rsplit(':').next())
        .expect("a port in the url");
    env.write_policy(format!(
        r#"
version = 1

[agent]
fs-read = [{{ root = "workspace" }}]
net     = [{{ host = "127.0.0.1", scheme = "http", port = {port} }}]
mcp     = [{{ server = "remote", tools = ["echo"], net = [{{ host = "127.0.0.1", scheme = "http", port = {port} }}] }}]
"#
    ));

    // No token yet: the failure has to say which command obtains one.
    let out = env.saddle(&["mcp", "call", "remote", "echo", "{}"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("saddle mcp auth remote"),
        "the refusal does not say how to sign in: {text}"
    );

    // The token the person stored is sent, and the call completes.
    let auth_path = env.xdg.path().join("state/saddle/auth.json");
    fs::create_dir_all(auth_path.parent().expect("the state directory")).unwrap();
    fs::write(
        &auth_path,
        // `AuthStore` is transparent over its map, so the key is the top level.
        r#"{"mcp:remote":{"oauth":{"access":"planted-token","refresh":"","expires-at":0}}}"#,
    )
    .unwrap();
    let out = env.saddle(&["mcp", "call", "remote", "echo", "{}"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("it works"), "{text}");
    let tokens = seen.lock().expect("the record").clone();
    assert!(
        tokens.iter().any(|t| t == "Bearer planted-token"),
        "the transport did not send the stored token: {tokens:?}"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn mcp_auth_signs_in_and_stores_the_token_outside_every_root() {
    // D115: the whole flow, driven by the test pretending to be the browser. The URL is taken from
    // what saddle printed, the callback is sent back with the state it carried, and what is stored
    // is the token from the round trip — in `auth.json`, which no command can read.
    let env = Env::new();
    let (url, stop, handle) = mock_auth_server();
    env.write_config(&format!("\n[mcp.remote]\nurl = {url:?}\n"));
    let port = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .and_then(|authority| authority.rsplit(':').next())
        .expect("a port in the url");
    env.write_policy(format!(
        r#"
version = 1

[agent]
fs-read = [{{ root = "workspace" }}]
net     = [{{ host = "127.0.0.1", scheme = "http", port = {port} }}]
mcp     = [{{ server = "remote", tools = ["echo"], net = [{{ host = "127.0.0.1", scheme = "http", port = {port} }}] }}]
"#
    ));

    let mut child = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .args(["mcp", "auth", "remote"])
        .current_dir(env.ws.path())
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("HOME", env.home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning saddle mcp auth");

    // Read until the URL, then behave like the browser: a GET on the redirect the URL names.
    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let mut printed = String::new();
    let mut authorize = String::new();
    while let Some(Ok(line)) = lines.next() {
        printed.push_str(&line);
        printed.push('\n');
        if let Some(rest) = line.trim().strip_prefix("http") {
            authorize = format!("http{rest}");
            break;
        }
    }
    assert!(
        !authorize.is_empty(),
        "no sign-in URL was printed: {printed}"
    );
    let redirect = authorize
        .split("redirect_uri=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .map(|encoded| encoded.replace("%3A", ":").replace("%2F", "/"))
        .expect("a redirect_uri in the url");
    let state = authorize
        .split("state=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .expect("a state in the url")
        .to_owned();
    let authority = redirect
        .trim_start_matches("http://")
        .split('/')
        .next()
        .expect("a host in the redirect");
    let mut browser = TcpStream::connect(authority).expect("reaching the callback");
    Write::write_all(
        &mut browser,
        format!("GET /callback?code=the-code&state={state} HTTP/1.1\r\nhost: {authority}\r\n\r\n")
            .as_bytes(),
    )
    .expect("sending the callback");
    let status = child.wait().expect("waiting for the sign-in");
    let rest: String = lines.map_while(Result::ok).collect();
    assert!(status.success(), "the sign-in failed: {printed}{rest}");

    let stored = fs::read_to_string(env.xdg.path().join("state/saddle/auth.json"))
        .expect("auth.json was not written");
    assert!(
        stored.contains("access-the-code"),
        "the token from the exchange was not stored: {stored}"
    );
    assert!(stored.contains("mcp:remote"), "{stored}");
    assert!(
        !env.ws.path().join("auth.json").exists(),
        "the token must not land inside the workspace"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn a_table_says_how_its_tools_reach_the_model_and_codemode_is_refused() {
    // D117: exposure is the table's own word for what reaches the prompt. `direct` is the one a
    // small frequent set wants, and `codemode` — pi's default — is a load error here rather than a
    // silent fall back to deferring, because a person who asked for it asked for something saddle
    // does not do.
    let env = Env::new();
    let script = env.ws.path().join("fake-mcp.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}\n' ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}}]}}\n' ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    let table = format!(
        "\n[mcp.fake]\ncommand = {:?}\nexposure = \"direct\"\n",
        script.display().to_string()
    );
    env.write_config(&table);
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
exec    = { timeout-secs = 20 }
mcp     = [{ server = "fake", tools = ["search"] }]
"#,
    );
    let out = env.saddle(&["check"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("in the prompt"),
        "check does not say the tools are declared up front: {text}"
    );
    let out = env.saddle(&["mcp", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("direct"),
        "the listing hides the exposure: {text}"
    );

    // `write_config` prepends, so the codemode case gets its own workspace rather than a second
    // `[mcp.fake]` table in the same file.
    let env = Env::new();
    let script = env.ws.path().join("fake-mcp.sh");
    fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    env.write_config(&format!(
        "\n[mcp.fake]\ncommand = {:?}\nexposure = \"codemode\"\n",
        script.display().to_string()
    ));
    let out = env.saddle(&["check"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("codemode"),
        "the refusal does not name what was asked for: {text}"
    );
}

#[test]
fn mcp_call_runs_one_tool_as_the_model_would_and_refuses_an_ungranted_one() {
    // D111: one call without a conversation around it, decided by the same grant the model's call
    // is decided by. It is what a person checks a server with, and the only way to reach a remote
    // one before a search has declared anything.
    let env = Env::new();
    let script = env.ws.path().join("fake-mcp.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{}}}\n' ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}},{"name":"rm_rf","description":"deletes everything","inputSchema":{}}]}}\n' ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"found two postings"}]}}\n' ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    env.write_config(&format!(
        "\n[mcp.fake]\ncommand = {:?}\n",
        script.display().to_string()
    ));
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
exec    = { timeout-secs = 20 }
mcp     = [{ server = "fake", tools = ["search"] }]
"#,
    );
    let out = env.saddle(&["mcp", "call", "fake", "search", r#"{"q":"rust"}"#]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("found two postings"), "{text}");

    // A tool the grant does not name is refused before the server is even started.
    let out = env.saddle(&["mcp", "call", "fake", "rm_rf"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("not one of the tools granted for `fake`"),
        "the refusal did not name the tool: {text}"
    );

    // D112: both calls are in the record — the allow and the deny — with the server, the tool and
    // a hash of the arguments rather than the arguments themselves.
    let log = env.audit();
    assert!(
        log.contains(r#""server":"fake","tool":"search""#),
        "the granted call was not recorded: {log}"
    );
    assert!(
        log.contains(r#""allow":true"#),
        "the granted call is not marked as allowed: {log}"
    );
    assert!(
        log.contains(r#""sha256""#),
        "the arguments were not summarised: {log}"
    );
    assert!(
        log.contains(r#""server":"fake","tool":"rm_rf""#),
        "the refused call was not recorded: {log}"
    );
    assert!(
        log.contains(r#""arguments":{"bytes":12,"sha256":"#),
        "the record does not summarise what was sent: {log}"
    );
    assert!(
        !log.contains("rust"),
        "the arguments themselves must never be recorded: {log}"
    );
}

#[test]
fn an_http_server_is_reached_only_through_its_own_narrowed_reach() {
    // D110: an HTTP server has no child to confine, so its authority is asked at every request —
    // against its own reach, narrowed from the agent's. The same server with the same grant but no
    // net reach is refused before anything is sent, and the record says which server it was for.
    let env = Env::new();
    let (url, seen, stop, handle) = mock_http_mcp();
    env.write_config(&format!("\n[mcp.remote]\nurl = {url:?}\n"));
    let port = url
        .rsplit(':')
        .next()
        .and_then(|rest| rest.split('/').next())
        .expect("a port in the url");
    env.write_policy(format!(
        r#"
version = 1

[agent]
fs-read = [{{ root = "workspace" }}]
net     = [{{ host = "127.0.0.1", scheme = "http", port = {port} }}]
mcp     = [{{ server = "remote", tools = ["echo"], net = [{{ host = "127.0.0.1", scheme = "http", port = {port} }}] }}]
"#
    ));
    let out = env.saddle(&["mcp", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("remote  1 tool(s)"), "not spoken to: {text}");
    assert!(text.contains("echo"), "{text}");
    assert!(text.contains("granted"), "{text}");
    let asked = seen.load(Ordering::SeqCst);

    // The same server, granted the tool and a reach that is *narrower* than the agent's: it names
    // another port, so the agent's own grant for this one does not answer for it. Refused before
    // anything is sent, and the mock hears nothing more.
    env.write_policy(format!(
        r#"
version = 1

[agent]
fs-read = [{{ root = "workspace" }}]
net     = [
  {{ host = "127.0.0.1", scheme = "http", port = {port} }},
  {{ host = "127.0.0.1", scheme = "http", port = 9 }},
]
mcp     = [{{ server = "remote", tools = ["echo"], net = [{{ host = "127.0.0.1", scheme = "http", port = 9 }}] }}]
"#
    ));
    let out = env.saddle(&["mcp", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("no net grant covering"),
        "the refusal did not say what was missing: {text}"
    );
    assert_eq!(
        seen.load(Ordering::SeqCst),
        asked,
        "a refused request must not be sent"
    );

    let log = env.audit();
    assert!(
        log.contains("\"via\":\"mcp\"") && log.contains("\"server\":\"remote\""),
        "the decision does not name the server it was for: {log}"
    );
    stop.store(true, Ordering::SeqCst);
    let _ = handle.join();
}

#[test]
fn mcp_list_starts_a_server_and_says_what_the_grant_allows() {
    // D107: proven from outside, through the real binary. A declared server is started under the
    // sandbox, spoken to over its own stdin and stdout, and its tools compared with the grant.
    let env = Env::new();
    let script = env.ws.path().join("fake-mcp.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}\n' ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}},{"name":"rm_rf","description":"deletes everything","inputSchema":{}}]}}\n' ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    env.write_config(&format!(
        "\n[mcp.fake]\ncommand = {:?}\n",
        script.display().to_string()
    ));
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
exec    = { timeout-secs = 20 }
mcp     = [{ server = "fake", tools = ["search"] }]
"#,
    );
    let out = env.saddle(&["mcp", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("fake  2 tool(s)"),
        "the server was not spoken to: {text}"
    );
    assert!(
        text.contains("search") && text.contains("granted      finds things"),
        "{text}"
    );
    assert!(
        text.contains("rm_rf") && text.contains("not granted  deletes everything"),
        "{text}"
    );
}

#[test]
fn tool_search_declares_what_the_grant_allows_and_nothing_else() {
    // D108: a server's tools are not in the prompt until the model searches for them, and a search
    // can only ever declare what the grant names.
    let env = Env::new();
    let script = env.ws.path().join("fake-mcp.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18"}}\n' ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{}},{"name":"rm_rf","description":"deletes everything","inputSchema":{}}]}}\n' ;;
  esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&script, Permissions::from_mode(0o755)).unwrap();
    env.write_config(&format!(
        "\n[mcp.fake]\ncommand = {:?}\n",
        script.display().to_string()
    ));
    env.write_policy(
        r#"
version = 1

[agent]
fs-read = [{ root = "workspace" }]
exec    = { timeout-secs = 20 }
mcp     = [{ server = "fake", tools = ["search"] }]
"#,
    );
    let (ok, text) = env.tool("tool_search", serde_json::json!({ "query": "" }));
    assert!(ok, "{text}");
    assert!(text.contains("mcp__fake__search: finds things"), "{text}");
    assert!(!text.contains("mcp__fake__rm_rf"), "{text}");
    assert!(
        text.contains("1 further tool(s) of these servers are not granted"),
        "{text}"
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
    fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    let sub = home.path().join("src/proj");
    fs::create_dir_all(sub.join(".git")).unwrap();
    let saddle = |dir: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", xdg.path().join("config"))
            .stdin(Stdio::null())
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
    fs::write(home.path().join("README"), "hello\n").unwrap();
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
    fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    fs::write(home.path().join("README"), "hello\n").unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_saddle"))
            .arg("-C")
            .arg(home.path())
            .args(["tool", "read", r#"{"path":"README"}"#])
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", xdg.path().join("config"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert_eq!(
        run().status.code(),
        Some(2),
        "refused while the flag is off"
    );
    fs::write(
        xdg.path().join("config/saddle/config.toml"),
        format!(
            "allow-sensitive = true\n{}",
            saddle_standard::DEFAULT_PROFILES
        ),
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
    fs::create_dir_all(&home).unwrap();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
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
fn default_includes_limit_children_to_toolchains_and_config_and_withhold_credentials() {
    // D160: default command reads have explicit narrow bases, not an implicit home walk.
    let home = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    let h = home.path();
    fs::create_dir_all(h.join(".cargo/registry")).unwrap();
    fs::create_dir_all(h.join(".config/git")).unwrap();
    fs::create_dir_all(h.join(".ssh")).unwrap();
    fs::create_dir_all(h.join("proj/.git")).unwrap();
    fs::write(h.join(".config/git/config"), "gitconfig\n").unwrap();
    fs::write(h.join(".ssh/id_rsa"), "sk-live-private\n").unwrap();
    let auth = xdg.path().join("state/saddle/auth.json");
    fs::create_dir_all(auth.parent().unwrap()).unwrap();
    // The shape `AuthStore` reads, so the file is a real store rather than a parse error.
    fs::write(&auth, r#"{"synthetic":{"api-key":"sk-live-api"}}"#).unwrap();
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

    let read = |path: &Path| -> bool {
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
    fs::create_dir_all(h.join("proj/.git")).unwrap();
    fs::create_dir_all(h.join(".config/git")).unwrap();
    fs::create_dir_all(h.join("notes/private")).unwrap();
    fs::write(h.join("notes/private/a.txt"), "secret\n").unwrap();
    fs::write(h.join("notes/public.txt"), "fine\n").unwrap();
    fs::create_dir_all(h.join(".cargo/registry")).unwrap();
    fs::write(h.join(".cargo/registry/hidden"), "secret").unwrap();
    fs::write(h.join(".cargo/credentials.toml"), "token").unwrap();
    fs::create_dir_all(xdg.path().join("config/saddle")).unwrap();
    fs::write(
        xdg.path().join("config/saddle/config.toml"),
        r#"
[[sandbox.include]]
base = "~"
depth = 2
except = ["^notes/private$", "^\\.cargo/registry$"]
"#
        .to_owned()
            + saddle_standard::DEFAULT_PROFILES,
    )
    .unwrap();
    let read = |path: &Path| -> bool {
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
    assert!(
        !read(&h.join(".cargo/registry/hidden")),
        "default child includes cannot undo an explicit home exclusion"
    );
    assert!(
        !read(&h.join(".cargo/credentials.toml")),
        "explicit home rules keep the prior credential mask"
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
    fs::write(&config, "provider = \"synthetic\"\n").unwrap();
    let dir = config.parent().unwrap();
    fs::write(
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
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(format!("{key}.toml"));
    fs::write(
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
    fs::write(
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
fn a_skill_is_loaded_with_the_read_tool() {
    // D67: a skill is a file, and `read` is how a file is loaded. Discovery advertises the name,
    // the description and the file to read; there is no tool of its own.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let dir = env.path(".agents/skills/house");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    fs::write(
        &file,
        "---\nname: house\ndescription: House style. Use when editing Rust here.\n---\n\n\
         lowercase doc comments.\n",
    )
    .unwrap();

    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, file.display()),
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("lowercase doc comments"),
        "the body is the file"
    );

    // Advertised, so the model knows it exists and which file to read.
    assert_eq!(env.advertised(home.path()), ["house"]);

    // There is no `skill` tool: it was removed rather than kept beside `read`.
    let out = saddle_in(&env, home.path(), &["tool", "skill", r#"{"name":"house"}"#]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown tool `skill`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_skill_the_policy_refuses_is_not_advertised() {
    // D67: discovery reads through the same gate as `read`, so a skill the grants refuse is never
    // pointed at by the prompt. The listing and the tool agree by construction, not by a second
    // reachability check that could drift.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let dir = env.path(".agents/skills/house");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
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
    assert!(
        env.advertised(home.path()).is_empty(),
        "refused, so not advertised"
    );
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "read",
            &format!(r#"{{"path":"{}"}}"#, dir.join("SKILL.md").display()),
        ],
    );
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn a_skill_under_the_config_directory_is_not_advertised() {
    // D67 removed the one reason the config-directory tier existed: a dedicated tool that could
    // read what no root reaches. Now nothing can read it, so advertising it would point at a denial.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let dir = env.xdg.path().join("config/saddle/skills/pdf-tools");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("SKILL.md");
    fs::write(
        &file,
        "---\nname: pdf-tools\ndescription: Extract text.\n---\nbody\n",
    )
    .unwrap();

    assert!(
        env.advertised(home.path()).is_empty(),
        "nothing can read it"
    );
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
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("a.md"), "hi\n").unwrap();

    // A path in [sandbox] read, not a root and not a grant.
    let config = env.policy_path();
    let mut doc: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let sandbox = doc
        .entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    sandbox
        .as_table_mut()
        .unwrap()
        .insert("read".into(), toml::Value::Array(vec!["~/notes".into()]));

    let run = |args: &[&str]| {
        let text = toml::to_string(&doc).unwrap();
        fs::write(&config, &text).unwrap();
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
fn saddle_in(env: &Env, home: &Path, args: &[&str]) -> Output {
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
    let mut doc: toml::Table = fs::read_to_string(&path)
        .unwrap_or_default()
        .parse()
        .unwrap();
    let sandbox = doc
        .entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    sandbox.as_table_mut().unwrap().insert("read".into(), value);
    fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
}

#[test]
fn a_sandbox_entry_naming_tools_lets_those_tools_reach_it() {
    // D66: a directory outside the workspace, reachable by the tools named on its `[sandbox]`
    // entry. It is not a root, so no policy can name it, and it is not part of the session.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let agents = home.path().join(".agents");
    fs::create_dir_all(agents.join("skills/a")).unwrap();
    fs::write(agents.join("skills/a/SKILL.md"), "---\nname: a\n---\nx\n").unwrap();
    let notes = home.path().join("notes");
    fs::create_dir_all(&notes).unwrap();
    fs::write(notes.join("n.md"), "notes\n").unwrap();

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
    assert!(text.contains("no tool-write role"), "{text}");

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
fn a_sandbox_entry_naming_tools_allows_a_write_and_nothing_inside_it_is_reserved() {
    // The tools run in saddle, so the list is what allows a write there; the kernel is not
    // involved. D178: what the entry allows is the whole of it — a `.git/hooks` path under a
    // tool-visible directory is the person's own directory like any other.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let scratch = home.path().join("scratch");
    fs::create_dir_all(scratch.join(".git/hooks")).unwrap();
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
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(scratch.join(".git/hooks/pre-commit")).unwrap(),
        "#!/bin/sh"
    );

    // What the entry does not name is still refused: the list is the authority, not the skeleton.
    let out = saddle_in(
        &env,
        home.path(),
        &[
            "tool",
            "write",
            &format!(
                r#"{{"path":"{}","content":"x"}}"#,
                home.path().join("elsewhere.txt").display()
            ),
        ],
    );
    assert!(!out.status.success(), "{}", stderr(&out));
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
fn sdk_resolution_ranges_and_mutation_preconditions_have_separate_access_records() {
    // Each SDK metadata/range/precondition access is decided; registration/emission are not file reads.
    // Resolution plus one range are two records. Edits also guard their preimage twice before publishing.
    let env = ready();
    let decisions = |name: &str, input: &str| -> usize {
        let before = env.audit_logs().len();
        env.tool(name, serde_json::from_str(input).unwrap());
        let logs = env.audit_logs();
        assert!(logs.len() > before, "{name} wrote no log");
        fs::read_to_string(logs.last().unwrap())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|r| r["kind"] == "decision" && r["scope"] != "tool")
            .count()
    };

    assert_eq!(decisions("read", r#"{"path":"README"}"#), 2, "a text file");
    assert_eq!(decisions("read", r#"{"path":"nope"}"#), 2, "a missing file");
    assert_eq!(
        decisions("read", r#"{"path":"../outside"}"#),
        1,
        "a refusal"
    );
    assert_eq!(decisions("ls", r#"{"path":"."}"#), 2);
    // edit legitimately makes two: it reads the old text and writes the new one.
    assert_eq!(
        decisions("edit", r#"{"path":"README","old":"hello","new":"bye"}"#),
        5
    );
}

#[test]
fn the_shared_skills_directory_is_readable_by_default() {
    // D66: ~/.agents is built in and read-only, because that is where skills are installed and a
    // skill's own references have to be readable. Nothing is written in the config for this.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let skill = home.path().join(".agents/skills/s");
    fs::create_dir_all(skill.join("references")).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: s\ndescription: d\n---\nRead references/r.md.\n",
    )
    .unwrap();
    fs::write(skill.join("references/r.md"), "the reference\n").unwrap();

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
        stderr(&out).contains("no tool-write role"),
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
    let mut doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    doc.entry("sandbox")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .unwrap()
        .insert("tools-defaults".into(), toml::Value::Boolean(false));
    fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();
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
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("a.txt"), "text\n").unwrap();
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
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: s\ndescription: d\n---\nx\n",
    )
    .unwrap();

    // Env::new wrote a config with `[agent]` and nothing else; make sure there is no [sandbox].
    let path = env.policy_path();
    let mut doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    doc.remove("sandbox");
    fs::write(&path, toml::to_string(&doc).unwrap()).unwrap();

    fs::create_dir_all(home.path().join(".cargo/registry")).unwrap();
    fs::write(home.path().join(".cargo/registry/probe"), "toolchain").unwrap();
    let out = saddle_in(&env, home.path(), &["check"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("exec reads"),
        "the narrow includes survived: {text}"
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
    fs::create_dir_all(real.join("references")).unwrap();
    fs::write(real.join("SKILL.md"), "---\nname: prose-lint\n---\nbody\n").unwrap();
    fs::write(real.join("references/tells.md"), "# Tells\n").unwrap();
    let agents = home.path().join(".agents/skills");
    fs::create_dir_all(&agents).unwrap();
    unix_fs::symlink(&real, agents.join("prose-lint")).unwrap();

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
    unix_fs::symlink(&policy, &escape).unwrap();
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

#[test]
fn the_projects_skills_are_in_one_directory_at_the_root() {
    // D68: one place, as pi has one. A skills directory in a subdirectory is not the project's, and
    // the scan does not go looking for it.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    for rel in [".agents/skills/at-root", "crates/sub/.agents/skills/nested"] {
        let dir = env.path(rel);
        fs::create_dir_all(&dir).unwrap();
        let name = rel.rsplit('/').next().unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
        )
        .unwrap();
    }
    assert_eq!(env.advertised(home.path()), ["at-root"]);
}

#[test]
fn skill_discovery_honours_ignore_files_and_skips_node_modules() {
    // The rules apply inside the one skills directory: a dependency's skills are not the project's,
    // and a project that ignores a path in its skills directory means it.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    for dir in ["keep", "ignored", "node_modules/pkg", ".hidden"] {
        let skills = env.path(&format!(".agents/skills/{dir}/s"));
        fs::create_dir_all(&skills).unwrap();
        fs::write(
            skills.join("SKILL.md"),
            format!("---\nname: s-{dir}\ndescription: d\n---\n"),
        )
        .unwrap();
    }
    fs::write(env.path(".agents/skills/.ignore"), "ignored\n").unwrap();
    assert_eq!(env.advertised(home.path()), ["s-keep"]);
}

#[test]
fn a_directory_holding_a_skill_yields_one_skill_and_a_bare_markdown_file_is_one() {
    // pi's rule: a directory with SKILL.md is a skill and is not searched further, so a skill's own
    // `references/` never becomes more skills. At the top level a bare `.md` is a skill too; deeper
    // down only SKILL.md counts.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let dir = env.path(".agents/skills/a");
    fs::create_dir_all(dir.join("references/inner")).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: a\ndescription: d\n---\nbody\n",
    )
    .unwrap();
    fs::write(
        dir.join("references/inner/SKILL.md"),
        "---\nname: inner\ndescription: d\n---\n",
    )
    .unwrap();
    fs::create_dir_all(env.path(".agents/skills/a/deep")).unwrap();
    fs::write(
        env.path(".agents/skills/a/deep/lonely.md"),
        "---\nname: lonely\ndescription: d\n---\n",
    )
    .unwrap();
    fs::write(
        env.path(".agents/skills/standalone.md"),
        "---\nname: standalone\ndescription: d\n---\n",
    )
    .unwrap();
    let mut found = env.advertised(home.path());
    found.sort();
    assert_eq!(found, ["a", "standalone"]);
}

#[test]
fn a_refusal_is_said_out_loud_and_the_first_skill_of_a_name_wins() {
    // pi's rule: the first discovered keeps the name and the collision is reported. The note reaches
    // stderr, which is where a silent skip used to be invisible.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    for (dir, declared) in [
        ("same", "same"),
        ("aa-same", "same"),
        ("refused", "refused"),
    ] {
        let path = env.path(&format!(".agents/skills/{dir}"));
        fs::create_dir_all(&path).unwrap();
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {declared}\ndescription: d\n---\nbody of {dir}\n"),
        )
        .unwrap();
    }
    env.edit_agent(|agent| {
        agent.insert(
            "fs-read".into(),
            toml::Value::Array(vec![toml::Value::Table(
                r#"root = "workspace"
                   exclude = [".agents/skills/refused/**"]"#
                    .parse()
                    .unwrap(),
            )]),
        );
    });
    let out = saddle_in(&env, home.path(), &["tool", "ls", r#"{"path":"."}"#]);
    let text = stderr(&out);
    assert!(
        text.contains("refused/SKILL.md was not advertised: denied:"),
        "the refusal must be visible: {text}"
    );
    assert!(
        text.contains("already has that name") && text.contains("aa-same/SKILL.md"),
        "the collision must be visible: {text}"
    );
    assert_eq!(
        env.advertised(home.path()),
        ["same"],
        "the first keeps the name"
    );
}

#[test]
fn the_session_offers_its_skills_and_records_a_load() {
    // D69: `/skill:name` is served from what discovery read, so the session must expose those
    // skills to the interface, and a load is recorded as the other half of discovery's record.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    let dir = env.path(".agents/skills/manim");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        "---\nname: manim\ndescription: Animate a proof.\ndisable-model-invocation: true\n---\nRender with Scene.\n",
    )
    .unwrap();

    // A hidden skill is not in the prompt but is still offered as a command, and the audit says
    // both things.
    let out = saddle_in(&env, home.path(), &["tool", "ls", r#"{"path":"."}"#]);
    assert!(out.status.success(), "{}", stderr(&out));
    let log = env.audit_logs().last().cloned().unwrap();
    let records: Vec<serde_json::Value> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let discovered = records
        .iter()
        .find(|r| r["kind"] == "skill")
        .expect("a discovery record");
    assert_eq!(discovered["args"]["discovered"][0]["name"], "manim");
}

#[test]
fn reload_reads_instruction_files_and_skills_again() {
    // D70: everything the prompt is built from, read again on request — files that appear after the
    // session opened. Driven through line mode, which is where a reload can be run without a
    // terminal; the session opens before it reads stdin, so writing the files first and reloading
    // second is a real mid-session change.
    let env = Env::new();
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(env.path(".agents/skills/first")).unwrap();
    fs::write(
        env.path(".agents/skills/first/SKILL.md"),
        "---\nname: first\ndescription: d\n---\nbody\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(env.ws.path())
        .arg("-p")
        .arg("synthetic")
        .arg("--model")
        .arg("echo")
        .env("HOME", home.path())
        .env("SYNTHETIC_API_KEY", "x")
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read as it is written, so a full pipe cannot stop the child before its session opens.
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let (ready, opened) = mpsc::channel();
    let out = stdout.clone();
    let mut piped = child.stdout.take().unwrap();
    let drain = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = piped.read(&mut buf) {
            if n == 0 {
                break;
            }
            out.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    });

    // The session is open and waiting on stdin; now the workspace gains a rule and a skill. A
    // fixed sleep would be a race on a loaded machine, so the banner line mode prints once its
    // session is up is waited for instead.
    let stderr = child.stderr.take().unwrap();
    let errors = Arc::new(Mutex::new(String::new()));
    let seen = errors.clone();
    let reader = thread::spawn(move || {
        let mut told = false;
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            // The first line names the provider, the model and the audit log, and it is written
            // once the session is open and about to read stdin.
            if !told && line.contains(" \u{b7} audit ") {
                told = true;
                let _ = ready.send(());
            }
            seen.lock().unwrap().push_str(&format!("{line}\n"));
        }
    });
    opened
        .recv_timeout(Duration::from_secs(30))
        .expect("the session never started");

    fs::write(env.path("AGENTS.md"), "a rule added mid-session\n").unwrap();
    fs::create_dir_all(env.path(".agents/skills/second")).unwrap();
    fs::write(
        env.path(".agents/skills/second/SKILL.md"),
        "---\nname: second\ndescription: d\n---\nbody\n",
    )
    .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"/reload\n/quit\n").unwrap();
    }
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
    // A pipe still being drained is closed once the child is gone, and joining it is what makes
    // the output complete before it is read.
    drain.join().unwrap();
    reader.join().unwrap();
    let mut text = String::from_utf8_lossy(&stdout.lock().unwrap()).into_owned();
    text.push_str(&errors.lock().unwrap());
    assert!(text.contains("skills reloaded: first, second"), "{text}");
    assert!(text.contains("instructions reloaded: AGENTS.md"), "{text}");
    assert!(
        text.contains("already read into this conversation is unchanged"),
        "what a reload cannot change is said: {text}"
    );
}

#[test]
fn a_custom_theme_is_read_from_the_users_config() {
    // D71: a theme is a table of roles in config.toml, selected by `[ui] theme`. The CLI has no
    // screen to paint, so what is checked here is that the file is read and that a theme which
    // cannot build is reported rather than silently replaced by the default.
    let env = Env::new();
    let path = env.policy_path();
    fs::write(
        &path,
        r##"[ui]
theme = "mine"

[themes.mine]
extends = "default"
accent = "#ff00ff"

[agent]
fs-read = [{ root = "workspace" }]
"##,
    )
    .unwrap();
    // `check` reads the same config the TUI would; the theme must not make it fail.
    let out = env.saddle(&["check"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // A theme naming an unknown base is an error the person sees, not a fallback.
    fs::write(
        &path,
        "[ui]\ntheme = \"nope\"\n\n[agent]\nfs-read = [{ root = \"workspace\" }]\n",
    )
    .unwrap();
    let out = env.saddle(&["check"]);
    assert!(
        out.status.success(),
        "check does not paint, so it does not care"
    );
    // The message itself comes from theme::build, which the unit tests cover.
}

#[test]
fn default_personal_read_denial_can_be_explicitly_enabled_and_revoked_without_repinning() {
    let env = Env::new();
    let home = env.home.path();
    for relative in [
        "Pictures/comfyui/render.png",
        "Documents/private.txt",
        "other-checkout/main.rs",
        ".unknown-private/token",
        ".cargo/credentials",
        ".cargo/credentials.toml",
        ".config/gcloud/credentials.db",
        ".config/op/token",
        ".config/google-chrome/Default/Cookies",
        ".config/git/credentials",
        ".cargo/bin/probe",
        ".rustup/toolchains/probe",
        ".config/git/config",
        ".config/jj/config.toml",
    ] {
        let path = home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "fixture").unwrap();
    }
    let read = |path: &Path| {
        let output = env.saddle(&[
            "tool",
            "bash",
            &json!({"command":format!("cat '{}' > /dev/null", path.display())}).to_string(),
        ]);
        output.status.success()
    };
    for relative in [
        "Pictures/comfyui/render.png",
        "Documents/private.txt",
        "other-checkout/main.rs",
        ".unknown-private/token",
        ".cargo/credentials",
        ".cargo/credentials.toml",
        ".config/gcloud/credentials.db",
        ".config/op/token",
        ".config/google-chrome/Default/Cookies",
        ".config/git/credentials",
    ] {
        assert!(
            !read(&home.join(relative)),
            "{relative} must not be readable by default"
        );
    }
    for relative in [
        ".cargo/bin/probe",
        ".rustup/toolchains/probe",
        ".config/git/config",
        ".config/jj/config.toml",
    ] {
        assert!(read(&home.join(relative)), "{relative} should be readable");
    }
    assert!(
        !read(&env.policy_path()),
        "protected configuration is never a default read"
    );
    let gallery = home.join("Pictures/comfyui");
    env.write_override(&format!(
        "[sandbox]\nread = [{:?}]",
        format!("+{}", gallery.display())
    ));
    assert!(read(&gallery.join("render.png")));
    let file_tool = env.saddle(&[
        "tool",
        "read",
        &json!({"path":gallery.join("render.png")}).to_string(),
    ]);
    assert!(
        !file_tool.status.success(),
        "a command-only path does not lend file-tool access"
    );
    env.write_override("");
    assert!(!read(&gallery.join("render.png")));
    let mut config: toml::Table = env.policy().parse().unwrap();
    config.insert(
        "sandbox".into(),
        toml::Value::try_from(json!({"include-defaults":false})).unwrap(),
    );
    fs::write(env.policy_path(), toml::to_string(&config).unwrap()).unwrap();
    assert!(!read(&home.join(".cargo/bin/probe")));
    assert!(!read(&home.join(".config/git/config")));
    env.write_override(&format!(
        "[sandbox]\nread = [{:?}]",
        format!("+{}", gallery.display())
    ));
    assert!(
        read(&gallery.join("render.png")),
        "disabling defaults retains explicit paths"
    );
}

#[test]
fn implicit_include_bases_cannot_follow_symlinks_to_unrelated_data() {
    let env = Env::new();
    let secret = env.outside.path().join("secret");
    for name in [".cargo", ".rustup", ".config"] {
        symlink(env.outside.path(), env.home.path().join(name)).unwrap();
    }
    let out = env.saddle(&[
        "tool",
        "bash",
        &json!({"command":format!("cat '{}'",secret.display())}).to_string(),
    ]);
    assert!(!out.status.success());
}
