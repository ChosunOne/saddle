//! Model providers. The conversation is held in one neutral form; each API module renders it into
//! its own request format and parses its own response stream back into it.
//!
//! Two wire formats are supported: the Anthropic Messages API and OpenAI-compatible chat
//! completions. The second covers OpenAI, DeepSeek, OpenRouter and local servers such as
//! llama-server, vLLM and Ollama.

mod anthropic;
mod http;
pub(crate) mod oauth;
mod openai;
mod reported;
mod responses;
mod sse;

pub use http::{ModelInfo, ModelsError, fetch_usage, list_models, page_paths, quota_from_page};
pub use oauth::Tokens;
pub use reported::Reported;

use crate::auth::AuthStore;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    Anthropic,
    Openai,
    /// OpenAI's Responses API: a different conversation shape and a different event stream, reached
    /// with a ChatGPT subscription token (D85).
    #[serde(rename = "openai-responses")]
    OpenaiResponses,
}

/// A tool as the model sees it.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The parsed input, or the raw text when it was not a JSON object.
    pub input: Result<Value, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Part {
    Text(String),
    Call(ToolCall),
    /// An image the user attached, as base64 and its media type (D58). It is a part of a message
    /// rather than a tool result because that is what the two APIs agree on: Anthropic accepts an
    /// image block in a tool result, OpenAI does not.
    Image(Image),
    /// A block only one API understands, such as an Anthropic thinking block. It is sent back
    /// unchanged to that API and skipped by the other.
    Native {
        api: Api,
        block: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolResult {
    pub id: String,
    pub content: String,
    pub is_error: bool,
    /// Pictures the tool returned, such as a screenshot `read` was asked for (D58). An older saved
    /// result has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
    /// For the transcript only; never sent to a provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Item {
    User(String),
    /// A prompt with images attached: the text, then the pictures (D58). Its own variant rather
    /// than a flag so that every place that matches on an item has to decide what an attachment
    /// means there.
    Attached {
        text: String,
        images: Vec<Image>,
    },
    Assistant(Vec<Part>),
    ToolResults(Vec<ToolResult>),
    /// A compaction summary standing in for the messages before it (D33). Sent as a user message.
    Summary(String),
}

/// One image, ready for the wire (D58).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`: what the APIs accept, and what the
    /// magic bytes said rather than what the extension claimed.
    pub media_type: String,
    /// The bytes, base64-encoded.
    pub data: String,
    /// The path it came from, for the line the terminal draws and the transcript keeps.
    pub name: String,
    pub width: u32,
    pub height: u32,
}

/// The largest image saddle will attach, before base64. Base64 is a third larger again, and both
/// APIs limit a request.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

impl Image {
    /// Reads an image file, deciding the media type from the bytes themselves (D58).
    ///
    /// The extension is not trusted: a `.png` that is really a JPEG is sent as a JPEG, which is what
    /// makes it render rather than fail at the provider.
    pub fn read(path: &std::path::Path, data: Vec<u8>) -> Result<Self, String> {
        if data.len() as u64 > MAX_IMAGE_BYTES {
            return Err(format!(
                "{} KiB is over the {} KiB limit for one image",
                data.len() / 1024,
                MAX_IMAGE_BYTES / 1024
            ));
        }
        let Some((media_type, width, height)) = sniff(&data) else {
            return Err("not a PNG, JPEG, GIF or WebP image".to_owned());
        };
        Ok(Self {
            media_type: media_type.to_owned(),
            data: base64_encode(&data),
            name: path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            width,
            height,
        })
    }

    /// The bytes back, for drawing them (D59).
    pub fn bytes(&self) -> Option<Vec<u8>> {
        base64_decode(&self.data)
    }
}

/// What the magic bytes say: the media type and the pixel size (D58).
fn sniff(d: &[u8]) -> Option<(&'static str, u32, u32)> {
    if d.starts_with(b"\x89PNG\r\n\x1a\n") {
        // The IHDR chunk holds the size at a fixed offset.
        let w = u32::from_be_bytes(d.get(16..20)?.try_into().ok()?);
        let h = u32::from_be_bytes(d.get(20..24)?.try_into().ok()?);
        return Some(("image/png", w, h));
    }
    if d.starts_with(b"GIF87a") || d.starts_with(b"GIF89a") {
        let w = u16::from_le_bytes(d.get(6..8)?.try_into().ok()?) as u32;
        let h = u16::from_le_bytes(d.get(8..10)?.try_into().ok()?) as u32;
        return Some(("image/gif", w, h));
    }
    if d.starts_with(b"RIFF") && d.get(8..12) == Some(b"WEBP") {
        // Three WebP variants; the size is in a different place in each.
        let (w, h) = match d.get(12..16)? {
            b"VP8 " => {
                let w = u16::from_le_bytes(d.get(26..28)?.try_into().ok()?) as u32 & 0x3fff;
                let h = u16::from_le_bytes(d.get(28..30)?.try_into().ok()?) as u32 & 0x3fff;
                (w, h)
            }
            b"VP8L" => {
                let bits = u32::from_le_bytes(d.get(21..25)?.try_into().ok()?);
                ((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
            }
            _ => (
                u16::from_le_bytes(d.get(30..32)?.try_into().ok()?) as u32,
                u16::from_le_bytes(d.get(32..34)?.try_into().ok()?) as u32,
            ),
        };
        return Some(("image/webp", w, h));
    }
    if d.starts_with(&[0xff, 0xd8, 0xff]) {
        // JPEG has no fixed size field: walk the segments to the frame header.
        let mut i = 2;
        while i + 9 < d.len() {
            if d[i] != 0xff {
                return None;
            }
            let marker = d[i + 1];
            let len = u16::from_be_bytes(d.get(i + 2..i + 4)?.try_into().ok()?) as usize;
            // SOF0..SOF15, except the three that are not frame headers.
            if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                let h = u16::from_be_bytes(d.get(i + 5..i + 7)?.try_into().ok()?) as u32;
                let w = u16::from_be_bytes(d.get(i + 7..i + 9)?.try_into().ok()?) as u32;
                return Some(("image/jpeg", w, h));
            }
            i = i.checked_add(2 + len)?;
        }
        return None;
    }
    None
}

/// Standard base64 with padding, which is what both APIs take.
pub fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The bytes back, or `None` if the string is not base64.
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(text).ok()
}

impl Item {
    /// The prompt text, with an attachment's own text standing in for a plain prompt.
    pub fn text(&self) -> Option<&str> {
        match self {
            Item::User(t) | Item::Summary(t) => Some(t),
            Item::Attached { text, .. } => Some(text),
            _ => None,
        }
    }
}

/// How a summary item is introduced to the model.
pub const SUMMARY_INTRO: &str =
    "The conversation so far was compacted. This summary replaces the earlier messages:\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    ToolUse,
    /// The reply hit the output limit; a tool call in it may be incomplete.
    MaxTokens,
    /// The model or a safety classifier declined; a tool call in it may be incomplete.
    Refusal,
    Other,
}

