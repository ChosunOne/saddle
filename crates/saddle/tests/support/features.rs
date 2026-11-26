use std::fs;
use std::io::{Read, Write};
use std::mem;
use std::net::TcpListener;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use saddle_policy::audit::sha256_hex;
use saddle_standard::SDK_FEATURES;
use serde_json::{Value, json};
use toml::{Table, Value as TomlValue};

use super::{
    Api, Env, KEY, Seen, model_request, openai_sse, records, result_text, serve, serve_dynamic,
    sse_response, text,
};

const TOOL: &str = "plugin__features__probe";

fn configure(env: &Env, extra: &str) {
    let config = env.xdg.path().join("config/saddle/config.toml");
    let mut table: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    for scope in ["persist", "veto", "context-contribute", "ui"] {
        table["agent"]
            .as_table_mut()
            .unwrap()
            .insert(scope.into(), TomlValue::Table(Table::new()));
    }
    table["agent"].as_table_mut().unwrap().insert(
        "model-invoke".into(),
        TomlValue::try_from(json!({"tokens-per-session":200000})).unwrap(),
    );
    fs::write(&config, toml::to_string(&table).unwrap()).unwrap();
    install(env, "features");
    if !extra.is_empty() {
        let mut table: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
        let update: Table = extra.parse().unwrap();
        for (key, value) in update {
            table.insert(key, value);
        }
        fs::write(config, toml::to_string(&table).unwrap()).unwrap();
    }
}

fn install(env: &Env, id: &str) {
    let config = env.xdg.path().join("config/saddle/config.toml");
    let dir = config.parent().unwrap().join(format!("plugins/{id}"));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("component.wasm"), SDK_FEATURES).unwrap();
    fs::write(dir.join("plugin.toml"), format!("id='{id}'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','persist','veto','context-contribute','model-invoke','ui']\n")).unwrap();
    let original = fs::read_to_string(&config).unwrap();
    fs::write(config, format!("{original}\n[plugins.{id}]\nenabled=true\nmanifest='{id}/plugin.toml'\nsha256='{}'\n[plugin.{id}]\ntool={{}}\npersist={{}}\nveto={{}}\ncontext-contribute={{}}\nui={{}}\nmodel-invoke={{tokens-per-session=200000}}\n", sha256_hex(SDK_FEATURES))).unwrap();
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn call(env: &Env, tool: &str, args: Value) -> Output {
    env.saddle(&["tool", tool, &args.to_string()])
}

#[test]
fn private_kv_survives_restart_isolates_principals_and_has_atomic_preconditions() {
    let mut env = Env::new(Api::Openai, "http://127.0.0.1:1");
    configure(&env, "");
    install(&env, "other");
    success(&call(
        &env,
        TOOL,
        json!({"action":"put","key":"../auth.json","value":"private payload"}),
    ));
    let got = call(&env, TOOL, json!({"action":"get","key":"../auth.json"}));
    success(&got);
    assert!(text(&got.stdout).contains("private payload"));
    let original_workspace = mem::replace(&mut env.ws, tempfile::tempdir().unwrap());
    let foreign = call(&env, TOOL, json!({"action":"get","key":"../auth.json"}));
    success(&foreign);
    assert!(text(&foreign.stdout).contains("missing"));
    env.ws = original_workspace;
    let other = call(
        &env,
        "plugin__other__probe",
        json!({"action":"get","key":"../auth.json"}),
    );
    success(&other);
    assert!(text(&other.stdout).contains("missing"));
    assert!(!env.xdg.path().join("config/saddle/auth.json").exists());
    let failed = call(
        &env,
        TOOL,
        json!({"action":"cas","key":"../auth.json","expected":"wrong","value":"not committed"}),
    );
    success(&failed);
    assert!(text(&failed.stdout).contains("false"));
    let swapped = call(
        &env,
        TOOL,
        json!({"action":"cas","key":"../auth.json","expected":"private payload","value":"updated"}),
    );
    success(&swapped);
    assert!(text(&swapped.stdout).contains("true"));
    let config = env.xdg.path().join("config/saddle/config.toml");
    let original: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let mut revoked = original.clone();
    revoked["plugin"]["features"]
        .as_table_mut()
        .unwrap()
        .remove("persist");
    fs::write(&config, toml::to_string(&revoked).unwrap()).unwrap();
    let denied = call(&env, TOOL, json!({"action":"get","key":"../auth.json"}));
    assert!(
        text(&denied.stderr).contains("denied:"),
        "{}",
        text(&denied.stderr)
    );
    fs::write(&config, toml::to_string(&original).unwrap()).unwrap();
    let got = call(&env, TOOL, json!({"action":"get","key":"../auth.json"}));
    success(&got);
    assert!(text(&got.stdout).contains("updated"));
    assert_eq!(
        fs::read(
            config
                .parent()
                .unwrap()
                .join("plugins/features/component.wasm")
        )
        .unwrap(),
        SDK_FEATURES
    );
}

