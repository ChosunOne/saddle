//! The ChatGPT backend's WebSocket transport, and the delta a kept connection allows (D101).
//!
//! The backend Codex talks to refuses `store: true`, so it keeps no conversation of its own; over a
//! WebSocket it can still be continued, because `previous_response_id` refers to state that lives on
//! that connection. That is what makes sending only the newest items legal, and it is why the socket
//! is kept between requests: a new connection starts from the whole history, because the state a
//! delta refers to was left behind on the old one.
//!
//! D125: opening or sending failures fall back to HTTP, and a read-time failure can be retried
//! there before any reply text or reasoning is shown. Partial replies are never silently replayed.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::client::ClientRequestBuilder;
use tungstenite::http::Uri;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message, WebSocket, connect};

use super::http::{self, Msg};
use super::retry::Transient;
use super::sse;
use super::{ProviderConfig, codex_backend};

/// One kept connection, and what it has already been told.
struct Link {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    continuation: Option<Continuation>,
    /// D102: a kept socket belongs to its endpoint and authentication context as well as its session.
    affinity: [u8; 32],
}

/// What makes the next request on this connection a delta rather than a whole history.
struct Continuation {
    /// The body the last request sent, with the fields a continuation changes left out, as JSON.
    body: String,
    /// The response the next request continues from.
    response_id: String,
    /// What the server has: the input sent so far and the items the replies produced. The next
    /// request must extend exactly this, or the delta would describe a conversation the server
    /// does not have.
    baseline: Vec<Value>,
}

/// The kept connections, one per conversation.
fn links() -> MutexGuard<'static, HashMap<String, Link>> {
    static LINKS: OnceLock<Mutex<HashMap<String, Link>>> = OnceLock::new();
    // A poisoned lock here means another thread panicked holding it; the map is a cache of sockets
    // and holding a stale one costs a reconnect, so recovering it is right.
    LINKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The URL to open for this provider, or `None` when it is not the backend that speaks it.
pub fn websocket_url(config: &ProviderConfig, url: &str) -> Option<String> {
    if !codex_backend(&config.base_url) {
        return None;
    }
    if let Some(rest) = url.strip_prefix("https://") {
        return Some(format!("wss://{rest}"));
    }
    url.strip_prefix("http://")
        .map(|rest| format!("ws://{rest}"))
}

/// What the caller keeps until the reply has been parsed, so the connection can be left ready for
/// the next request (D101).
pub struct Pending {
    handed: Arc<Mutex<Option<Handed>>>,
    session: String,
    /// The body as sent, minus the fields a continuation changes: the next request must match it.
    body: String,
    /// Everything the server holds once this request has been answered.
    baseline: Vec<Value>,
    affinity: [u8; 32],
}

/// The socket a finished stream gives back, and the response it ended on.
struct Handed {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    response_id: Option<String>,
}

impl Pending {
    /// Keeps the connection, ready for a delta, once the reply is known.
    ///
    /// `produced` is the reply rendered as the input items the next request will send, so the
    /// baseline is what the server has rather than what it said it had: the two differ, because a
    /// response's own `output` items are not the shape saddle sends back.
    pub fn finish(self, produced: Vec<Value>) {
        let Some(handed) = self.handed.lock().ok().and_then(|mut slot| slot.take()) else {
            return;
        };
        let Some(response_id) = handed.response_id else {
            return;
        };
        let mut baseline = self.baseline;
        baseline.extend(produced);
        let mut map = links();
        // A concurrent request may have kept a connection of its own meanwhile; dropping this one
        // is right, and taking its place would close a stream still being read.
        if map.contains_key(&self.session) {
            return;
        }
        map.insert(
            self.session,
            Link {
                socket: handed.socket,
                affinity: self.affinity,
                continuation: Some(Continuation {
                    body: self.body,
                    response_id,
                    baseline,
                }),
            },
        );
    }
}