/// What one request cost. The transcript keeps it now (D35), so it reads back as well as writes,
/// and a field added later must not make an older record unreadable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// What one request cost, in dollars per token, as the provider or the user states it (D36).
///
/// Prices are small enough that per-token is the only lossless unit; the config states them per
/// million, which is how model cards publish them, and they are divided here.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// Absent where the server states one price for all input; then reads cost the input price.
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

impl Price {
    /// Nothing priced means nothing to show, rather than a free model.
    pub fn is_known(&self) -> bool {
        self.input > 0.0 || self.output > 0.0
    }

    /// What a reply's usage cost. A cache price the server does not state is charged at the input
    /// price, so an unpriced cache errs high rather than pretending those tokens were free.
    pub fn cost(&self, u: &Usage) -> f64 {
        let read = self.cache_read.unwrap_or(self.input);
        let write = self.cache_write.unwrap_or(self.input);
        u.input as f64 * self.input
            + u.output as f64 * self.output
            + u.cache_read as f64 * read
            + u.cache_write as f64 * write
    }
}

/// The reasoning levels a person can cycle through, in order (D57). "default" sends nothing, so a
/// provider that has no such setting is not sent one it would reject.
pub const EFFORTS: &[&str] = &["default", "low", "medium", "high"];

/// The next level after `current`, wrapping. An unrecognised level goes back to the start rather
/// than to an arbitrary neighbour.
pub fn next_effort(current: Option<&str>) -> Option<String> {
    let at = current
        .and_then(|c| EFFORTS.iter().position(|e| *e == c))
        .unwrap_or(0);
    let next = EFFORTS[(at + 1) % EFFORTS.len()];
    (next != "default").then(|| next.to_owned())
}

/// A `[providers.<name>.pricing]` table: dollars per million tokens, as model cards publish them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PriceEntry {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    /// `"million"` (the default here, as model cards publish) or `"token"` for a table copied from
    /// an API that prices per token (D36).
    pub unit: Option<String>,
}

impl PriceEntry {
    fn to_price(&self) -> Option<Price> {
        // A config table is dollars per million unless it says otherwise.
        let per = match self.unit.as_deref().map(str::to_ascii_lowercase) {
            Some(u) if u.contains("token") && !u.contains("million") => 1.0,
            _ => 1_000_000.0,
        };
        let price = Price {
            input: self.input.unwrap_or(0.0) / per,
            output: self.output.unwrap_or(0.0) / per,
            cache_read: self.cache_read.map(|p| p / per),
            cache_write: self.cache_write.map(|p| p / per),
        };
        price.is_known().then_some(price)
    }
}

/// What the provider said about the allowance left, from the response headers (D36). Servers state
/// whichever parts they count, so every field is optional and a session shows only what it was told.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quota {
    pub tokens: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub requests: Option<u64>,
    pub requests_limit: Option<u64>,
    /// A money or credit balance, in whatever unit the provider sells in (D36).
    pub credits: Option<f64>,
    /// When the allowance refills, as the provider spelled it ("1s", "6m0s", an RFC 3339 stamp).
    pub reset: Option<String>,
    /// How much of the period is left, when the provider states a percentage instead of a count —
    /// which is what a plan that refills in steps reports (D49).
    pub percent: Option<f64>,
}

impl Quota {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_none()
            && self.requests.is_none()
            && self.credits.is_none()
            && self.percent.is_none()
    }
}

/// How long until the allowance refills, in the fewest characters that stay honest: a status line
/// has room for `↺1h` and not for a timestamp (D49).
///
/// A provider that spells the reset as a duration (`6m0s`) is not second-guessed: that is already
/// short, and converting it would mean guessing the instant it was measured from.
pub fn reset_left(spelled: &str, now: std::time::SystemTime) -> String {
    let Ok(at) = humantime::parse_rfc3339(spelled) else {
        return spelled.to_owned();
    };
    let left = at.duration_since(now).map(|d| d.as_secs()).unwrap_or(0);
    // Rounded up, so the last minute reads `1m` rather than `0m` while there is still time.
    let up = |unit: u64| left.div_ceil(unit);
    match left {
        0 => "now".into(),
        1..=59 => format!("{left}s"),
        60..=3599 => format!("{}m", up(60)),
        3600..=86_399 => format!("{}h", up(3600)),
        _ => format!("{}d", up(86_400)),
    }
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub parts: Vec<Part>,
    pub stop: Stop,
    /// The stop reason as the API spelled it.
    pub stop_raw: String,
    pub model: String,
    pub usage: Usage,
}

impl Reply {
    pub fn calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|p| match p {
            Part::Call(c) => Some(c),
            _ => None,
        })
    }
}

pub struct Conversation<'a> {
    pub system: &'a str,
    pub tools: &'a [ToolDef],
    pub items: &'a [Item],
}

/// Receives a reply while it streams in.
pub trait Sink {
    fn text(&mut self, s: &str);
    fn note(&mut self, s: &str);
    /// Reasoning from a thinking model, as it streams.
    fn reasoning(&mut self, _s: &str) {}
    /// What the response's headers said about the allowance left (D36). A provider that states
    /// nothing never calls this.
    fn quota(&mut self, _quota: &Quota) {}
    /// Checked between stream events; `true` abandons the request with [`Cancelled`].
    fn cancelled(&self) -> bool {
        false
    }
}

/// The user cancelled the request.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// One `[providers.<name>]` table in `config.toml`, or a built-in preset. Fields set in the
/// config override the preset of the same name.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProviderEntry {
    pub api: Option<Api>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Environment variable holding the API key. Empty means the server takes no key.
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    /// The model's context window in tokens, for compaction (D33).
    pub context_window: Option<u64>,
    /// Anthropic: `output_config.effort`. OpenAI-compatible: `reasoning_effort`.
    pub effort: Option<String>,
    /// Dollars per million tokens, for a server that states no prices of its own (D36).
    pub pricing: Option<PriceEntry>,
    /// The provider's own usage or quota endpoint, fetched for the status line (D36). A server
    /// that states nothing in its response headers can still state a balance here.
    pub usage_url: Option<String>,
    /// The provider's web search, when it serves one (D82). The `websearch` tool asks here before
    /// it looks for a key of its own, so a provider's own credential pays for its own search.
    pub search_url: Option<String>,
    /// Headers sent with every request, for what a compatible server needs beyond the wire format:
    /// OpenRouter's `HTTP-Referer` and `X-Title`, a gateway's routing header, an account id (D84).
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The header the API key goes in, when it is not the one its `api` implies. A JSON body with
    /// an `Authorization: Bearer` key is the norm; Azure wants `api-key` and no prefix, which is the
    /// case this exists for (D84).
    pub key_header: Option<String>,
    /// How the provider authenticates (D85). An OAuth provider has no key: its access token comes
    /// from the store `/login` writes, and is refreshed when it expires.
    pub auth: Option<Auth>,
    /// This provider's OAuth endpoints, from a preset or `[providers.<name>.oauth]` (D85).
    pub oauth: Option<crate::provider::oauth::Flow>,
}

