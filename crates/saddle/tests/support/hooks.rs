//! real hook-only SDK components through the session policy and both disjoint channels (D155).

use std::fs;
use std::io::Write;
use std::process::{Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use saddle_policy::audit::sha256_hex;
use saddle_standard::{SDK_EVENT_LISTENER, SDK_HOOKS};
use serde_json::{Value, json};
use toml::{Table, Value as TomlValue};

use super::{Api, Env, openai_sse, records, serve, sse_response, text};

fn configure(env: &Env, extra: &str) {
    let config = env.xdg.path().join("config/saddle/config.toml");
    let original = fs::read_to_string(&config).unwrap();
    let mut config_table: Table = original.parse().unwrap();
    for scope in ["transcript-read", "event-publish", "event-subscribe"] {
        config_table["agent"]
            .as_table_mut()
            .unwrap()
            .insert(scope.into(), TomlValue::Table(Table::new()));
    }
    let text = toml::to_string(&config_table).unwrap();
    let dir = config.parent().unwrap().join("plugins/hooks");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("component.wasm"), SDK_HOOKS).unwrap();
    fs::write(dir.join("plugin.toml"), "id='hooks'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['transcript-read','event-publish','event-subscribe','fs-read','fs-write','tool-read']\n").unwrap();
    fs::write(config, format!("{text}\n[plugins.hooks]\nenabled=true\nmanifest='hooks/plugin.toml'\nsha256='{}'\n[plugin.hooks]\ntranscript-read={{}}\nevent-publish={{}}\nevent-subscribe={{}}\nfs-read=[{{root='workspace',exclude=['.env*']}}]\nfs-write=[{{root='workspace'}}]\n{extra}\n", sha256_hex(SDK_HOOKS))).unwrap();
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "{} {}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

#[test]
fn hook_only_plugin_registers_two_channels_without_registering_a_tool() {
    let (base, seen) = serve(vec![sse_response(&openai_sse("done", &[]))]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    fs::write(env.ws.path().join("sample"), "secret fixture text").unwrap();
    let output = env.saddle(&["run", "sample"]);
    assert_success(&output);
    assert!(
        env.ws.path().join("hook-output").exists(),
        "{}\n{}",
        text(&output.stderr),
        fs::read_to_string(env.audit_log()).unwrap()
    );
    assert_eq!(
        fs::read_to_string(env.ws.path().join("hook-output")).unwrap(),
        "hooks:turn-start:secret fixture text"
    );
    let rows = records(&env.audit_log());
    let observed: Vec<_> = rows
        .iter()
        .filter(|row| row["args"]["operation"] == "observe" && row["allow"] == true)
        .collect();
    let received: Vec<_> = rows
        .iter()
        .filter(|row| row["args"]["operation"] == "receive" && row["allow"] == true)
        .collect();
    assert_eq!(
        observed.len(),
        1,
        "a custom turn-start must not re-enter harness hooks"
    );
    assert_eq!(received.len(), 1);
    assert_eq!(observed[0]["args"]["channel"], "harness");
    assert_eq!(received[0]["args"]["channel"], "plugin");
    assert_eq!(received[0]["args"]["publisher"], "hooks");
    assert_eq!(received[0]["principal"], "plugin.hooks");
    assert!(
        !fs::read_to_string(env.audit_log())
            .unwrap()
            .contains("secret fixture text")
    );
    let seen = seen.lock().unwrap();
    let request = &seen[0].body;
    assert!(!request.to_string().contains("secret fixture text"));
    assert!(!request["tools"].as_array().unwrap().iter().any(|tool| {
        tool["function"]["name"]
            .as_str()
            .unwrap()
            .starts_with("plugin__hooks__")
    }));
    drop(seen);
    assert_success(&env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()]));
}

#[test]
fn custom_event_cycles_stop_at_the_runtime_dispatch_budget() {
    let (base, _) = serve(vec![sse_response(&openai_sse("done", &[]))]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "[tool-limits]\nevent-deliveries=4\n");
    let output = env.saddle(&["run", "cycle"]);
    assert_success(&output);
    let rows = records(&env.audit_log());
    assert_eq!(
        rows.iter()
            .filter(|row| row["args"]["operation"] == "receive")
            .count(),
        3
    );
    assert!(
        rows.iter()
            .any(|row| row["args"]["operation"] == "event-drain" && row["allow"] == false)
    );
}

