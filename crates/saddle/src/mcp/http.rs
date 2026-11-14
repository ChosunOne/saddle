//! D110: the streamable HTTP transport. An HTTP server has no child process, so the host makes each
//! request itself — but every one of them is decided against the *server's* narrowed reach (D105)
//! before it is sent, and audited with the server named. That is the same authority the child's
//! sandbox is derived from, asked at the one moment a request is made rather than once at exec.
//!
//! A reply arrives either as one JSON object or as an event stream, so both are read here and
//! neither is a protocol the rest of the crate knows about (D107's framing, on a socket instead of
//! a pipe). A 401 is not retried: without OAuth there is nothing to retry with, and a loop of
//! refusals would spend the person's quota to learn the same thing twice.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use saddle_policy::Request;

use crate::mcp::protocol::{Reply, notification, reply, request};
use crate::mcp::stdio::Tool;
use crate::session::Session;
use crate::tools::fetch::{Target, agent, authority, resolve_public};

/// What the server said about one call, once it answered.
pub enum Outcome {
    Answered(String),
    Failed(String),
    Denied(String),
}

/// The protocol this client speaks, and the one header the spec requires on every request after
/// the handshake.
const PROTOCOL: &str = "2025-06-18";

/// How much of a body is read: a tool result is text, and a server that streams forever must not be
/// able to fill the process.
const MAX_BODY: u64 = 4 * 1024 * 1024;

/// One remote server.
pub struct Client {
    server: String,
    target: Target,
    /// The server's own net grants, narrowed from the agent's (D105).
    reach: Vec<saddle_cap::policy::NetGrant>,
    session_id: Option<String>,
    next: u64,
}

impl Client {
    pub fn start(
        server: &str,
        url: &str,
        reach: Vec<saddle_cap::policy::NetGrant>,
    ) -> Result<Self> {
        let target = Target::parse(url).map_err(|why| anyhow!("{url}: {why}"))?;
        Ok(Self {
            server: server.to_owned(),
            target,
            reach,
            session_id: None,
            next: 0,
        })
    }

    pub fn initialize(&mut self, session: &mut Session) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL,
            "capabilities": {},
            "clientInfo": {
                "name": "saddle",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let result = self.request(session, "initialize", params)?;
        // The spec has the client confirm before it asks for anything else; a server that is
        // strict about it will not answer a second request until this arrives.
        self.send(
            session,
            &notification("notifications/initialized", json!({})),
        )?;
        if let Some(version) = result.get("protocolVersion").and_then(Value::as_str)
            && version != PROTOCOL
        {
            // Not fatal: the spec has the client accept what the server picked, and every method
            // used here exists in both.
            let _ = version;
        }
        Ok(())
    }