/// How a provider authenticates (D85).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    /// A key from the environment or from `/login`.
    #[default]
    ApiKey,
    /// Tokens from an OAuth login, refreshed when they expire.
    Oauth,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub api: Api,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u64>,
    pub effort: Option<String>,
    /// Settled from the config or the endpoint's listing, like the window (D36).
    pub price: Option<Price>,
    /// Where to ask what is left, when the provider has an endpoint for it (D36).
    pub usage_url: Option<String>,
    /// Where to ask the web a question, when the provider searches on its own (D82).
    pub search_url: Option<String>,
    /// Headers sent with every request (D84).
    pub headers: BTreeMap<String, String>,
    /// The header the key goes in, when it is not the default for this `api` (D84).
    pub key_header: Option<String>,
    /// How the provider authenticates, and where, when it is OAuth (D85).
    pub auth: Auth,
    pub oauth: Option<crate::provider::oauth::Flow>,
}

/// Built-in providers.
///
/// Two wire formats and one preset each, because those are the two that every other service is
/// compatible with: `openai` speaks `/chat/completions` and `anthropic` speaks `/v1/messages`. Any
/// other server — a gateway, a local model, a proxy — is a `[providers.<name>]` table with `api`
/// set to whichever of the two it imitates (D84).
fn presets() -> BTreeMap<&'static str, ProviderEntry> {
    let entry = |base: &str, key: &str| ProviderEntry {
        api: Some(Api::Openai),
        base_url: Some(base.into()),
        api_key_env: Some(key.into()),
        ..ProviderEntry::default()
    };
    BTreeMap::from([
        (
            // Synthetic serves its chats under `/openai/v1` and its allowance under `/v2/quotas`,
            // which is why a provider carries a usage URL of its own rather than a rule for
            // deriving one (D36).
            "synthetic",
            ProviderEntry {
                usage_url: Some("https://api.synthetic.new/v2/quotas".into()),
                // Same host, same version prefix as the quotas endpoint above (D82).
                search_url: Some("https://api.synthetic.new/v2/search".into()),
                ..entry("https://api.synthetic.new/openai/v1", "SYNTHETIC_API_KEY")
            },
        ),
        (
            "openai",
            entry("https://api.openai.com/v1", "OPENAI_API_KEY"),
        ),
        (
            // A ChatGPT subscription, which is a different endpoint and a different wire format
            // from the API-key provider above, not a setting on it (D85).
            "openai-chatgpt",
            ProviderEntry {
                api: Some(Api::OpenaiResponses),
                base_url: Some(oauth::OPENAI_API_BASE.into()),
                auth: Some(Auth::Oauth),
                oauth: Some(oauth::Flow {
                    method: Some(oauth::Method::Device),
                    authorize_url: Some(oauth::OPENAI_AUTHORIZE_URL.into()),
                    token_url: Some(oauth::OPENAI_TOKEN_URL.into()),
                    client_id: Some(oauth::OPENAI_CLIENT_ID.into()),
                    scopes: Some(oauth::OPENAI_SCOPES.into()),
                    redirect_uri: Some(oauth::OPENAI_REDIRECT_URI.into()),
                    // The code a device login issues belongs to a fixed address of the server's,
                    // not to the loopback one the browser flow uses (D85).
                    device_redirect_uri: Some(oauth::OPENAI_DEVICE_REDIRECT_URI.into()),
                    redirect_port: Some(oauth::OPENAI_REDIRECT_PORT),
                    device_code_url: Some(oauth::OPENAI_DEVICE_CODE_URL.into()),
                    device_token_url: Some(oauth::OPENAI_DEVICE_TOKEN_URL.into()),
                    verification_url: Some(oauth::OPENAI_VERIFICATION_URL.into()),
                    ..oauth::Flow::default()
                }),
                // The subscription backend wants both, and states so in its own clients.
                headers: [(
                    "OpenAI-Beta".to_owned(),
                    "responses=experimental".to_owned(),
                )]
                .into_iter()
                .collect(),
                ..ProviderEntry::default()
            },
        ),
        (
            // The Anthropic base carries no `/v1`: its client appends `/v1/messages`, which keeps a
            // gateway's own prefix intact (D84).
            "anthropic",
            ProviderEntry {
                api: Some(Api::Anthropic),
                ..entry("https://api.anthropic.com", "ANTHROPIC_API_KEY")
            },
        ),
    ])
}

/// Merges the preset named `name` with the user's table of the same name.
pub fn resolve(
    name: &str,
    user: &BTreeMap<String, ProviderEntry>,
    model: Option<&str>,
) -> Result<ProviderConfig> {
    let config = endpoint(name, user)?;
    let model = model
        .map(str::to_owned)
        .or_else(|| Some(config.model.clone()).filter(|m| !m.is_empty()))
        .with_context(|| {
            format!(
                "provider `{name}` has no model yet; pick one with /login in the TUI, pass \
                 --model, or set `model` under [providers.{name}]"
            )
        })?;
    Ok(ProviderConfig { model, ..config })
}

/// Like [`resolve`], but the model may be missing (empty), as when `/login` lists the models.
pub fn endpoint(name: &str, user: &BTreeMap<String, ProviderEntry>) -> Result<ProviderConfig> {
    let presets = presets();
    let preset = presets.get(name);
    let mine = user.get(name);
    if preset.is_none() && mine.is_none() {
        let known: Vec<&str> = presets.keys().copied().collect();
        bail!(
            "unknown provider `{name}`; the built-in ones are {}, and others go under \
             [providers.{name}] in config.toml",
            known.join(", ")
        );
    }
    let pick = |f: fn(&ProviderEntry) -> &Option<String>| {
        mine.and_then(|e| f(e).clone())
            .or_else(|| preset.and_then(|e| f(e).clone()))
    };
    let api = mine
        .and_then(|e| e.api)
        .or(preset.and_then(|e| e.api))
        .with_context(|| {
            format!("provider `{name}` needs `api = \"anthropic\"` or `\"openai\"`")
        })?;
    let base_url =
        pick(|e| &e.base_url).with_context(|| format!("provider `{name}` needs a `base-url`"))?;
    let model = pick(|e| &e.model).unwrap_or_default();
    Ok(ProviderConfig {
        api,
        base_url: base_url.trim_end_matches('/').to_owned(),
        model,
        api_key_env: pick(|e| &e.api_key_env).filter(|k| !k.is_empty()),
        max_tokens: mine
            .and_then(|e| e.max_tokens)
            .or(preset.and_then(|e| e.max_tokens)),
        context_window: mine
            .and_then(|e| e.context_window)
            .or(preset.and_then(|e| e.context_window)),
        effort: pick(|e| &e.effort).filter(|e| !e.is_empty()),
        price: mine
            .and_then(|e| e.pricing.as_ref())
            .and_then(PriceEntry::to_price)
            .or_else(|| {
                preset
                    .and_then(|e| e.pricing.as_ref())
                    .and_then(PriceEntry::to_price)
            }),
        usage_url: pick(|e| &e.usage_url).filter(|u| !u.is_empty()),
        search_url: pick(|e| &e.search_url).filter(|u| !u.is_empty()),
        headers: mine
            .map(|e| e.headers.clone())
            .or_else(|| preset.map(|e| e.headers.clone()))
            .unwrap_or_default(),
        key_header: pick(|e| &e.key_header).filter(|h| !h.is_empty()),
        auth: mine
            .and_then(|e| e.auth)
            .or_else(|| preset.and_then(|e| e.auth))
            .unwrap_or_default(),
        oauth: mine
            .and_then(|e| e.oauth.clone())
            .or_else(|| preset.and_then(|e| e.oauth.clone())),
    })
}

