//! D110: the streamable HTTP transport. An HTTP server has no child process, so the host makes each
//! request itself — but every one of them is decided against the *server's* narrowed reach (D105)
//! before it is sent, and audited with the server named. That is the same authority the child's
//! sandbox is derived from, asked at the one moment a request is made rather than once at exec.
//!
//! A reply arrives either as one JSON object or as an event stream, so both are read here and
//! neither is a protocol the rest of the crate knows about (D107's framing, on a socket instead of
//! a pipe). A 401 is not retried: without OAuth there is nothing to retry with, and a loop of
//! refusals would spend the person's quota to learn the same thing twice.

use crate::auth::AuthStore;
use crate::mcp::oauth;
use crate::tools::fetch;
use saddle_cap::policy::NetGrant;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::time::Duration;
use ureq::http::Response;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use saddle_policy::Request;

use crate::mcp::protocol::{Reply, notification, reply, request};
use crate::mcp::stdio::Tool;
use crate::session::Session;
use crate::tools::fetch::{Target, authority, resolve_public};

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
    /// The URL as the table spells it, for the discovery a refresh has to do (D116).
    url: String,
    target: Target,
    /// The server's own net grants, narrowed from the agent's (D105).
    reach: Vec<NetGrant>,
    session_id: Option<String>,
    next: u64,
    /// The entry's `timeout`: the deadline every request of this server gets (D113).
    timeout: Duration,
}

impl Client {
    pub fn start(server: &str, url: &str, reach: Vec<NetGrant>, timeout: Duration) -> Result<Self> {
        let target = Target::parse(url).map_err(|why| anyhow!("{url}: {why}"))?;
        Ok(Self {
            server: server.to_owned(),
            url: url.to_owned(),
            target,
            reach,
            session_id: None,
            next: 0,
            timeout,
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
                "`{}` answered {status} and no usable token is stored: run `saddle mcp auth {}` to \
                 sign in",
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

    /// One POST, decided and then sent.
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
        // A token the person stored is sent; there is no path here that asks for one by itself,
        // because obtaining one is a person's act at a terminal (D115).
        let store_path = session.setup.paths.auth_store();
        let held = AuthStore::load(&store_path)
            .ok()
            .and_then(|store| store.oauth(&format!("mcp:{}", self.server)).cloned());
        if let Some(tokens) = &held {
            headers.push(("authorization", format!("Bearer {}", tokens.access)));
        }
        let mut answer = self.attempt(session, &headers, payload)?;
        let refused = answer.status == 401 || answer.status == 403;
        // One refresh and one retry: a loop here would spend the person's account on a server that
        // will never accept it (D116). A token with no refresh is left alone — the error below
        // names `saddle mcp auth`, which is the only thing that can fix it.
        if refused && let Some(held) = held.clone().filter(|t| !t.refresh.is_empty()) {
            let mut flow = crate::mcp::oauth::Flow {
                session,
                reach: self.reach.clone(),
                label: format!("oauth:{}", self.server),
                timeout: self.timeout,
            };
            let fresh = oauth::refresh(&mut flow, &self.server, &self.url, &held, &store_path)
                .with_context(|| {
                    format!(
                        "refreshing the token for `{}`; `saddle mcp auth {}` signs in again",
                        self.server, self.server
                    )
                })?;
            headers.retain(|(name, _)| *name != "authorization");
            headers.push(("authorization", format!("Bearer {}", fresh.access)));
            answer = self.attempt(session, &headers, payload)?;
        }
        Ok((
            answer.status,
            answer.content_type,
            answer.session_id,
            answer.body,
        ))
    }

    /// One decided POST with these headers, so a retry after a refresh sends the new token through
    /// exactly the same gate as the first attempt (D116).
    fn attempt(
        &self,
        session: &mut Session,
        headers: &[(&str, String)],
        payload: &str,
    ) -> Result<Answer> {
        send(
            session,
            &self.server,
            &self.reach,
            &self.target,
            "POST",
            headers,
            Some(payload),
            self.timeout,
        )
    }

    fn send(&mut self, session: &mut Session, payload: &str) -> Result<()> {
        self.exchange(session, payload)?;
        Ok(())
    }
}

/// A refusal the model should read as one.
#[derive(Debug)]
pub struct Denied(pub String);

impl Display for Denied {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(&self.0)
    }
}

impl Error for Denied {}

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

/// One request the host makes for a server, decided against that server's reach before it is sent
/// (D110) and recorded whatever the answer. It is shared with OAuth discovery, because a metadata
/// document fetched from a URL a server named is exactly as much of an egress as a tool call is
/// (D115).
#[allow(clippy::too_many_arguments)]
pub(crate) fn send(
    session: &mut Session,
    label: &str,
    reach: &[NetGrant],
    target: &Target,
    method: &str,
    headers: &[(&str, String)],
    body: Option<&str>,
    timeout: Duration,
) -> Result<Answer> {
    let names: Vec<String> = headers.iter().map(|(name, _)| (*name).to_owned()).collect();
    let request = Request::Net {
        method,
        scheme: &target.scheme,
        host: &target.host,
        port: target.port,
        path: target.request_path(),
        headers: &names,
    };
    // A private address is reachable only when the reach spells it out (D40).
    let named = Session::mcp_names_host(reach, &target.host);
    let decision = session.decide_mcp_net(label, reach, &request)?;
    if !decision.allow {
        return Err(Denied(decision.explanation).into());
    }
    if !named && let Err(why) = resolve_public(&target.host, target.port) {
        return Err(Denied(why).into());
    }
    let url = format!(
        "{}://{}{}",
        target.scheme,
        authority(&target.host, target.port, target.scheme == "https"),
        target.request_path()
    );
    let agent = fetch::agent_for(named, timeout);
    // ureq types a request by whether it carries a body, so the two are built apart.
    let outcome = if method == "GET" {
        let mut request = agent.get(&url);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        request.call()
    } else if method == "POST" {
        let mut request = agent.post(&url);
        for (name, value) in headers {
            request = request.header(*name, value.as_str());
        }
        request.send(body.unwrap_or_default())
    } else {
        return Err(anyhow!("{method} is not a method this client makes"));
    };
    let mut response = outcome.map_err(|e| anyhow!("{url}: {e}"))?;
    let header = |response: &Response<ureq::Body>, name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let status = response.status().as_u16();
    let content_type = header(&response, "content-type").unwrap_or_default();
    let session_id = header(&response, "mcp-session-id");
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .context("reading the server's reply")?;
    Ok(Answer {
        status,
        content_type,
        session_id,
        body,
    })
}

/// What one request came back with.
pub(crate) struct Answer {
    pub status: u16,
    pub content_type: String,
    pub session_id: Option<String>,
    pub body: String,
}
