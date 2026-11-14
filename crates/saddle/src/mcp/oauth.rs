//! D115: OAuth for a remote MCP server, discovered rather than configured. A server that answers
//! 401 says where its authorization server is (RFC 9728), that server says where to authorize, to
//! exchange and to register, and saddle does the rest: register itself, generate a PKCE verifier,
//! print a URL, catch the loopback redirect and store the tokens beside the provider credentials —
//! outside every root, in `auth.json`, where no command can read them (D16, D42).
//!
//! Every request here is decided like any other egress. The hosts are named by a document the
//! server supplied, so they are exactly the kind of address a grant has to allow rather than a
//! harness constant; the label in the record says it was authorization and for which server.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use crate::auth::AuthStore;
use crate::mcp::http::{Answer, send};
use crate::provider::oauth::Tokens;
use crate::provider::oauth::pkce;
use crate::session::Session;
use crate::tools::fetch::Target;

/// How long the person has to finish in the browser. Long, because a sign-in is a person's time,
/// but finite, because a terminal waiting forever is worse than one that says it gave up.
const WAIT: Duration = Duration::from_secs(300);

/// The authorization server one MCP server points at.
pub struct AuthServer {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub register: Option<String>,
}

/// What `saddle mcp auth` needs from the session: a way to make decided requests.
pub struct Flow<'a> {
    pub session: &'a mut Session,
    /// The net grants these requests are decided against, and the label they are recorded under.
    pub reach: Vec<saddle_cap::policy::NetGrant>,
    pub label: String,
    pub timeout: Duration,
}

impl Flow<'_> {
    fn get(&mut self, url: &str) -> Result<Answer> {
        let target = Target::parse(url).map_err(|why| anyhow!("{url}: {why}"))?;
        send(
            self.session,
            &self.label,
            &self.reach,
            &target,
            "GET",
            &[("accept", "application/json".to_owned())],
            None,
            self.timeout,
        )
    }

    fn post(&mut self, url: &str, body: &str) -> Result<Answer> {
        let target = Target::parse(url).map_err(|why| anyhow!("{url}: {why}"))?;
        send(
            self.session,
            &self.label,
            &self.reach,
            &target,
            "POST",
            &[
                ("content-type", "application/json".to_owned()),
                ("accept", "application/json".to_owned()),
            ],
            Some(body),
            self.timeout,
        )
    }

    fn post_form(&mut self, url: &str, body: &str) -> Result<Answer> {
        let target = Target::parse(url).map_err(|why| anyhow!("{url}: {why}"))?;
        send(
            self.session,
            &self.label,
            &self.reach,
            &target,
            "POST",
            &[
                (
                    "content-type",
                    "application/x-www-form-urlencoded".to_owned(),
                ),
                ("accept", "application/json".to_owned()),
            ],
            Some(body),
            self.timeout,
        )
    }
}

/// The authorization server a server's URL points at, by the two metadata documents the spec
/// defines. A server that publishes nothing but is itself the issuer is the common case, so the
/// origin is tried as the issuer when the resource metadata is absent.
pub fn discover(flow: &mut Flow<'_>, url: &str) -> Result<AuthServer> {
    let target = Target::parse(url).map_err(|why| anyhow!("{url}: {why}"))?;
    let origin = format!("{}://{}", target.scheme, authority(&target));
    let mut issuer = origin.clone();
    let metadata = format!("{origin}/.well-known/oauth-protected-resource");
    match flow.get(&metadata) {
        Ok(answer) if answer.status == 200 => {
            let value: Value = serde_json::from_str(&answer.body)
                .with_context(|| format!("{metadata} is not JSON"))?;
            if let Some(first) = value
                .get("authorization_servers")
                .and_then(Value::as_array)
                .and_then(|list| list.first())
                .and_then(Value::as_str)
            {
                issuer = first.trim_end_matches('/').to_owned();
            }
        }
        // No protected-resource document: the server is its own issuer.
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    let as_metadata = format!("{issuer}/.well-known/oauth-authorization-server");
    let answer = flow.get(&as_metadata)?;
    if answer.status != 200 {
        bail!(
            "{issuer} publishes no authorization-server metadata ({})",
            answer.status
        );
    }
    let value: Value =
        serde_json::from_str(&answer.body).with_context(|| format!("{as_metadata} is not JSON"))?;
    let field = |name: &str| {
        value[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .with_context(|| format!("{as_metadata} has no `{name}`"))
    };
    Ok(AuthServer {
        issuer,
        authorize: field("authorization_endpoint")?,
        token: field("token_endpoint")?,
        register: value["registration_endpoint"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    })
}

/// Registers saddle as a client of the server's authorization server, which is what lets there be
/// no client id in the config for a server the person has never set up before.
pub fn register(flow: &mut Flow<'_>, auth: &AuthServer, redirect: &str) -> Result<String> {
    let endpoint = auth.register.as_deref().with_context(|| {
        format!(
            "{} takes no client registration, so a client id has to be configured; \
             this build does not read one yet",
            auth.issuer
        )
    })?;
    let body = serde_json::json!({
        "client_name": "saddle",
        "redirect_uris": [redirect],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    })
    .to_string();
    let answer = flow.post(endpoint, &body)?;
    let value: Value = serde_json::from_str(&answer.body)
        .with_context(|| format!("{endpoint} did not answer with JSON"))?;
    if answer.status != 200 && answer.status != 201 {
        bail!(
            "{endpoint} refused the registration ({}): {}",
            answer.status,
            detail(&answer.body)
        );
    }
    value["client_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .with_context(|| format!("{endpoint} registered a client with no client_id"))
}

/// The whole sign-in: discover, register, send the person to the browser, catch the callback, and
/// store what comes back.
pub fn sign_in(
    flow: &mut Flow<'_>,
    server: &str,
    url: &str,
    store_path: &std::path::Path,
) -> Result<String> {
    let auth = discover(flow, url)?;
    // A loopback listener on any port: the registration names the redirect, so it need not be
    // fixed, and a fixed port is the one thing most likely to be taken.
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("reserving a loopback port")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let client_id = register(flow, &auth, &redirect)?;
    let (verifier, challenge) = pkce();
    let state = crate::provider::oauth::random_b64(16);
    let mut authorize = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}\
         &code_challenge_method=S256",
        auth.authorize,
        encode(&client_id),
        encode(&redirect),
        encode(&state),
        encode(&challenge)
    );
    authorize.push_str(&format!("&resource={}", encode(url)));
    println!("open this in a browser:");
    println!("  {authorize}");
    println!("waiting for the callback on {redirect}");
    let code = wait_for_callback(listener, &state)?;
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        encode(&code),
        encode(&redirect),
        encode(&client_id),
        encode(&verifier)
    );
    let answer = flow.post_form(&auth.token, &body)?;
    let tokens = tokens_from(&answer, &auth.token)?;
    let mut store = AuthStore::load(store_path)?;
    store.set_oauth(&format!("mcp:{server}"), tokens.clone());
    store.save(store_path)?;
    Ok(format!("{server}: signed in via {}", auth.issuer))
}

/// A refresh, for the transport to call when a request comes back 401 (D116).
pub fn refresh(
    flow: &mut Flow<'_>,
    server: &str,
    url: &str,
    tokens: &Tokens,
    store_path: &std::path::Path,
) -> Result<Tokens> {
    let client = tokens
        .client_id
        .clone()
        .context("the stored token has no client id, so it cannot be refreshed")?;
    let auth = discover(flow, url)?;
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        encode(&tokens.refresh),
        encode(&client)
    );
    let answer = flow.post_form(&auth.token, &body)?;
    let fresh = tokens_from(&answer, &auth.token)?;
    let mut store = AuthStore::load(store_path)?;
    store.set_oauth(&format!("mcp:{server}"), fresh.clone());
    store.save(store_path)?;
    Ok(fresh)
}

