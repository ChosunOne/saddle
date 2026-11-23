//! protected launch profiles layer without lending any authority (D184).

use std::fs;
use std::os::unix::fs::symlink;

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
