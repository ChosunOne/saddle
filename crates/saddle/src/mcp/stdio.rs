//! D107: the stdio transport. A declared command is started under the same sandbox a `bash`
//! command gets (D106), and spoken to with JSON-RPC over its stdin and stdout. The command's stderr
//! is drained into a bounded tail, never mixed into the protocol, and quoted only when something
//! failed: "exited with status 1" without the reason is the message that costs an hour.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use crate::mcp::protocol::{self, Message, Reply};

/// How long one reply may take before the server is treated as broken. A handshake that silently
/// never arrives is a stall rather than an error (D106), so this is not optional.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a waiting caller is asked to service the network relay, so a server that opens a
/// connection while it starts is answered rather than left hanging.
const POLL: Duration = Duration::from_millis(20);

/// How much of the server's stderr is kept for the error message.
const STDERR_TAIL: usize = 4096;

/// One tool a server offers, as it describes itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

/// One running server.
pub struct Client {
    name: String,
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Message>,
    stderr: Arc<Mutex<Vec<u8>>>,
    next: u64,
    timeout: Duration,
    /// What `roots/list` is answered with, handed in because this client has no session (D114).
    roots: Vec<Value>,
    /// The server said its tool list changed, so the catalogue it gave earlier is not the truth.
    stale: bool,
}

impl Client {
    /// Starts a server under the sandbox spec its grant was lowered to (D105, D106).
    pub fn start(
        name: &str,
        helper: &Path,
        spec: &saddle_cap::sandbox::SandboxSpec,
        timeout: Duration,
        roots: Vec<Value>,
    ) -> Result<Self> {
        // The helper is this binary, re-executed: without its argument it would run the CLI
        // instead of applying the sandbox.
        let child = saddle_sandbox::spawn(helper, &[crate::tools::HELPER_ARG], spec)
            .with_context(|| format!("starting the `{name}` server"))?;
        let mut client = Self::from_child(name, child)?;
        client.timeout = timeout;
        client.roots = roots;
        Ok(client)
    }

    /// A client over any child with piped stdio, sandboxed or not. The tests use this so the
    /// protocol is checked without needing landlock.
    pub fn from_child(name: &str, mut child: Child) -> Result<Self> {
        let stdin = child.stdin.take().context("the server has no stdin")?;
        let stdout = child.stdout.take().context("the server has no stdout")?;
        let stderr = child.stderr.take().context("the server has no stderr")?;
        let (tx, messages) = channel();
        // The pipes are read on their own thread so the caller can service the relay while it
        // waits, and so a server that writes a lot of stderr cannot fill a pipe and deadlock.
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // Everything the server says goes to the waiter, including the requests it makes
                // of us: a request answered only by someone who happened to be reading would be
                // answered by nobody (D114).
                if let Some(parsed) = protocol::message(&line)
                    && tx.send(parsed).is_err()
                {
                    break;
                }
            }
        });
        let tail = Arc::new(Mutex::new(Vec::new()));
        let sink = tail.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            loop {
                let mut buf = Vec::new();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut held) = sink.lock() {
                            held.extend_from_slice(&buf);
                            if held.len() > STDERR_TAIL {
                                let cut = held.len() - STDERR_TAIL;
                                held.drain(..cut);
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            name: name.to_owned(),
            child,
            stdin,
            messages,
            stderr: tail,
            next: 0,
            timeout: REPLY_TIMEOUT,
            roots: Vec::new(),
            stale: false,
        })
    }

    /// Whether the server has said its catalogue changed since it was read (D114).
    pub fn stale(&self) -> bool {
        self.stale
    }

    /// Forgets a stale catalogue once it has been read again.
    pub fn refreshed(&mut self) {
        self.stale = false;
    }

    /// The handshake, and the notification that says this client is ready. `service` runs whenever
    /// the caller is waiting, which is when the server may be asking the relay for a connection.
    pub fn initialize(&mut self, service: &mut dyn FnMut()) -> Result<Value> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "saddle", "version": env!("CARGO_PKG_VERSION")},
            }),
            service,
        )?;
        self.send(&protocol::notification(
            "notifications/initialized",
            json!({}),
        ))?;
        Ok(result)
    }

    /// What the server says it offers. Nothing here decides what the model may call: that is the
    /// grant (D103).
    pub fn tools(&mut self, service: &mut dyn FnMut()) -> Result<Vec<Tool>> {
        let result = self.request("tools/list", json!({}), service)?;
        let list = result
            .get("tools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(list
            .into_iter()
            .filter_map(|tool| {
                Some(Tool {
                    name: tool.get("name")?.as_str()?.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    schema: tool.get("inputSchema").cloned().unwrap_or(json!({})),
                })
            })
            .collect())
    }

    /// Calls one tool. `Ok(false, _)` means the call failed on the server's own terms (`isError`),
    /// which is the model's problem rather than the host's.
    pub fn call(
        &mut self,
        tool: &str,
        arguments: &Value,
        service: &mut dyn FnMut(),
    ) -> Result<(bool, String)> {
        let result = self.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            service,
        )?;
        let failed = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok((!failed, content_text(&result)))
    }

    fn send(&mut self, line: &str) -> Result<()> {
        writeln!(self.stdin, "{line}")
            .and_then(|_| self.stdin.flush())
            .with_context(|| format!("writing to the `{}` server", self.name))
    }

    fn request(&mut self, method: &str, params: Value, service: &mut dyn FnMut()) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        self.send(&protocol::request(id, method, params.clone()))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            match self.messages.recv_timeout(POLL) {
                Ok(Message::Reply(Reply {
                    id: answered,
                    result,
                })) if answered == id => {
                    return result.map_err(|why| anyhow!("`{}`: {why}", self.name));
                }
                // A request of the server's own: answered here, because this is the only place
                // holding the pipe it must go back down (D114). The session is out of reach in a
                // `service` closure, so the answer comes from what the client was given.
                Ok(Message::Request { id, method, .. }) => {
                    let reply = crate::mcp::host::answer(&self.roots, &method);
                    self.send(&protocol::response(id, reply))?;
                    continue;
                }
                Ok(Message::Notification { method }) => {
                    // A server that changed its tool list mid-session: noticed, and re-read when
                    // the next search runs rather than trusting a stale catalogue (D114).
                    if method == "notifications/tools/list_changed" {
                        self.stale = true;
                    }
                    continue;
                }
                // A reply to something else: keep waiting for ours.
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => {
                    service();
                    if Instant::now() >= deadline {
                        let (name, secs) = (self.name.clone(), self.timeout.as_secs());
                        bail!(
                            "`{name}` did not answer `{method}` within {secs}s{}",
                            self.why()
                        );
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let name = self.name.clone();
                    bail!(
                        "`{name}` closed its output while answering `{method}`{}",
                        self.why()
                    )
                }
            }
        }
    }

    /// What the server said before it stopped, for an error message that explains itself.
    fn why(&mut self) -> String {
        let tail = self
            .stderr
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_owned())
            .unwrap_or_default();
        let status = match self.child.try_wait() {
            Ok(Some(status)) => format!(", exited {status}"),
            _ => String::new(),
        };
        if tail.is_empty() {
            status
        } else {
            format!("{status}: {tail}")
        }
    }

    /// Ends the server and everything it started. The child has its own process group (D106), so a
    /// negative pid reaches the group rather than only the process that speaks the protocol.
    pub fn close(&mut self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: `pid` is the group leader this client started; the call has no other effect.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        // The group kill does not reach a child that was started without its own group (the test
        // path), and `wait` on a process still holding its stdin would block forever.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.close();
    }
}