/// The token answer, as the store keeps it.
fn tokens_from(answer: &Answer, endpoint: &str) -> Result<Tokens> {
    let value: Value = serde_json::from_str(&answer.body)
        .with_context(|| format!("{endpoint} did not answer with JSON"))?;
    if answer.status != 200 {
        bail!(
            "{endpoint} refused the token request ({}): {}",
            answer.status,
            detail(&answer.body)
        );
    }
    let field = |name: &str| {
        value[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .with_context(|| format!("{endpoint} answered with no `{name}`"))
    };
    let lifetime = value["expires_in"].as_u64().unwrap_or(0);
    Ok(Tokens {
        access: field("access_token")?,
        refresh: value["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        expires_at: if lifetime == 0 {
            0
        } else {
            crate::provider::oauth::now() + lifetime
        },
        client_id: value["client_id"].as_str().map(str::to_owned),
        account_id: None,
    })
}

/// One request on the loopback listener, answered with a page the person can read, and read for
/// the code the browser carried back.
fn wait_for_callback(listener: TcpListener, state: &str) -> Result<String> {
    listener.set_nonblocking(false)?;
    let (mut stream, _) = listener.accept().context("waiting for the browser")?;
    stream.set_read_timeout(Some(WAIT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let target = request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let parsed = callback(&target, state);
    let body = match &parsed {
        Ok(_) => "<html><body><p>signed in — you can close this tab.</p></body></html>",
        Err(_) => "<html><body><p>that did not work; see the terminal.</p></body></html>",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
    parsed
}

/// The `code` out of a callback path, refusing one whose `state` is not the one we sent: a code
/// that arrives without it is a code someone else put in the person's browser.
fn callback(target: &str, state: &str) -> Result<String> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut code = None;
    let mut seen = None;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        match name {
            "code" => code = Some(decode(value)),
            "state" => seen = Some(decode(value)),
            _ => {}
        }
    }
    if seen.as_deref() != Some(state) {
        bail!("the callback did not carry the state this sign-in sent");
    }
    code.filter(|c| !c.is_empty())
        .context("the callback carried no code")
}

fn authority(target: &Target) -> String {
    let host = if target.host.contains(':') {
        format!("[{}]", target.host)
    } else {
        target.host.clone()
    };
    let default = if target.scheme == "https" { 443 } else { 80 };
    if target.port == default {
        host
    } else {
        format!("{host}:{}", target.port)
    }
}

/// Percent-encoding, for the query and the form body. Everything but the unreserved characters,
/// which is the conservative reading and what every authorization server expects.
fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' if at + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap_or_default();
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        at += 3;
                    }
                    Err(_) => {
                        out.push(bytes[at]);
                        at += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            other => {
                out.push(other);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A bounded excerpt of what a server said, for an error the person has to act on.
fn detail(body: &str) -> String {
    let text = body.trim();
    if text.len() > 300 {
        format!("{}…", &text[..300])
    } else if text.is_empty() {
        "(nothing)".to_owned()
    } else {
        text.to_owned()
    }
}
