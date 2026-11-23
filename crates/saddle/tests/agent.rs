//! The model loop end to end: `saddle run` against a mock provider that speaks either API. The mock
//! asks for tool calls, and the test checks what the policy let through, what went back to the
//! model, and what reached the audit log.

#[path = "support/features.rs"]
mod feature_tests;
#[path = "support/hooks.rs"]
mod hook_tests;
#[path = "support/http.rs"]
mod http_fixture;
#[path = "support/plugin.rs"]
mod plugin_fixture;

use std::fs;
use std::io::ErrorKind;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use http_fixture::{Server as HttpServer, reply as http_reply};
use rusqlite::Connection;
use saddle_policy::audit::sha256_hex;
use serde_json::{Value, json};
use tempfile::TempDir;

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

/// dynamic tool ids and deliberate provider stalls exercise background work during model I/O.
fn serve_dynamic(
    count: usize,
    mut respond: impl FnMut(usize, &Value) -> String + Send + 'static,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    thread::spawn(move || {
        for index in 0..count {
            let (mut stream, mut reader, head) = model_request(&listener);
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse::<usize>()
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            record.lock().unwrap().push(Seen {
                head,
                body: body.clone(),
            });
            let response = respond(index, &body);
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    (base, seen)
}

fn result_text<'a>(body: &'a Value, call: &str) -> &'a str {
    tool_text(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == call)
            .unwrap()["content"]
            .as_str()
            .unwrap(),
    )
}

fn background_id(body: &Value) -> String {
    result_text(body, "start")
        .strip_prefix("task ")
        .unwrap()
        .split(':')
        .next()
        .unwrap()
        .to_owned()
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

fn tool_text(text: &str) -> &str {
    if text.starts_with("<plugin-output-")
        && let Some((_, body)) = text.split_once('\n')
        && let Some((body, _)) = body.rsplit_once("\n</plugin-output-")
    {
        body
    } else {
        text
    }
}

fn standard_component(name: &str) -> Vec<u8> {
    saddle_standard::artifact(name)
        .expect("shipped standard artifact")
        .wasm
        .to_vec()
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
        let path = config.join("config.toml");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(
            path,
            format!("{text}\n{}", saddle_standard::DEFAULT_PROFILES),
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
            json!({"path": "../outside-the-workspace", "content": "#!/bin/sh\necho pwned\n"}),
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
    // D178: `.git` is inside the grant, so the refusal comes from the grant not covering `..`.
    assert!(!ws.parent().unwrap().join("outside-the-workspace").exists());

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
            "tool_search",
            "bash",
            "config_help",
            "edit",
            "find",
            "grep",
            "ls",
            "patch",
            "read",
            "tasks",
            "write"
        ]
    );
    let mut first_text = String::new();
    strings(first, &mut first_text);
    assert!(first_text.contains(&ws.canonicalize().unwrap().display().to_string()));
    assert!(first_text.contains("agent.fs-write[0] = workspace"));

    let mut second = String::new();
    strings(&seen[1].body, &mut second);
    assert!(
        second.contains("wrote notes/hello.txt (2 bytes"),
        "{second}"
    );
    assert!(second.contains("outside every root"), "{second}");
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
        .filter(|r| {
            r["kind"] == "decision" && r["allow"] == false && r["args"]["operation"] != "load"
        })
        .count();
    assert_eq!(denied, 1);
    let verify = env.saddle(&["audit", "verify", log.to_str().unwrap()]);
    assert!(verify.status.success(), "{}", text(&verify.stdout));
}

