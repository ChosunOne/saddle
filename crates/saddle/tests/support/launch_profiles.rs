//! protected launch profiles layer without lending any authority (D184).

use std::fs;
use std::os::unix::fs::symlink;
#[cfg(target_os = "linux")]
use std::path::Path;

use serde_json::json;

use super::{Env, stderr};

#[test]
fn launch_profile_grants_are_independent_and_layer_per_workspace() {
    let env = Env::new();
    env.write_config("[launch-profiles.fixture]\nexecutable = '/bin/cat'\n");
    let check = |args: &[&str], access: &str| {
        let output = env.saddle(args);
        assert!(output.status.success(), "{}", stderr(&output));
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.lines()
                .any(|line| line.starts_with("  fixture:") && line.ends_with(access)),
            "{text}"
        );
        text.contains("agent.exec =")
    };
    check(&["check"], "(not granted)");
    env.edit_agent(|agent| {
        agent.insert(
            "run-binary".into(),
            toml::Value::try_from(json!([{"profile":"fixture"}])).unwrap(),
        );
    });
    assert!(!check(&["--revoke", "exec", "check"], "(granted)"));
    assert!(check(&["--revoke", "run-binary", "check"], "(not granted)"));
    env.write_override("[agent]\nrun-binary = ['-fixture']\n");
    check(&["check"], "(not granted)");
    check(&["--grant", "run-binary:fixture", "check"], "(granted)");
    env.write_override("[agent]\nrun-binary = [{ profile = 'undefined' }]\n");
    let output = env.saddle(&["check"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("names undefined launch profile"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn launch_grants_layer_through_permission_profiles() {
    let env = Env::new();
    env.write_config(
        "[launch-profiles.fixture]\nexecutable = '/usr/bin/cat'\n\
         [profiles.with-launch]\nrun-binary = [{profile='fixture'}]\n\
         [profiles.empty]\nrun-binary = []\n",
    );
    let check = |profile, access| {
        let out = env.saddle(&["--profile", profile, "check"]);
        assert!(out.status.success(), "{}", stderr(&out));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.lines()
                .any(|line| line.starts_with("  fixture:") && line.ends_with(access)),
            "{text}"
        );
    };
    check("none", "(not granted)");
    check("with-launch", "(granted)");
    env.write_override("[agent]\nrun-binary = ['-fixture']\n");
    check("with-launch", "(not granted)");
    env.write_override("");
    env.edit_agent(|agent| {
        agent.insert(
            "run-binary".into(),
            toml::Value::try_from(json!([{"profile":"fixture"}])).unwrap(),
        );
    });
    check("none", "(granted)");
    check("empty", "(not granted)");
}

#[cfg(target_os = "linux")]
#[test]
fn launch_profile_runtime_is_independent_of_bash_and_revocable() {
    if saddle_sandbox::launch_availability().is_err() {
        return;
    }
    let env = Env::new();
    let secret = env.home.path().join("private");
    let sibling = env.home.path().join("sibling");
    fs::write(&secret, "named-launch-fixture").unwrap();
    fs::write(&sibling, "not-for-the-child").unwrap();
    fs::copy("/bin/cat", env.ws.path().join("untrusted-cat")).unwrap();
    env.write_config(&format!(
        "[launch-profiles.cat]\nexecutable = '/usr/bin/cat'\nread = ['~/private']\n\
         [launch-profiles.env]\nexecutable = '/usr/bin/env'\n\
         [launch-profiles.untrusted]\nexecutable = {:?}\n",
        env.ws.path().join("untrusted-cat").to_str().unwrap(),
    ));
    let named = |extra: &[&str], profile: &str, argv: serde_json::Value| {
        let input = json!({"profile":profile,"argv":argv}).to_string();
        let mut args = extra.to_vec();
        args.extend(["tool", "run_binary", &input]);
        let output = env.saddle(&args);
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            stderr(&output)
        );
        (output.status.success(), text)
    };
    let (ok, text) = named(&[], "cat", json!([secret]));
    assert!(!ok && !text.contains("named-launch-fixture"), "{text}");
    env.write_override(
        "[agent]\nrun-binary = [{profile='cat'}, {profile='env'}, {profile='untrusted'}]\n",
    );
    let no_exec = ["--revoke", "exec"];
    let (ok, text) = named(&no_exec, "cat", json!([secret]));
    assert!(ok && text.contains("named-launch-fixture"), "{text}");
    let (ok, text) = named(&no_exec, "cat", json!([sibling]));
    assert!(
        !ok && text.contains("Permission denied") && !text.contains("not-for-the-child"),
        "{text}"
    );
    let (ok, text) = named(&no_exec, "env", json!(["/bin/true"]));
    assert!(!ok && text.contains("Function not implemented"), "{text}");
    let (ok, text) = named(&no_exec, "untrusted", json!([secret]));
    assert!(!ok && text.contains("agent-writable paths"), "{text}");
    let (ok, text) = env.tool(
        "bash",
        json!({"command":format!("/bin/cat '{}'",secret.display())}),
    );
    assert!(!ok && text.contains("Permission denied"), "{text}");
    let (ok, text) = env.tool("read", json!({"path":secret}));
    assert!(!ok && !text.contains("named-launch-fixture"), "{text}");

    env.write_override("[agent]\nrun-binary = [{profile='cat'}, {profile='env'}]\n[launch-profiles.cat]\nread = []\n");
    let (ok, text) = named(&no_exec, "cat", json!([secret]));
    assert!(
        !ok && text.contains("Permission denied") && !text.contains("named-launch-fixture"),
        "{text}"
    );

    env.write_override("[agent]\nrun-binary = [{profile='env'}]\n");
    let (ok, text) = named(&no_exec, "cat", json!([secret]));
    assert!(
        !ok && text.contains("no run-binary grant for profile"),
        "{text}"
    );
}

#[test]
fn launch_profile_fields_inherit_replace_and_clear() {
    let env = Env::new();
    let base_read = env.outside.path().join("secret");
    let project_read = env.outside.path().join("project-secret");
    env.write_config(&format!(
        "[launch-profiles.fixture]\nexecutable = '/bin/cat'\nread = [{:?}]\ntimeout-secs = 30\n",
        base_read.to_str().unwrap(),
    ));
    env.write_override("[launch-profiles.fixture]\ntimeout-secs = 10\n");
    let check = env.saddle(&["check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(text.contains(base_read.to_str().unwrap()));
    assert!(text.contains("timeout-secs = 10"));
    assert!(text.contains("workspace fields: timeout-secs"));

    env.write_override(&format!(
        "[launch-profiles.fixture]\nread = [{:?}]\n[launch-profiles.local]\nexecutable = '/bin/true'\n",
        project_read.to_str().unwrap(),
    ));
    let check = env.saddle(&["check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(!text.contains(base_read.to_str().unwrap()));
    assert!(text.contains(project_read.to_str().unwrap()));
    assert!(text.contains("timeout-secs = 30"));
    assert!(text.contains("local:"));

    env.write_override("[launch-profiles.fixture]\nread = []\n");
    let check = env.saddle(&["check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    assert!(String::from_utf8_lossy(&check.stdout).contains("read = []"));
}

#[test]
fn launch_profile_definition_does_not_lend_read_access() {
    let env = Env::new();
    // Use the throwaway home, outside the workspace and the session's tmp root. The ordinary
    // sandbox's default includes do not enumerate this directory.
    let secret = env.home.path().join("private");
    fs::write(&secret, "fixture secret").unwrap();
    env.write_config("[launch-profiles.fixture]\nexecutable = '/bin/cat'\nread = ['~/private']\n");
    let (ok, text) = env.tool("read", json!({"path": secret}));
    assert!(!ok, "{text}");
    if saddle_sandbox::availability().is_ok() {
        let (ok, text) = env.tool(
            "bash",
            json!({"command": format!("/bin/cat '{}'", secret.display())}),
        );
        assert!(!ok, "{text}");
        assert!(text.contains("Permission denied"), "{text}");
    }
}

#[test]
fn launch_profile_invalid_workspace_fields_do_not_fall_back() {
    let env = Env::new();
    env.write_config("[launch-profiles.fixture]\nexecutable = '/bin/cat'\n");
    for (override_doc, error) in [
        (
            "[launch-profiles.fixture]\nexecutable = '/bin/true'\n",
            "inherits its executable",
        ),
        ("[launch-profiles.fixture]\nunknown = []\n", "unknown field"),
        (
            "[launch-profiles.fixture]\nread = ['relative']\n",
            "expected an absolute path",
        ),
        (
            "[launch-profiles.local]\nread = []\n",
            "needs an executable",
        ),
    ] {
        env.write_override(override_doc);
        let check = env.saddle(&["check"]);
        assert!(!check.status.success());
        assert!(stderr(&check).contains(error), "{}", stderr(&check));
    }
    let alias = env.outside.path().join("alias");
    symlink(env.policy_path().parent().unwrap(), &alias).unwrap();
    env.write_override(&format!(
        "[launch-profiles.fixture]\nread = [{:?}]\n",
        alias.join("missing").to_str().unwrap(),
    ));
    let check = env.saddle(&["check"]);
    assert!(!check.status.success());
    assert!(
        stderr(&check).contains("overlaps protected config/state"),
        "{}",
        stderr(&check)
    );

    fs::write(
        env.override_path(),
        "[launch-profiles.fixture]\nread = []\n",
    )
    .unwrap();
    let check = env.saddle(&["check"]);
    assert!(!check.status.success());
    assert!(
        stderr(&check).contains("require a matching canonical workspace line"),
        "{}",
        stderr(&check)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn launch_profile_helpers_are_default_on_bounded_and_narrowable() {
    if saddle_sandbox::launch_availability().is_err() {
        return;
    }
    let env = Env::new();
    let allowed = env.home.path().join("helper-input");
    let denied = env.home.path().join("not-helper-input");
    fs::write(&allowed, "helper-read-fixture").unwrap();
    fs::write(&denied, "must-not-be-disclosed").unwrap();
    let local_helper = env.ws.path().join("local-cat");
    fs::copy("/usr/bin/cat", &local_helper).unwrap();
    env.write_config(
        "[launch-profiles.fixture]\nexecutable = '/usr/bin/bash'\n\
         helpers = ['/usr/bin/cat']\nread = ['~/helper-input', '/usr/bin/true']\n",
    );
    env.write_override("[agent]\nrun-binary = [{profile='fixture'}]\n");
    let run = |command: &str, arg: &str, allow_children: Option<bool>| {
        let mut input = json!({"profile":"fixture","argv":["-c",command,"fixture",arg]});
        if let Some(allow) = allow_children {
            input["allow_children"] = json!(allow);
        }
        let out = env.saddle(&["--revoke", "exec", "tool", "run_binary", &input.to_string()]);
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out)),
        )
    };
    // Force a child process, not bash's optimization of a final external command into exec.
    let read = |path: &Path, allow_children| {
        run(
            "cat -- \"$1\" & wait $!",
            path.to_str().unwrap(),
            allow_children,
        )
    };
    let (ok, text) = read(&allowed, None);
    assert!(ok && text.contains("helper-read-fixture"), "{text}");
    let (ok, text) = read(&denied, None);
    assert!(
        !ok && text.contains("Permission denied") && !text.contains("must-not-be-disclosed"),
        "{text}"
    );
    let (ok, text) = run("exec /usr/bin/true", "", Some(true));
    assert!(!ok && text.contains("Permission denied"), "{text}");
    let (ok, text) = run(
        "exec /usr/bin/cat -- \"$1\"",
        allowed.to_str().unwrap(),
        Some(false),
    );
    assert!(!ok && !text.contains("helper-read-fixture"), "{text}");

    env.write_override(
        "[agent]\nrun-binary = [{profile='fixture'}]\n[launch-profiles.fixture]\nhelpers = []\n",
    );
    let check = env.saddle(&["check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let text = String::from_utf8_lossy(&check.stdout);
    assert!(
        text.contains("helpers = []") && text.contains("workspace fields: helpers"),
        "{text}"
    );
    let (ok, text) = read(&allowed, Some(true));
    assert!(!ok && !text.contains("helper-read-fixture"), "{text}");

    env.write_override(&format!("[agent]\nrun-binary = [{{profile='fixture'}}]\n[launch-profiles.fixture]\nhelpers = [{:?}]\n", local_helper.to_str().unwrap()));
    let (ok, text) = run("printf must-not-start", "", None);
    assert!(
        !ok && text.contains("agent-writable paths") && !text.contains("must-not-start"),
        "{text}"
    );
}