#[test]
fn excluded_paths_and_omitted_handler_write_authority_still_deny() {
    let (base, _) = serve(vec![
        sse_response(&openai_sse("done", &[])),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    fs::write(env.ws.path().join(".env.fixture"), "must stay excluded").unwrap();
    assert_success(&env.saddle(&["run", ".env.fixture"]));
    assert!(!env.ws.path().join("hook-output").exists());
    let config = env.xdg.path().join("config/saddle/config.toml");
    let original = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        original.replace("fs-write=[{root='workspace'}]", ""),
    )
    .unwrap();
    fs::write(
        env.ws.path().join("sample"),
        "read succeeds but handler cannot write",
    )
    .unwrap();
    assert_success(&env.saddle(&["run", "sample"]));
    assert!(!env.ws.path().join("hook-output").exists());
    assert!(
        records(&env.audit_log())
            .iter()
            .any(|row| row["principal"] == "plugin.hooks"
                && row["scope"] == "fs-write"
                && row["allow"] == false)
    );
}

#[test]
fn custom_event_grants_do_not_come_from_the_agent_or_a_sibling_implicitly() {
    let (base, _) = serve(vec![sse_response(&openai_sse("done", &[]))]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let config = env.xdg.path().join("config/saddle/config.toml");
    let mut table: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    table["plugin"]["hooks"]
        .as_table_mut()
        .unwrap()
        .remove("event-publish");
    fs::write(&config, toml::to_string(&table).unwrap()).unwrap();
    fs::write(env.ws.path().join("sample"), "not emitted").unwrap();
    assert_success(&env.saddle(&["run", "sample"]));
    assert!(!env.ws.path().join("hook-output").exists());
}

#[test]
fn same_hook_artifact_gains_and_loses_external_access_across_reload() {
    let (base, seen) = serve(
        (0..4)
            .map(|_| sse_response(&openai_sse("done", &[])))
            .collect(),
    );
    let env = Env::new(Api::Openai, &base);
    let external = tempfile::tempdir().unwrap();
    let target = external.path().join("external.txt");
    fs::write(&target, "external fixture text").unwrap();
    configure(&env, "tool-read={tools=['read']}\n");
    let config = env.xdg.path().join("config/saddle/config.toml");
    let mut initial: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    initial["agent"].as_table_mut().unwrap().insert(
        "tool-read".into(),
        TomlValue::try_from(json!({"tools":["read"]})).unwrap(),
    );
    initial.insert(
        "sandbox".into(),
        TomlValue::try_from(json!({"read":[{"path":external.path(),"tools":["read"]}]})).unwrap(),
    );
    // The external declaration exists, but no tool role is delegated initially.
    initial["plugin"]["hooks"]
        .as_table_mut()
        .unwrap()
        .remove("tool-read");
    fs::write(&config, toml::to_string(&initial).unwrap()).unwrap();
    fs::write(env.ws.path().join("sample"), "workspace fixture text").unwrap();
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait = |count| {
        let end = Instant::now() + Duration::from_secs(20);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(10));
        }
    };
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"sample\n")
        .unwrap();
    wait(1);
    assert_eq!(
        fs::read_to_string(env.ws.path().join("hook-output")).unwrap(),
        "hooks:turn-start:workspace fixture text"
    );
    fs::remove_file(env.ws.path().join("hook-output")).unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "{}", target.display()).unwrap();
    wait(2);
    assert!(!env.ws.path().join("hook-output").exists());
    let mut enabled = initial.clone();
    enabled["plugin"]["hooks"].as_table_mut().unwrap().insert(
        "tool-read".into(),
        TomlValue::try_from(json!({"tools":["read"]})).unwrap(),
    );
    fs::write(&config, toml::to_string(&enabled).unwrap()).unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "/reload\n{}",
        target.display()
    )
    .unwrap();
    wait(3);
    assert_eq!(
        fs::read_to_string(env.ws.path().join("hook-output")).unwrap(),
        "hooks:turn-start:external fixture text"
    );
    fs::remove_file(env.ws.path().join("hook-output")).unwrap();
    fs::write(&config, toml::to_string(&initial).unwrap()).unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "/reload\n{}\n/quit",
        target.display()
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
    assert!(!env.ws.path().join("hook-output").exists());
    assert_eq!(
        fs::read(
            config
                .parent()
                .unwrap()
                .join("plugins/hooks/component.wasm")
        )
        .unwrap(),
        SDK_HOOKS
    );
    assert_eq!(seen.lock().unwrap().len(), 4);
    let rows: Vec<Value> = records(&env.audit_log());
    let loads: Vec<_> = rows
        .iter()
        .filter(|row| row["args"]["operation"] == "load-hooks")
        .collect();
    assert_eq!(loads.len(), 3);
    for row in loads {
        assert_eq!(row["args"]["sha256"], sha256_hex(SDK_HOOKS));
    }
}