/// Every variable that may hold a provider key. These never reach a sandboxed child (D19), even
/// when an exec grant lists one.
pub fn secret_env_names(user: &BTreeMap<String, ProviderEntry>) -> Vec<String> {
    let mut names: Vec<String> = presets()
        .values()
        .chain(user.values())
        .filter_map(|e| e.api_key_env.clone())
        .filter(|k| !k.is_empty())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Where to get a key for a built-in provider.
pub fn key_page(name: &str) -> Option<&'static str> {
    match name {
        "synthetic" => Some("synthetic.new"),
        "openai" => Some("platform.openai.com/api-keys"),
        _ => None,
    }
}

pub fn builtin_names() -> Vec<String> {
    presets().keys().map(|k| k.to_string()).collect()
}

/// Every provider that can be chosen: the presets merged with the user's tables, then the user's
/// own providers.
pub fn catalog(user: &BTreeMap<String, ProviderEntry>) -> Vec<(String, ProviderEntry)> {
    let presets = presets();
    let merge = |base: Option<&ProviderEntry>, mine: Option<&ProviderEntry>| {
        let base = base.cloned().unwrap_or_default();
        let Some(m) = mine else { return base };
        ProviderEntry {
            api: m.api.or(base.api),
            base_url: m.base_url.clone().or(base.base_url),
            model: m.model.clone().or(base.model),
            api_key_env: m.api_key_env.clone().or(base.api_key_env),
            max_tokens: m.max_tokens.or(base.max_tokens),
            context_window: m.context_window.or(base.context_window),
            effort: m.effort.clone().or(base.effort),
            pricing: m.pricing.clone().or(base.pricing),
            usage_url: m.usage_url.clone().or(base.usage_url),
            search_url: m.search_url.clone().or(base.search_url),
            headers: if m.headers.is_empty() {
                base.headers
            } else {
                m.headers.clone()
            },
            key_header: m.key_header.clone().or(base.key_header),
            auth: m.auth.or(base.auth),
            oauth: m.oauth.clone().or(base.oauth),
        }
    };
    let mut out: Vec<(String, ProviderEntry)> = presets
        .iter()
        .map(|(name, p)| (name.to_string(), merge(Some(p), user.get(*name))))
        .collect();
    out.extend(
        user.iter()
            .filter(|(name, _)| !presets.contains_key(name.as_str()))
            .map(|(name, e)| (name.clone(), merge(None, Some(e)))),
    );
    out
}

/// The context window compaction assumes when neither the config, the provider nor the table
/// below says otherwise (D34).
pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

/// Windows published for models served over these APIs, as the longest substring of the
/// lowercased model id, so `qwen3-coder` beats a broader `qwen3`. They come from the model cards
/// and exist so that a fresh login is not wrong by default — a window the provider states, or the
/// user sets, always wins (D34).
const KNOWN_WINDOWS: &[(&str, u64)] = &[
    ("kimi-k2", 262_144),
    ("qwen3-coder", 262_144),
    ("minimax-m2", 204_800),
    ("glm-4.6", 200_000),
    ("kimi-k3", 524_288),
    ("glm-5", 524_288),
    ("gpt-oss", 131_072),
    ("deepseek-v4", 524_288),
    ("deepseek-v3", 131_072),
    ("deepseek-r1", 131_072),
    ("llama-3.3", 131_072),
];

/// The published window for a model, by the most specific family its id names.
///
/// Synthetic serves ids like `hf:deepseek-ai/DeepSeek-V4.1-Flash-1017`, so the family table is
/// matched against the part after `hf:` too, and `deepseek-v4` is in it because that listing states
/// its own windows anyway; the table is for endpoints that state none.
pub fn known_window(model: &str) -> Option<u64> {
    // `hf:vendor/Model-Name` is the same model as `Model-Name`; the prefix and the vendor are not
    // part of the family name.
    let id = model
        .trim_start_matches("hf:")
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .to_ascii_lowercase();
    KNOWN_WINDOWS
        .iter()
        .filter(|(family, _)| id.contains(family))
        .max_by_key(|(family, _)| family.len())
        .map(|(_, window)| *window)
}

/// The window to compact against: what the user set, else what this endpoint last reported for
/// the model, else the published window, else [`DEFAULT_CONTEXT_WINDOW`] (D34).
pub fn context_window(config: &ProviderConfig, reported: &Reported) -> u64 {
    window_and_source(config, reported).0
}

/// Where a window came from, so that `saddle check` and `/settings` can say which source answered
/// rather than only what the number is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// `context-window` in config.toml: the user's own answer.
    Config,
    /// This endpoint's `/models` listing, which is the only place a server states its own limit.
    Endpoint,
    /// The table of published windows, for a server that states none.
    Published,
    /// Nothing knew the model.
    Default,
}

impl std::fmt::Display for WindowSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            WindowSource::Config => "set in config.toml",
            WindowSource::Endpoint => "stated by the provider",
            WindowSource::Published => "published for this model family",
            WindowSource::Default => "the default, as nothing knows this model",
        })
    }
}

/// [`context_window`], with the source it came from (D34).
pub fn window_and_source(config: &ProviderConfig, reported: &Reported) -> (u64, WindowSource) {
    if let Some(window) = config.context_window {
        return (window, WindowSource::Config);
    }
    if let Some(window) = reported.window(&config.base_url, &config.model) {
        return (window, WindowSource::Endpoint);
    }
    if let Some(window) = known_window(&config.model) {
        return (window, WindowSource::Published);
    }
    (DEFAULT_CONTEXT_WINDOW, WindowSource::Default)
}