/// Opens the connection for one request and returns its event stream.
///
/// The error is the caller's cue to send the same request over HTTP: nothing has been written to
/// the reply yet, so nothing is lost by trying again the old way.
pub fn stream(
    session: &str,
    url: &str,
    headers: &[(String, String)],
    body: &Value,
) -> Result<(http::EventStream, Pending)> {
    // Hash the full headers, never retaining another copy of credentials in the socket cache.
    let affinity: [u8; 32] = Sha256::digest(serde_json::to_vec(&(url, headers))?).into();
    let kept = links()
        .remove(session)
        .filter(|link| link.affinity == affinity);
    let reused = kept.is_some();
    let (mut socket, continuation) = match kept {
        Some(link) => (link.socket, link.continuation),
        None => (open(url, headers)?, None),
    };
    let (frame, baseline) = prepare(body, continuation.as_ref());
    if let Err(error) = socket.send(Message::Text(frame.to_string().into())) {
        // A kept socket the far side has since closed is the ordinary case: an idle timeout ends
        // it. A new connection cannot continue what the old one knew, so this is a whole history.
        if !reused {
            return Err(error).context("sending the first frame");
        }
        let mut fresh = open(url, headers)?;
        let (frame, baseline) = prepare(body, None);
        fresh
            .send(Message::Text(frame.to_string().into()))
            .context("sending the first frame")?;
        return run(
            fresh,
            session,
            stripped(body),
            baseline,
            body.clone(),
            affinity,
        );
    }
    run(
        socket,
        session,
        stripped(body),
        baseline,
        body.clone(),
        affinity,
    )
}

fn run(
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    session: &str,
    body: String,
    baseline: Vec<Value>,
    whole: Value,
    affinity: [u8; 32],
) -> Result<(http::EventStream, Pending)> {
    let stop = Arc::new(AtomicBool::new(false));
    let handed = Arc::new(Mutex::new(None));
    let (tx, rx) = channel();
    let pump_stop = stop.clone();
    let pump_handed = handed.clone();
    thread::Builder::new()
        .name("saddle-ws".into())
        .spawn(move || pump(socket, &tx, &pump_stop, &pump_handed, whole))
        .context("starting the websocket thread")?;
    Ok((
        http::event_stream(rx, stop),
        Pending {
            handed,
            session: session.to_owned(),
            body,
            baseline,
            affinity,
        },
    ))
}

fn open(url: &str, headers: &[(String, String)]) -> Result<WebSocket<MaybeTlsStream<TcpStream>>> {
    let uri: Uri = url.parse().with_context(|| format!("not a URL: {url}"))?;
    let mut request = ClientRequestBuilder::new(uri);
    // D102: HTTP's responses=experimental is not the WebSocket v2 protocol. Replace it
    // case-insensitively rather than appending a second, conflicting beta header.
    for (name, value) in headers
        .iter()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("openai-beta"))
    {
        request = request.with_header(name.as_str(), value.as_str());
    }
    request = request.with_header("OpenAI-Beta", "responses_websockets=2026-02-06");
    let (socket, _) = connect(request).context("opening the websocket")?;
    // Match the HTTP stream's idle-read bound; an unresponsive peer must eventually recover too.
    let tcp = match socket.get_ref() {
        MaybeTlsStream::Plain(tcp) => Some(tcp),
        MaybeTlsStream::Rustls(tls) => Some(&tls.sock),
        _ => None,
    };
    if let Some(tcp) = tcp {
        tcp.set_read_timeout(Some(Duration::from_secs(300)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(30)))?;
    }
    Ok(socket)
}

