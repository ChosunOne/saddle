//! The model loop end to end: `saddle run` against a mock provider that speaks either API. The mock
//! asks for tool calls, and the test checks what the policy let through, what went back to the
//! model, and what reached the audit log.

#[path = "support/plugin.rs"]
mod plugin_fixture;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use rusqlite::Connection;
use serde_json::{Value, json};

const POLICY: &str = r#"
version = 1

[agent]
fs-read  = [{ root = "workspace", why = "reads the repo" }]
fs-write = [{ root = "workspace", why = "edits the repo" }]
# Listing the provider key here must not pass it to commands.
exec     = { why = "runs commands", env = ["SADDLE_TEST_KEY"], timeout-secs = 20 }
"#;

const KEY: &str = "test-secret";

struct Seen {
    head: String,
    body: Value,
}

/// Answers catalogue probes without consuming a canned model response (D34).
fn model_request(listener: &TcpListener) -> (TcpStream, BufReader<TcpStream>, String) {
    loop {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line.to_ascii_lowercase());
        }
        if head.starts_with("get ") {
            let path = head.split_whitespace().nth(1).unwrap();
            assert!(
                matches!(path, "/models" | "/v1/models?limit=1000")
                    || path.starts_with("/models?client_version="),
                "{path}"
            );
            stream
                .write_all(error_response("200 OK", None, &json!({"data": []})).as_bytes())
                .unwrap();
            continue;
        }
        assert!(head.starts_with("post "), "{head}");
        return (stream, reader, head);
    }
}