#[test]
fn persistent_values_and_total_quota_are_runtime_budgets() {
    let env = Env::new(Api::Openai, "http://127.0.0.1:1");
    configure(
        &env,
        "[tool-limits]\npersist-value-bytes=8\npersist-total-bytes=10\n",
    );
    success(&call(
        &env,
        TOOL,
        json!({"action":"put","key":"a","value":"12345678"}),
    ));
    let denied = call(&env, TOOL, json!({"action":"put","key":"b","value":"123"}));
    assert!(
        text(&denied.stderr).contains("quota exceeded"),
        "{}",
        text(&denied.stderr)
    );
    let denied = call(
        &env,
        TOOL,
        json!({"action":"put","key":"a","value":"123456789"}),
    );
    assert!(text(&denied.stderr).contains("limit exceeded"));
    let got = call(&env, TOOL, json!({"action":"get","key":"a"}));
    success(&got);
    assert!(text(&got.stdout).contains("12345678"));
}

#[test]
fn vetoes_deny_and_fail_closed_before_tool_effects_without_rewriting_allowed_calls() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[
                (
                    "blocked",
                    "write",
                    json!({"path":"blocked","content":"must not appear"}),
                ),
                (
                    "spin",
                    "write",
                    json!({"path":"veto-spin","content":"must not appear","timeout_secs":1}),
                ),
                (
                    "ok",
                    "write",
                    json!({"path":"allowed","content":"original arguments"}),
                ),
                (
                    "policy",
                    "write",
                    json!({"path":"../not-permitted","content":"no"}),
                ),
            ],
        )),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    success(&env.saddle(&["run", "try three writes"]));
    assert!(!env.ws.path().join("blocked").exists());
    assert!(!env.ws.path().join("veto-spin").exists());
    assert_eq!(
        fs::read_to_string(env.ws.path().join("allowed")).unwrap(),
        "original arguments"
    );
    let seen = seen.lock().unwrap();
    assert!(result_text(&seen[1].body, "blocked").contains("fixture veto"));
    assert!(result_text(&seen[1].body, "spin").contains("veto failed closed"));
    assert!(result_text(&seen[1].body, "policy").contains("denied:"));
    assert!(
        !env.ws
            .path()
            .parent()
            .unwrap()
            .join("not-permitted")
            .exists()
    );
    let origin = seen[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "blocked")
        .unwrap();
    assert!(
        origin["content"]
            .as_str()
            .unwrap()
            .contains("plugin=\"features\"")
    );
}