#[test]
fn background_network_is_attributed_or_fails_closed_during_model_and_input_waits() {
    if let Err(reason) = saddle_sandbox::network_availability() {
        eprintln!("background network case unavailable on this kernel: {reason}");
        return;
    }
    let source = HttpServer::new(vec![
        http_reply(200, "", "BACKGROUND-NETWORK"),
        http_reply(200, "", "IDLE-NETWORK"),
    ]);
    let url = source.url.clone();
    let marker = Arc::new(Mutex::new(PathBuf::new()));
    let done = marker.clone();
    let (base, _) = serve_dynamic(2, move |index, _| {
        if index == 0 {
            sse_response(&openai_sse(
                "",
                &[(
                    "start",
                    "bash",
                    json!({"command":format!("curl -fsS --max-time 5 --noproxy '' {url} > fetched 2> curl-error; printf ready > ready; while [ ! -f go ]; do sleep 0.01; done; curl -fsS --max-time 5 --noproxy '' {url} > idle-fetched 2> idle-error; printf ready > idle-ready; sleep 30"),"background":true,"timeout_secs":10}),
                )],
            ))
        } else {
            let path = done.lock().unwrap().clone();
            let end = Instant::now() + Duration::from_secs(7);
            while !path.exists() {
                assert!(
                    Instant::now() < end,
                    "background network stalled during model request"
                );
                thread::sleep(Duration::from_millis(10));
            }
            sse_response(&openai_sse("started", &[]))
        }
    });
    let env = Env::new(Api::Openai, &base);
    *marker.lock().unwrap() = env.ws.path().join("ready");
    env.write_override(&format!(
        "[agent]\nnet=[{{host='127.0.0.1',scheme='http',port={}}}]\n",
        source.port
    ));
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"start\n").unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !env.ws.path().join("ready").exists() {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(10));
    }
    let reachable = match UnixListener::bind(env.ws.path().join("probe.sock")) {
        Ok(listener) => {
            drop(listener);
            fs::remove_file(env.ws.path().join("probe.sock")).unwrap();
            true
        }
        Err(error) if error.kind() == ErrorKind::PermissionDenied => {
            eprintln!(
                "outer exec sandbox refuses AF_UNIX listeners; validating the no-network fallback, not an allowed proxy connection"
            );
            false
        }
        Err(error) => panic!("unexpected proxy probe failure: {error}"),
    };
    let fetched = fs::read_to_string(env.ws.path().join("fetched")).unwrap();
    if reachable {
        assert_eq!(
            fetched,
            "BACKGROUND-NETWORK",
            "{}",
            fs::read_to_string(env.ws.path().join("curl-error")).unwrap()
        );
    } else {
        assert!(fetched.is_empty());
    }
    fs::write(env.ws.path().join("go"), "continue").unwrap();
    let end = Instant::now() + Duration::from_secs(7);
    while !env.ws.path().join("idle-ready").exists() {
        assert!(Instant::now() < end);
        thread::sleep(Duration::from_millis(10));
    }
    let fetched = fs::read_to_string(env.ws.path().join("idle-fetched")).unwrap();
    if reachable {
        assert_eq!(fetched, "IDLE-NETWORK");
    } else {
        assert!(fetched.is_empty());
    }

    // A separate Saddle process must not run an overlapping command.
    let competing = env.saddle(&["tool", "bash", r#"{"command":"printf raced > raced"}"#]);
    assert!(!competing.status.success());
    assert!(text(&competing.stderr).contains("workspace shell execution is busy"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/tasks\n/tasks cancel nonexistent\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!env.ws.path().join("raced").exists());
    let directory = env.xdg.path().join("state/saddle/audit");
    let rows: Vec<_> = fs::read_dir(directory)
        .unwrap()
        .flat_map(|path| records(&path.unwrap().path()))
        .collect();
    assert_eq!(
        rows.iter().any(|row| row["scope"] == "net"
            && row["principal"] == "plugin.standard-bash"
            && row["allow"] == true
            && row["args"]["call_id"].is_string()),
        reachable
    );
}

#[test]
fn background_output_and_completed_task_retention_are_bounded_by_runtime_budgets() {
    // Both starts decline a wake (D174), so the run ends with its turn: the budgets, not the
    // resumption, are what this test is about.
    let (base, _) = serve_dynamic(5, |index, body| {
        let calls = match index {
            0 => vec![(
                "start",
                "bash",
                json!({"command":"printf 1234567890123456789012345678901234567890; printf error >&2", "background":true, "wake":false}),
            )],
            1 => {
                thread::sleep(Duration::from_millis(150));
                vec![(
                    "status",
                    "tasks",
                    json!({"action":"status","task_id":background_id(body)}),
                )]
            }
            2 => {
                let output = result_text(body, "status");
                assert!(
                    output.contains("1234567890123456") && !output.contains("12345678901234567"),
                    "{output}"
                );
                assert!(output.contains("capture truncated"), "{output}");
                vec![(
                    "second",
                    "bash",
                    json!({"command":"printf second", "background":true, "wake":false}),
                )]
            }
            3 => vec![(
                "old",
                "tasks",
                json!({"action":"status","task_id":background_id(body)}),
            )],
            _ => {
                assert!(result_text(body, "old").contains("unknown task"));
                return sse_response(&openai_sse("done", &[]));
            }
        };
        sse_response(&openai_sse("", &calls))
    });
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let mut settings: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    settings.insert(
        "tool-limits".into(),
        toml::Value::Table(
            "exec-capture-bytes=16\nexec-tasks=1\n"
                .parse::<toml::Table>()
                .unwrap(),
        ),
    );
    fs::write(config, toml::to_string(&settings).unwrap()).unwrap();
    let out = env.saddle(&["run", "bounded tasks"]);
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

#[test]
fn background_shell_returns_live_output_isolates_principals_and_serializes_until_cancel() {
    let mut id = String::new();
    let (base, seen) = serve_dynamic(6, move |index, body| {
        let calls = match index {
            0 => vec![(
                "start",
                "bash",
                json!({"command":"printf SECRET-EARLY; printf changed > .git/config; sleep 30", "background":true, "timeout_secs":10}),
            )],
            1 => {
                id = background_id(body);
                assert!(result_text(body, "start").contains("running"));
                thread::sleep(Duration::from_millis(200));
                vec![
                    ("live", "tasks", json!({"action":"status","task_id":id})),
                    (
                        "foreign",
                        "plugin__reader__tasks",
                        json!({"action":"status","task_id":id}),
                    ),
                    (
                        "foreign-cancel",
                        "plugin__reader__tasks",
                        json!({"action":"cancel","task_id":id}),
                    ),
                    (
                        "busy",
                        "bash",
                        json!({"command":"printf must-not-run > forbidden"}),
                    ),
                ]
            }
            2 => {
                assert!(result_text(body, "live").contains("SECRET-EARLY"));
                assert!(result_text(body, "live").contains("running"));
                assert!(result_text(body, "foreign").contains("unknown task for this principal"));
                assert!(!result_text(body, "foreign").contains("SECRET-EARLY"));
                assert!(result_text(body, "foreign-cancel").contains("unknown task"));
                assert!(result_text(body, "busy").contains("workspace shell execution is busy"));
                vec![("cancel", "tasks", json!({"action":"cancel","task_id":id}))]
            }
            3 => {
                // D180: the call that asked for the cancellation answers with the ending, rather
                // than a snapshot taken before the process group was reaped.
                let text = result_text(body, "cancel");
                assert!(text.contains("finished"), "{text}");
                assert!(text.contains("cancelled by the user"), "{text}");
                thread::sleep(Duration::from_millis(150));
                vec![("done", "tasks", json!({"action":"status","task_id":id}))]
            }
            4 => {
                assert!(result_text(body, "done").contains("cancelled"));
                vec![("unlocked", "bash", json!({"command":"printf unlocked"}))]
            }
            _ => {
                assert!(result_text(body, "unlocked").contains("unlocked"));
                return sse_response(&openai_sse("done", &[]));
            }
        };
        sse_response(&openai_sse("", &calls))
    });
    let env = Env::new(Api::Openai, &base);
    fs::write(env.ws.path().join(".git/config"), "original").unwrap();
    let config = env.xdg.path().join("config/saddle/config.toml");
    // The foreign principal is the tasks surface installed as a third-party plugin, so it asks the
    // same host service under its own id and must still be refused (D182).
    plugin_fixture::configure(
        &config,
        &standard_component("tasks"),
        "tool={}\nexec={timeout-secs=10}\nfs-read=[{root='workspace'}]\nfs-write=[{root='workspace'}]",
    );
    fs::write(config.parent().unwrap().join("plugins/reader/plugin.toml"), "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','exec','fs-read','fs-write','net']\n").unwrap();
    let out = env.saddle(&["run", "background work"]);
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert_eq!(seen.lock().unwrap().len(), 6);
    // D178: the task wrote `.git/config` inside the grant before it was cancelled, and that stays.
    assert_eq!(
        fs::read_to_string(env.ws.path().join(".git/config")).unwrap(),
        "changed"
    );
    assert!(!env.ws.path().join("forbidden").exists());
    let rows = records(&env.audit_log());
    assert!(
        rows.iter()
            .any(|row| row["args"]["operation"] == "task-start"
                && row["principal"] == "plugin.standard-bash")
    );
    assert!(rows.iter().any(
        |row| row["args"]["operation"] == "exec-completed" && row["args"]["cancelled"] == true
    ));
    assert!(
        env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn background_timeout_runs_during_model_wait_and_notices_do_not_expose_process_output() {
    let (base, seen) = serve_dynamic(4, |index, body| {
        let calls = match index {
            0 => vec![(
                "start",
                "bash",
                json!({"command":"printf OUTPUT-NOT-A-NOTICE; sleep 30", "background":true, "timeout_secs":1}),
            )],
            1 => {
                thread::sleep(Duration::from_millis(1300));
                vec![(
                    "status",
                    "tasks",
                    json!({"action":"status","task_id":background_id(body)}),
                )]
            }
            2 => {
                assert!(result_text(body, "status").contains("command timed out"));
                assert!(result_text(body, "status").contains("OUTPUT-NOT-A-NOTICE"));
                vec![("list", "tasks", json!({"action":"list"}))]
            }
            _ => {
                let messages = body["messages"].as_array().unwrap();
                let notices: Vec<_> = messages
                    .iter()
                    .filter_map(|message| message["content"].as_str())
                    .filter(|text| text.starts_with("[saddle background task notification]"))
                    .collect();
                assert_eq!(notices.len(), 1);
                assert!(
                    notices[0].contains("timed out") && !notices[0].contains("OUTPUT-NOT-A-NOTICE")
                );
                return sse_response(&openai_sse("done", &[]));
            }
        };
        sse_response(&openai_sse("", &calls))
    });
    let env = Env::new(Api::Openai, &base);
    let start = Instant::now();
    let out = env.saddle(&["run", "timeout background"]);
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn background_completion_wakes_idle_sessions_once_without_new_input() {
    for (ending, state, timeout) in [
        ("exit 0", "completed", 10),
        ("exit 7", "failed", 10),
        ("sleep 30", "timed out", 2),
    ] {
        let (base, seen) = serve_dynamic(5, move |index, body| match index {
            0 => sse_response(&openai_sse(
                "",
                &[(
                    "start",
                    "bash",
                    json!({
                        "command":format!("while [ ! -f finish ]; do sleep 0.01; done; printf PRIVATE-TASK-OUTPUT; {ending}"),
                        "background":true,"timeout_secs":timeout
                    }),
                )],
            )),
            1 => sse_response(&openai_sse("started", &[])),
            2 => {
                let notices: Vec<_> = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|message| message["content"].as_str())
                    .filter(|text| text.starts_with("[saddle background task notification]"))
                    .collect();
                assert_eq!(notices.len(), 1);
                assert!(notices[0].contains(state), "{}", notices[0]);
                assert!(!notices[0].contains("PRIVATE-TASK-OUTPUT"));
                sse_response(&openai_sse(
                    "",
                    &[(
                        "status",
                        "tasks",
                        json!({"action":"status","task_id":background_id(body)}),
                    )],
                ))
            }
            3 => {
                assert!(result_text(body, "status").contains("PRIVATE-TASK-OUTPUT"));
                sse_response(&openai_sse("finished", &[]))
            }
            _ => panic!("completion caused a duplicate idle request"),
        });
        let env = Env::new(Api::Openai, &base);
        let mut child = env
            .command(&["run", "--json"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let wait_end = || {
            loop {
                let line = receive
                    .recv_timeout(Duration::from_secs(15))
                    .expect("waiting for turn end");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["type"] == "turn_end" {
                    assert_eq!(value["end"], "answered", "{line}");
                    break;
                }
            }
        };
        writeln!(child.stdin.as_mut().unwrap(), "start work").unwrap();
        wait_end();
        assert_eq!(seen.lock().unwrap().len(), 2, "no wakeup before completion");
        // The first turn is over and stdin remains open, but no further prompt is sent.
        fs::write(env.ws.path().join("finish"), "go").unwrap();
        wait_end();
        thread::sleep(Duration::from_millis(200));
        assert_eq!(
            seen.lock().unwrap().len(),
            4,
            "idle ticks must not duplicate the wakeup"
        );
        writeln!(child.stdin.take().unwrap(), "/quit").unwrap();
        let output = child.wait_with_output().unwrap();
        reader.join().unwrap();
        assert!(output.status.success(), "{}", text(&output.stderr));
    }
}

/// D174: a background task the agent asked to be woken by keeps a one-shot run alive, so the work it
/// started is not killed by the process leaving, and the completion arrives as its own turn.
#[test]
fn a_one_shot_run_waits_for_a_task_it_will_be_woken_by() {
    let (base, seen) = serve_dynamic(4, |index, body| match index {
        0 => sse_response(&openai_sse(
            "",
            &[(
                "start",
                "bash",
                json!({
                    "command":"while [ ! -f finish ]; do sleep 0.01; done; printf PRIVATE-TASK-OUTPUT",
                    "background":true,"timeout_secs":10
                }),
            )],
        )),
        // The turn ends here, with the task still running: only a run that waits reaches request 2.
        1 => {
            let started = result_text(body, "start");
            assert!(started.contains("will be resumed"), "{started}");
            sse_response(&openai_sse("started", &[]))
        }
        2 => {
            let notices: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .filter(|text| text.starts_with("[saddle background task notification]"))
                .collect();
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert!(notices[0].contains("completed"), "{}", notices[0]);
            assert!(!notices[0].contains("PRIVATE-TASK-OUTPUT"));
            sse_response(&openai_sse(
                "",
                &[(
                    "status",
                    "tasks",
                    json!({"action":"status","task_id":background_id(body)}),
                )],
            ))
        }
        _ => {
            assert!(result_text(body, "status").contains("PRIVATE-TASK-OUTPUT"));
            sse_response(&openai_sse("finished", &[]))
        }
    });
    let env = Env::new(Api::Openai, &base);
    // The task is released only once the first turn is over, so the run has to wait for it rather
    // than reading a completion that was already there.
    let released = seen.clone();
    let workspace = env.ws.path().to_path_buf();
    let release = thread::spawn(move || {
        while released.lock().unwrap().len() < 2 {
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(workspace.join("finish"), "go").unwrap();
    });
    let out = env.saddle(&["run", "wait for the build"]);
    release.join().unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert_eq!(seen.lock().unwrap().len(), 4);
}

/// The other half of the same choice: a declined wake ends the run with its turn, and `wait` is how
/// the agent arms one later (D174).
#[test]
fn a_declined_wake_ends_the_run_and_wait_arms_one_later() {
    let (base, seen) = serve_dynamic(2, |index, body| match index {
        0 => sse_response(&openai_sse(
            "",
            &[(
                "start",
                "bash",
                json!({"command":"sleep 30","background":true,"wake":false,"timeout_secs":10}),
            )],
        )),
        _ => {
            let started = result_text(body, "start");
            assert!(started.contains("will not be resumed"), "{started}");
            sse_response(&openai_sse("started", &[]))
        }
    });
    let env = Env::new(Api::Openai, &base);
    let start = Instant::now();
    let out = env.saddle(&["run", "quiet background"]);
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "a declined wake does not hold the run open"
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "no follow-up turn for a declined wake"
    );

    // The same start, armed afterwards with `wait`, is waited for.
    let (base, seen) = serve_dynamic(5, |index, body| match index {
        0 => sse_response(&openai_sse(
            "",
            &[(
                "start",
                "bash",
                json!({
                    "command":"while [ ! -f finish ]; do sleep 0.01; done; printf PRIVATE-TASK-OUTPUT",
                    "background":true,"wake":false,"timeout_secs":10
                }),
            )],
        )),
        1 => sse_response(&openai_sse(
            "",
            &[(
                "armed",
                "tasks",
                json!({"action":"wait","task_id":background_id(body)}),
            )],
        )),
        2 => {
            let armed = result_text(body, "armed");
            assert!(armed.contains("armed:"), "{armed}");
            sse_response(&openai_sse("started", &[]))
        }
        3 => {
            let notices: Vec<_> = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|message| message["content"].as_str())
                .filter(|text| text.starts_with("[saddle background task notification]"))
                .collect();
            assert_eq!(notices.len(), 1, "{notices:?}");
            assert!(!notices[0].contains("PRIVATE-TASK-OUTPUT"));
            sse_response(&openai_sse(
                "",
                &[(
                    "status",
                    "tasks",
                    json!({"action":"status","task_id":background_id(body)}),
                )],
            ))
        }
        _ => {
            assert!(result_text(body, "status").contains("PRIVATE-TASK-OUTPUT"));
            sse_response(&openai_sse("finished", &[]))
        }
    });
    let env = Env::new(Api::Openai, &base);
    let released = seen.clone();
    let workspace = env.ws.path().to_path_buf();
    let release = thread::spawn(move || {
        while released.lock().unwrap().len() < 3 {
            thread::sleep(Duration::from_millis(10));
        }
        fs::write(workspace.join("finish"), "go").unwrap();
    });
    let out = env.saddle(&["run", "arm the wake later"]);
    release.join().unwrap();
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert_eq!(seen.lock().unwrap().len(), 5);
}

#[test]
fn a_finished_task_answers_a_later_poll_with_its_exit_status_and_output() {
    // D179: a task's record outlives its command, so polling is how a launch that declined a wake
    // is read afterwards — while it runs, before it is reaped and long after the turn that started it.
    let mut id = String::new();
    let (base, _seen) = serve_dynamic(8, move |index, body| {
        let calls = match index {
            0 => vec![(
                "start",
                "bash",
                json!({"command":"printf early; printf done > done.txt; sleep 1", "background":true, "wake":false, "timeout_secs":10}),
            )],
            1 => {
                id = background_id(body);
                // Still running: no claim about how it ended, and no output yet to show.
                assert!(result_text(body, "start").contains("running"));
                assert!(!result_text(body, "start").contains("exited"));
                thread::sleep(Duration::from_millis(300));
                vec![("live", "tasks", json!({"action":"status","task_id":id}))]
            }
            2 => {
                assert!(
                    result_text(body, "live").contains("running"),
                    "{}",
                    result_text(body, "live")
                );
                assert!(
                    !result_text(body, "live").contains("exited"),
                    "{}",
                    result_text(body, "live")
                );
                assert!(
                    result_text(body, "live").contains("early"),
                    "the capture so far is readable: {}",
                    result_text(body, "live")
                );
                thread::sleep(Duration::from_millis(1500));
                vec![("done", "tasks", json!({"action":"status","task_id":id}))]
            }
            3 => {
                let text = result_text(body, "done");
                assert!(text.contains("finished"), "{text}");
                assert!(text.contains("exit code 0"), "{text}");
                assert!(
                    text.contains("early"),
                    "its output is still readable: {text}"
                );
                assert!(!text.contains("unknown task"), "{text}");
                vec![("list", "tasks", json!({"action":"list"}))]
            }
            4 => {
                let text = result_text(body, "list");
                assert!(text.contains(&id), "{text}");
                assert!(text.contains("finished (exit 0)"), "{text}");
                return sse_response(&openai_sse("done", &[]));
            }
            _ => return sse_response(&openai_sse("done", &[])),
        };
        sse_response(&openai_sse("", &calls))
    });
    let env = Env::new(Api::Openai, &base);
    plugin_fixture::configure(
        &env.xdg.path().join("config/saddle/config.toml"),
        &standard_component("bash"),
        "tool={}\nexec={timeout-secs=10}\nfs-read=[{root='workspace'}]\nfs-write=[{root='workspace'}]",
    );
    let out = env.saddle(&["run", "poll it"]);
    assert!(
        out.status.success(),
        "{} {}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(env.ws.path().join("done.txt")).unwrap(),
        "done"
    );
}

#[test]
fn closing_a_session_cancels_background_work_before_the_audit_closes() {
    // The task declines a wake (D174), so the run leaves while it is still running and closing the
    // session is what stops it — which is what this test is about.
    let (base, _) = serve_dynamic(2, |index, _| {
        if index == 0 {
            sse_response(&openai_sse(
                "",
                &[(
                    "start",
                    "bash",
                    json!({"command":"printf changed > .git/config; printf ready > ready; sleep 30; printf leaked > leaked", "background":true, "wake":false}),
                )],
            ))
        } else {
            thread::sleep(Duration::from_millis(150));
            sse_response(&openai_sse("leaving", &[]))
        }
    });
    let env = Env::new(Api::Openai, &base);
    fs::write(env.ws.path().join(".git/config"), "original").unwrap();
    let start = Instant::now();
    let out = env.saddle(&["run", "start and exit"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(env.ws.path().join("ready").exists());
    assert!(!env.ws.path().join("leaked").exists());
    // D178: the write the command already made stays; only the work still running is stopped.
    assert_eq!(
        fs::read_to_string(env.ws.path().join(".git/config")).unwrap(),
        "changed"
    );
    let rows = records(&env.audit_log());
    assert!(rows.iter().any(
        |row| row["args"]["operation"] == "exec-completed" && row["args"]["cancelled"] == true
    ));
    assert!(
        env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn background_tasks_are_cancelled_before_permission_reload_and_the_artifact_stays_pinned() {
    let (base, seen) = serve(vec![
        sse_response(&openai_sse(
            "",
            &[(
                "start",
                "bash",
                json!({"command":"printf ready > ready; sleep 30; printf leaked > leaked","background":true}),
            )],
        )),
        sse_response(&openai_sse("started", &[])),
        sse_response(&openai_sse(
            "",
            &[("revoked", "tasks", json!({"action":"list"}))],
        )),
        sse_response(&openai_sse("done", &[])),
        sse_response(&openai_sse(
            "",
            &[("enabled", "tasks", json!({"action":"list"}))],
        )),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let initial: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    let wait_for = |count| {
        let end = Instant::now() + Duration::from_secs(15);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for(2);
    let mut revoked = initial.clone();
    revoked["agent"].as_table_mut().unwrap().remove("exec");
    fs::write(&config, toml::to_string(&revoked).unwrap()).unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/reload\nsecond\n")
        .unwrap();
    wait_for(4);
    fs::write(&config, toml::to_string(&initial).unwrap()).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nthird\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!env.ws.path().join("leaked").exists());
    let seen = seen.lock().unwrap();
    assert!(result_text(&seen[3].body, "revoked").contains("denied:"));
    assert!(result_text(&seen[5].body, "enabled").contains("no background tasks"));
    let rows = records(&env.audit_log());
    assert!(rows.iter().any(
        |row| row["args"]["operation"] == "exec-completed" && row["args"]["cancelled"] == true
    ));
    for row in rows.iter().filter(|row| {
        row["principal"] == "plugin.standard-bash"
            && row["args"]["operation"] == "load"
            && row["allow"] == true
    }) {
        assert_eq!(
            row["args"]["sha256"],
            saddle_standard::artifact("bash").unwrap().sha256
        );
    }
}

#[test]
fn ordinary_tool_delegation_changes_on_reload_without_changing_the_shipped_digest() {
    let mut replies = Vec::new();
    for phase in 0..3 {
        replies.push(sse_response(&openai_sse(
            "",
            &[(&format!("read-{phase}"), "read", json!({"path":"file"}))],
        )));
        replies.push(sse_response(&openai_sse("done", &[])));
    }
    let (base, seen) = serve(replies);
    let env = Env::new(Api::Openai, &base);
    fs::write(env.ws.path().join("file"), "ONLY-AUTHORIZED-READ").unwrap();
    let path = env.xdg.path().join("config/saddle/config.toml");
    let initial: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
    let configure = |allow| {
        let mut settings = initial.clone();
        settings["standard-tools"]["read"] = if allow {
            toml::Value::Array(vec!["fs-read".into()])
        } else {
            toml::Value::Array(vec![])
        };
        fs::write(&path, toml::to_string(&settings).unwrap()).unwrap();
    };
    configure(false);
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let wait_for = |count| {
        let deadline = Instant::now() + Duration::from_secs(30);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(10));
        }
    };
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    wait_for(2);
    configure(true);
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/reload\nsecond\n")
        .unwrap();
    wait_for(4);
    configure(false);
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nthird\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    for phase in 0..3 {
        let message = seen[phase * 2 + 1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["tool_call_id"] == format!("read-{phase}"))
            .unwrap();
        assert_eq!(
            message["content"]
                .as_str()
                .unwrap()
                .contains("ONLY-AUTHORIZED-READ"),
            phase == 1
        );
        if phase != 1 {
            assert!(message["content"].as_str().unwrap().starts_with("denied:"));
        }
    }
    let loads = records(&env.audit_log());
    let loads: Vec<_> = loads
        .iter()
        .filter(|row| {
            row["principal"] == "plugin.standard-read"
                && row["args"]["operation"] == "load"
                && row["allow"] == true
        })
        .collect();
    assert_eq!(loads.len(), 1);
    assert_eq!(
        loads[0]["args"]["sha256"],
        saddle_standard::artifact("read").unwrap().sha256
    );
}

#[test]
fn ordinary_edit_and_patch_emit_minimal_hunks_keep_them_out_of_model_text_and_save_them() {
    let original: String = (1..=20).map(|line| format!("line {line}\n")).collect();
    let updated = original
        .replace("line 5\n", "line five\n")
        .replace("line 18\n", "line eighteen\n");
    let calls = [
        (
            "edit",
            "edit",
            json!({"path":"edit.txt","old":original,"new":updated}),
        ),
        (
            "patch",
            "patch",
            json!({"patch":"*** Begin Patch\n*** Update File: patch.txt\n@@\n-line 5\n+line five\n@@\n-line 18\n+line eighteen\n*** End Patch"}),
        ),
    ];
    let (base, seen) = serve(vec![
        sse_response(&openai_sse("", &calls)),
        sse_response(&openai_sse("done", &[])),
    ]);
    let env = Env::new(Api::Openai, &base);
    for path in ["edit.txt", "patch.txt"] {
        fs::write(env.ws.path().join(path), &original).unwrap();
    }
    let out = env.saddle(&["run", "--json", "make two disjoint changes"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let events: Vec<Value> = text(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|event: &Value| event["type"] == "result")
        .collect();
    assert_eq!(events.len(), 2);
    for (event, name) in events.iter().zip(["edit", "patch"]) {
        assert_eq!(
            event["provenance"]["principal"],
            format!("plugin.standard-{name}")
        );
        let diff = event["diff"]
            .as_str()
            .expect("successful ordinary mutation has a human diff");
        assert!(diff.contains("-5 line 5\n+5 line five\n"), "{diff}");
        assert!(diff.contains("-18 line 18\n+18 line eighteen\n"), "{diff}");
        assert!(diff.contains("~\n"), "{diff}");
        assert!(
            !diff.contains("line 10"),
            "unchanged middle must not become a broad replacement: {diff}"
        );
        assert_eq!(
            fs::read_to_string(env.ws.path().join(format!("{name}.txt"))).unwrap(),
            updated
        );
    }
    let seen = seen.lock().unwrap();
    assert!(!seen[1].body.to_string().contains("\"diff\""));
    for result in seen[1].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
    {
        assert!(
            result["content"]
                .as_str()
                .unwrap()
                .starts_with("<plugin-output-")
        );
        assert!(!result["content"].as_str().unwrap().contains("-5 line 5"));
    }
    let connection = Connection::open(env.xdg.path().join("state/saddle/sessions.db")).unwrap();
    let saved: String = connection
        .query_row(
            "SELECT body FROM nodes WHERE kind='item' AND body LIKE '%\"diff\":%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for name in ["edit", "patch"] {
        assert!(saved.contains(&format!("plugin.standard-{name}")));
    }
    assert!(saved.contains("-18 line 18"));
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
    let result: Value = serde_json::from_str(tool_text(&result)).unwrap();
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
    let result: Value =
        serde_json::from_str(tool_text(results[0]["content"].as_str().unwrap())).unwrap();
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

/// The first conversation this workspace saved, as the store names it (D163).
fn first_session(db: &Path) -> String {
    Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT id FROM sessions ORDER BY rowid LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// A mock whose second request asks `session_query` a list of questions, the first conversation's
/// id looked up in the database when the request arrives: it does not exist until the first run has
/// saved it. A question marked `true` asks about that conversation rather than this one (D163).
fn reading_the_first_conversation(
    db: Arc<Mutex<PathBuf>>,
    asked: &'static [(&'static str, bool)],
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    serve_dynamic(3, move |index, _| match index {
        0 => sse_response(&openai_sse("remember teal", &[])),
        1 => {
            let id = first_session(&db.lock().unwrap().clone());
            let calls: Vec<_> = asked
                .iter()
                .enumerate()
                .map(|(i, (sql, other))| {
                    let input = if *other {
                        json!({"sql": sql, "session": id})
                    } else {
                        json!({"sql": sql})
                    };
                    (format!("q{i}"), "session_query", input)
                })
                .collect();
            sse_response(&openai_sse(
                "",
                &calls
                    .iter()
                    .map(|(a, b, c)| (a.as_str(), *b, c.clone()))
                    .collect::<Vec<_>>(),
            ))
        }
        _ => sse_response(&openai_sse("done", &[])),
    })
}

#[test]
fn another_conversation_of_this_workspace_is_readable_and_listed() {
    // D163: the workspace's conversations are one person's work, so a bare `transcript-read`
    // covers them; the second call in the same turn shows what the `sessions` view says.
    let db = Arc::new(Mutex::new(PathBuf::new()));
    let (base, seen) = reading_the_first_conversation(
        db.clone(),
        &[
            ("SELECT text FROM messages WHERE role='user'", true),
            ("SELECT id, title FROM sessions", false),
        ],
    );
    let env = Env::new(Api::Openai, &base);
    *db.lock().unwrap() = env.xdg.path().join("state/saddle/sessions.db");
    env.write_override("[agent]\ntranscript-read = {}\n");
    assert!(env.saddle(&["run", "remember teal"]).status.success());
    let out = env.saddle(&["run", "what did we say before"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let results: Vec<_> = seen[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    let quoted: Value =
        serde_json::from_str(tool_text(results[0]["content"].as_str().unwrap())).unwrap();
    assert_eq!(quoted["rows"], json!([["remember teal"]]), "{quoted}");
    // The other conversation is listed as well, so an id can be found rather than guessed.
    let listed: Value =
        serde_json::from_str(tool_text(results[1]["content"].as_str().unwrap())).unwrap();
    let rows = listed["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{listed}");
    assert!(
        rows.iter().any(|r| r[1] == json!("remember teal")),
        "the other conversation is named: {listed}"
    );

    // The same decision, audited against the conversation it read, not just the one it ran in.
    let records = records(&env.audit_log());
    let decisions: Vec<_> = records
        .iter()
        .filter(|r| r["scope"] == "transcript-read")
        .collect();
    let other = decisions
        .iter()
        .find(|r| r["args"]["others"] == true)
        .expect("a query against another conversation is recorded as one");
    assert_eq!(other["allow"], true);
    assert_eq!(
        other["args"]["session"],
        json!(first_session(
            &env.xdg.path().join("state/saddle/sessions.db")
        ))
    );
}

#[test]
fn a_transcript_grant_that_names_this_session_only_refuses_another_one() {
    // D163: `sessions = "self"` is the narrowing, and it is enforced before anything is resolved
    // or listed, so a refused query says nothing about which conversations exist.
    let db = Arc::new(Mutex::new(PathBuf::new()));
    let (base, seen) = reading_the_first_conversation(db.clone(), &[("SELECT 1", true)]);
    let env = Env::new(Api::Openai, &base);
    *db.lock().unwrap() = env.xdg.path().join("state/saddle/sessions.db");
    env.write_override("[agent]\ntranscript-read = { sessions = \"self\" }\n");
    assert!(env.saddle(&["run", "remember teal"]).status.success());
    let out = env.saddle(&["run", "what did we say before"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let results: Vec<_> = seen[2].body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    let denied = results[0]["content"].as_str().unwrap();
    assert!(
        denied.contains("denied:") && denied.contains("only this conversation"),
        "{denied}"
    );
    let records = records(&env.audit_log());
    assert!(
        records.iter().any(|r| r["scope"] == "transcript-read"
            && r["allow"] == false
            && r["args"]["others"] == true),
        "the refusal is recorded"
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
        request.contains("denied:") && request.contains("transcript-read"),
        "{request}"
    );
    assert!(
        records(&env.audit_log())
            .iter()
            .any(|r| r["principal"] == "plugin.standard-session-query" && r["allow"] == false)
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
    assert!(stderr.contains("cancelled"), "{stderr}");
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
    let component = "read";
    plugin_fixture::configure(
        &env.xdg.path().join("config/saddle/config.toml"),
        &standard_component(component),
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
    let component = "session_query";
    plugin_fixture::configure_transcript(
        &env.xdg.path().join("config/saddle/config.toml"),
        &standard_component(component),
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
    let component = "read";
    let bytes = standard_component(component);
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
    assert!(result(1, "shared-1").contains("denied:"));
    assert!(result(3, "shared-2").contains("SHARED-REFERENCE-CONTENT"));
    assert!(!result(3, "excluded").contains("ROOT-EXCLUSION-SECRET"));
    assert!(result(3, "excluded").contains("excluded"));
    assert!(result(5, "root-3").contains("workspace content"));
    assert!(!result(5, "shared-3").contains("SHARED-REFERENCE-CONTENT"));
    assert!(result(5, "shared-3").contains("denied:"));
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
        .filter(|row| {
            row["args"]["operation"] == "load"
                && row["allow"] == true
                && row["principal"] == "plugin.reader"
        })
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
fn standard_search_keeps_credentials_private_and_snapshots_principal_scoped_across_reload() {
    let first = json!({"results":[{"title":"first","url":"https://example.test/first","text":format!("full first page {KEY}")},{"title":"unshown","url":"https://example.test/unshown","text":"must not acquire a number"}]}).to_string();
    let second = json!({"results":[{"title":"second","url":"https://example.test/second","text":"full second page"}]}).to_string();
    let search = HttpServer::new(vec![
        http_reply(200, "Content-Type: application/json\r\n", &first),
        http_reply(200, "Content-Type: application/json\r\n", &second),
    ]);
    let reader = "plugin__reader__websearch";
    let sibling = "plugin__sibling__websearch";
    let calls = [
        ("query", reader, json!({"query":"first","num_results":1})),
        ("unshown", reader, json!({"result":2})),
        ("expand", reader, json!({"result":1})),
        ("sibling", sibling, json!({"result":1})),
        ("different", reader, json!({"query":"second","result":1})),
        ("reuse", reader, json!({"result":1})),
    ];
    let mut responses: Vec<_> = calls
        .iter()
        .map(|(id, tool, args)| sse_response(&openai_sse("", &[(*id, *tool, args.clone())])))
        .collect();
    responses.push(sse_response(&openai_sse("phase one done", &[])));
    responses.push(sse_response(&openai_sse(
        "",
        &[("revoked", reader, json!({"result":1}))],
    )));
    responses.push(sse_response(&openai_sse("done", &[])));
    let (base, seen) = serve(responses);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let bytes = standard_component("websearch");
    let grant = format!("{{host='127.0.0.1',scheme='http',port={}}}", search.port);
    let model_port: u16 = base
        .strip_prefix("http://127.0.0.1:")
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let other = format!("{{host='127.0.0.1',scheme='http',port={model_port}}}");
    let mut settings: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let reach: toml::Table = format!("net=[{grant},{other}]").parse().unwrap();
    settings["agent"]
        .as_table_mut()
        .unwrap()
        .insert("net".into(), reach["net"].clone());
    settings["providers"]["mock"]
        .as_table_mut()
        .unwrap()
        .insert("search-url".into(), format!("{}/search", search.url).into());
    fs::write(&config, toml::to_string(&settings).unwrap()).unwrap();
    plugin_fixture::configure(&config, &bytes, &format!("tool={{}}\nnet=[{grant}]"));
    let root = config.parent().unwrap().join("plugins");
    fs::write(
        root.join("reader/plugin.toml"),
        "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','net']\n",
    )
    .unwrap();
    fs::create_dir_all(root.join("sibling")).unwrap();
    fs::write(root.join("sibling/component.wasm"), &bytes).unwrap();
    fs::write(
        root.join("sibling/plugin.toml"),
        "id='sibling'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','net']\n",
    )
    .unwrap();
    let pin = sha256_hex(&bytes);
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(&config,format!("{settings}\n[plugins.sibling]\nenabled=true\nmanifest='sibling/plugin.toml'\nsha256='{pin}'\n[plugin.sibling]\ntool={{}}\nnet=[{grant}]\n")).unwrap();
    let listing = env.saddle(&["plugin", "list"]);
    assert!(
        text(&listing.stdout)
            .lines()
            .filter(|line| line.starts_with("reader ") || line.starts_with("sibling "))
            .filter(|line| line.contains("ready:"))
            .count()
            == 2,
        "{} {}",
        text(&listing.stdout),
        text(&listing.stderr)
    );
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"phase one\n")
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while seen.lock().unwrap().len() < 7 {
        assert!(Instant::now() < deadline, "search turn did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    let settings = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        settings.replace(
            &format!("[plugin.reader]\ntool={{}}\nnet=[{grant}]"),
            &format!("[plugin.reader]\ntool={{}}\nnet=[{other}]"),
        ),
    )
    .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nphase two\n/quit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let seen = seen.lock().unwrap();
    let content = |index: usize, id: &str| {
        seen[index].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["tool_call_id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        content(1, "query").contains("1. first"),
        "{}\nstderr: {}",
        content(1, "query"),
        text(&out.stderr)
    );
    assert!(content(2, "unshown").contains("returned 1"));
    assert!(content(3, "expand").contains("full first page"));
    assert!(content(4, "sibling").contains("no search to expand"));
    assert!(content(5, "different").contains("full second page"));
    assert!(content(6, "reuse").contains("full second page"));
    assert!(content(8, "revoked").contains("denied:"));
    for (index, (id, _, _)) in calls.iter().enumerate() {
        assert!(!content(index + 1, id).contains(KEY));
    }
    let requests = search.seen.lock().unwrap();
    assert_eq!(requests.len(), 2, "expansions must not make more requests");
    assert!(
        requests
            .iter()
            .all(|request| request.contains(&format!("Bearer {KEY}")))
    );
    let audit = fs::read_to_string(env.audit_log()).unwrap();
    assert!(!audit.contains(KEY));
    assert!(
        records(&env.audit_log())
            .iter()
            .any(|row| row["principal"] == "plugin.reader"
                && row["scope"] == "net"
                && row["args"]["call_id"].is_string())
    );
    assert_eq!(fs::read(root.join("reader/component.wasm")).unwrap(), bytes);
    assert!(
        env.saddle(&["audit", "verify", env.audit_log().to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn one_writer_artifact_changes_resource_budgets_on_reload_without_changing_authority() {
    let content = "x".repeat(70 * 1024);
    let mut responses = Vec::new();
    for turn in 0..3 {
        responses.push(sse_response(&openai_sse(
            "",
            &[(
                &format!("budget-{turn}"),
                "plugin__reader__write",
                json!({"path":"large", "content":content}),
            )],
        )));
        responses.push(sse_response(&openai_sse("done", &[])));
    }
    let (base, seen) = serve(responses);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let component = "write";
    let bytes = standard_component(component);
    plugin_fixture::configure(
        &config,
        &bytes,
        "tool={}\nfs-read=[{root='workspace'}]\nfs-write=[{root='workspace',exclude=['private']}]\n",
    );
    fs::write(
        config.parent().unwrap().join("plugins/reader/plugin.toml"),
        "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','fs-read','fs-write']\n",
    )
    .unwrap();
    let original = format!(
        "{}\n[compaction]\nauto=false\nbackground=false\nidle=false\n",
        fs::read_to_string(&config).unwrap()
    );
    fs::write(
        &config,
        format!("{original}\n[tool-limits]\ndiff-present-bytes=1024\n"),
    )
    .unwrap();
    let pin = sha256_hex(&bytes);
    let mut child = env
        .command(&["run", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Tool-call events include the large arguments: drain pipes while waiting for model POSTs.
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let output_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let error_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        bytes
    });
    let wait_for = |count| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while seen.lock().unwrap().len() < count {
            assert!(Instant::now() < deadline, "turn did not finish");
            thread::sleep(Duration::from_millis(10));
        }
    };
    child.stdin.as_mut().unwrap().write_all(b"first\n").unwrap();
    wait_for(2);
    assert!(!env.ws.path().join("large").exists());
    fs::write(&config, format!("{original}\n[tool-limits]\nplugin-input-bytes=131072\nmutation-bytes=131072\ndiff-present-bytes=1024\n")).unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"/reload\nsecond\n")
        .unwrap();
    wait_for(4);
    assert_eq!(
        fs::read_to_string(env.ws.path().join("large")).unwrap(),
        content
    );
    fs::remove_file(env.ws.path().join("large")).unwrap();
    fs::write(&config, format!("{original}\n[tool-limits]\nplugin-input-bytes=131072\nmutation-bytes=65536\ndiff-present-bytes=1024\n")).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reload\nthird\n/quit\n")
        .unwrap();
    let status = child.wait().unwrap();
    let out = Output {
        status,
        stdout: output_reader.join().unwrap(),
        stderr: error_reader.join().unwrap(),
    };
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(!env.ws.path().join("large").exists());
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
    let final_config: toml::Table = fs::read_to_string(config).unwrap().parse().unwrap();
    assert_eq!(
        final_config["plugins"]["reader"]["sha256"]
            .as_str()
            .unwrap(),
        pin
    );
    let seen = seen.lock().unwrap();
    for turn in 0..3 {
        let result = seen[turn * 2 + 1].body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["tool_call_id"] == format!("budget-{turn}"))
            .unwrap()["content"]
            .as_str()
            .unwrap();
        assert_eq!(result.contains("wrote large"), turn == 1, "{result}");
        if turn == 0 {
            assert!(result.contains("65536"), "{result}");
        }
        if turn == 2 {
            assert!(result.contains("configured"), "{result}");
        }
    }
}

#[test]
fn one_compiled_writer_changes_write_permissions_at_runtime_without_repinning() {
    writer_runtime_case(false);
    writer_runtime_case(true);
}

fn writer_runtime_case(external: bool) {
    let outside = TempDir::new().unwrap();
    let path = if external {
        outside.path().join("file").display().to_string()
    } else {
        "file".into()
    };
    let tool = "plugin__reader__write";
    let mut responses = Vec::new();
    for turn in 0..3 {
        let id = format!("write-{turn}");
        responses.push(sse_response(&openai_sse(
            "",
            &[(
                &id,
                tool,
                json!({"path":path,"content":format!("turn {turn}")}),
            )],
        )));
        responses.push(sse_response(&openai_sse("done", &[])));
    }
    let (base, seen) = serve(responses);
    let env = Env::new(Api::Openai, &base);
    let config = env.xdg.path().join("config/saddle/config.toml");
    let component = "write";
    let bytes = standard_component(component);
    plugin_fixture::configure(
        &config,
        &bytes,
        if external {
            "tool={}\nfs-read=[{root='workspace'}]\ntool-read={tools=['read']}"
        } else {
            "tool={}\nfs-read=[{root='workspace'}]"
        },
    );
    fs::write(config.parent().unwrap().join("plugins/reader/plugin.toml"), "id='reader'\nversion='0.1.0'\ncomponent='component.wasm'\nscopes=['tool','fs-read','fs-write','tool-read','tool-write']\n").unwrap();
    if external {
        let settings = fs::read_to_string(&config).unwrap();
        fs::write(&config, format!("{settings}\n[sandbox]\ntools-defaults=false\nwrite=[{{path={:?}, tools=['write','read']}}]\n", outside.path().display().to_string())).unwrap();
    }
    let target = if external {
        outside.path().join("file")
    } else {
        env.ws.path().join("file")
    };
    let initial: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let pin = initial["plugins"]["reader"]["sha256"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(target.clone(), "original").unwrap();
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
    assert_eq!(fs::read_to_string(target.clone()).unwrap(), "original");
    let grant = if external {
        "tool-write={tools=['write']}\n"
    } else {
        "fs-write=[{root='workspace'}]\n"
    };
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
    assert_eq!(fs::read_to_string(target.clone()).unwrap(), "turn 1");
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
    let results: Vec<Value> = text(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row["type"] == "result")
        .collect();
    assert!(results.iter().any(|row| {
        row["diff"]
            .as_str()
            .is_some_and(|diff| diff.contains("+1 turn 1"))
    }));
    assert_eq!(fs::read_to_string(target.clone()).unwrap(), "turn 1");
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
        assert_eq!(
            result.contains(&format!("wrote {path}")),
            turn == 1,
            "{result}"
        );
        if turn != 1 {
            assert!(result.contains("denied:"), "{result}");
        }
    }
    let audit = records(&env.audit_log());
    assert!(
        audit
            .iter()
            .filter(|row| row["args"]["operation"] == "load"
                && row["allow"] == true
                && row["principal"] == "plugin.reader")
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
    let home = env.xdg.path().join("home");
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::write(home.join(".config/prompt\nmetadata"), "fixture").unwrap();
    fs::create_dir_all(home.join("Pictures/comfyui")).unwrap();
    fs::write(home.join("Pictures/comfyui/render.png"), "personal").unwrap();
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
            "A command inherits its delegated filesystem roots",
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

    assert!(
        system.contains("~/.config/prompt\\nmetadata"),
        "effective include paths are JSON quoted"
    );
    assert!(
        !system.contains("prompt\nmetadata"),
        "filenames cannot introduce prompt structure"
    );
    assert!(!system.contains("Pictures/comfyui"));
    assert!(!system.contains("your home directory with the credential stores left out"));

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
    assert!(bash.contains("policy proxy"), "{bash}");
    assert!(bash.contains("plugin's runtime grants"), "{bash}");
    assert!(bash.contains("falls back to none"), "{bash}");
    assert!(!bash.contains("sandbox: no network"), "{bash}");
    assert!(
        !bash.contains("files limited to the granted roots"),
        "{bash}"
    );

    let fetch = description("fetch");
    assert!(fetch.contains("delegated net grants"), "{fetch}");
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
        .map(|m| tool_text(m["content"].as_str().unwrap()).to_owned())
        .collect();
    let [grep, find, ls, edit] = &results[..] else {
        panic!("{results:?}")
    };
    assert!(grep.starts_with("src/main.rs:1: fn main() {}"), "{grep}");
    assert!(
        !grep.contains("hunter2") && !grep.contains("target/"),
        "{grep}"
    );
    assert!(grep.contains("paths skipped by policy"), "{grep}");
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
        saved.contains(
            r#""diff":" 0 src/main.rs\n-1 fn main() {}\n+1 fn main() {\n+2     run();\n+3 }\n""#
        ),
        "{saved}"
    );
}

/// D170: `!cmd` shares the command and its output as one user message. It goes to the model and to
/// the saved conversation without the host's `plugin-output` frame, which belongs on a tool result
/// rather than on a prompt, and a resumed transcript reads the shape back as the block the run drew.
#[test]
fn a_shared_bang_command_carries_no_plugin_frame() {
    let api = Api::Openai;
    let (base, seen) = serve(vec![sse_response(&api.reply("noted", &[]))]);
    let env = Env::new(api, &base);
    let mut child = env
        .command(&["run"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"!printf shared-marker\nand then this\n/exit\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));

    // What the model was sent: the command on its `$ ` line, its output, and no delimiter.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let mut sent = String::new();
    strings(&seen[0].body, &mut sent);
    // The tool descriptions are framed, so the frame's absence is checked on the user turn itself.
    let start = sent
        .find("I ran a command in the workspace:")
        .unwrap_or_else(|| panic!("{sent}"));
    let turn = &sent[start..];
    let end = turn
        .find("and then this")
        .expect("the next prompt follows it");
    assert!(turn[..end].contains("$ printf shared-marker"), "{turn}");
    assert!(turn[..end].contains("shared-marker"), "{turn}");
    assert!(!turn[..end].contains("<plugin-output-"), "{}", &turn[..end]);

    // What was saved: the same message, which is what a resume reads back.
    let conn = Connection::open(env.xdg.path().join("state/saddle/sessions.db")).unwrap();
    let saved: Vec<String> = conn
        .prepare("SELECT body FROM nodes WHERE kind = 'item'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let shared = saved
        .iter()
        .find(|body| body.contains("shared-marker"))
        .unwrap_or_else(|| panic!("{saved:?}"));
    assert!(
        shared.contains("I ran a command in the workspace:\\n$ printf shared-marker\\n"),
        "{shared}"
    );
    assert!(!shared.contains("<plugin-output-"), "{shared}");
    // It is a prompt, not a tool result: the model sees a user turn (D170).
    assert!(shared.starts_with("{\"user\":"), "{shared}");
}