/// Answers one model request per canned response, in order, and records it.
fn serve(responses: Vec<String>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    thread::spawn(move || {
        for response in responses {
            let (mut stream, mut reader, head) = model_request(&listener);
            let len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse::<usize>().unwrap())
                .expect("request has a content-length");
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            record.lock().unwrap().push(Seen {
                head,
                body: serde_json::from_slice(&body).unwrap(),
            });
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (base, seen)
}

fn sse_response(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{body}")
}

fn error_response(status: &str, retry_after: Option<u32>, body: &Value) -> String {
    let body = body.to_string();
    let retry = retry_after.map_or(String::new(), |s| format!("retry-after: {s}\r\n"));
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{retry}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[derive(Clone, Copy, PartialEq)]
enum Api {
    Anthropic,
    Openai,
}

impl Api {
    fn name(self) -> &'static str {
        match self {
            Api::Anthropic => "anthropic",
            Api::Openai => "openai",
        }
    }

    /// A reply with optional text and tool calls `(id, name, input)`.
    fn reply(self, text: &str, calls: &[(&str, &str, Value)]) -> String {
        match self {
            Api::Anthropic => anthropic_sse(text, calls),
            Api::Openai => openai_sse(text, calls),
        }
    }
}

fn anthropic_sse(text: &str, calls: &[(&str, &str, Value)]) -> String {
    let mut events = vec![
        json!({"type":"message_start","message":{"model":"mock-model","usage":{"input_tokens":100,"output_tokens":1}}}),
        // Every reply starts with a thinking block, which must come back unchanged next request.
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":format!("sig-{}", calls.len())}}),
        json!({"type":"content_block_stop","index":0}),
    ];
    let mut index = 1;
    if !text.is_empty() {
        events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}));
        events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}));
        events.push(json!({"type":"content_block_stop","index":index}));
        index += 1;
    }
    for (id, name, input) in calls {
        let json = input.to_string();
        let (a, b) = json.split_at(json.len() / 2);
        events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
        for part in [a, b] {
            events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":part}}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
        index += 1;
    }
    let stop = if calls.is_empty() {
        "end_turn"
    } else {
        "tool_use"
    };
    events.push(
        json!({"type":"message_delta","delta":{"stop_reason":stop},"usage":{"output_tokens":20}}),
    );
    events.push(json!({"type":"message_stop"}));
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn openai_sse(text: &str, calls: &[(&str, &str, Value)]) -> String {
    openai_sse_usage(text, calls, 100, None)
}

/// The same, with the prompt usage stated, for the cache-miss test (D50).
fn openai_sse_usage(
    text: &str,
    calls: &[(&str, &str, Value)],
    prompt_tokens: u64,
    cached: Option<u64>,
) -> String {
    let mut chunks = Vec::new();
    if !text.is_empty() {
        chunks.push(json!({"model":"mock-model","choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}));
    }
    for (i, (id, name, input)) in calls.iter().enumerate() {
        let json = input.to_string();
        let (a, b) = json.split_at(json.len() / 2);
        chunks.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":i,"id":id,"type":"function","function":{"name":name,"arguments":a}}]}}]}));
        chunks.push(json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":i,"function":{"arguments":b}}]}}]}));
    }
    let finish = if calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    };
    chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
    let mut usage = json!({"prompt_tokens": prompt_tokens, "completion_tokens": 20});
    if let Some(cached) = cached {
        usage["prompt_tokens_details"] = json!({ "cached_tokens": cached });
    }
    chunks.push(json!({"choices":[],"usage":usage}));
    let mut s: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    s.push_str("data: [DONE]\n\n");
    s
}

struct Env {
    ws: tempfile::TempDir,
    xdg: tempfile::TempDir,
}

impl Env {
    fn new(api: Api, base: &str) -> Self {
        let env = Self {
            ws: tempfile::tempdir().unwrap(),
            xdg: tempfile::tempdir().unwrap(),
        };
        let ws = env.ws.path();
        fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        fs::create_dir_all(env.xdg.path().join("state/saddle")).unwrap();
        fs::create_dir_all(env.xdg.path().join("home")).unwrap();
        let config = env.xdg.path().join("config/saddle");
        fs::create_dir_all(&config).unwrap();
        let policy: toml::Table = POLICY.parse().unwrap();
        fs::write(
            config.join("config.toml"),
            format!(
                "provider = \"mock\"\n{}\n[providers.mock]\napi = \"{}\"\nbase-url = \"{base}\"\n\
                 model = \"mock-model\"\napi-key-env = \"SADDLE_TEST_KEY\"\n",
                toml::to_string(&json!({"agent": policy["agent"]})).unwrap(),
                api.name()
            ),
        )
        .unwrap();
        env
    }

    /// This workspace's own policy, layered on the global one.
    fn write_override(&self, text: &str) {
        let ws = self.ws.path().canonicalize().unwrap();
        let key = ws
            .to_string_lossy()
            .trim_start_matches('/')
            .replace(['/', '\\', ':'], "-");
        let dir = self.xdg.path().join("config/saddle/policies");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("--{key}--.toml")),
            format!("workspace = {:?}\n{text}", ws.display().to_string()),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_saddle"));
        cmd.arg("-C")
            .arg(self.ws.path())
            .args(args)
            .env("HOME", self.xdg.path().join("home"))
            .env("XDG_STATE_HOME", self.xdg.path().join("state"))
            .env("XDG_CONFIG_HOME", self.xdg.path().join("config"))
            .env("SADDLE_TEST_KEY", KEY);
        cmd
    }

    fn saddle(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn audit_log(&self) -> PathBuf {
        let dir = self.xdg.path().join("state/saddle/audit");
        let mut logs: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        logs.sort_by_key(|p| p.metadata().unwrap().modified().unwrap());
        logs.pop().unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Every string in a JSON value, for searching a request body.
fn strings(v: &Value, out: &mut String) {
    match v {
        Value::String(s) => {
            out.push_str(s);
            out.push('\n');
        }
        Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
        Value::Object(o) => o.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

fn records(log: &Path) -> Vec<Value> {
    fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn tool_loop(api: Api) {
    let calls = [
        (
            "t1",
            "write",
            json!({"path": "notes/hello.txt", "content": "hi"}),
        ),
        (
            "t2",
            "write",
            json!({"path": ".git/hooks/pre-commit", "content": "#!/bin/sh\necho pwned\n"}),
        ),
        (
            "t3",
            "bash",
            json!({"command": "echo key=${SADDLE_TEST_KEY:-unset}; cat notes/hello.txt"}),
        ),
    ];
    let (base, seen) = serve(vec![
        sse_response(&api.reply("Writing.", &calls)),
        sse_response(&api.reply("All done.", &[])),
    ]);
    let env = Env::new(api, &base);
    let out = env.saddle(&["run", "write a note"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        text(&out.stdout).contains("All done."),
        "{}",
        text(&out.stdout)
    );
    assert!(stderr.contains("● write notes/hello.txt"), "{stderr}");

    let ws = env.ws.path();
    assert_eq!(
        fs::read_to_string(ws.join("notes/hello.txt")).unwrap(),
        "hi"
    );
    assert!(!ws.join(".git/hooks/pre-commit").exists());

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let auth = match api {
        Api::Anthropic => format!("x-api-key: {KEY}"),
        Api::Openai => format!("authorization: bearer {KEY}"),
    };
    assert!(seen[0].head.contains(&auth), "{}", seen[0].head);
    assert!(seen[0].head.starts_with("post "), "{}", seen[0].head);

    let first = &seen[0].body;
    assert_eq!(first["model"], "mock-model");
    assert_eq!(first["stream"], true);
    let names: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            t["name"]
                .as_str()
                .or(t["function"]["name"].as_str())
                .unwrap()
        })
        .collect();
    assert_eq!(
        names,
        [
            "session_query",
            "tool_search",
            "read",
            "write",
            "edit",
            "patch",
            "ls",
            "find",
            "grep",
            "bash",
            "fetch",
            "websearch"
        ]
    );
    let mut first_text = String::new();
    strings(first, &mut first_text);
    assert!(first_text.contains(&ws.canonicalize().unwrap().display().to_string()));
    assert!(first_text.contains("agent.fs-write[0] = workspace"));

    let mut second = String::new();
    strings(&seen[1].body, &mut second);
    assert!(
        second.contains("created notes/hello.txt (2 bytes"),
        "{second}"
    );
    assert!(
        second.contains("denied: workspace:.git/hooks/pre-commit is reserved by the host"),
        "{second}"
    );
    assert!(second.contains("key=unset\nhi"), "{second}");
    assert!(!second.contains(KEY));
    if api == Api::Anthropic {
        // The thinking block went back first in the assistant message, unchanged.
        let assistant = &seen[1].body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(
            assistant["content"][0],
            json!({"type":"thinking","thinking":"","signature":"sig-3"})
        );
    }

    let log = env.audit_log();
    let recs = records(&log);
    let models: Vec<&Value> = recs.iter().filter(|r| r["kind"] == "model").collect();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0]["args"]["provider"], "mock");
    assert_eq!(models[0]["args"]["usage"]["input"], 100);
    let denied = recs
        .iter()
        .filter(|r| r["kind"] == "decision" && r["allow"] == false)
        .count();
    assert_eq!(denied, 1);
    let verify = env.saddle(&["audit", "verify", log.to_str().unwrap()]);
    assert!(verify.status.success(), "{}", text(&verify.stdout));
}

#[test]
fn a_request_is_recorded_with_what_it_used_and_which_model_answered() {
    // D94: the tool can see what each request cost and whether it hit the provider's cache, which
    // is the question a poor hit rate is asked about. The mock states one request without a cached
    // prompt and one with, so both columns are exercised.
    let sql =
        "SELECT kind, provider, model, input, cache_read, cached FROM requests ORDER BY ordinal";
    let calls = [("q1", "session_query", json!({"sql": sql}))];
    let (base, seen) = serve(vec![
        sse_response(&openai_sse_usage("", &calls, 100, Some(40))),
        sse_response(&openai_sse_usage("done", &[], 250, None)),
    ]);
    let env = Env::new(Api::Openai, &base);
    env.write_override("[agent]\ntranscript-read = {}\n");
    let out = env.saddle(&["run", "what did that cost"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let messages = seen[1].body["messages"].as_array().unwrap();
    let result = messages
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("the query ran")["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let result: Value = serde_json::from_str(&result).unwrap();
    // One row for the request that carried the call, which is the one that has happened so far.
    // The provider states 100 prompt tokens of which 40 were cached, and the uncached 60 is what
    // `input` holds, so a hit rate is read as cache_read over input + cache_read.
    assert_eq!(
        result["rows"],
        json!([["reply", "mock", "mock-model", 60, 40, 1]]),
        "{result}"
    );
}

#[test]
fn session_queries_are_scoped_read_only_and_audited() {
    let sql = "SELECT text FROM messages WHERE role='user'";
    let calls = [
        ("q1", "session_query", json!({"sql": sql})),
        (
            "q2",
            "session_query",
            json!({"sql": "DELETE FROM messages"}),
        ),
        ("q3", "session_query", json!({"sql": sql})),
    ];
    let (base, seen) = serve(vec![
        sse_response(&openai_sse("", &calls)),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    env.write_override("[agent]\ntranscript-read = {}\n");
    let out = env.saddle(&["run", "remember teal"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let messages = seen[1].body["messages"].as_array().unwrap();
    let results: Vec<_> = messages.iter().filter(|m| m["role"] == "tool").collect();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0]["content"], results[2]["content"]);
    let result: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["rows"], json!([["remember teal"]]));
    assert!(
        results[1]["content"]
            .as_str()
            .unwrap()
            .contains("session query failed")
    );
    let records = records(&env.audit_log());
    let decisions: Vec<_> = records
        .iter()
        .filter(|r| r["scope"] == "transcript-read")
        .collect();
    assert_eq!(decisions.len(), 3);
    assert!(
        decisions
            .iter()
            .all(|r| r["allow"] == true && r["args"]["session"].is_string())
    );
}

#[test]
fn session_queries_require_a_transcript_grant() {
    let calls = [(
        "q",
        "session_query",
        json!({"sql": "SELECT * FROM messages"}),
    )];
    let (base, seen) = serve(vec![
        sse_response(&openai_sse("", &calls)),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let out = env.saddle(&["run", "hello"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let mut request = String::new();
    strings(&seen.lock().unwrap()[1].body, &mut request);
    assert!(
        request.contains("denied: agent has no transcript-read grant"),
        "{request}"
    );
    assert!(
        records(&env.audit_log())
            .iter()
            .any(|r| r["scope"] == "transcript-read" && r["allow"] == false)
    );
}

#[test]
fn anthropic_tool_loop() {
    tool_loop(Api::Anthropic);
}

#[test]
fn openai_tool_loop() {
    tool_loop(Api::Openai);
}

#[test]
fn overload_is_retried_and_a_bad_request_fails_the_turn() {
    let (base, seen) = serve(vec![
        error_response(
            "529 Overloaded",
            Some(0),
            &json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        ),
        sse_response(&anthropic_sse("Fine.", &[])),
        error_response(
            "400 Bad Request",
            None,
            &json!({"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}),
        ),
    ]);
    let env = Env::new(Api::Anthropic, &base);
    let out = env.saddle(&["run", "hello"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("Fine."));
    assert!(
        text(&out.stderr).contains("retrying"),
        "{}",
        text(&out.stderr)
    );

    let out = env.saddle(&["run", "again"]);
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("mock returned 400: bad model"), "{stderr}");
    assert_eq!(seen.lock().unwrap().len(), 3);
    let recs = records(&env.audit_log());
    assert!(recs.iter().any(|r| {
        r["kind"] == "model"
            && r["args"]["error"]
                .as_str()
                .is_some_and(|e| e.contains("bad model"))
    }));
}

#[test]
fn responses_recovers_a_transient_stream_failure_without_a_second_user_prompt() {
    let failed = json!({"type":"response.failed","response":{
        "error":{"code":"server_error","message":"temporary backend failure"}}});
    let answer = [
        json!({"type":"response.output_text.delta","delta":"Recovered."}),
        json!({"type":"response.completed","response":{"id":"resp_ok","status":"completed",
            "model":"mock-model","usage":{"input_tokens":10,"output_tokens":2}}}),
    ];
    let answer: String = answer
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let (base, seen) = serve(vec![
        sse_response(&format!("data: {failed}\n\n")),
        sse_response(&answer),
    ]);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("api = \"openai\"", "api = \"openai-responses\"");
    fs::write(config, text).unwrap();

    let out = env.saddle(&["run", "hello"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout)
            .matches("Recovered.")
            .count(),
        1
    );
    assert!(stderr.contains("retrying once over HTTP"), "{stderr}");
    let sent = seen.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].body, sent[1].body,
        "retry must not append another prompt or transport note"
    );
    assert_eq!(sent[1].body["input"].as_array().unwrap().len(), 1);
}

#[test]
fn missing_key_stops_before_the_session_opens() {
    let env = Env::new(Api::Openai, "http://127.0.0.1:9");
    let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(env.ws.path())
        .args(["run", "hi"])
        .env("HOME", env.xdg.path().join("home"))
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .env_remove("SADDLE_TEST_KEY")
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("no API key for mock"), "{stderr}");
    assert!(
        stderr.contains("/login") && stderr.contains("set SADDLE_TEST_KEY"),
        "{stderr}"
    );
    assert!(!env.xdg.path().join("state/saddle/audit").exists());
}

#[test]
fn a_key_stored_by_login_is_used_when_the_variable_is_unset() {
    let (base, seen) = serve(vec![sse_response(&anthropic_sse("Hi.", &[]))]);
    let env = Env::new(Api::Anthropic, &base);
    fs::write(
        env.xdg.path().join("state/saddle/auth.json"),
        r#"{"mock": {"api-key": "stored-secret"}}"#,
    )
    .unwrap();
    let out = env
        .command(&["run", "hi"])
        .env_remove("SADDLE_TEST_KEY")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        seen.lock().unwrap()[0]
            .head
            .contains("x-api-key: stored-secret")
    );
}

#[test]
fn a_workspace_with_no_override_uses_the_global_policy() {
    let calls = [
        ("t1", "write", json!({"path": "made.txt", "content": "ok"})),
        ("t2", "read", json!({"path": ".env"})),
    ];
    let (base, seen) = serve(vec![
        sse_response(&anthropic_sse("", &calls)),
        sse_response(&anthropic_sse("Done.", &[])),
    ]);
    let env = Env::new(Api::Anthropic, &base);
    // No workspace override: the global policy applies as it is (D43).
    fs::write(env.ws.path().join(".env"), "TOKEN=1\n").unwrap();

    let out = env.saddle(&["run", "go"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(env.ws.path().join("made.txt")).unwrap(),
        "ok"
    );
    let mut second = String::new();
    strings(&seen.lock().unwrap()[1].body, &mut second);
    assert!(
        !second.contains("workspace:.env is excluded"),
        "the global policy has no exclusion for this fixture: {second}"
    );

    let check = env.saddle(&["check"]);
    let stdout = text(&check.stdout);
    assert!(
        stdout.contains("grants     ") && stdout.contains("config.toml"),
        "{stdout}"
    );
    // The override is created on first use, and adds nothing of its own.
    assert!(
        stdout.contains("has no grants for this directory: it inherits the base"),
        "{stdout}"
    );
}

#[test]
fn sigint_cancels_a_running_command_in_line_mode() {
    let calls = [("t1", "bash", json!({"command": "sleep 30"}))];
    let (base, seen) = serve(vec![sse_response(&anthropic_sse("", &calls))]);
    let env = Env::new(Api::Anthropic, &base);
    let child = env
        .command(&["run", "wait"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Long enough for the mock reply and the sandbox to start the sleep.
    thread::sleep(Duration::from_millis(1500));
    let start = Instant::now();
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    let stderr = text(&out.stderr);
    assert!(start.elapsed() < Duration::from_secs(5), "{stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("cancelled by the user"), "{stderr}");
    // The model was not asked again, and the session closed normally.
    assert_eq!(seen.lock().unwrap().len(), 1);
    let index = fs::read_to_string(env.xdg.path().join("state/saddle/sessions.jsonl")).unwrap();
    assert_eq!(index.lines().count(), 1);
}

#[test]
fn continue_picks_up_the_last_session_in_this_workspace() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse("First answer.", &[])),
        sse_response(&openai_sse("Second answer.", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let out = env.saddle(&["run", "remember the word teal"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = env.saddle(&["run", "-c", "which word?"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("continuing a session"));

    let seen = seen.lock().unwrap();
    let roles: Vec<&str> = seen[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(
        seen[1].body["messages"][1]["content"],
        "remember the word teal"
    );
    assert_eq!(seen[1].body["messages"][2]["content"], "First answer.");

    // One saved conversation, two turns, each naming the audit log of the run that made it (D53).
    let db = env.xdg.path().join("state/saddle/sessions.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let sessions: Vec<String> = conn
        .prepare("SELECT id FROM sessions")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let audits: Vec<String> = conn
        .prepare("SELECT body FROM nodes WHERE session = ?1 AND kind = 'turn' ORDER BY id")
        .unwrap()
        .query_map([&sessions[0]], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|b| {
            let v: Value = serde_json::from_str(&b.unwrap()).unwrap();
            v["audit"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(audits.len(), 2);
    assert_ne!(audits[0], audits[1]);
    let logs = env.xdg.path().join("state/saddle/audit");
    for audit in audits {
        assert!(logs.join(format!("{audit}.jsonl")).exists());
    }
}

#[test]
fn cancel_is_immediate_while_the_server_is_silent() {
    // Accepts the request and never answers, like a server still reading a long prompt.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (ready, accepted) = mpsc::channel();
    thread::spawn(move || {
        let (stream, _reader, _head) = model_request(&listener);
        ready.send(()).unwrap();
        thread::sleep(Duration::from_secs(60));
        drop(stream);
    });
    let env = Env::new(Api::Openai, &base);
    let child = env
        .command(&["run", "hello"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    accepted.recv_timeout(Duration::from_secs(10)).unwrap();
    let start = Instant::now();
    let killed = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let out = child.wait_with_output().unwrap();
    let elapsed = start.elapsed();
    let stderr = text(&out.stderr);
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}: {stderr}");
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("cancelled"), "{stderr}");
}

#[test]
fn plugin_results_have_host_provenance_saved_and_replayed_without_rewrapping() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[(
                "plugin-call",
                "plugin__reader__reader",
                json!({"secret":"not-for-the-audit-log"}),
            )],
        )),
        sse_response(&openai_sse("done", &[])),
        sse_response(&openai_sse("continued", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    plugin_fixture::configure(
        &env.xdg.path().join("config/saddle/config.toml"),
        &plugin_fixture::reader("file"),
        plugin_fixture::GRANTS,
    );
    fs::write(env.ws.path().join("file"), "a fact from the plugin").unwrap();
    let out = env.saddle(&["run", "--json", "read"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let events: Vec<Value> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let result = events.iter().find(|e| e["type"] == "result").unwrap();
    let provenance = &result["provenance"];
    assert_eq!(provenance["principal"], "plugin.reader");
    assert_eq!(provenance["scope"], "tool");
    assert_eq!(provenance["version"], "0.1.0");
    assert_eq!(provenance["component_sha256"].as_str().unwrap().len(), 64);
    assert!(provenance["call_id"].as_str().is_some());
    let saved = {
        let connection = Connection::open(env.xdg.path().join("state/saddle/sessions.db")).unwrap();
        let mut query = connection
            .prepare("SELECT body FROM nodes WHERE kind = 'item' ORDER BY id")
            .unwrap();
        let bodies = query.query_map([], |r| r.get::<_, String>(0)).unwrap();
        bodies
            .map(|s| serde_json::from_str::<Value>(&s.unwrap()).unwrap())
            .find_map(|item| item.get("tool-results").and_then(|r| r.get(0)).cloned())
            .unwrap()
    };
    assert_eq!(saved["provenance"], *provenance);
    assert_eq!(saved["content"], result["content"]);
    {
        let seen = seen.lock().unwrap();
        let tool = seen[0].body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["function"]["name"] == "plugin__reader__reader")
            .unwrap();
        assert!(
            tool["function"]["description"]
                .as_str()
                .unwrap()
                .contains("<plugin-output-")
        );
        let message = seen[1].body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(message["content"], saved["content"]);
        assert!(
            message.get("provenance").is_none(),
            "host metadata is not provider parameters"
        );
    }
    let audit = fs::read_to_string(env.audit_log()).unwrap();
    assert!(!audit.contains("not-for-the-audit-log"));
    let out = env.saddle(&["run", "--continue", "continue"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let mut replay = String::new();
    strings(&seen[2].body, &mut replay);
    assert!(
        replay.contains(saved["content"].as_str().unwrap()),
        "saved output was rewrapped: {replay}"
    );
}

#[test]
fn plugin_images_are_validated_delivered_saved_and_discarded_on_traps() {
    for fail_after in [false, true] {
        let (base, seen) = serve(vec![
            sse_response(&openai_sse(
                "",
                &[("picture", "plugin__reader__reader", json!({}))],
            )),
            sse_response(&openai_sse("done", &[])),
        ]);
        let env = Env::new(Api::Openai, &base);
        plugin_fixture::configure(
            &env.xdg.path().join("config/saddle/config.toml"),
            &plugin_fixture::image_emitter(&plugin_fixture::png(), fail_after),
            "tool={}",
        );
        let out = env.saddle(&["run", "--json", "show the picture"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let seen = seen.lock().unwrap();
        let mut sent = String::new();
        strings(&seen[1].body, &mut sent);
        assert_eq!(
            sent.contains("data:image/png;base64,"),
            !fail_after,
            "{sent}"
        );
        let connection = Connection::open(env.xdg.path().join("state/saddle/sessions.db")).unwrap();
        let mut query = connection
            .prepare("SELECT body FROM nodes WHERE kind='item' ORDER BY id")
            .unwrap();
        let saved = query
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str::<Value>(&row.unwrap()).unwrap())
            .find_map(|item| item.get("tool-results").and_then(|r| r.get(0)).cloned())
            .unwrap();
        assert_eq!(
            saved["images"].as_array().map_or(0, Vec::len),
            usize::from(!fail_after)
        );
        assert_eq!(saved["provenance"]["principal"], "plugin.reader");
        if !fail_after {
            assert_eq!(saved["images"][0]["width"], 1);
            assert_eq!(saved["images"][0]["media_type"], "image/png");
            assert!(
                saved["images"][0]["name"]
                    .as_str()
                    .unwrap()
                    .starts_with("plugin.reader:")
            );
        }
        let audit = fs::read_to_string(env.audit_log()).unwrap();
        assert!(audit.contains("emit-image"));
        assert!(!audit.contains("iVBORw0KGgo"));
    }
}

#[test]
#[ignore = "build the standard read component first; see docs/plugins.md"]
fn standard_read_plugin_sends_file_images_to_the_model() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[(
                "picture",
                "plugin__reader__read",
                json!({"path":"photo.txt","format":"image"}),
            )],
        )),
        sse_response(&openai_sse("seen", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/read-plugin/read.component.wasm");
    plugin_fixture::configure(
        &env.xdg.path().join("config/saddle/config.toml"),
        &fs::read(component).unwrap(),
        plugin_fixture::GRANTS,
    );
    fs::write(env.ws.path().join("photo.txt"), plugin_fixture::png()).unwrap();
    let out = env.saddle(&["run", "show the picture"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let mut sent = String::new();
    strings(&seen[1].body, &mut sent);
    assert!(sent.contains("data:image/png;base64,"), "{sent}");
    assert!(sent.contains("1×1, image/png"));
}

#[test]
fn plugin_transcript_queries_use_the_active_branch_and_plugin_identity_without_files() {
    for (sql, expected) in [
        (
            "SELECT text FROM messages WHERE role='user'",
            "\"rows\":[[\"remember teal\"]]",
        ),
        ("DELETE FROM messages", "session query failed"),
    ] {
        let (base, seen) = serve(vec![
            sse_response(&openai_sse(
                "",
                &[("query", "plugin__reader__session_query", json!({}))],
            )),
            sse_response(&openai_sse("done", &[])),
        ]);
        let env = Env::new(Api::Openai, &base);
        env.write_override("[agent]\ntranscript-read={}\n");
        plugin_fixture::configure_transcript(
            &env.xdg.path().join("config/saddle/config.toml"),
            &plugin_fixture::transcript_query(sql),
            "tool={}\ntranscript-read={}",
        );
        let out = env.saddle(&["run", "remember teal"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let seen = seen.lock().unwrap();
        let messages = seen[1].body["messages"].as_array().unwrap();
        let result = messages.iter().find(|m| m["role"] == "tool").unwrap()["content"]
            .as_str()
            .unwrap();
        assert!(result.contains(expected), "{result}");
        assert!(result.contains("plugin-output"), "{result}");
        let audit = records(&env.audit_log());
        let queries: Vec<_> = audit
            .iter()
            .filter(|r| r["scope"] == "transcript-read")
            .collect();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0]["principal"], "plugin.reader");
        assert_eq!(queries[0]["allow"], true);
        assert!(queries[0]["args"]["call_id"].is_string());
        assert!(
            !audit
                .iter()
                .any(|r| r["principal"] == "plugin.reader" && r["scope"] == "fs-read")
        );
        assert!(
            env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()])
                .status
                .success()
        );
    }
}

#[test]
#[ignore = "build the standard session_query component first; see docs/plugins.md"]
fn standard_transcript_component_queries_the_active_branch_without_filesystem_grants() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[(
                "query",
                "plugin__reader__session_query",
                json!({"sql":"SELECT text FROM messages WHERE role='user'"}),
            )],
        )),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    env.write_override("[agent]\ntranscript-read={}\n");
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/session-query-plugin/session_query.component.wasm");
    plugin_fixture::configure_transcript(
        &env.xdg.path().join("config/saddle/config.toml"),
        &fs::read(component).expect("build the standard session_query component first"),
        "tool={}\ntranscript-read={}",
    );
    let out = env.saddle(&["run", "remember amber"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let messages = seen[1].body["messages"].as_array().unwrap();
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        result.contains("\"rows\":[[\"remember amber\"]]"),
        "{result}"
    );
}

#[test]
fn failed_plugins_are_not_advertised_and_do_not_remove_native_tools() {
    let (base, seen) = serve(vec![sse_response(&openai_sse("ok", &[]))]);
    let env = Env::new(Api::Openai, &base);
    plugin_fixture::configure(
        &env.xdg.path().join("config/saddle/config.toml"),
        &plugin_fixture::forbidden(),
        plugin_fixture::GRANTS,
    );
    let out = env.saddle(&["run", "hello"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let tools = seen[0].body["tools"].as_array().unwrap();
    assert!(!tools.iter().any(|t| {
        t["function"]["name"]
            .as_str()
            .unwrap()
            .starts_with("plugin__")
    }));
    assert!(tools.iter().any(|t| t["function"]["name"] == "read"));
}

#[test]
#[ignore = "build the standard read component first; see docs/plugins.md"]
fn one_compiled_reader_changes_directory_permissions_at_runtime_without_repinning() {
    let tool = "plugin__reader__read";
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[
                ("root-1", tool, json!({"path":"file"})),
                ("shared-1", tool, json!({"path":"~/.agents/reference"})),
            ],
        )),
        sse_response(&openai_sse("first done", &[])),
        sse_response(&openai_sse(
            "",
            &[
                ("shared-2", tool, json!({"path":"~/.agents/reference"})),
                ("excluded", tool, json!({"path":".env"})),
            ],
        )),
        sse_response(&openai_sse("second done", &[])),
        sse_response(&openai_sse(
            "",
            &[
                ("root-3", tool, json!({"path":"file"})),
                ("shared-3", tool, json!({"path":"~/.agents/reference"})),
            ],
        )),
        sse_response(&openai_sse("third done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    env.write_override("[agent]\nfs-read=[{root='workspace',exclude=['.env*']}]\n");
    let config = env.xdg.path().join("config/saddle/config.toml");
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/read-plugin/read.component.wasm");
    let bytes = fs::read(component).expect("build the standard read component first");
    plugin_fixture::configure(&config, &bytes, plugin_fixture::GRANTS);
    let artifact = config
        .parent()
        .unwrap()
        .join("plugins/reader/component.wasm");
    fs::write(config.parent().unwrap().join("plugins/reader/plugin.toml"), "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','fs-read','tool-read']\n").unwrap();
    let initial: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let pin = initial["plugins"]["reader"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let shared = env.xdg.path().join("home/.agents");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("reference"), "SHARED-REFERENCE-CONTENT").unwrap();
    fs::write(env.ws.path().join("file"), "workspace content").unwrap();
    fs::write(env.ws.path().join(".env"), "ROOT-EXCLUSION-SECRET").unwrap();
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait_for = |count| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < deadline, "turn did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    };
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    wait_for(2);
    let role = "tool-read={tools=['read']}\n";
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        settings.replace("[plugin.reader]\n", &format!("[plugin.reader]\n{role}")),
    )
    .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/reload\nsecond\n")
        .unwrap();
    wait_for(4);
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(&config, settings.replace(role, "")).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nthird\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let result = |request: usize, id: &str| {
        seen[request].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap()
    };
    for index in [0, 2, 4] {
        assert!(
            seen[index].body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["function"]["name"] == tool)
        );
    }
    assert!(result(1, "root-1").contains("workspace content"));
    assert!(!result(1, "shared-1").contains("SHARED-REFERENCE-CONTENT"));
    assert!(result(1, "shared-1").contains("Denied"));
    assert!(result(3, "shared-2").contains("SHARED-REFERENCE-CONTENT"));
    assert!(!result(3, "excluded").contains("ROOT-EXCLUSION-SECRET"));
    assert!(result(3, "excluded").contains("excluded"));
    assert!(result(5, "root-3").contains("workspace content"));
    assert!(!result(5, "shared-3").contains("SHARED-REFERENCE-CONTENT"));
    assert!(result(5, "shared-3").contains("Denied"));
    assert_eq!(fs::read(artifact).unwrap(), bytes);
    let final_config: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(
        final_config["plugins"]["reader"]["sha256"]
            .as_str()
            .unwrap(),
        pin
    );
    let audit = records(&env.audit_log());
    let loads: Vec<_> = audit
        .iter()
        .filter(|row| row["args"]["operation"] == "load" && row["allow"] == true)
        .collect();
    assert_eq!(loads.len(), 3);
    assert!(loads.iter().all(|row| row["args"]["sha256"] == pin));
    assert!(
        env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
#[ignore = "build the standard write component first; see docs/plugins.md"]
fn one_compiled_writer_changes_write_permissions_at_runtime_without_repinning() {
    let tool = "plugin__reader__write";
    let mut responses = Vec::new();
    for turn in 0..3 {
        let id = format!("write-{turn}");
        responses.push(sse_response(&openai_sse(
            "",
            &[(
                &id,
                tool,
                json!({"path":"file","content":format!("turn {turn}")}),
            )],
        )));
        responses.push(sse_response(&openai_sse("done", &[])));
    }
    let (base, seen) = serve(responses);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let component = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/write-plugin/write.component.wasm");
    let bytes = fs::read(component).expect("build the standard write component first");
    plugin_fixture::configure(&config, &bytes, "tool={}\nfs-read=[{root='workspace'}]");
    fs::write(config.parent().unwrap().join("plugins/reader/plugin.toml"), "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','fs-read','fs-write']\n").unwrap();
    let initial: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let pin = initial["plugins"]["reader"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(env.ws.path().join("file"), "original").unwrap();
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait_for = |count| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < deadline, "turn did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    };
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    wait_for(2);
    assert_eq!(
        fs::read_to_string(env.ws.path().join("file")).unwrap(),
        "original"
    );
    let grant = "fs-write=[{root='workspace'}]\n";
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        settings.replace("[plugin.reader]\n", &format!("[plugin.reader]\n{grant}")),
    )
    .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/reload\nsecond\n")
        .unwrap();
    wait_for(4);
    assert_eq!(
        fs::read_to_string(env.ws.path().join("file")).unwrap(),
        "turn 1"
    );
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        settings.replace(&format!("[plugin.reader]\n{grant}"), "[plugin.reader]\n"),
    )
    .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nthird\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(env.ws.path().join("file")).unwrap(),
        "turn 1"
    );
    assert_eq!(
        fs::read(
            config
                .parent()
                .unwrap()
                .join("plugins/reader/component.wasm")
        )
        .unwrap(),
        bytes
    );
    let final_config: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(
        final_config["plugins"]["reader"]["sha256"]
            .as_str()
            .unwrap(),
        pin
    );
    let seen = seen.lock().unwrap();
    for turn in 0..3 {
        assert!(
            seen[turn * 2].body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["function"]["name"] == tool)
        );
        let result = seen[turn * 2 + 1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| {
                message["role"] == "tool" && message["tool_call_id"] == format!("write-{turn}")
            })
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(result.contains("wrote file"), turn == 1, "{result}");
        if turn != 1 {
            assert!(result.contains("denied:"), "{result}");
        }
    }
    let audit = records(&env.audit_log());
    assert!(
        audit
            .iter()
            .filter(|row| row["args"]["operation"] == "load" && row["allow"] == true)
            .all(|row| row["args"]["sha256"] == pin)
    );
    assert!(
        env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn reloading_managed_paths_revokes_targets_even_when_the_plugin_stays_loaded() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[("first", "plugin__reader__reader", json!({}))],
        )),
        sse_response(&openai_sse("first done", &[])),
        sse_response(&openai_sse(
            "",
            &[("second", "plugin__reader__reader", json!({}))],
        )),
        sse_response(&openai_sse("second done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    plugin_fixture::configure(
        &config,
        &plugin_fixture::tool_range_reader("sandbox-1", "linked", 0, 128),
        "tool={}\ntool-read={tools=['read']}",
    );
    fs::write(
        config.parent().unwrap().join("plugins/reader/plugin.toml"),
        "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','tool-read']\n",
    )
    .unwrap();
    let shared = env.xdg.path().join("home/.agents");
    let external = env.xdg.path().join("external");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(&external).unwrap();
    fs::write(external.join("reference"), "before reload").unwrap();
    symlink(external.join("reference"), shared.join("linked")).unwrap();
    let declaration = format!(
        "read=[{{path={:?},tools=['read']}}]",
        external.to_str().unwrap()
    );
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(&config, format!("{settings}\n[sandbox]\n{declaration}\n")).unwrap();
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while seen.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "first turn did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(&config, settings.replace(&declaration, "read=[]")).unwrap();
    fs::write(external.join("reference"), "AFTER-REVOCATION-SECRET").unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nsecond\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!text(&out.stdout).contains("AFTER-REVOCATION-SECRET"));
    let seen = seen.lock().unwrap();
    assert!(
        seen[2].body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "plugin__reader__reader")
    );
    let messages = seen[3].body["messages"].as_array().unwrap();
    let results: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert!(
        results[0]["content"]
            .as_str()
            .unwrap()
            .contains("before reload")
    );
    assert!(
        !results.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains("AFTER-REVOCATION-SECRET")
    );
    assert!(
        records(&env.audit_log())
            .iter()
            .any(|row| row["principal"] == "plugin.reader"
                && row["scope"] == "tool-read"
                && row["allow"] == false)
    );
}

#[test]
fn reloading_revokes_plugin_catalogues_and_denies_stale_model_calls() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[("first", "plugin__reader__reader", json!({}))],
        )),
        sse_response(&openai_sse("first done", &[])),
        sse_response(&openai_sse(
            "",
            &[("stale", "plugin__reader__reader", json!({}))],
        )),
        sse_response(&openai_sse("second done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    plugin_fixture::configure(
        &config,
        &plugin_fixture::reader("file"),
        plugin_fixture::GRANTS,
    );
    fs::write(env.ws.path().join("file"), "before reload").unwrap();
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while seen.lock().unwrap().len() < 2 {
        assert!(Instant::now() < deadline, "first turn did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    let text = fs::read_to_string(&config).unwrap();
    fs::write(config, text.replace("enabled=true", "enabled=false")).unwrap();
    fs::write(env.ws.path().join("file"), "AFTER-REVOCATION-SECRET").unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nsecond\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("AFTER-REVOCATION-SECRET"));
    let seen = seen.lock().unwrap();
    assert!(!seen[2].body["tools"].as_array().unwrap().iter().any(|t| {
        t["function"]["name"]
            .as_str()
            .unwrap()
            .starts_with("plugin__")
    }));
    let result = seen[3].body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(result.starts_with("denied:"), "{result}");
    let rows = records(&env.audit_log());
    assert_eq!(
        rows.iter()
            .filter(|r| r["scope"] == "fs-read" && r["principal"] == "plugin.reader")
            .count(),
        1
    );
}

#[test]
fn the_prompt_and_tool_descriptions_explain_policy_checked_command_networking() {
    // D79: a prompt that still claims D19's unconditional no-network boundary prevents builds
    // from even trying an allowed download. Check the actual request, not just the source strings.
    let (base, seen) = serve(vec![sse_response(&openai_sse("ok", &[]))]);
    let env = Env::new(Api::Openai, &base);
    env.write_override("\n[agent]\nnet = [{ host = \"registry.example\" }]\n");
    let out = env.saddle(&["run", "hi"]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let seen = seen.lock().unwrap();
    let body = &seen[0].body;
    let system = body["messages"][0]["content"].as_str().unwrap();
    if !system.contains("bash is unavailable in this session") {
        for required in [
            "proxy checked against the same net grants as fetch",
            "builds may download dependencies",
            "If the network namespace or relay cannot start",
            "commands run with no network instead",
            "A command sees more",
        ] {
            assert!(system.contains(required), "missing {required}: {system}");
        }
    }
    for stale in [
        "Commands run in a sandbox: no network access",
        "Do not try to download packages",
        "call web services from a command",
    ] {
        assert!(!system.contains(stale), "stale instruction: {system}");
    }

    let tools = body["tools"].as_array().unwrap();
    let description = |name: &str| {
        tools
            .iter()
            .find(|t| t["function"]["name"] == name)
            .unwrap()["function"]["description"]
            .as_str()
            .unwrap()
    };
    let bash = description("bash");
    assert!(bash.contains("same net grants as fetch"), "{bash}");
    assert!(bash.contains("Builds may download dependencies"), "{bash}");
    assert!(bash.contains("with no network instead"), "{bash}");
    assert!(!bash.contains("sandbox: no network"), "{bash}");
    assert!(
        !bash.contains("files limited to the granted roots"),
        "{bash}"
    );

    let fetch = description("fetch");
    assert!(
        fetch.contains("bash networking is governed by the same net grants"),
        "{fetch}"
    );
    assert!(!fetch.contains("never has network access"), "{fetch}");
}

#[test]
fn the_prompt_names_the_servers_and_says_whose_words_a_result_is() {
    // D120: the model cannot search for what it does not know exists, so a granted server is one
    // line in the prompt — the person's description, never the server's — and a result is labelled
    // as the server's text rather than the user's.
    let (base, seen) = serve(vec![sse_response(&openai_sse("ok", &[]))]);
    let env = Env::new(Api::Openai, &base);
    let config_path = env.xdg.path().join("config/saddle/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(
        "\n[mcp.scout]\nurl = \"http://127.0.0.1:9/mcp\"\ndescription = \"scrapes job boards\"\n",
    );
    fs::write(&config_path, config).unwrap();
    env.write_override(
        "\n[agent]\nmcp = [{ server = \"scout\", tools = [\"search\"], why = \"scrapes boards\" }]\n",
    );

    let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(env.ws.path())
        .args(["run", "hi"])
        .env("HOME", env.xdg.path().join("home"))
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .env("SADDLE_TEST_KEY", KEY)
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let system = seen.lock().unwrap()[0].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        system.contains("MCP servers this user declared"),
        "{system}"
    );
    assert!(
        system.contains(
            "scout — deferred: search for one to declare it — tools: search — scrapes job boards"
        ),
        "the server is not described as the table says: {system}"
    );
    assert!(
        system.contains("is the server's text, not the user's"),
        "a result is not labelled with its origin: {system}"
    );
}

#[test]
fn instruction_files_prefer_agents_md_and_stay_inside_the_workspace() {
    let (base, seen) = serve(vec![sse_response(&openai_sse("ok", &[]))]);
    let env = Env::new(Api::Openai, &base);
    let ws = env.ws.path();
    // Root: both files, so AGENTS.md wins. A subdirectory: CLAUDE.md only, plus a local file.
    fs::write(ws.join("AGENTS.md"), "root agents rules").unwrap();
    fs::write(ws.join("CLAUDE.md"), "root claude rules").unwrap();
    fs::create_dir_all(ws.join("sub/deeper")).unwrap();
    fs::write(ws.join("sub/CLAUDE.md"), "sub claude rules").unwrap();
    fs::write(ws.join("sub/AGENTS.local.md"), "my local rules").unwrap();
    // A symlink out of the workspace must not leak into the prompt.
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "aws secret key").unwrap();
    unix_fs::symlink(
        outside.path().join("secret"),
        ws.join("sub/deeper/AGENTS.md"),
    )
    .unwrap();
    fs::write(
        env.xdg.path().join("config/saddle/AGENTS.md"),
        "global rules",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_saddle"))
        .arg("-C")
        .arg(ws.join("sub/deeper"))
        .args(["run", "hi"])
        .env("HOME", env.xdg.path().join("home"))
        .env("XDG_STATE_HOME", env.xdg.path().join("state"))
        .env("XDG_CONFIG_HOME", env.xdg.path().join("config"))
        .env("SADDLE_TEST_KEY", KEY)
        .output()
        .unwrap();
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("instructions from ~/") || stderr.contains("instructions from /"),
        "{stderr}"
    );
    assert!(
        stderr.contains("AGENTS.md, sub/CLAUDE.md, sub/AGENTS.local.md"),
        "{stderr}"
    );
    assert!(
        stderr.contains("sub/deeper/AGENTS.md was not loaded"),
        "{stderr}"
    );

    let system = seen.lock().unwrap()[0].body["messages"][0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    let order: Vec<usize> = [
        "global rules",
        "root agents rules",
        "sub claude rules",
        "my local rules",
    ]
    .iter()
    .map(|s| system.find(s).unwrap_or_else(|| panic!("missing {s}")))
    .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    assert!(!system.contains("root claude rules"));
    assert!(!system.contains("aws secret key"));

    let log = env.audit_log();
    let recs = records(&log);
    let context = recs.iter().find(|r| r["kind"] == "context").unwrap();
    assert_eq!(context["args"]["files"].as_array().unwrap().len(), 4);
}

#[test]
fn json_mode_is_one_object_per_line_on_stdout_and_usable_without_prose() {
    // The contract a program depends on (D52): every stdout line parses, the events name what
    // happened, and nothing else is mixed in.
    let (base, _seen) = serve(vec![
        sse_response(&openai_sse("Looking.", &[("t1", "ls", json!({}))])),
        sse_response(&openai_sse("Done.", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    fs::write(env.ws.path().join("note.txt"), "hi\n").unwrap();
    let out = env.saddle(&["run", "--json", "look around"]);
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{}{}", stdout, text(&out.stderr));

    let events: Vec<Value> = stdout
        .lines()
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {l:?}"))
        })
        .collect();
    let kinds: Vec<&str> = events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    // The session comes first, then the answer streams, then the call and its result, and the
    // turn ends with a word a program can switch on.
    assert_eq!(kinds.first(), Some(&"session"), "{kinds:?}");
    assert!(kinds.contains(&"text"), "{kinds:?}");
    assert!(kinds.contains(&"call"), "{kinds:?}");
    assert!(kinds.contains(&"result"), "{kinds:?}");
    assert!(kinds.contains(&"reply"), "{kinds:?}");
    assert_eq!(kinds.last(), Some(&"turn_end"), "{kinds:?}");

    let call = events.iter().find(|e| e["type"] == "call").unwrap();
    assert_eq!(call["name"], "ls");
    let result = events.iter().find(|e| e["type"] == "result").unwrap();
    assert_eq!(result["ok"], true);
    assert!(result["content"].as_str().unwrap().contains("note.txt"));
    // The arguments travel as the model sent them, so a caller need not parse the detail line.
    assert_eq!(call["input"], json!({}));
    let end = events.last().unwrap();
    assert_eq!(end["end"], "answered");
}

#[test]
fn a_cache_miss_is_told_to_the_user_and_never_to_the_model() {
    // A request whose prompt is read fresh after the cache has been working costs several times
    // what it should, and no token count shows it (D50). The line is for the person at the
    // terminal: what goes back to the model is the conversation, and a notice is not part of it.
    let (base, seen) = serve(vec![
        // Caching is working: 50 of the prompt came from the cache.
        sse_response(&openai_sse_usage(
            "",
            &[("t1", "ls", json!({}))],
            100,
            Some(50),
        )),
        // Then the prefix changed, and the whole 10k prompt is billed at full price.
        sse_response(&openai_sse_usage("Done.", &[], 10_000, Some(0))),
    ]);
    let env = Env::new(Api::Openai, &base);
    let out = env.saddle(&["run", "look around"]);
    let stderr = text(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("cache miss: 10.0k tokens read at full price"),
        "the user is told: {stderr}"
    );
    // The second request is the whole conversation as the model sees it. A notice is not in it.
    let sent = seen.lock().unwrap()[1].body.to_string();
    assert!(
        !sent.contains("cache miss"),
        "the model is not told: {sent}"
    );
}

#[test]
fn search_tools_follow_the_policy_and_edits_keep_their_diff_out_of_the_request() {
    let calls = [
        ("g", "grep", json!({"pattern": "SECRET|fn main"})),
        ("f", "find", json!({"pattern": "*.rs"})),
        ("l", "ls", json!({})),
        (
            "e",
            "edit",
            json!({"path": "src/main.rs", "old": "fn main() {}", "new": "fn main() {\n    run();\n}"}),
        ),
    ];
    let (base, seen) = serve(vec![
        sse_response(&openai_sse("", &calls)),
        sse_response(&openai_sse("Done.", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let ws = env.ws.path();
    // Narrow one scope in this workspace's own policy; the rest is inherited (D43).
    env.write_override(
        r#"version = 1

[agent]
fs-read = [{ root = "workspace", exclude = [".env*"], why = "reads the repo" }]
"#,
    );
    fs::create_dir_all(ws.join("src")).unwrap();
    fs::create_dir_all(ws.join("target")).unwrap();
    fs::write(ws.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(ws.join("target/gen.rs"), "fn main() {}\n").unwrap();
    fs::write(ws.join(".gitignore"), "target/\n").unwrap();
    fs::write(ws.join(".env"), "SECRET=hunter2\n").unwrap();

    let out = env.saddle(&["run", "look around"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let results: Vec<String> = seen[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap().to_owned())
        .collect();
    let [grep, find, ls, edit] = &results[..] else {
        panic!("{results:?}")
    };
    assert!(grep.starts_with("src/main.rs:1: fn main() {}"), "{grep}");
    assert!(
        !grep.contains("hunter2") && !grep.contains("target/"),
        "{grep}"
    );
    assert!(
        grep.contains("1 path skipped: the policy excludes it"),
        "{grep}"
    );
    assert_eq!(find.lines().next(), Some("src/main.rs"), "{find}");
    assert!(!find.contains("target"), "{find}");
    assert!(
        ls.contains("src/") && ls.contains(".gitignore") && !ls.contains(".env\n"),
        "{ls}"
    );
    assert_eq!(edit, "edited src/main.rs at line 1");
    assert!(!seen[1].body.to_string().contains("\"diff\""));

    // The diff is kept for the transcript, in the saved conversation (D53).
    let db = env.xdg.path().join("state/saddle/sessions.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    let saved: String = conn
        .query_row(
            "SELECT body FROM nodes WHERE kind = 'item' AND body LIKE '%diff%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        saved.contains(r#""diff":"-1 fn main() {}\n+1 fn main() {\n+2     run();\n+3 }\n""#),
        "{saved}"
    );
}