#[test]
fn contributions_are_framed_branch_items_and_corrections_append_instead_of_rewriting() {
    let (base, seen) = serve_dynamic(3, move |index, body| match index {
        0 => sse_response(&openai_sse(
            "",
            &[(
                "first",
                TOOL,
                json!({"action":"note","value":"first plugin fact"}),
            )],
        )),
        1 => {
            let correction = result_text(body, "first").to_owned();
            assert!(body.to_string().contains("first plugin fact"));
            sse_response(&openai_sse(
                "",
                &[(
                    "correct",
                    TOOL,
                    json!({"action":"note","value":"corrected plugin fact","corrects":correction}),
                )],
            ))
        }
        _ => sse_response(&openai_sse("done", &[])),
    });
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let output = env.saddle(&["run", "--json", "produce two facts"]);
    success(&output);
    let events: Vec<Value> = text(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let facts: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "plugin-contribution")
        .collect();
    assert_eq!(facts.len(), 2);
    assert_eq!(
        facts[1]["contribution"]["corrects"],
        facts[0]["contribution"]["id"]
    );
    assert_eq!(
        facts[0]["contribution"]["provenance"]["scope"],
        "context-contribute"
    );
    let seen = seen.lock().unwrap();
    let last = &seen[2].body;
    assert!(
        !last["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("first plugin fact")
    );
    let messages = last["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "user"
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("<plugin-output-")))
            .count(),
        2
    );
    let db = Connection::open(env.xdg.path().join("state/saddle/sessions.db")).unwrap();
    let saved: i64 = db
        .query_row(
            "SELECT count(*) FROM nodes WHERE kind='item' AND body LIKE '%first plugin fact%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(saved >= 1);
}

#[test]
fn structured_diagnostics_validate_locations_without_reading_the_named_file() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[
                (
                    "valid",
                    TOOL,
                    json!({"action":"diagnostic","value":"not-created.rs"}),
                ),
                (
                    "invalid",
                    TOOL,
                    json!({"action":"diagnostic","value":"../outside.rs"}),
                ),
            ],
        )),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let output = env.saddle(&["run", "--json", "produce diagnostics"]);
    success(&output);
    let events: Vec<Value> = text(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let facts: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "plugin-contribution")
        .collect();
    assert_eq!(facts.len(), 1);
    let diagnostic = &facts[0]["contribution"]["diagnostic"];
    assert_eq!(diagnostic["root"], "workspace");
    assert_eq!(diagnostic["path"], "not-created.rs");
    assert_eq!(diagnostic["severity"], "warning");
    assert!(!env.ws.path().join("not-created.rs").exists());
    assert!(result_text(&seen.lock().unwrap()[1].body, "invalid").contains("relative path"));
}

#[test]
fn contributions_without_a_conversation_and_foreign_corrections_are_refused() {
    let env = Env::new(Api::Openai, "http://127.0.0.1:1");
    configure(&env, "");
    let denied = call(&env, TOOL, json!({"action":"note","value":"not attached"}));
    assert!(text(&denied.stderr).contains("no active conversation"));
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[(
                "bad",
                TOOL,
                json!({"action":"note","value":"bad correction","corrects":"not-on-this-branch"}),
            )],
        )),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    success(&env.saddle(&["run", "try a correction"]));
    assert!(result_text(&seen.lock().unwrap()[1].body, "bad").contains("active branch"));
}

#[test]
fn isolated_model_requests_do_not_receive_main_context_or_execute_returned_tools() {
    for model_calls_tools in [false, true] {
        let sdk = if model_calls_tools {
            openai_sse(
                "",
                &[(
                    "unexpected",
                    "write",
                    json!({"path":"forbidden","content":"no"}),
                )],
            )
        } else {
            openai_sse("sdk-only", &[])
        };
        let (base, seen) = serve(vec![
            sse_response(&openai_sse(
                "",
                &[(
                    "sdk",
                    TOOL,
                    json!({"action":"model","value":"plugin question"}),
                )],
            )),
            sse_response(&sdk),
            sse_response(&openai_sse("done", &[])),
        ]);
        let env = Env::new(Api::Openai, &base);
        configure(&env, "");
        success(&env.saddle(&["run", "main private prompt"]));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[1].body.get("tools").is_none());
        assert!(seen[1].body.to_string().contains("plugin question"));
        assert!(!seen[1].body.to_string().contains("main private prompt"));
        assert!(seen[1].head.contains(KEY));
        assert!(!result_text(&seen[2].body, "sdk").contains(KEY));
        assert!(!env.ws.path().join("forbidden").exists());
        assert!(
            result_text(&seen[2].body, "sdk").contains(if model_calls_tools {
                "none were executed"
            } else {
                "sdk-only"
            })
        );
        let db = Connection::open(env.xdg.path().join("state/saddle/sessions.db")).unwrap();
        let usage: i64 = db
            .query_row(
                "SELECT count(*) FROM nodes WHERE kind='usage' AND body LIKE '%plugin%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(usage, 1);
        let body: String = db
            .query_row(
                "SELECT body FROM nodes WHERE kind='usage' AND body LIKE '%\"kind\":\"plugin\"%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let saved: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(saved["principal"], "plugin.features");
        assert!(!saved["call_id"].as_str().unwrap().is_empty());
    }
}

#[test]
fn model_cost_caps_refuse_unknown_prices_without_sending_a_plugin_request() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse("", &[("sdk", TOOL, json!({"action":"model"}))])),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let config = env.xdg.path().join("config/saddle/config.toml");
    let mut table: Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    table["agent"]["model-invoke"]
        .as_table_mut()
        .unwrap()
        .insert("cost-per-session".into(), TomlValue::Float(1.0));
    fs::write(config, toml::to_string(&table).unwrap()).unwrap();
    success(&env.saddle(&["run", "try a bounded request"]));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(result_text(&seen[1].body, "sdk").contains("price is unknown"));
}