/// Asks the endpoint itself what this model takes, caches every window the listing states, and
/// answers with the one for this model (D34).
///
/// The listing is the provider's own word on its limits, so a session asks for it rather than
/// carrying a guess into its first turn. Failing is silence: a server that is unreachable, has no
/// models endpoint, or states no window leaves the caller with the best answer it already had.
pub fn ask_endpoint(p: &Provider, cache: &Path) -> Option<Stated> {
    let config = &p.config;
    let models = list_models(p).ok()?;
    let mut reported = Reported::stored(cache);
    reported.record(&config.base_url, &models);
    let _ = reported.save(cache);
    let stated = Stated {
        window: reported.window(&config.base_url, &config.model),
        price: reported.price(&config.base_url, &config.model),
    };
    (stated.window.is_some() || stated.price.is_some()).then_some(stated)
}

/// What a listing said about one model (D34, D36). Either can be absent: a server may state a
/// window and no price, a price and no window, or neither.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stated {
    pub window: Option<u64>,
    pub price: Option<Price>,
}

/// The key a provider would use: its environment variable, else the stored key.
/// The headers one request carries: the key where this provider says it goes, then the provider's
/// own (D84).
///
/// One function, used by both wire formats, because this is the part that makes a compatible server
/// usable — a gateway that wants `api-key` rather than a bearer token, an OpenRouter that wants its
/// own attribution header — and two copies of it would drift the moment one gained a case.
///
/// The prefix is decided by the header, not by the wire format: a key in `authorization` is a bearer
/// token, which is what every OAuth token here is too, and a key in a header the server named is
/// sent as it is. That is what Azure's `api-key` needs.
pub fn request_headers(config: &ProviderConfig, key: Option<&str>) -> Vec<(String, String)> {
    let mut headers = Vec::new();
    let default = match config.api {
        Api::Anthropic => "x-api-key",
        Api::Openai | Api::OpenaiResponses => "authorization",
    };
    let name = config
        .key_header
        .as_deref()
        .filter(|h| !h.is_empty())
        .unwrap_or(default);
    if let Some(key) = key.filter(|k| !k.trim().is_empty()) {
        let value = if name.eq_ignore_ascii_case("authorization") {
            format!("Bearer {key}")
        } else {
            key.to_owned()
        };
        headers.push((name.to_owned(), value));
    }
    for (name, value) in &config.headers {
        // A header the config sets explicitly replaces the one derived above, so a provider can
        // override even the key's own header without a second mechanism.
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        headers.push((name.clone(), value.clone()));
    }
    headers
}

pub fn available_key(config: &ProviderConfig, stored_key: Option<&str>) -> Option<String> {
    if config.auth == Auth::Oauth {
        return None;
    }
    config
        .api_key_env
        .as_ref()
        .and_then(|var| std::env::var(var).ok())
        .filter(|k| !k.is_empty())
        .or_else(|| stored_key.filter(|k| !k.is_empty()).map(str::to_owned))
}

/// A provider's own web search, as the tool needs it: a URL, and the name of the variable whose key
/// pays for it (D82). The key value lives in the environment or in the auth store, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTarget {
    pub provider: String,
    pub url: String,
    pub key_env: Option<String>,
}

/// A provider's live OAuth state: where its endpoints are, its tokens, and where to write a
/// refreshed one (D85).
///
/// The tokens are shared rather than copied, so a background compaction and the turn it belongs to
/// see one token: a refresh rotates the refresh token, and two concurrent refreshes would leave one
/// of them holding a token the server has already retired.
#[derive(Clone)]
pub struct OAuthState {
    pub flow: oauth::Flow,
    pub tokens: Arc<Mutex<Tokens>>,
    pub store: PathBuf,
    pub provider: String,
}

pub struct Provider {
    pub name: String,
    pub config: ProviderConfig,
    key: Option<String>,
    oauth: Option<OAuthState>,
    agent: ureq::Agent,
    window: u64,
    source: WindowSource,
    price: Option<Price>,
}

impl Provider {
    /// The key comes from the provider's environment variable, or else from the key stored by
    /// `/login`. A provider that names a variable cannot be used without one of the two.
    ///
    /// The context window is settled here, from [`context_window`], so that everything downstream
    /// — compaction, the TUI header, `saddle check` — reads one number rather than each deciding
    /// for itself what a model takes (D34).
    /// Where this provider's web search is, when it serves one (D82).
    ///
    /// The key is not carried: it is named by `key_env` and looked up at the call, the same way the
    /// model loop looks it up, so a key put in place mid-session is the key the search uses.
    pub fn search_target(&self) -> Option<SearchTarget> {
        self.config.search_url.as_ref().map(|url| SearchTarget {
            provider: self.name.clone(),
            url: url.clone(),
            key_env: self.config.api_key_env.clone(),
        })
    }

    pub fn new(
        name: &str,
        config: ProviderConfig,
        stored_key: Option<&str>,
        reported: &Reported,
    ) -> Self {
        let (window, source) = window_and_source(&config, reported);
        let key = available_key(&config, stored_key);
        let price = config
            .price
            .or_else(|| reported.price(&config.base_url, &config.model));
        Self {
            oauth: None,
            name: name.to_owned(),
            config,
            key,
            agent: http::agent(),
            window,
            source,
            price,
        }
    }

    /// A second client with the same settings, for a request that runs while the first is busy
    /// (D78).
    ///
    /// A compaction in the background must not share the client the turn is using: `complete` takes
    /// `&self` and would be fine, but the point is to have an independent request in flight, and a
    /// clone of the settings is what makes that true without either one borrowing the other.
    pub fn twin(&self) -> Self {
        Self {
            name: self.name.clone(),
            config: self.config.clone(),
            key: self.key.clone(),
            oauth: self.oauth.clone(),
            agent: http::agent(),
            window: self.window,
            source: self.source,
            price: self.price,
        }
    }

    /// Tokens the model takes, as far as the config, the provider and the table can say.
    pub fn window(&self) -> u64 {
        self.window
    }

    /// Which of those answered [`window`](Self::window).
    pub fn window_source(&self) -> WindowSource {
        self.source
    }

    /// What this model costs per token, when the config or the endpoint says (D36).
    pub fn price(&self) -> Option<Price> {
        self.price
    }

    /// Adopts a price the endpoint stated after the provider was built, unless the config set one.
    pub fn set_price(&mut self, price: Price) {
        if self.config.price.is_none() && price.is_known() {
            self.price = Some(price);
        }
    }

    /// Changes how much the model reasons, for this session (D57). `None` sends nothing, which is
    /// the provider's own default.
    pub fn set_effort(&mut self, effort: Option<String>) {
        self.config.effort = effort;
    }

    /// The effort in force, if one is set.
    pub fn effort(&self) -> Option<&str> {
        self.config.effort.as_deref()
    }

    /// Adopts a window the endpoint stated after the provider was built. Only a window the config
    /// does not set is replaced, and the source moves with it, so the header and the loop that
    /// both read this cannot disagree (D34).
    pub fn set_window(&mut self, window: u64) {
        if self.config.context_window.is_some() {
            return;
        }
        self.window = window;
        self.source = WindowSource::Endpoint;
    }