    /// Every tool the server lists, following `nextCursor` so a paginated catalogue is complete.
    pub fn tools(&mut self, session: &mut Session) -> Result<Vec<Tool>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        // A server that keeps handing back a cursor could otherwise make this loop forever.
        for _ in 0..32 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.request(session, "tools/list", params)?;
            let listed = result
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for tool in listed {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                out.push(Tool {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                });
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    pub fn call(
        &mut self,
        session: &mut Session,
        tool: &str,
        arguments: &Value,
    ) -> Result<Outcome> {
        let params = json!({ "name": tool, "arguments": arguments });
        let result = match self.request(session, "tools/call", params) {
            Ok(result) => result,
            Err(e) => {
                // A denial is the model's to see as a denial, not the host's to report as a fault.
                if let Some(denied) = e.downcast_ref::<Denied>() {
                    return Ok(Outcome::Denied(denied.0.clone()));
                }
                return Err(e);
            }
        };
        let failed = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = content_text(&result);
        Ok(if failed {
            Outcome::Failed(text)
        } else {
            Outcome::Answered(text)
        })
    }

    /// One request and its reply, on a deadline, decided before it is sent.
    fn request(&mut self, session: &mut Session, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        let id = self.next;
        let payload = request(id, method, params);
        let reply = self.exchange(session, &payload)?;
        let Some(reply) = reply else {
            bail!("`{}` answered `{method}` with nothing", self.server)
        };
        if reply.id != id {
            bail!(
                "`{}` answered `{method}` with a reply to request {}",
                self.server,
                reply.id
            );
        }
        reply.result.map_err(|why| anyhow!("{why}"))
    }

    /// Sends one JSON-RPC message and returns the reply that carries its id, whether it arrived as
    /// a JSON body or as one event of a stream.
    fn exchange(&mut self, session: &mut Session, payload: &str) -> Result<Option<Reply>> {
        let (status, content_type, session_id, body) = self.post(session, payload)?;
        if let Some(id) = session_id {
            self.session_id = Some(id);
        }
        if status == 401 || status == 403 {
            bail!(
                "`{}` answered {status}: it wants an OAuth token, which this build cannot yet \
                 obtain (`saddle mcp auth {}` is not built)",
                self.server,
                self.server
            );
        }
        if !(200..300).contains(&status) {
            let head: String = body.chars().take(400).collect();
            bail!("`{}` answered {status}: {head}", self.server);
        }
        // A notification gets no reply: 202 with no body is the spec's answer to one.
        if body.trim().is_empty() {
            return Ok(None);
        }
        if content_type.contains("text/event-stream") {
            for line in body.lines() {
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                if let Some(reply) = reply(data.trim()) {
                    return Ok(Some(reply));
                }
            }
            bail!("`{}` sent an event stream with no reply in it", self.server);
        }
        Ok(reply(&body))
    }

    /// One POST, decided and then sent. The headers the decision is shown are the ones that go out,
    /// so a name in the record is a name on the wire (D48, D110).
    fn post(
        &mut self,
        session: &mut Session,
        payload: &str,
    ) -> Result<(u16, String, Option<String>, String)> {
        let mut headers: Vec<(&str, String)> = vec![
            ("content-type", "application/json".to_owned()),
            ("accept", "application/json, text/event-stream".to_owned()),
            ("mcp-protocol-version", PROTOCOL.to_owned()),
        ];
        if let Some(id) = &self.session_id {
            headers.push(("mcp-session-id", id.clone()));
        }
        let names: Vec<String> = headers.iter().map(|(name, _)| (*name).to_owned()).collect();
        let request = Request::Net {
            method: "POST",
            scheme: &self.target.scheme,
            host: &self.target.host,
            port: self.target.port,
            path: self.target.request_path(),
            headers: &names,
        };
        // A private address is reachable only when the server's own grant spells it out (D40).
        let named = Session::mcp_names_host(&self.reach, &self.target.host);
        let decision = session.decide_mcp_net(&self.server, &self.reach, &request)?;
        if !decision.allow {
            return Err(Denied(decision.explanation).into());
        }
        if !named && let Err(why) = resolve_public(&self.target.host, self.target.port) {
            return Err(Denied(why).into());
        }
        let url = format!(
            "{}://{}{}",
            self.target.scheme,
            authority(
                &self.target.host,
                self.target.port,
                self.target.scheme == "https"
            ),
            self.target.request_path()
        );
        let agent = agent(named);
        let mut outgoing = agent.post(&url);
        for (name, value) in &headers {
            outgoing = outgoing.header(*name, value.as_str());
        }
        let mut response = match outgoing.send(payload) {
            Ok(response) => response,
            Err(e) => return Err(anyhow!("{url}: {e}")),
        };
        let status = response.status().as_u16();
        let header = |response: &ureq::http::Response<ureq::Body>, name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let content_type = header(&response, "content-type").unwrap_or_default();
        let session_id = header(&response, "mcp-session-id");
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_string()
            .context("reading the server's reply")?;
        Ok((status, content_type, session_id, body))
    }

    fn send(&mut self, session: &mut Session, payload: &str) -> Result<()> {
        self.exchange(session, payload)?;
        Ok(())
    }
}

/// A refusal the model should read as one.
#[derive(Debug)]
pub struct Denied(pub String);

impl std::fmt::Display for Denied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Denied {}

/// The text of a `CallToolResult`, with anything that is not text named rather than dropped.
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