#[test]
fn a_custom_event_recipient_cannot_borrow_the_publishers_write_grant() {
    let (base, _) = serve(vec![sse_response(&openai_sse("done", &[]))]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let config = env.xdg.path().join("config/saddle/config.toml");
    let dir = config.parent().unwrap().join("plugins/listener");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("component.wasm"), SDK_EVENT_LISTENER).unwrap();
    fs::write(dir.join("plugin.toml"), "id='listener'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['event-subscribe','fs-read']\n").unwrap();
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{original}\n[plugins.listener]\nenabled=true\nmanifest='listener/plugin.toml'\nsha256='{}'\n[plugin.listener]\nevent-subscribe={{}}\nfs-read=[{{root='workspace'}}]\n",sha256_hex(SDK_EVENT_LISTENER))).unwrap();
    fs::write(env.ws.path().join("sample"), "from publisher").unwrap();
    assert_success(&env.saddle(&["run", "sample"]));
    assert_eq!(
        fs::read_to_string(env.ws.path().join("hook-output")).unwrap(),
        "hooks:turn-start:from publisher"
    );
    let rows = records(&env.audit_log());
    assert!(rows.iter().any(|row| row["args"]["operation"] == "receive"
        && row["args"]["publisher"] == "hooks"
        && row["principal"] == "plugin.listener"
        && row["allow"] == true));
    assert!(rows.iter().any(|row| row["scope"] == "fs-write"
        && row["principal"] == "plugin.listener"
        && row["allow"] == false));
}

#[test]
fn publication_rejects_oversize_payloads_and_full_queues_before_enqueueing() {
    for (prompt, extra, rejected_name) in [
        ("overflow", "[tool-limits]\nevent-queue=1\n", "two"),
        ("sample", "[tool-limits]\nevent-bytes=40\n", "turn-start"),
    ] {
        let (base, _) = serve(vec![sse_response(&openai_sse("done", &[]))]);
        let env = Env::new(Api::Openai, &base);
        configure(&env, extra);
        fs::write(env.ws.path().join("sample"), "x".repeat(100)).unwrap();
        assert_success(&env.saddle(&["run", prompt]));
        assert!(!env.ws.path().join("hook-output").exists());
        assert!(
            records(&env.audit_log())
                .iter()
                .any(|row| row["args"]["operation"] == "publish"
                    && row["args"]["name"] == rejected_name
                    && row["allow"] == false)
        );
    }
}

#[test]
fn custom_only_listener_can_receive_without_transcript_or_publish_grants() {
    let (base, _) = serve(vec![sse_response(&openai_sse("done", &[]))]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let config = env.xdg.path().join("config/saddle/config.toml");
    let dir = config.parent().unwrap().join("plugins/listener");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("component.wasm"), SDK_EVENT_LISTENER).unwrap();
    fs::write(dir.join("plugin.toml"), "id='listener'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['event-subscribe','fs-write']\n").unwrap();
    let original = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{original}\n[plugins.listener]\nenabled=true\nmanifest='listener/plugin.toml'\nsha256='{}'\n[plugin.listener]\nevent-subscribe={{}}\nfs-write=[{{root='workspace'}}]\n",sha256_hex(SDK_EVENT_LISTENER))).unwrap();
    fs::write(env.ws.path().join("sample"), "custom payload").unwrap();
    assert_success(&env.saddle(&["run", "sample"]));
    assert_eq!(
        fs::read_to_string(env.ws.path().join("listener-output")).unwrap(),
        "hooks:turn-start:custom payload"
    );
    assert!(
        !records(&env.audit_log())
            .iter()
            .any(|row| row["principal"] == "plugin.listener" && row["scope"] == "transcript-read")
    );
}