    /// Why requests would fail for want of a key, if they would.
    /// Attaches whatever `/login` stored for this provider, when it authenticates with OAuth (D85).
    ///
    /// A provider built without this says so in `missing_key`, which is the honest answer: the
    /// tokens exist but this process was not told where to find them.
    pub fn with_stored_oauth(mut self, store: &Path) -> Self {
        let Some(flow) = self.config.oauth.clone() else {
            return self;
        };
        if self.config.auth != Auth::Oauth {
            return self;
        }
        if let Ok(auth) = AuthStore::load(store)
            && let Some(tokens) = auth.oauth(&self.name).cloned()
        {
            self.with_oauth(flow, tokens, store.to_path_buf());
        }
        self
    }

    /// Attaches the OAuth tokens a login stored, so this provider can use and refresh them (D85).
    pub fn with_oauth(&mut self, flow: oauth::Flow, tokens: Tokens, store: PathBuf) {
        self.oauth = Some(OAuthState {
            flow,
            tokens: Arc::new(Mutex::new(tokens)),
            store,
            provider: self.name.clone(),
        });
    }

    /// The credential this request is sent with: the access token for an OAuth provider, else the
    /// API key. A copy, because an OAuth token can be replaced by a refresh.
    pub fn key(&self) -> Option<String> {
        match &self.oauth {
            Some(oauth) => oauth
                .tokens
                .lock()
                .ok()
                .map(|t| t.access.clone())
                .filter(|a| !a.is_empty()),
            None => self.key.clone(),
        }
    }

    /// The headers this request carries: the credential where the provider wants it, the
    /// provider's own, and the account an OAuth token belongs to (D84, D85).
    pub fn request_headers(&self) -> Vec<(String, String)> {
        let key = self.key();
        let mut headers = request_headers(&self.config, key.as_deref());
        if let Some(oauth) = &self.oauth
            && let Ok(tokens) = oauth.tokens.lock()
        {
            headers.extend(oauth::headers(&tokens));
        }
        headers
    }