/// Reads the connection until the response is complete, handing each frame on as an event.
///
/// The socket goes back to the caller only when the stream ended by itself on a completed response:
/// anything else — a cancel, a closed connection, an error frame — leaves a connection nothing can
/// be continued from.
fn pump(
    mut socket: WebSocket<MaybeTlsStream<TcpStream>>,
    tx: &Sender<Msg>,
    stop: &AtomicBool,
    handed: &Mutex<Option<Handed>>,
    whole: Value,
) {
    // A continuation the backend no longer has is answered with an error rather than a reply. It
    // costs one frame to find out, and resending the whole history is what turns a stale
    // connection into the request that would have been made anyway (D101).
    let mut retried = false;
    let mut saw_output = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match socket.read() {
            Ok(Message::Text(text)) => {
                let data = text.as_str().to_owned();
                let event: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                let kind = event["type"].as_str().unwrap_or_default().to_owned();
                if !retried && !saw_output && kind == "error" && continuation_lost(&event) {
                    retried = true;
                    let mut full = whole.clone();
                    full["type"] = json!("response.create");
                    if let Err(error) = socket.send(Message::Text(full.to_string().into())) {
                        let _ = tx.send(Msg::Failed(
                            Transient::new(format!("websocket resend: {error}"))
                                .with_reason("websocket resend failed")
                                .into(),
                        ));
                        return;
                    }
                    continue;
                }
                let done = matches!(
                    kind.as_str(),
                    "response.completed" | "response.done" | "response.incomplete"
                );
                let completed = matches!(kind.as_str(), "response.completed" | "response.done")
                    && !matches!(
                        event["response"]["status"].as_str(),
                        Some("incomplete" | "failed")
                    );
                let response_id = completed
                    .then(|| event["response"]["id"].as_str().map(str::to_owned))
                    .flatten();
                if done {
                    // D102: publish the socket before the terminal event. The parser may finish
                    // immediately on that event, and only a completed response seeds a delta.
                    if completed && let Ok(mut slot) = handed.lock() {
                        *slot = Some(Handed {
                            socket,
                            response_id,
                        });
                    }
                    let _ = tx.send(Msg::Event(sse::Event {
                        event: String::new(),
                        data,
                    }));
                    return;
                }
                saw_output |= kind.starts_with("response.output")
                    || kind.starts_with("response.reasoning")
                    || kind.starts_with("response.function_call");
                if tx
                    .send(Msg::Event(sse::Event {
                        event: String::new(),
                        data,
                    }))
                    .is_err()
                {
                    return;
                }
                if kind == "error" || kind == "response.failed" {
                    return;
                }
            }
            Ok(Message::Close(_)) => {
                let _ = tx.send(Msg::Failed(
                    Transient::new("websocket closed before response.completed")
                        .with_reason("websocket closed before completion")
                        .into(),
                ));
                return;
            }
            // tungstenite answers a ping itself while reading; a pong it does not answer is ours
            // to ignore, and a binary frame is not part of this API.
            Ok(_) => {}
            Err(error) => {
                let _ = tx.send(Msg::Failed(
                    Transient::new(format!("websocket: {error}"))
                        .with_reason(read_failure_reason(&error))
                        .into(),
                ));
                return;
            }
        }
    }
}

fn read_failure_reason(error: &WsError) -> &'static str {
    match error {
        WsError::Io(error)
            if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
        {
            "websocket read timed out"
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => "websocket connection closed",
        WsError::Protocol(_) => "websocket protocol read failed",
        WsError::Io(_) => "websocket I/O read failed",
        WsError::Tls(_) => "websocket TLS read failed",
        _ => "websocket read failed",
    }
}