#[test]
fn workspace_sandbox_overrides_enable_revoke_and_keep_invalid_reloads_atomic() {
    let (base, seen) = serve(
        (0..7)
            .map(|_| sse_response(&openai_sse("done", &[])))
            .collect(),
    );
    let env = Env::new(Api::Openai, &base);
    configure(&env, "tool-read={tools=['read']}\n");
    let external = tempfile::tempdir().unwrap();
    let target = external.path().join("external.txt");
    fs::write(&target, "workspace override content").unwrap();
    fs::write(env.ws.path().join("sample"), "workspace content").unwrap();
    fs::write(env.ws.path().join(".env"), "excluded content").unwrap();
    let config = env.xdg.path().join("config/saddle/config.toml");
    let mut table: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    table["agent"].as_table_mut().unwrap().insert(
        "tool-read".into(),
        TomlValue::try_from(json!({"tools":["read"]})).unwrap(),
    );
    // Keep the parent read role available without declaring the external directory yet.
    table.insert(
        "sandbox".into(),
        TomlValue::try_from(json!({"read":[{"path":env.ws.path(),"tools":["read"]}]})).unwrap(),
    );
    fs::write(&config, toml::to_string(&table).unwrap()).unwrap();
    env.write_override("");
    let workspace = env.ws.path().canonicalize().unwrap();
    let key = workspace
        .to_string_lossy()
        .trim_start_matches('/')
        .replace(['/', '\\', ':'], "-");
    let path = config
        .parent()
        .unwrap()
        .join("policies")
        .join(format!("--{key}--.toml"));
    let override_text = format!(
        "workspace={:?}\n[sandbox]\nread=[{{path={:?},tools=['read']}}]\n",
        env.ws.path().canonicalize().unwrap().display().to_string(),
        format!("+{}", external.path().display())
    );
    let mut child = env
        .command(&["--session", "scoped", "run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait = |count| {
        let end = Instant::now() + Duration::from_secs(20);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < end, "waiting for request {count}");
            thread::sleep(Duration::from_millis(10));
        }
    };
    let output_file = env.ws.path().join("hook-output");
    writeln!(child.stdin.as_mut().unwrap(), "sample").unwrap();
    wait(1);
    assert!(
        fs::read_to_string(&output_file)
            .unwrap_or_else(|error| panic!(
                "{error}; audit: {:?}",
                records(&env.audit_log())
                    .into_iter()
                    .filter(|row| row["allow"] == false)
                    .collect::<Vec<_>>()
            ))
            .contains("workspace content")
    );
    fs::remove_file(&output_file).unwrap();
    writeln!(child.stdin.as_mut().unwrap(), "{}", target.display()).unwrap();
    wait(2);
    assert!(!output_file.exists());
    fs::write(&path, &override_text).unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "/reload\n{}",
        target.display()
    )
    .unwrap();
    wait(3);
    assert!(
        fs::read_to_string(&output_file)
            .unwrap_or_else(|error| panic!(
                "{error}; audit: {:?}",
                records(&env.audit_log())
                    .into_iter()
                    .filter(|row| row["allow"] == false)
                    .collect::<Vec<_>>()
            ))
            .contains("workspace override content")
    );
    fs::remove_file(&output_file).unwrap();
    assert_success(&env.saddle(&["--session", "other", "run", target.to_str().unwrap()]));
    assert_eq!(seen.lock().unwrap().len(), 4);
    assert!(
        fs::read_to_string(&output_file)
            .unwrap()
            .contains("workspace override content")
    );
    fs::remove_file(&output_file).unwrap();
    fs::write(
        &path,
        format!(
            "workspace={:?}\n[sandbox]\nread=['+/one','/two']",
            env.ws.path().display().to_string()
        ),
    )
    .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "/reload\n{}",
        target.display()
    )
    .unwrap();
    wait(5);
    assert!(
        fs::read_to_string(&output_file)
            .unwrap_or_else(|error| panic!(
                "{error}; audit: {:?}",
                records(&env.audit_log())
                    .into_iter()
                    .filter(|row| row["allow"] == false)
                    .collect::<Vec<_>>()
            ))
            .contains("workspace override content")
    );
    fs::remove_file(&output_file).unwrap();
    writeln!(child.stdin.as_mut().unwrap(), ".env").unwrap();
    wait(6);
    assert!(!output_file.exists());
    fs::remove_file(&path).unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "/reload\n{}\n/quit",
        target.display()
    )
    .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);
    assert!(!output_file.exists());
    assert!(text(&output.stdout).contains("cannot mix prefixed and unprefixed"));
    assert_eq!(seen.lock().unwrap().len(), 7);
    assert_eq!(
        fs::read(
            config
                .parent()
                .unwrap()
                .join("plugins/hooks/component.wasm")
        )
        .unwrap(),
        SDK_HOOKS
    );
    let rows = records(&env.audit_log());
    assert!(
        rows.iter()
            .any(|row| row["args"]["sandbox-config"]["override"]["sha256"].is_string())
    );
}