    /// Refreshes an expired OAuth token before a request, and remembers the new one (D85).
    ///
    /// A refresh is a request of its own, so it happens here rather than inside the stream: a 401
    /// part-way through a reply is not recoverable, and a token that expires mid-turn would
    /// otherwise fail the turn that was working.
    pub fn refresh_tokens(&self) -> Result<()> {
        let Some(oauth) = &self.oauth else {
            return Ok(());
        };
        let settings = oauth.flow.settings()?;
        let mut tokens = oauth
            .tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("the OAuth token state was poisoned"))?;
        if tokens.usable(120, oauth::now()) {
            return Ok(());
        }
        let fresh = oauth::refresh(&settings, &tokens).with_context(|| {
            format!(
                "refreshing the {} login; run `saddle login {}` again",
                oauth.provider, oauth.provider
            )
        })?;
        if let Ok(mut store) = AuthStore::load(&oauth.store) {
            store.set_oauth(&oauth.provider, fresh.clone());
            let _ = store.save(&oauth.store);
        }
        *tokens = fresh;
        Ok(())
    }

    pub fn missing_key(&self) -> Option<String> {
        if self.config.auth == Auth::Oauth {
            return match &self.oauth {
                Some(_) => None,
                None => Some(format!(
                    "{} authenticates with OAuth: run `saddle login {}`",
                    self.name, self.name
                )),
            };
        }
        match (&self.key, &self.config.api_key_env) {
            (None, Some(var)) => Some(format!(
                "no API key for {}: log in with /login in the TUI, or set {var}",
                self.name
            )),
            _ => None,
        }
    }

    pub fn complete(&self, conv: &Conversation<'_>, sink: &mut dyn Sink) -> Result<Reply> {
        if let Some(why) = self.missing_key() {
            bail!(why);
        }
        self.refresh_tokens()?;
        match self.config.api {
            Api::Anthropic => anthropic::complete(self, conv, sink),
            Api::Openai => openai::complete(self, conv, sink),
            Api::OpenaiResponses => responses::complete(self, conv, sink),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::http::ModelInfo;

    #[test]
    fn presets_merge_with_user_tables() {
        let user: BTreeMap<String, ProviderEntry> = toml::from_str(
            r#"
[synthetic]
model = "hf:zai-org/GLM-5.3"

[local]
api = "openai"
base-url = "http://127.0.0.1:8080/v1/"
model = "qwen"
"#,
        )
        .unwrap();
        let syn = resolve("synthetic", &user, None).unwrap();
        assert_eq!(syn.model, "hf:zai-org/GLM-5.3");
        assert_eq!(syn.base_url, "https://api.synthetic.new/openai/v1");
        assert_eq!(syn.api_key_env.as_deref(), Some("SYNTHETIC_API_KEY"));

        let l = resolve("local", &user, Some("other")).unwrap();
        assert_eq!(l.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(l.model, "other");
        assert_eq!(l.api_key_env, None);

        let e = resolve("openai", &user, None).unwrap_err().to_string();
        assert!(e.contains("--model"), "{e}");
        // `anthropic` used to be the example of an unknown provider, because there was no preset
        // for it; it is a preset now (D84), so the unknown name is one that really is unknown.
        let e = resolve("nonesuch", &user, None).unwrap_err().to_string();
        assert!(e.contains("[providers.nonesuch]"), "{e}");

        assert_eq!(
            builtin_names(),
            ["anthropic", "openai", "openai-chatgpt", "synthetic"]
        );
        let names: Vec<String> = catalog(&user).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            [
                "anthropic",
                "openai",
                "openai-chatgpt",
                "synthetic",
                "local"
            ]
        );
        let secrets = secret_env_names(&user);
        assert_eq!(
            secrets,
            ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "SYNTHETIC_API_KEY"]
        );
    }

    fn config(model: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            api: Api::Openai,
            base_url: base_url.into(),
            model: model.into(),
            api_key_env: None,
            max_tokens: None,
            context_window: None,
            effort: None,
            price: None,
            usage_url: None,
            search_url: None,
            headers: BTreeMap::new(),
            key_header: None,
            auth: Auth::ApiKey,
            oauth: None,
        }
    }

    #[test]
    fn a_provider_says_where_its_key_and_its_own_headers_go() {
        // D84: what makes a third-party compatible server usable. The prefix comes from the header,
        // not from the wire format, because `api-key: <token>` is Azure and `authorization` is
        // everyone else.
        let mut headers = BTreeMap::new();
        headers.insert("HTTP-Referer".to_owned(), "https://example.test".to_owned());
        headers.insert("X-Title".to_owned(), "saddle".to_owned());
        let cfg = ProviderConfig {
            headers: headers.clone(),
            ..config("m", "https://gateway.test/v1")
        };
        let sent = request_headers(&cfg, Some("sk-1"));
        assert!(sent.contains(&("authorization".to_owned(), "Bearer sk-1".to_owned())));
        assert!(sent.contains(&("HTTP-Referer".to_owned(), "https://example.test".to_owned())));
        assert!(sent.contains(&("X-Title".to_owned(), "saddle".to_owned())));

        // Azure: the key in `api-key`, sent raw, and no bearer prefix anywhere.
        let cfg = ProviderConfig {
            key_header: Some("api-key".to_owned()),
            ..config("m", "https://x.test/v1")
        };
        let sent = request_headers(&cfg, Some("secret"));
        assert_eq!(sent, vec![("api-key".to_owned(), "secret".to_owned())]);

        // An Anthropic-shaped server gets the key where Anthropic wants it unless told otherwise.
        let cfg = ProviderConfig {
            api: Api::Anthropic,
            ..config("m", "https://x.test")
        };
        assert_eq!(
            request_headers(&cfg, Some("k")),
            vec![("x-api-key".to_owned(), "k".to_owned())]
        );

        // A local server that takes no key sends none.
        assert!(request_headers(&config("m", "http://localhost:8080"), None).is_empty());

        // A configured header wins over the derived one rather than being sent twice.
        let mut headers = BTreeMap::new();
        headers.insert("authorization".to_owned(), "Bearer own".to_owned());
        let cfg = ProviderConfig {
            headers,
            ..config("m", "https://x.test/v1")
        };
        let sent = request_headers(&cfg, Some("sk-1"));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, "Bearer own");
    }

    #[test]
    fn a_third_party_provider_is_just_a_table() {
        // The whole of "generic support": `api` picks the wire format, and everything else is a
        // field. No code, no preset (D84).
        let user: BTreeMap<String, ProviderEntry> = [
            (
                "gateway".to_owned(),
                ProviderEntry {
                    api: Some(Api::Openai),
                    base_url: Some("https://gateway.test/v1".into()),
                    api_key_env: Some("GATEWAY_KEY".into()),
                    model: Some("m1".into()),
                    headers: [("X-Title".to_owned(), "saddle".to_owned())]
                        .into_iter()
                        .collect(),
                    ..ProviderEntry::default()
                },
            ),
            (
                "claude-proxy".to_owned(),
                ProviderEntry {
                    api: Some(Api::Anthropic),
                    base_url: Some("https://proxy.test".into()),
                    api_key_env: Some("PROXY_KEY".into()),
                    model: Some("c1".into()),
                    ..ProviderEntry::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        let gateway = resolve("gateway", &user, None).unwrap();
        assert_eq!(gateway.api, Api::Openai);
        assert_eq!(gateway.base_url, "https://gateway.test/v1");
        assert_eq!(gateway.headers.len(), 1);
        let proxy = resolve("claude-proxy", &user, None).unwrap();
        assert_eq!(proxy.api, Api::Anthropic);
        // The presets are still there beside them, and `anthropic` is now one of them.
        let names = builtin_names();
        assert!(names.iter().any(|n| n == "anthropic"), "{names:?}");
        let official = endpoint("anthropic", &BTreeMap::new()).unwrap();
        assert_eq!(official.api, Api::Anthropic);
        assert_eq!(official.base_url, "https://api.anthropic.com");
        assert_eq!(official.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn the_synthetic_preset_names_its_search() {
        // D82: the search is a provider setting, so it is resolved like any other endpoint field.
        // The value has to be a sibling of the quotas endpoint, not a guess at a route.
        let user = BTreeMap::new();
        let syn = endpoint("synthetic", &user).unwrap();
        assert_eq!(
            syn.search_url.as_deref(),
            Some("https://api.synthetic.new/v2/search")
        );
        assert_eq!(
            syn.usage_url.as_deref(),
            Some("https://api.synthetic.new/v2/quotas")
        );
        // A provider that does not search says so by leaving it empty, which is what sends the tool
        // to the fallback rather than to a route that is not there.
        let config = config("m", "https://example.test/v1");
        assert_eq!(config.search_url, None);
        let user: BTreeMap<String, ProviderEntry> = [(
            "mine".to_owned(),
            ProviderEntry {
                api: Some(Api::Openai),
                base_url: Some("https://mine.test/v1".into()),
                search_url: Some("https://mine.test/v2/search".into()),
                ..ProviderEntry::default()
            },
        )]
        .into_iter()
        .collect();
        let config = endpoint("mine", &user).unwrap();
        assert_eq!(
            config.search_url.as_deref(),
            Some("https://mine.test/v2/search"),
            "a provider may be told where it searches"
        );
    }

    #[test]
    fn the_window_is_what_the_endpoint_stated_not_the_default() {
        let mut reported = Reported::default();
        reported.record(
            "https://api.synthetic.new/openai/v1",
            &[ModelInfo {
                id: "hf:moonshotai/Kimi-K2.6".into(),
                context_window: Some(262_144),
                price: None,
            }],
        );
        let c = config(
            "hf:moonshotai/Kimi-K2.6",
            "https://api.synthetic.new/openai/v1",
        );
        assert_eq!(context_window(&c, &reported), 262_144);

        // The same id at an endpoint that stated nothing falls to the published table.
        let elsewhere = config("hf:moonshotai/Kimi-K2.6", "http://127.0.0.1:8080/v1");
        assert_eq!(context_window(&elsewhere, &reported), 262_144);
        assert_eq!(
            context_window(&config("kimi-k2.6", "http://x/v1"), &Reported::default()),
            262_144
        );
    }

    #[test]
    fn the_config_overrides_every_other_answer() {
        let mut reported = Reported::default();
        reported.record(
            "https://api.synthetic.new/openai/v1",
            &[ModelInfo {
                id: "kimi-k2".into(),
                context_window: Some(262_144),
                price: None,
            }],
        );
        let mut c = config("kimi-k2", "https://api.synthetic.new/openai/v1");
        c.context_window = Some(32_768);
        assert_eq!(context_window(&c, &reported), 32_768);
        assert_eq!(
            window_and_source(&c, &reported),
            (32_768, WindowSource::Config)
        );
    }

    #[test]
    fn every_answer_says_which_source_gave_it() {
        // What the endpoint stated beats the published table, and says so.
        let mut reported = Reported::default();
        reported.record(
            "https://api.deepseek.com/v1",
            &[ModelInfo {
                id: "deepseek-v4.1-flash-1017".into(),
                context_window: Some(500_000),
                price: None,
            }],
        );
        let stated = config("deepseek-v4.1-flash-1017", "https://api.deepseek.com/v1");
        assert_eq!(
            window_and_source(&stated, &reported),
            (500_000, WindowSource::Endpoint)
        );

        // The same model at an endpoint that stated nothing falls to the table, then the default.
        let silent = config("deepseek-v3", "https://elsewhere/v1");
        assert_eq!(
            window_and_source(&silent, &reported),
            (131_072, WindowSource::Published)
        );
        let unknown = config("something-new", "https://elsewhere/v1");
        assert_eq!(
            window_and_source(&unknown, &reported),
            (DEFAULT_CONTEXT_WINDOW, WindowSource::Default)
        );
    }

    #[test]
    fn a_window_the_endpoint_states_later_is_adopted_unless_the_config_says_one() {
        let reported = Reported::default();
        // A model no table knows, so the default is what it starts on.
        let mut p = Provider::new(
            "unknown",
            config("some-new-model", "https://api.deepseek.com/v1"),
            None,
            &reported,
        );
        assert_eq!(p.window(), DEFAULT_CONTEXT_WINDOW);
        assert_eq!(p.window_source(), WindowSource::Default);

        p.set_window(500_000);
        assert_eq!(p.window(), 500_000);
        assert_eq!(p.window_source(), WindowSource::Endpoint);

        // A window the user set is the last word, so the provider does not unseat it.
        let mut c = config("deepseek-v4.1-flash-1017", "https://api.deepseek.com/v1");
        c.context_window = Some(64_000);
        let mut p = Provider::new("deepseek", c, None, &reported);
        p.set_window(500_000);
        assert_eq!(p.window(), 64_000);
        assert_eq!(p.window_source(), WindowSource::Config);
    }

    #[test]
    fn a_model_nothing_knows_keeps_the_default() {
        assert_eq!(known_window("some-new-model"), None);
        assert_eq!(
            context_window(
                &config("some-new-model", "http://x/v1"),
                &Reported::default()
            ),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn a_config_price_table_says_its_unit() {
        let user: BTreeMap<String, ProviderEntry> = toml::from_str(
            r#"
[per-million]
api = "openai"
base-url = "https://x/v1"
model = "m"
[per-million.pricing]
input = 3.0
output = 15.0

[per-token]
api = "openai"
base-url = "https://x/v1"
model = "m"
[per-token.pricing]
unit = "token"
input = 0.000003
output = 0.000015
"#,
        )
        .unwrap();
        // Model cards publish per million; that is the default.
        let c = endpoint("per-million", &user).unwrap();
        assert_eq!(c.price.map(|p| p.input), Some(3e-6));
        // A table copied from a per-token API says so.
        let c = endpoint("per-token", &user).unwrap();
        assert_eq!(c.price.map(|p| p.input), Some(3e-6));
    }

    #[test]
    fn an_image_is_recognised_by_its_bytes_not_its_name() {
        // A 1x1 PNG.
        let png = base64_decode(PNG_1X1).unwrap();
        let image = Image::read(std::path::Path::new("shot.txt"), png.clone()).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!((image.width, image.height), (1, 1));
        // The name is only for showing; the type came from the bytes.
        assert_eq!(image.name, "shot.txt");
        assert_eq!(image.bytes().unwrap(), png);

        // The same bytes behind the wrong extension are still a PNG (D58).
        let gif = base64_decode(GIF_1X1).unwrap();
        let image = Image::read(std::path::Path::new("x.png"), gif).unwrap();
        assert_eq!(image.media_type, "image/gif");
        assert_eq!((image.width, image.height), (1, 1));

        // Text is refused with a reason rather than sent and rejected by the provider.
        let e = Image::read(std::path::Path::new("a.txt"), b"hello".to_vec()).unwrap_err();
        assert!(e.contains("not a PNG, JPEG, GIF or WebP"), "{e}");
        // And so is a picture over the limit.
        let mut big = png.clone();
        big.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        let e = Image::read(std::path::Path::new("big.png"), big).unwrap_err();
        assert!(e.contains("over the"), "{e}");
    }

    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const GIF_1X1: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

    #[test]
    fn a_jpeg_size_is_found_by_walking_its_segments() {
        // SOI, an APP0 segment, then SOF0 with the size at a known offset.
        let mut d = vec![0xff, 0xd8];
        // The length field counts itself, so 0x10 is the field plus fourteen bytes of payload.
        d.extend_from_slice(&[0xff, 0xe0, 0x00, 0x10]);
        d.extend_from_slice(&[0u8; 14]);
        d.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x64, 0x00, 0xc8]);
        d.extend_from_slice(&[0u8; 4]);
        let image = Image::read(std::path::Path::new("a.jpg"), d).unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert_eq!((image.width, image.height), (200, 100));
    }

    #[test]
    fn the_reasoning_levels_cycle_and_wrap() {
        // No level set is the first in the list, so the first cycle picks `low` (D57).
        assert_eq!(next_effort(None), Some("low".to_owned()));
        assert_eq!(next_effort(Some("low")), Some("medium".to_owned()));
        assert_eq!(next_effort(Some("medium")), Some("high".to_owned()));
        // After the last level comes the default, which is no level at all.
        assert_eq!(next_effort(Some("high")), None);
        // An unrecognised level restarts rather than guessing a neighbour.
        assert_eq!(next_effort(Some("max")), Some("low".to_owned()));
        // The list `/thinking` accepts is the one that cycles.
        assert_eq!(EFFORTS, ["default", "low", "medium", "high"]);
    }

    #[test]
    fn a_usage_url_comes_from_the_config_and_the_preset() {
        let user: BTreeMap<String, ProviderEntry> = toml::from_str(
            r#"
[openai]
usage-url = "https://api.openai.com/v1/usage"
"#,
        )
        .unwrap();
        let c = resolve("openai", &user, Some("gpt-x")).unwrap();
        assert_eq!(
            c.usage_url.as_deref(),
            Some("https://api.openai.com/v1/usage")
        );
        // The preset brings its own, since Synthetic keeps the allowance at a path of its own.
        let c = resolve("synthetic", &user, Some("hf:x")).unwrap();
        assert_eq!(
            c.usage_url.as_deref(),
            Some("https://api.synthetic.new/v2/quotas")
        );
        // A user's table overrides the preset, and an empty one turns the request off.
        let user: BTreeMap<String, ProviderEntry> =
            toml::from_str("[synthetic]\nusage-url = \"\"\n").unwrap();
        let c = resolve("synthetic", &user, Some("hf:x")).unwrap();
        assert_eq!(c.usage_url, None);
    }

    #[test]
    fn the_most_specific_family_names_the_window() {
        // `qwen3-coder` is longer than `qwen3`, so it is the one that answers.
        assert_eq!(known_window("hf:Qwen/Qwen3-Coder-30B"), Some(262_144));
        assert_eq!(known_window("Qwen3-32B"), None);
        assert_eq!(known_window("glm-4.6"), Some(200_000));
        // Synthetic's ids carry a prefix and a vendor, which are not part of the family.
        assert_eq!(
            known_window("hf:zai-org/GLM-5.3"),
            Some(524_288),
            "glm-5 or glm-4.6: the longer match wins, and both are known"
        );
        assert_eq!(
            known_window("hf:deepseek-ai/DeepSeek-V4.1-Flash-1017"),
            Some(524_288)
        );
        // `gpt-oss` and `deepseek-v3` share a window, so either naming it is the same answer.
        assert_eq!(known_window("gpt-oss-120b"), Some(131_072));
    }
}
