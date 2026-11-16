//! real command confinement and session identity for sandbox list operators (D157).

use std::fs;
use std::os::unix::fs::symlink;
use std::process::Output;

use serde_json::json;
use toml::Table;

use super::{Env, stderr};

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn session_file(env: &Env, id: &str, sandbox: &str) {
    let directory = env.xdg.path().join("config/saddle/session-overrides");
    fs::create_dir_all(&directory).unwrap();
    fs::write(
        directory.join(format!("{id}.toml")),
        format!(
            "workspace = {:?}\n[sandbox]\n{sandbox}",
            env.ws.path().canonicalize().unwrap().display().to_string()
        ),
    )
    .unwrap();
}

#[test]
fn sandbox_lists_append_subtract_replace_and_isolate_sessions_without_changing_root_grants() {
    let env = Env::new();
    let one = env.outside.path().join("one");
    let two = env.outside.path().join("two");
    let extra = env.outside.path().join("extra");
    for path in [&one, &two, &extra] {
        fs::create_dir(path).unwrap();
    }
    let mut config: Table = env.policy().parse().unwrap();
    config.insert(
        "sandbox".into(),
        toml::Value::try_from(json!({"write":[one, two]})).unwrap(),
    );
    fs::write(env.policy_path(), toml::to_string(&config).unwrap()).unwrap();
    session_file(
        &env,
        "chosen",
        &format!(
            "write = [{:?}, {:?}]\n",
            format!("+{}", extra.display()),
            format!("-{}", one.display())
        ),
    );
    let check = env.saddle(&["--session", "chosen", "check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    assert!(stdout(&check).contains("session-overrides/chosen.toml"));
    if saddle_sandbox::availability().is_ok() {
        let command = format!(
            "for d in '{}' '{}' '{}'; do if (printf x > \"$d/probe\") 2>/dev/null; then printf 'yes:%s\\n' \"$d\"; else printf 'no:%s\\n' \"$d\"; fi; done; printf kept > workspace-write",
            one.display(),
            two.display(),
            extra.display()
        );
        let args = json!({"command":command}).to_string();
        let output = env.saddle(&["--session", "chosen", "tool", "bash", &args]);
        assert!(
            output.status.success(),
            "{} {}",
            stdout(&output),
            stderr(&output)
        );
        assert!(
            stdout(&output).contains(&format!("no:{}", one.display())),
            "{}",
            stdout(&output)
        );
        assert!(stdout(&output).contains(&format!("yes:{}", two.display())));
        assert!(stdout(&output).contains(&format!("yes:{}", extra.display())));
        assert_eq!(
            fs::read_to_string(env.ws.path().join("workspace-write")).unwrap(),
            "kept"
        );
        let other = env.saddle(&[
            "--session",
            "other",
            "tool",
            "bash",
            &json!({"command":format!("printf x > '{}/other'",extra.display())}).to_string(),
        ]);
        assert!(!other.status.success());
        let other = env.saddle(&[
            "--session",
            "other",
            "tool",
            "bash",
            &json!({"command":format!("printf x > '{}/other'",one.display())}).to_string(),
        ]);
        assert!(other.status.success(), "{}", stderr(&other));
    }
    // A bare command path does not become a file-tool root or lend a D66 tool role.
    let read = env.saddle(&[
        "--session",
        "chosen",
        "tool",
        "read",
        &json!({"path":extra.join("probe")}).to_string(),
    ]);
    assert!(!read.status.success());
    assert!(stderr(&read).contains("denied:"), "{}", stderr(&read));
    session_file(
        &env,
        "chosen",
        &format!("write = [{:?}]\n", extra.display().to_string()),
    );
    let check = env.saddle(&["--session", "chosen", "check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let text = stdout(&check);
    assert!(text.contains(&extra.display().to_string()));
    assert!(!text.contains(&one.display().to_string()), "{text}");
    assert!(!text.contains(&two.display().to_string()), "{text}");
}

#[test]
fn invalid_override_operators_protected_paths_and_symlinked_authority_are_refused() {
    let env = Env::new();
    for invalid in [
        "write = ['+/one', '/two']",
        "write = ['+']",
        "write = ['+relative']",
        "typo = ['+/one']",
    ] {
        session_file(&env, "chosen", invalid);
        let out = env.saddle(&["--session", "chosen", "check"]);
        assert!(!out.status.success(), "{invalid}");
        assert!(
            stderr(&out).contains("invalid session override"),
            "{}",
            stderr(&out)
        );
    }
    let protected = env.policy_path().parent().unwrap().to_path_buf();
    session_file(
        &env,
        "chosen",
        &format!("write = [{:?}]", format!("+{}", protected.display())),
    );
    let out = env.saddle(&["--session", "chosen", "check"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no sandbox entry may reach it"),
        "{}",
        stderr(&out)
    );
    session_file(&env, "chosen", "write = []");
    let path = env
        .xdg
        .path()
        .join("config/saddle/session-overrides/chosen.toml");
    fs::write(&path, "workspace = '/different'\n[sandbox]\nwrite=[]").unwrap();
    let out = env.saddle(&["--session", "chosen", "check"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot cross workspaces"));
    fs::remove_file(&path).unwrap();
    let source = env.ws.path().join("untrusted.toml");
    fs::write(
        &source,
        format!(
            "workspace={:?}\n[sandbox]\nwrite=[]",
            env.ws.path().display().to_string()
        ),
    )
    .unwrap();
    symlink(&source, &path).unwrap();
    let out = env.saddle(&["--session", "chosen", "check"]);
    assert!(!out.status.success(), "{}", stderr(&out));
    fs::remove_file(&path).unwrap();
    for prefix in ['+', '-'] {
        let mut config: Table = env.policy().parse().unwrap();
        config.insert(
            "sandbox".into(),
            toml::Value::try_from(json!({"write":[format!("{prefix}/one")]})).unwrap(),
        );
        fs::write(env.policy_path(), toml::to_string(&config).unwrap()).unwrap();
        let out = env.saddle(&["check"]);
        assert!(!out.status.success());
        assert!(
            stderr(&out).contains("invalid config: sandbox.write uses + or - in global config"),
            "{}",
            stderr(&out)
        );
    }
}
