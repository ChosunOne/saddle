use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::str;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::tools::timeout;

pub const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const OPENAI_SCOPES: &str = "openid profile email offline_access";
pub const OPENAI_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
pub const OPENAI_DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
pub const OPENAI_REDIRECT_PORT: u16 = 1455;
pub const OPENAI_REDIRECT_PATH: &str = "/auth/callback";
pub const OPENAI_DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const OPENAI_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const OPENAI_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
pub const OPENAI_API_BASE: &str = "https://chatgpt.com/backend-api/codex";
const DEVICE_TIMEOUT: u64 = 900;
const ACCOUNT_CLAIM: &str = "https://api.openai.com/auth";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Flow {
    pub authorize_url: Option<String>,
    pub token_url: Option<String>,
    pub client_id: Option<String>,
    pub scopes: Option<String>,
    pub resource: Option<String>,
    pub redirect_port: Option<u16>,
    pub redirect_path: Option<String>,
    pub redirect_uri: Option<String>,
    pub device_redirect_uri: Option<String>,
    pub method: Option<Method>,
    pub device_code_url: Option<String>,
    pub device_token_url: Option<String>,
    pub verification_url: Option<String>,
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl Flow {
    pub fn missing(&self) -> Option<String> {
        let mut want = Vec::new();
        if self.authorize_url.is_none() {
            want.push("authorize-url");
        }
        if self.token_url.is_none() {
            want.push("token-url");
        }
        if self.client_id.is_none() {
            want.push("client-id");
        }
        if self.method == Some(Method::Device) && self.device_redirect_uri.is_none() {
            want.push("device-redirect-uri");
        }
        (!want.is_empty()).then(|| {
            format!(
                "this provider's oauth table needs {} — add it under [providers.<name>.oauth] in \
                 config.toml",
                want.join(", ")
            )
        })
    }

    pub fn settings(&self) -> Result<Settings> {
        if let Some(why) = self.missing() {
            bail!("{why}");
        }
        Ok(Settings {
            authorize_url: self.authorize_url.clone().unwrap_or_default(),
            token_url: self.token_url.clone().unwrap_or_default(),
            client_id: self.client_id.clone().unwrap_or_default(),
            scopes: self.scopes.clone().unwrap_or_else(|| "openid".to_owned()),
            resource: self.resource.clone().filter(|r| !r.is_empty()),
            port: self.redirect_port.unwrap_or(OPENAI_REDIRECT_PORT),
            path: self
                .redirect_path
                .clone()
                .unwrap_or_else(|| OPENAI_REDIRECT_PATH.to_owned()),
            redirect_uri: self.redirect_uri.clone().unwrap_or_default(),
            device_redirect_uri: self.device_redirect_uri.clone(),
            method: self.method.unwrap_or_default(),
            device_code_url: self.device_code_url.clone(),
            device_token_url: self.device_token_url.clone(),
            verification_url: self.verification_url.clone(),
            extra: self.extra.clone(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Method {
    Device,
    #[default]
    Browser,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub scopes: String,
    pub resource: Option<String>,
    pub port: u16,
    pub path: String,
    pub redirect_uri: String,
    pub device_redirect_uri: Option<String>,
    pub method: Method,
    pub device_code_url: Option<String>,
    pub device_token_url: Option<String>,
    pub verification_url: Option<String>,
    pub extra: BTreeMap<String, String>,
}

impl Settings {
    pub fn redirect_uri(&self) -> String {
        if self.redirect_uri.is_empty() {
            format!("http://127.0.0.1:{}{}", self.port, self.path)
        } else {
            self.redirect_uri.clone()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct Tokens {
    pub access: String,
    pub refresh: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl Tokens {
    pub fn usable(&self, margin: u64, now: u64) -> bool {
        self.expires_at == 0 || self.expires_at > now.saturating_add(margin)
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn pkce() -> (String, String) {
    let verifier = random_b64(32);
    let challenge = B64.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

pub fn random_b64(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the system random source");
    B64.encode(buf)
}

pub fn uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the system random source");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub fn authorize_request(flow: &Settings, state: &str, nonce: &str, challenge: &str) -> String {
    let mut params: Vec<(String, String)> = vec![
        ("client_id".into(), flow.client_id.clone()),
        ("response_type".into(), "code".into()),
        ("redirect_uri".into(), flow.redirect_uri()),
        ("scope".into(), flow.scopes.clone()),
        ("state".into(), state.into()),
        ("code_challenge".into(), challenge.into()),
        ("code_challenge_method".into(), "S256".into()),
        ("nonce".into(), nonce.into()),
    ];
    if let Some(resource) = &flow.resource {
        params.push(("resource".into(), resource.clone()));
    }
    for (k, v) in &flow.extra {
        params.push((k.clone(), v.clone()));
    }
    let query = params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{}?{}", flow.authorize_url, query)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_auth_id: String,
    pub user_code: String,
    pub interval: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Poll {
    Pending,
    Ready { code: String, verifier: String },
    SlowDown,
}

pub fn request_device_code(flow: &Settings) -> Result<DeviceCode> {
    let url = flow
        .device_code_url
        .clone()
        .context("this provider's oauth table has no device-code-url")?;
    let body = serde_json::json!({ "client_id": flow.client_id }).to_string();
    let (status, text) = post_json(&url, &body)?;
    if status != 200 {
        bail!(
            "the device code request was refused ({status}): {}",
            detail(&text)
        );
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).context("the device code answer is not JSON")?;
    let field = |name: &str| {
        value[name]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .with_context(|| format!("the device code answer has no `{name}`"))
    };
    Ok(DeviceCode {
        device_auth_id: field("device_auth_id")?,
        user_code: field("user_code")?,
        interval: value["interval"]
            .as_u64()
            .or_else(|| value["interval"].as_str().and_then(|s| s.parse().ok()))
            .unwrap_or(5)
            .max(1),
    })
}

pub fn poll_device_code(flow: &Settings, device: &DeviceCode) -> Result<Poll> {
    let url = flow
        .device_token_url
        .clone()
        .context("this provider's oauth table has no device-token-url")?;
    let body = serde_json::json!({
        "device_auth_id": device.device_auth_id,
        "user_code": device.user_code,
    })
    .to_string();
    let (status, text) = post_json(&url, &body)?;
    if status == 200 {
        let value: serde_json::Value =
            serde_json::from_str(&text).context("the device token answer is not JSON")?;
        let code = value["authorization_code"].as_str().unwrap_or_default();
        let verifier = value["code_verifier"].as_str().unwrap_or_default();
        if code.is_empty() || verifier.is_empty() {
            bail!(
                "the device login answered without an authorization code: {}",
                detail(&text)
            );
        }
        return Ok(Poll::Ready {
            code: code.to_owned(),
            verifier: verifier.to_owned(),
        });
    }
    let reason = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v["error"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| v["error"]["code"].as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    if status == 403 || status == 404 || reason == "deviceauth_authorization_pending" {
        return Ok(Poll::Pending);
    }
    if reason == "slow_down" {
        return Ok(Poll::SlowDown);
    }
    bail!("the device login failed ({status}): {}", detail(&text));
}

pub fn login(
    flow: &Flow,
    agent_name: &str,
    tell: &mut dyn FnMut(&str),
    ask: &mut dyn FnMut(&str) -> Result<String>,
    pause: &mut dyn FnMut(u64),
) -> Result<Tokens> {
    let settings = flow.settings()?;
    match settings.method {
        Method::Device => device_login(flow, tell, pause),
        Method::Browser => browser_login(flow, agent_name, tell, ask),
    }
}

pub fn device_login_step(
    flow: &Settings,
    show: &mut dyn FnMut(&DeviceCode, &str),
    pause: &mut dyn FnMut(u64),
    stop: &AtomicBool,
) -> Result<Tokens> {
    let device = request_device_code(flow)?;
    let where_to_go = flow
        .verification_url
        .clone()
        .unwrap_or_else(|| OPENAI_VERIFICATION_URL.to_owned());
    show(&device, &where_to_go);
    let started = now();
    let mut interval = device.interval;
    loop {
        if stop.load(Ordering::Relaxed) {
            bail!("the login was cancelled");
        }
        match poll_device_code(flow, &device)? {
            Poll::Pending => {}
            Poll::SlowDown => interval += 5,
            Poll::Ready { code, verifier } => {
                return exchange_with(flow, &code, &verifier);
            }
        }
        if now().saturating_sub(started) > DEVICE_TIMEOUT {
            bail!("the login code expired before it was approved; run the login again");
        }
        pause(interval);
    }
}

pub fn device_login(
    flow: &Flow,
    tell: &mut dyn FnMut(&str),
    pause: &mut dyn FnMut(u64),
) -> Result<Tokens> {
    let settings = flow.settings()?;
    let mut show = |device: &DeviceCode, url: &str| {
        tell(&format!(
            "open {url} and enter the code {}",
            device.user_code
        ));
    };
    device_login_step(&settings, &mut show, pause, &AtomicBool::new(false))
}

impl Settings {
    pub fn callback_for_exchange(&self) -> String {
        match self.method {
            Method::Device => self
                .device_redirect_uri
                .clone()
                .unwrap_or_else(|| self.redirect_uri.clone()),
            Method::Browser => self.redirect_uri(),
        }
    }
}

pub fn describe(text: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return text.chars().take(300).collect::<String>();
    };
    let message = value["error"]["message"]
        .as_str()
        .or(value["error_description"].as_str())
        .or(value["error"].as_str())
        .or(value["message"].as_str());
    let code = value["error"]["code"]
        .as_str()
        .filter(|c| message != Some(*c));
    match (message, code) {
        (Some(m), Some(c)) => format!("{m} ({c})"),
        (Some(m), None) => m.to_owned(),
        (None, Some(c)) => c.to_owned(),
        (None, None) => text.chars().take(300).collect(),
    }
}

fn exchange_with(flow: &Settings, code: &str, verifier: &str) -> Result<Tokens> {
    exchange(flow, code, verifier, None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    pub code: String,
    pub client_id: Option<String>,
}

pub fn parse_callback(input: &str, state: &str) -> Result<Callback> {
    let query = input
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or(input)
        .split('#')
        .next()
        .unwrap_or_default();
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    for part in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=').unwrap_or((part, ""));
        pairs.insert(decode(k), decode(v));
    }
    if let Some(error) = pairs.get("error") {
        let detail = pairs
            .get("error_description")
            .map(String::as_str)
            .unwrap_or_default();
        bail!(
            "the login was refused: {error}{}{detail}",
            if detail.is_empty() { "" } else { " — " }
        );
    }
    if let Some(got) = pairs.get("state")
        && got != state
    {
        bail!("the login came back with the wrong state, so it was not this request's answer");
    }
    let code = pairs
        .get("code")
        .filter(|c| !c.is_empty())
        .context("the redirect carried no authorization code")?;
    Ok(Callback {
        code: code.clone(),
        client_id: pairs.get("client_id").cloned(),
    })
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or_default();
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn browser_login(
    flow: &Flow,
    agent_name: &str,
    tell: &mut dyn FnMut(&str),
    ask: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<Tokens> {
    let mut flow = flow.settings()?;
    let (verifier, challenge) = pkce();
    let state = random_b64(16);
    let nonce = random_b64(16);
    flow.extra
        .insert("agent_name_hint".into(), agent_name.into());
    flow.extra
        .insert("ext_agent_host_id".into(), format!("urn:uuid:{}", uuid()));
    let url = authorize_request(&flow, &state, &nonce, &challenge);
    tell(&url);

    let listener = TcpListener::bind(("127.0.0.1", flow.port)).ok();
    let callback = match listener {
        Some(listener) => {
            tell(
                "waiting for the browser to come back; paste the redirect URL instead if it cannot",
            );
            wait_for_callback(listener, &state, ask)?
        }
        None => {
            tell(&format!(
                "port {} is in use, so the redirect cannot be caught here; paste the URL the \
                 browser was sent to",
                flow.port
            ));
            parse_callback(&ask("redirect URL: ")?, &state)?
        }
    };
    tell("exchanging the code for tokens");
    let issued = callback.client_id.clone();
    let tokens = exchange(&flow, &callback.code, &verifier, issued.as_deref())?;
    Ok(tokens)
}

fn wait_for_callback(
    listener: TcpListener,
    state: &str,
    ask: &mut dyn FnMut(&str) -> Result<String>,
) -> Result<Callback> {
    let (tx, rx) = mpsc::channel();
    let state_owned = state.to_owned();
    let from_browser = tx.clone();
    thread::Builder::new()
        .name("saddle-oauth".into())
        .spawn(move || {
            let _ = from_browser.send(accept_one(listener, &state_owned));
        })
        .context("starting the login listener")?;
    let prompt = ask("redirect URL, or press enter to wait for the browser: ")?;
    if !prompt.trim().is_empty() {
        return parse_callback(&prompt, state);
    }
    rx.recv()
        .context("the login listener stopped without an answer")?
}

fn accept_one(listener: TcpListener, state: &str) -> Result<Callback> {
    let (mut stream, _) = listener.accept().context("waiting for the browser")?;
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let target = request
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let parsed = parse_callback(&target, state);
    let body = if parsed.is_ok() {
        "<html><body><p>signed in — you can close this tab.</p></body></html>"
    } else {
        "<html><body><p>that did not work; see the terminal.</p></body></html>"
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

pub fn exchange(
    flow: &Settings,
    code: &str,
    verifier: &str,
    issued_client: Option<&str>,
) -> Result<Tokens> {
    let client = issued_client
        .filter(|c| !c.is_empty())
        .unwrap_or(&flow.client_id);
    let mut form = vec![
        ("grant_type".to_owned(), "authorization_code".to_owned()),
        ("client_id".to_owned(), client.to_owned()),
        ("code".to_owned(), code.to_owned()),
        ("code_verifier".to_owned(), verifier.to_owned()),
        ("redirect_uri".to_owned(), flow.callback_for_exchange()),
    ];
    if let Some(resource) = &flow.resource {
        form.push(("resource".to_owned(), resource.clone()));
    }
    post_form(&flow.token_url, &form)
}

pub fn refresh(flow: &Settings, tokens: &Tokens) -> Result<Tokens> {
    let client = tokens
        .client_id
        .as_deref()
        .filter(|c| !c.is_empty())
        .unwrap_or(&flow.client_id);
    let mut form = vec![
        ("grant_type".to_owned(), "refresh_token".to_owned()),
        ("client_id".to_owned(), client.to_owned()),
        ("refresh_token".to_owned(), tokens.refresh.clone()),
    ];
    if let Some(resource) = &flow.resource {
        form.push(("resource".to_owned(), resource.clone()));
    }
    let mut fresh = post_form(&flow.token_url, &form)?;
    if fresh.refresh.is_empty() {
        fresh.refresh = tokens.refresh.clone();
    }
    if fresh.client_id.is_none() {
        fresh.client_id = tokens.client_id.clone();
    }
    if fresh.account_id.is_none() {
        fresh.account_id = tokens.account_id.clone();
    }
    Ok(fresh)
}

fn post_json(url: &str, body: &str) -> Result<(u16, String)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout::remaining(Duration::from_secs(60))?))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .send(body.as_bytes())
        .map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()
        .unwrap_or_default();
    Ok((status, text))
}

fn detail(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error_description"]
                .as_str()
                .or(v["error"]["message"].as_str())
                .or(v["error"].as_str())
                .or(v["message"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

fn post_form(url: &str, form: &[(String, String)]) -> Result<Tokens> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout::remaining(Duration::from_secs(60))?))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .send(body.as_bytes())
        .map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()
        .unwrap_or_default();
    if status != 200 {
        bail!("the token endpoint refused ({status}): {}", describe(&text));
    }
    let value: serde_json::Value =
        serde_json::from_str(&text).context("the token endpoint's answer is not JSON")?;
    let access = value["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .context("the token endpoint sent no access token")?
        .to_owned();
    let expires_at = value["expires_in"]
        .as_u64()
        .map(|s| now().saturating_add(s))
        .unwrap_or(0);
    Ok(Tokens {
        account_id: account_id(&access),
        client_id: value["client_id"].as_str().map(str::to_owned),
        access,
        refresh: value["refresh_token"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        expires_at,
    })
}

pub fn account_id(access: &str) -> Option<String> {
    let payload = access.split('.').nth(1)?;
    let bytes = B64.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims[ACCOUNT_CLAIM]["chatgpt_account_id"]
        .as_str()
        .map(str::to_owned)
}

pub fn headers(tokens: &Tokens) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(account) = &tokens.account_id {
        out.push(("chatgpt-account-id".to_owned(), account.clone()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{Api, Auth, Provider, Reported, endpoint};
    use super::*;
    use crate::auth::AuthStore;
    use std::path::PathBuf;
    use std::time::Instant;

    #[test]
    fn token_requests_inherit_the_plugin_deadline() {
        for form in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                thread::sleep(Duration::from_secs(30));
                drop(stream);
            });
            let started = Instant::now();
            let result = timeout::using(Some(started + Duration::from_millis(100)), || {
                if form {
                    post_form(&url, &[]).map(|_| ())
                } else {
                    post_json(&url, "{}").map(|_| ())
                }
            });
            assert!(result.is_err());
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }

    fn flow() -> Settings {
        Flow {
            authorize_url: Some(OPENAI_AUTHORIZE_URL.into()),
            token_url: Some(OPENAI_TOKEN_URL.into()),
            client_id: Some(OPENAI_CLIENT_ID.into()),
            scopes: Some(OPENAI_SCOPES.into()),
            resource: None,
            ..Flow::default()
        }
        .settings()
        .unwrap()
    }

    #[test]
    fn the_authorize_url_carries_a_pkce_challenge_and_a_state() {
        let (verifier, challenge) = pkce();
        assert!(
            verifier.len() >= 43,
            "a verifier must be 43..128 characters"
        );
        assert!(verifier.len() <= 128);
        assert_eq!(challenge, B64.encode(Sha256::digest(verifier.as_bytes())));
        let url = authorize_request(&flow(), "st", "no", &challenge);
        assert!(url.starts_with(OPENAI_AUTHORIZE_URL), "{url}");
        for want in [
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "response_type=code",
            "code_challenge_method=S256",
            "state=st",
            "nonce=no",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback",
            "scope=openid%20profile%20email%20offline_access",
        ] {
            assert!(url.contains(want), "{want} missing from {url}");
        }
        assert!(!url.contains(&verifier), "the verifier leaked into the URL");
    }

    #[test]
    fn a_callback_is_read_from_a_url_or_a_path_and_checked_against_the_state() {
        let ok = parse_callback(
            "http://127.0.0.1:1455/auth/callback?code=abc&state=st&client_id=issued-1",
            "st",
        )
        .unwrap();
        assert_eq!(ok.code, "abc");
        assert_eq!(ok.client_id.as_deref(), Some("issued-1"));
        assert_eq!(
            parse_callback("/auth/callback?code=abc&state=st", "st")
                .unwrap()
                .code,
            "abc"
        );

        let err = parse_callback("/x?code=abc&state=other", "st")
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong state"), "{err}");
        let err = parse_callback("/x?error=access_denied&error_description=no+thanks", "st")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("access_denied") && err.contains("no thanks"),
            "{err}"
        );
        assert!(parse_callback("/x?state=st", "st").is_err());
        assert_eq!(
            parse_callback("/x?code=a%2Fb%2Bc&state=st", "st")
                .unwrap()
                .code,
            "a/b+c"
        );
    }

    #[test]
    fn the_account_id_comes_out_of_the_access_token() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acct-42" }
        });
        let payload = B64.encode(serde_json::to_vec(&claims).unwrap());
        let token = format!("header.{payload}.signature");
        assert_eq!(account_id(&token).as_deref(), Some("acct-42"));
        assert_eq!(account_id("not-a-jwt"), None);
        assert_eq!(account_id("a.!!!!.c"), None);
    }

    #[test]
    fn tokens_are_usable_until_their_margin_runs_out() {
        let t = Tokens {
            access: "a".into(),
            refresh: "r".into(),
            expires_at: 1000,
            client_id: None,
            account_id: None,
        };
        assert!(t.usable(60, 900), "still valid with a minute to spare");
        assert!(!t.usable(60, 950), "inside the margin: refresh first");
        assert!(!t.usable(60, 1000), "expired");
        let unknown = Tokens { expires_at: 0, ..t };
        assert!(unknown.usable(60, u64::MAX - 1));
    }

    #[test]
    fn a_provider_that_states_nothing_gets_the_openai_defaults() {
        let empty = Flow::default();
        assert!(empty.missing().unwrap().contains("authorize-url"));
        assert!(empty.settings().is_err());
        let f = flow();
        assert_eq!(f.authorize_url, OPENAI_AUTHORIZE_URL);
        assert_eq!(f.token_url, OPENAI_TOKEN_URL);
        assert_eq!(f.client_id, OPENAI_CLIENT_ID);
        assert_eq!(f.port, 1455);
        assert_eq!(f.redirect_uri(), "http://127.0.0.1:1455/auth/callback");
        let f = Flow {
            authorize_url: Some("https://id.test/authorize".into()),
            token_url: None,
            client_id: Some("me".into()),
            scopes: Some("openid".into()),
            resource: None,
            redirect_port: Some(7777),
            redirect_path: Some("/cb".into()),
            redirect_uri: None,
            device_redirect_uri: None,
            method: None,
            device_code_url: None,
            device_token_url: None,
            verification_url: None,
            extra: Default::default(),
        };
        let missing = f.missing().unwrap();
        assert!(missing.contains("token-url"), "{missing}");
        assert!(f.settings().is_err(), "an incomplete table cannot settle");
        let f = Flow {
            token_url: Some("https://id.test/token".into()),
            ..f
        }
        .settings()
        .unwrap();
        assert_eq!(f.token_url, "https://id.test/token");
        let url = authorize_request(&f, "s", "n", "c");
        assert!(url.starts_with("https://id.test/authorize?"), "{url}");
        assert!(url.contains("client_id=me"), "{url}");
        assert!(
            url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A7777%2Fcb"),
            "{url}"
        );
        assert!(url.contains("scope=openid"), "{url}");
    }

    #[test]
    fn an_oauth_error_reads_as_a_sentence() {
        assert_eq!(
            describe(
                r#"{"error": {"message": "Could not validate your token.", "code": "token_exchange_user_error"}}"#
            ),
            "Could not validate your token. (token_exchange_user_error)"
        );
        assert_eq!(
            describe(r#"{"error": "invalid_grant", "error_description": "code expired"}"#),
            "code expired"
        );
        assert_eq!(describe(r#"{"error": "invalid_client"}"#), "invalid_client");
        assert_eq!(describe("upstream is down"), "upstream is down");
        assert!(describe(&"x".repeat(5000)).chars().count() <= 300);
    }

    #[test]
    fn a_device_code_is_exchanged_for_the_address_it_was_issued_for() {
        let flow = endpoint("openai-chatgpt", &Default::default())
            .unwrap()
            .oauth
            .unwrap();
        let settings = flow.settings().expect("the preset is complete");
        assert_eq!(settings.method, Method::Device);
        assert_eq!(
            settings.callback_for_exchange(),
            OPENAI_DEVICE_REDIRECT_URI,
            "the device flow exchanges against its own callback"
        );
        assert_ne!(
            settings.callback_for_exchange(),
            OPENAI_REDIRECT_URI,
            "and never the browser flow's"
        );
        let browser = Settings {
            method: Method::Browser,
            redirect_uri: OPENAI_REDIRECT_URI.into(),
            ..settings
        };
        assert_eq!(browser.callback_for_exchange(), OPENAI_REDIRECT_URI);
    }

    #[test]
    fn a_device_table_without_its_callback_is_incomplete() {
        let flow = Flow {
            method: Some(Method::Device),
            authorize_url: Some("https://a.test/authorize".into()),
            token_url: Some("https://a.test/token".into()),
            client_id: Some("cid".into()),
            ..Flow::default()
        };
        let why = flow.settings().unwrap_err().to_string();
        assert!(why.contains("device-redirect-uri"), "{why}");
        let browser = Flow {
            method: Some(Method::Browser),
            ..flow
        };
        assert!(browser.settings().is_ok());
    }

    #[test]
    fn a_stored_login_is_found_and_a_fresh_one_is_not_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut cred = AuthStore::load(&path).unwrap();
        cred.set_oauth(
            "openai-chatgpt",
            Tokens {
                access: "at".into(),
                refresh: "rt".into(),
                expires_at: now() + 3600,
                client_id: Some(OPENAI_CLIENT_ID.into()),
                account_id: Some("acct".into()),
            },
        );
        cred.save(&path).unwrap();

        let config = endpoint("openai-chatgpt", &Default::default()).unwrap();
        assert_eq!(config.auth, Auth::Oauth);
        assert_eq!(config.api, Api::OpenaiResponses);
        assert_eq!(config.base_url, OPENAI_API_BASE);
        let mut p = Provider::new("openai-chatgpt", config, None, &Reported::default());
        assert!(p.missing_key().is_some(), "not attached yet");
        p = p.with_stored_oauth(&path);
        assert!(p.missing_key().is_none(), "the stored login was not found");
        assert_eq!(p.key().as_deref(), Some("at"));
        let headers = p.request_headers();
        assert!(headers.contains(&("chatgpt-account-id".to_owned(), "acct".to_owned())));
        assert!(headers.contains(&("authorization".to_owned(), "Bearer at".to_owned())));
        assert!(p.refresh_tokens().is_ok());
        assert_eq!(p.key().as_deref(), Some("at"));
    }

    #[test]
    fn an_expired_login_is_refreshed_before_the_request() {
        let tokens = Tokens {
            access: "old".into(),
            refresh: "rt".into(),
            expires_at: now().saturating_sub(10),
            client_id: Some(OPENAI_CLIENT_ID.into()),
            account_id: None,
        };
        assert!(
            !tokens.usable(120, now()),
            "an expired token must not be sent"
        );
        let config = endpoint("openai-chatgpt", &Default::default()).unwrap();
        let mut p = Provider::new("openai-chatgpt", config, None, &Reported::default());
        p.with_oauth(
            Flow::default(),
            tokens,
            PathBuf::from("/nonexistent/auth.json"),
        );
        let err = p.refresh_tokens().unwrap_err().to_string();
        assert!(err.contains("oauth table needs"), "{err}");
    }

    #[test]
    fn the_agent_host_id_is_a_uuid() {
        let id = uuid();
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert_ne!(id, uuid(), "two ids must not be the same");
        assert_eq!(&id[14..15], "4", "version 4: {id}");
    }
}