/// Whether an error frame says the connection no longer has the response being continued from.
fn continuation_lost(event: &Value) -> bool {
    let code = |value: &Value| {
        value["code"]
            .as_str()
            .or_else(|| value["error"]["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    let message = |value: &Value| {
        value["message"]
            .as_str()
            .or_else(|| value["error"]["message"].as_str())
            .unwrap_or_default()
            .to_owned()
    };
    [code(event), message(event)]
        .iter()
        .any(|said| said.contains("previous_response_not_found"))
}

/// The frame to send, and everything the server holds afterwards.
fn prepare(body: &Value, continuation: Option<&Continuation>) -> (Value, Vec<Value>) {
    let input: Vec<Value> = body["input"].as_array().cloned().unwrap_or_default();
    let mut frame = body.clone();
    frame["type"] = json!("response.create");
    if let Some(continuation) = continuation
        && continuation.body == stripped(body)
        && input.len() >= continuation.baseline.len()
        && input[..continuation.baseline.len()] == continuation.baseline[..]
    {
        let tail: Vec<Value> = input[continuation.baseline.len()..].to_vec();
        frame["previous_response_id"] = json!(continuation.response_id);
        frame["input"] = Value::Array(tail.clone());
        let mut baseline = continuation.baseline.clone();
        baseline.extend(tail);
        return (frame, baseline);
    }
    (frame, input)
}

/// The body with the fields a continuation rewrites left out, as canonical JSON to compare against.
fn stripped(body: &Value) -> String {
    let mut bare = body.clone();
    if let Value::Object(map) = &mut bare {
        map.remove("input");
        map.remove("previous_response_id");
        map.remove("type");
    }
    serde_json::to_string(&bare).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::io::{Error as IoError, ErrorKind};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    use tungstenite::handshake::server::{Request as UpgradeRequest, Response as UpgradeResponse};
    use tungstenite::{accept, accept_hdr};

    use super::*;
    use crate::provider::sse::Events;
    use crate::provider::{Api, Sink};

    struct Silent;
    impl Sink for Silent {
        fn text(&mut self, _: &str) {}
        fn note(&mut self, _: &str) {}
    }

    fn config(host: &str) -> ProviderConfig {
        ProviderConfig {
            api: Api::OpenaiResponses,
            base_url: format!("https://{host}/backend-api/codex"),
            model: "m".into(),
            api_key_env: None,
            max_tokens: None,
            context_window: None,
            effort: None,
            price: None,
            usage_url: None,
            search_url: None,
            headers: Default::default(),
            key_header: None,
            cache_retention: Default::default(),
            cache_affinity: false,
            auth: Default::default(),
            oauth: None,
        }
    }

    /// A server that answers every frame on one kept connection, and reports what it was asked.
    ///
    /// It reads with a timeout so the thread ends when the client goes quiet, which is what lets
    /// the test join it while the client still holds the socket.
    fn server(answer: fn(&Value) -> Vec<Value>) -> (String, thread::JoinHandle<Vec<Value>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let mut received = Vec::new();
            let Ok((stream, _)) = listener.accept() else {
                return received;
            };
            stream
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let Ok(mut socket) = accept(stream) else {
                return received;
            };
            loop {
                match socket.read() {
                    Ok(Message::Text(text)) => {
                        let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else {
                            break;
                        };
                        received.push(frame.clone());
                        for reply in answer(&frame) {
                            if socket
                                .send(Message::Text(reply.to_string().into()))
                                .is_err()
                            {
                                return received;
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            received
        });
        (
            format!("ws://127.0.0.1:{port}/backend-api/codex/responses"),
            handle,
        )
    }

    fn reply(id: &str, text: &str) -> Vec<Value> {
        vec![
            json!({"type":"response.output_text.delta","delta":text}),
            json!({"type":"response.completed","response":{"id":id,"output":[
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}
            ]}}),
        ]
    }

    /// Reads a whole stream the way the reply parser does.
    fn drain(events: http::EventStream) -> Vec<String> {
        let mut events = events;
        let mut out = Vec::new();
        let mut sink = Silent;
        while let Ok(Some(event)) = events.next(&mut sink) {
            out.push(event.data);
        }
        out
    }

    fn body(input: Value) -> Value {
        json!({
            "model": "m",
            "instructions": "s",
            "input": input,
            "stream": true,
            "store": false,
        })
    }

    #[test]
    fn only_the_backend_that_speaks_it_gets_a_websocket() {
        // D101: the subscription backend, and nothing else. OpenAI's API-key endpoint is a
        // different host with a different transport.
        assert_eq!(
            websocket_url(
                &config("chatgpt.com"),
                "https://chatgpt.com/backend-api/codex/responses"
            )
            .as_deref(),
            Some("wss://chatgpt.com/backend-api/codex/responses")
        );
        assert_eq!(
            websocket_url(
                &config("api.openai.com"),
                "https://api.openai.com/v1/responses"
            ),
            None
        );
    }

    #[test]
    fn read_diagnostics_distinguish_timeout_disconnect_and_io_without_echoing_errors() {
        for kind in [ErrorKind::TimedOut, ErrorKind::WouldBlock] {
            let error = WsError::Io(IoError::new(kind, "untrusted diagnostics"));
            assert_eq!(read_failure_reason(&error), "websocket read timed out");
        }
        assert_eq!(
            read_failure_reason(&WsError::ConnectionClosed),
            "websocket connection closed"
        );
        let error = WsError::Io(IoError::new(ErrorKind::ConnectionReset, "private details"));
        assert_eq!(read_failure_reason(&error), "websocket I/O read failed");
    }

    #[test]
    #[allow(clippy::result_large_err)] // tungstenite fixes the handshake callback's error type.
    fn the_upgrade_uses_v2_even_when_http_headers_supply_a_different_beta() {
        for name in ["OpenAI-Beta", "openai-beta", "oPeNaI-bEtA"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("ws://{}/responses", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                let mut socket = accept_hdr(
                    accept_connection(&listener),
                    |request: &UpgradeRequest, response: UpgradeResponse| {
                        let values: Vec<_> =
                            request.headers().get_all("openai-beta").iter().collect();
                        assert_eq!(values.len(), 1);
                        assert_eq!(values[0], "responses_websockets=2026-02-06");
                        assert_eq!(request.headers()["authorization"], "Bearer test-only");
                        assert_eq!(request.headers()["session_id"], "upgrade-test");
                        Ok(response)
                    },
                )
                .unwrap();
                assert!(matches!(socket.read().unwrap(), Message::Text(_)));
                for event in reply("resp_upgrade", "ok") {
                    socket
                        .send(Message::Text(event.to_string().into()))
                        .unwrap();
                }
            });
            let headers = vec![
                (name.into(), "responses=experimental".into()),
                ("authorization".into(), "Bearer test-only".into()),
                ("session_id".into(), "upgrade-test".into()),
            ];
            let (events, _pending) =
                stream("upgrade-test", &url, &headers, &body(json!([]))).unwrap();
            assert_eq!(drain(events).len(), 2);
            server.join().unwrap();
            assert_eq!(
                headers[0].1, "responses=experimental",
                "HTTP headers are not mutated"
            );
        }
    }

    #[test]
    fn a_second_request_sends_only_what_the_connection_has_not_seen() {
        // D101: the point of keeping the socket. The first request carries the whole history; the
        // second carries the new items and names the response it continues from, which is what
        // makes a turn cost a delta rather than the conversation again.
        let (url, handle) = server(|_| reply("resp_1", "one"));
        let first = body(json!([{"role":"user","content":"hello"}]));

        let (events, pending) = stream("s1", &url, &[], &first).unwrap();
        assert_eq!(drain(events).len(), 2);
        pending.finish(vec![json!({"role":"assistant","content":"one"})]);

        let next = body(json!([
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"one"},
            {"type":"function_call_output","call_id":"c","output":"the file"}
        ]));
        let (events, pending) = stream("s1", &url, &[], &next).unwrap();
        drain(events);
        pending.finish(Vec::new());

        let received = handle.join().unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0]["type"], "response.create");
        assert_eq!(received[0]["input"].as_array().unwrap().len(), 1);
        assert!(received[0].get("previous_response_id").is_none());
        // The delta: one new item, and the response it follows.
        assert_eq!(received[1]["previous_response_id"], "resp_1");
        assert_eq!(
            received[1]["input"],
            json!([{"type":"function_call_output","call_id":"c","output":"the file"}])
        );
    }

    #[test]
    fn a_body_that_changed_sends_the_whole_history_again() {
        // A continuation is only legal while the conversation is the one the connection has. A
        // changed instruction, or an input that is not an extension, goes back in full.
        let (url, handle) = server(|_| reply("resp_1", "one"));
        let first = body(json!([{"role":"user","content":"hello"}]));
        let (events, pending) = stream("s2", &url, &[], &first).unwrap();
        drain(events);
        pending.finish(vec![json!({"role":"assistant","content":[]})]);

        let mut changed = body(json!([
            {"role":"user","content":"hello"},
            {"role":"assistant","content":[]},
            {"role":"user","content":"another"}
        ]));
        changed["instructions"] = json!("a different system prompt");
        let (events, pending) = stream("s2", &url, &[], &changed).unwrap();
        drain(events);
        pending.finish(Vec::new());

        let received = handle.join().unwrap();
        assert!(received[1].get("previous_response_id").is_none());
        assert_eq!(received[1]["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_continuation_the_backend_lost_is_resent_in_full() {
        // D101: a connection the far side has dropped its state for answers the delta with an
        // error, not a reply. Resending the whole history is what keeps that from being a failed
        // turn, and it is cheap: one frame to find out.
        let (url, handle) = server(|frame| {
            if frame.get("previous_response_id").is_some() {
                return vec![json!({"type":"error","code":"previous_response_not_found",
                    "error":{"message":"no such response"}})];
            }
            reply("resp_1", "one")
        });
        let first = body(json!([{"role":"user","content":"hello"}]));
        let (events, pending) = stream("s4", &url, &[], &first).unwrap();
        drain(events);
        pending.finish(vec![json!({"role":"assistant","content":"one"})]);

        let next = body(json!([
            {"role":"user","content":"hello"},
            {"role":"assistant","content":"one"},
            {"role":"user","content":"again"}
        ]));
        let (events, pending) = stream("s4", &url, &[], &next).unwrap();
        // The reply, not the error: the error frame is not forwarded.
        assert_eq!(drain(events).len(), 2);
        pending.finish(vec![json!({"role":"assistant","content":"one"})]);
        assert!(
            links().contains_key("s4"),
            "the retry completed, so it is kept"
        );

        let received = handle.join().unwrap();
        assert_eq!(received.len(), 3, "delta, then the whole history");
        assert_eq!(received[1]["previous_response_id"], "resp_1");
        assert!(received[2].get("previous_response_id").is_none());
        assert_eq!(received[2]["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_reply_that_did_not_finish_keeps_no_connection() {
        // The socket goes back only on a completed response: a failed stream has nothing the next
        // request could be continued from.
        let (url, _handle) = server(|_| vec![json!({"type":"error","error":{"message":"no"}})]);
        let (events, pending) = stream("s3", &url, &[], &body(json!([]))).unwrap();
        drain(events);
        pending.finish(vec![json!({"role":"assistant","content":[]})]);
        assert!(!links().contains_key("s3"));
    }

    #[test]
    fn the_socket_is_available_before_the_completed_event_is_published() {
        let (url, handle) = server(|_| reply("resp_1", "one"));
        let body = body(json!([{"role":"user","content":"hello"}]));
        let mut sink = Silent;
        for _ in 0..32 {
            let (mut events, pending) = stream("handover", &url, &[], &body).unwrap();
            loop {
                let event = events.next(&mut sink).unwrap().unwrap();
                let event: Value = serde_json::from_str(&event.data).unwrap();
                if event["type"] == "response.completed" {
                    assert!(pending.handed.lock().unwrap().is_some());
                    break;
                }
            }
            drop(events);
            pending.finish(Vec::new());
            assert!(links().contains_key("handover"));
        }
        assert_eq!(handle.join().unwrap().len(), 32);
    }

    #[test]
    fn an_incomplete_response_does_not_seed_a_continuation() {
        let (url, handle) = server(|_| {
            vec![json!({"type":"response.incomplete",
            "response":{"id":"resp_partial","status":"incomplete"}})]
        });
        let (events, pending) = stream("incomplete", &url, &[], &body(json!([]))).unwrap();
        assert_eq!(drain(events).len(), 1);
        pending.finish(Vec::new());
        assert!(!links().contains_key("incomplete"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }

    #[test]
    fn a_changed_endpoint_starts_a_new_connection_with_full_input() {
        let (old_url, old_server) = server(|_| reply("old_response", "old"));
        let (new_url, new_server) = server(|_| reply("new_response", "new"));
        let first = body(json!([{"role":"user","content":"hello"}]));
        let (events, pending) = stream("destination", &old_url, &[], &first).unwrap();
        drain(events);
        pending.finish(Vec::new());
        let (events, pending) = stream("destination", &new_url, &[], &first).unwrap();
        drain(events);
        pending.finish(Vec::new());
        assert_eq!(old_server.join().unwrap().len(), 1);
        let received = new_server.join().unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["input"], first["input"]);
        assert!(received[0].get("previous_response_id").is_none());
    }

    fn accept_connection(listener: &TcpListener) -> TcpStream {
        let start = Instant::now();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    assert!(
                        start.elapsed() < Duration::from_secs(5),
                        "expected a fresh connection"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        }
    }

    #[test]
    fn changed_credentials_do_not_reuse_an_authenticated_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("ws://{}/responses", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let mut sockets = Vec::new();
            let mut received = Vec::new();
            for _ in 0..2 {
                let mut socket = accept(accept_connection(&listener)).unwrap();
                let Message::Text(frame) = socket.read().unwrap() else {
                    panic!("expected response.create");
                };
                received.push(serde_json::from_str::<Value>(frame.as_str()).unwrap());
                for event in reply("resp_1", "one") {
                    socket
                        .send(Message::Text(event.to_string().into()))
                        .unwrap();
                }
                // Keep both server sockets alive: a reconnect must be caused by the changed
                // identity, not a server-side close that would repair a wrong reuse by accident.
                sockets.push(socket);
            }
            received
        });
        let first = body(json!([{"role":"user","content":"hello"}]));
        for token in ["first-test-token", "second-test-token"] {
            let headers = [("authorization".into(), format!("Bearer {token}"))];
            let (events, pending) = stream("credentials", &url, &headers, &first).unwrap();
            assert_eq!(drain(events).len(), 2);
            pending.finish(Vec::new());
        }
        let received = handle.join().unwrap();
        assert_eq!(received.len(), 2);
        assert!(received[1].get("previous_response_id").is_none());
        assert_eq!(received[1]["input"], first["input"]);
    }

    #[test]
    fn a_missing_continuation_after_output_is_not_silently_resent() {
        let (url, handle) = server(|_| {
            vec![
                json!({"type":"response.output_text.delta","delta":"partial"}),
                json!({"type":"error","code":"previous_response_not_found"}),
            ]
        });
        let (events, pending) = stream("partial", &url, &[], &body(json!([]))).unwrap();
        assert_eq!(drain(events).len(), 2);
        pending.finish(Vec::new());
        assert!(!links().contains_key("partial"));
        assert_eq!(handle.join().unwrap().len(), 1);
    }
}