/// The text of a `CallToolResult`: its text blocks joined, and anything else named rather than
/// dropped, so an image or a resource link is visible instead of silently missing (D108).
fn content_text(result: &Value) -> String {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        match kind {
            "text" => out.push(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            "image" => out.push(format!(
                "[an image, {}]",
                block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type")
            )),
            "resource_link" => out.push(format!(
                "[a link to {}]",
                block
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("an unnamed resource")
            )),
            other => out.push(format!("[{other}]")),
        }
    }
    out.join("\n")
}

/// A command with the pipes a client needs, for a caller that is not using the sandbox (tests).
#[cfg(test)]
fn piped(mut command: std::process::Command) -> Result<Child> {
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("starting the fake server")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that answers the two methods a session needs, in the order they are asked, so the
    /// transport is checked without a sandbox or a real MCP implementation.
    const FAKE: &str = r#"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}}}}\n' ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"finds things","inputSchema":{"type":"object"}}]}}\n' ;;
    *'"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"found it"}],"isError":false}}\n' ;;
    *)
      printf '{"jsonrpc":"2.0","id":9,"error":{"code":-32601,"message":"no such method"}}\n' ;;
  esac
done
"#;

    fn fake() -> Client {
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(FAKE);
        Client::from_child("fake", piped(command).unwrap()).unwrap()
    }

    #[test]
    fn a_server_is_handshaken_and_listed_over_its_own_pipes() {
        let mut client = fake();
        let mut service = || {};
        let hello = client.initialize(&mut service).unwrap();
        assert_eq!(hello["protocolVersion"], "2025-06-18");
        let tools = client.tools(&mut service).unwrap();
        assert_eq!(
            tools,
            [Tool {
                name: "search".to_owned(),
                description: "finds things".to_owned(),
                schema: json!({"type": "object"}),
            }]
        );
    }

    #[test]
    fn a_server_that_dies_is_reported_with_what_it_said() {
        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("echo 'no database for you' >&2; exit 3");
        let mut client = Client::from_child("broken", piped(command).unwrap()).unwrap();
        let err = format!("{:#}", client.initialize(&mut || {}).unwrap_err());
        assert!(err.contains("broken"), "{err}");
        assert!(err.contains("no database for you"), "{err}");
    }
}