#[test]
fn ui_panels_are_labelled_sanitized_and_not_model_context_and_render_is_read_only() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[(
                "show",
                TOOL,
                json!({"action":"panel","value":"private-panel\u{1b}]52;c;payload\u{7}"}),
            )],
        )),
        sse_response(&openai_sse("done", &[])),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let output = env.saddle(&["run", "--json", "show a panel"]);
    success(&output);
    let events: Vec<Value> = text(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let panel = events
        .iter()
        .rfind(|event| event["type"] == "plugin-panels")
        .unwrap();
    assert_eq!(panel["panels"][0]["plugin"], "plugin.features");
    assert!(
        panel["panels"][0]["title"]
            .as_str()
            .unwrap()
            .starts_with("private-panel")
    );
    assert!(!panel.to_string().contains("\\u001b"));
    let seen_guard = seen.lock().unwrap();
    assert!(
        !seen_guard[1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.contains("private-panel")))
    );
    drop(seen_guard);
    success(&call(
        &env,
        TOOL,
        json!({"action":"put","key":"render-mutate","value":"1"}),
    ));
    let output = env.saddle(&["run", "--json", "render without effects"]);
    success(&output);
    assert!(text(&output.stdout).contains("refused"));
    assert!(
        !seen.lock().unwrap()[2]
            .body
            .to_string()
            .contains("illegal render contribution")
    );
    assert!(
        records(&env.audit_log())
            .iter()
            .any(|row| row["scope"] == "context-contribute" && row["allow"] == false)
    );
}

#[test]
fn model_invocation_timeout_stops_local_wait_and_does_not_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let record = seen.clone();
    let times = Arc::new(Mutex::new(Vec::new()));
    let timing = times.clone();
    thread::spawn(move || {
        for index in 0..3 {
            let (mut stream, mut reader, head) = model_request(&listener);
            let len: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let mut bytes = vec![0; len];
            reader.read_exact(&mut bytes).unwrap();
            record.lock().unwrap().push(Seen {
                head,
                body: serde_json::from_slice(&bytes).unwrap(),
            });
            timing.lock().unwrap().push(Instant::now());
            if index == 1 {
                stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n").unwrap();
                thread::spawn(move || {
                    thread::sleep(Duration::from_secs(30));
                    drop(stream);
                });
            } else {
                let response = if index == 0 {
                    openai_sse(
                        "",
                        &[("slow", TOOL, json!({"action":"model","timeout_secs":1}))],
                    )
                } else {
                    openai_sse("done", &[])
                };
                stream
                    .write_all(sse_response(&response).as_bytes())
                    .unwrap();
            }
        }
    });
    let env = Env::new(Api::Openai, &base);
    configure(&env, "");
    let output = env.saddle(&["run", "deadline test"]);
    success(&output);
    let times = times.lock().unwrap();
    assert_eq!(times.len(), 3);
    assert!(times[2].duration_since(times[1]) < Duration::from_secs(5));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(result_text(&seen[2].body, "slow").contains("timed out"));
}
