use crate::provider::oauth::Flow;
use base64::engine::general_purpose;
use saddle_limits as budgets;
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::fmt::Formatter;
use std::fmt::Result as FmtResult;
use std::time::SystemTime;
mod anthropic;
mod http;
pub(crate) mod oauth;
mod openai;
mod openai_account;
mod reported;
mod responses;
mod retry;
mod ws;

pub use ws::websocket_url;
mod sse;

pub use http::{ModelInfo, ModelsError, list_models, page_paths, quota_from_page};
pub use oauth::Tokens;
pub use reported::Reported;

use crate::auth::AuthStore;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread;
use std::time::Duration;

use crate::cancel;
use crate::tools::timeout;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use saddle_cap::Scope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Api {
    Anthropic,
    Openai,
    #[serde(rename = "openai-responses")]
    OpenaiResponses,
}

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: Cow<'static, str>,
    pub description: Cow<'static, str>,
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Result<Value, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Part {
    Text(String),
    Call(ToolCall),
    Image(Image),
    Native { api: Api, block: Value },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub principal: String,
    pub scope: Scope,
    pub call_id: String,
    pub component_sha256: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ToolResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Box<Provenance>>,
    pub id: String,
    pub content: String,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub root: String,
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contribution {
    pub id: String,
    pub corrects: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<Diagnostic>,
    pub provenance: Provenance,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Item {
    Contribution(Box<Contribution>),
    User(String),
    Attached { text: String, images: Vec<Image> },
    Assistant(Vec<Part>),
    ToolResults(Vec<ToolResult>),
    Summary(String),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Omitted {
    pub pictures: usize,
    pub bytes: usize,
}

pub fn within_image_budget(items: &[Item], budget: usize) -> Option<(Vec<Item>, Omitted)> {
    if image_bytes(items) <= budget {
        return None;
    }
    let mut left = budget;
    let mut omitted = Omitted::default();
    let mut out: Vec<Item> = Vec::with_capacity(items.len());
    for item in items.iter().rev() {
        out.push(trimmed(item, budget, &mut left, &mut omitted));
    }
    out.reverse();
    Some((out, omitted))
}

fn image_bytes(items: &[Item]) -> usize {
    items.iter().map(item_image_bytes).sum()
}

fn item_image_bytes(item: &Item) -> usize {
    match item {
        Item::Attached { images, .. } => images.iter().map(|i| i.data.len()).sum(),
        Item::ToolResults(results) => results
            .iter()
            .flat_map(|r| &r.images)
            .map(|i| i.data.len())
            .sum(),
        Item::Assistant(parts) => parts
            .iter()
            .filter_map(|p| match p {
                Part::Image(image) => Some(image.data.len()),
                _ => None,
            })
            .sum(),
        _ => 0,
    }
}

fn trimmed(item: &Item, budget: usize, left: &mut usize, omitted: &mut Omitted) -> Item {
    match item {
        Item::Attached { text, images } => {
            let (kept, notes) = keep(images, budget, left, omitted);
            Item::Attached {
                text: with_notes(text, &notes),
                images: kept,
            }
        }
        Item::ToolResults(results) => Item::ToolResults(
            results
                .iter()
                .map(|r| {
                    if r.images.is_empty() {
                        return r.clone();
                    }
                    let (kept, notes) = keep(&r.images, budget, left, omitted);
                    let mut r = r.clone();
                    r.content = with_notes(&r.content, &notes);
                    r.images = kept;
                    r
                })
                .collect(),
        ),
        Item::Assistant(parts) => Item::Assistant(
            parts
                .iter()
                .map(|p| match p {
                    Part::Image(image) if image.data.len() > *left => {
                        *omitted = Omitted {
                            pictures: omitted.pictures + 1,
                            bytes: omitted.bytes + image.data.len(),
                        };
                        Part::Text(omitted_note(image, budget))
                    }
                    Part::Image(image) => {
                        *left -= image.data.len();
                        Part::Image(image.clone())
                    }
                    other => other.clone(),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn keep(
    images: &[Image],
    budget: usize,
    left: &mut usize,
    omitted: &mut Omitted,
) -> (Vec<Image>, Vec<String>) {
    let mut kept = Vec::new();
    let mut notes = Vec::new();
    for image in images {
        if image.data.len() <= *left {
            *left -= image.data.len();
            kept.push(image.clone());
        } else {
            omitted.pictures += 1;
            omitted.bytes += image.data.len();
            notes.push(omitted_note(image, budget));
        }
    }
    (kept, notes)
}

fn with_notes(text: &str, notes: &[String]) -> String {
    if notes.is_empty() {
        return text.to_owned();
    }
    if text.is_empty() {
        return notes.join("\n");
    }
    format!("{text}\n{}", notes.join("\n"))
}

fn omitted_note(image: &Image, budget: usize) -> String {
    format!(
        "[picture {} ({}×{}) was not sent: this request carries the newest pictures, up to {}]",
        image.name,
        image.width,
        image.height,
        human_size(budget)
    )
}

pub fn human_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        return format!("{} MiB", bytes / (1024 * 1024));
    }
    format!("{} KiB", bytes.div_ceil(1024))
}

pub fn is_too_large(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("returned 413")
        || [
            "payload too large",
            "request entity too large",
            "request too large",
            "entity too large",
            "content too large",
            "body too large",
            "request size",
        ]
        .iter()
        .any(|phrase| m.contains(phrase))
}

pub fn visible_tool_text<'a>(text: &'a str, provenance: Option<&Provenance>) -> &'a str {
    let Some(origin) = provenance else {
        return text;
    };
    let Some(id) = origin.principal.strip_prefix("plugin.") else {
        return text;
    };
    let Some((header, body)) = text.split_once('\n') else {
        return text;
    };
    let Some(header) = header.strip_prefix("<plugin-output-") else {
        return text;
    };
    let suffix = format!(" plugin=\"{id}\">");
    let Some(nonce) = header.strip_suffix(suffix.as_str()) else {
        return text;
    };
    if nonce.is_empty() || !nonce.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return text;
    }
    body.strip_suffix(&format!("\n</plugin-output-{nonce}>"))
        .unwrap_or(text)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub media_type: String,
    pub data: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
}

#[cfg(test)]
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

impl Image {
    pub fn read(path: &Path, data: Vec<u8>) -> Result<Self, String> {
        if data.len() > budgets::image_bytes() {
            return Err(format!(
                "{} KiB is over the {} KiB limit for one image",
                data.len() / 1024,
                budgets::image_bytes() / 1024
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
}

fn sniff(d: &[u8]) -> Option<(&'static str, u32, u32)> {
    if d.starts_with(b"\x89PNG\r\n\x1a\n") {
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
        let mut i = 2;
        while i + 9 < d.len() {
            if d[i] != 0xff {
                return None;
            }
            let marker = d[i + 1];
            let len = u16::from_be_bytes(d.get(i + 2..i + 4)?.try_into().ok()?) as usize;
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

pub fn base64_encode(bytes: &[u8]) -> String {
    general_purpose::STANDARD.encode(bytes)
}

impl Item {
    pub fn prompt(&self) -> Option<&str> {
        match self {
            Item::User(text) => Some(text),
            Item::Attached { text, .. } => Some(text),
            _ => None,
        }
    }
}

pub fn reasoning_text(part: &Part) -> Option<String> {
    let Part::Native { api, block } = part else {
        return None;
    };
    let text = match api {
        Api::Anthropic => (block["type"] == "thinking")
            .then(|| block["thinking"].as_str())
            .flatten()?,
        Api::Openai => block["reasoning_content"]
            .as_str()
            .or(block["reasoning"].as_str())?,
        Api::OpenaiResponses => return reasoning_summary(block),
    };
    (!text.is_empty()).then(|| text.to_owned())
}

fn reasoning_summary(block: &Value) -> Option<String> {
    ["summary", "content"].iter().copied().find_map(|key| {
        let text = block[key]
            .as_array()?
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
    })
}

pub const SUMMARY_INTRO: &str =
    "The conversation so far was compacted. This summary replaces the earlier messages:\n\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

impl Price {
    pub fn is_known(&self) -> bool {
        self.input > 0.0 || self.output > 0.0
    }

    pub fn cost(&self, u: &Usage) -> f64 {
        let read = self.cache_read.unwrap_or(self.input);
        let write = self.cache_write.unwrap_or(self.input);
        u.input as f64 * self.input
            + u.output as f64 * self.output
            + u.cache_read as f64 * read
            + u.cache_write as f64 * write
    }
}

pub const EFFORTS: &[&str] = &["default", "low", "medium", "high"];

pub fn next_effort(current: Option<&str>) -> Option<String> {
    let at = current
        .and_then(|c| EFFORTS.iter().position(|e| *e == c))
        .unwrap_or(0);
    let next = EFFORTS[(at + 1) % EFFORTS.len()];
    (next != "default").then(|| next.to_owned())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct PriceEntry {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub unit: Option<String>,
}

impl PriceEntry {
    fn to_price(&self) -> Option<Price> {
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

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quota {
    pub windows: Vec<QuotaWindow>,
    pub tokens: Option<u64>,
    pub tokens_limit: Option<u64>,
    pub requests: Option<u64>,
    pub requests_limit: Option<u64>,
    pub credits: Option<f64>,
    pub reset: Option<String>,
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaWindow {
    pub label: String,
    pub percent: f64,
    pub reset: Option<String>,
}

impl Quota {
    pub fn is_empty(&self) -> bool {
        self.tokens.is_none()
            && self.requests.is_none()
            && self.credits.is_none()
            && self.percent.is_none()
            && self.windows.is_empty()
    }
}

pub fn reset_left(spelled: &str, now: SystemTime) -> String {
    let Ok(at) = humantime::parse_rfc3339(spelled) else {
        return spelled.to_owned();
    };
    let left = at.duration_since(now).map(|d| d.as_secs()).unwrap_or(0);
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
    pub stop_raw: String,
    pub model: String,
    pub usage: Usage,
    pub reasoning_tokens: u64,
    pub dropped_reasoning: u64,
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
    pub session: &'a str,
}

pub trait Sink {
    fn text(&mut self, s: &str);
    fn note(&mut self, s: &str);
    fn reasoning(&mut self, _s: &str) {}
    fn quota(&mut self, _quota: &Quota) {}
    fn cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug)]
pub struct Cancelled;

impl Display for Cancelled {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str("cancelled")
    }
}

impl Error for Cancelled {}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct ProviderEntry {
    pub api: Option<Api>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u64>,
    pub effort: Option<String>,
    pub pricing: Option<PriceEntry>,
    pub usage_url: Option<String>,
    pub search_url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub cache_retention: Option<CacheRetention>,
    pub cache_affinity: Option<bool>,
    pub key_header: Option<String>,
    pub auth: Option<Auth>,
    pub oauth: Option<Flow>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheRetention {
    #[default]
    Short,
    Long,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Auth {
    #[default]
    ApiKey,
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
    pub price: Option<Price>,
    pub usage_url: Option<String>,
    pub search_url: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub key_header: Option<String>,
    pub cache_retention: CacheRetention,
    pub cache_affinity: bool,
    pub auth: Auth,
    pub oauth: Option<Flow>,
}

fn presets() -> BTreeMap<&'static str, ProviderEntry> {
    let entry = |base: &str, key: &str| ProviderEntry {
        api: Some(Api::Openai),
        base_url: Some(base.into()),
        api_key_env: Some(key.into()),
        ..ProviderEntry::default()
    };
    BTreeMap::from([
        (
            "synthetic",
            ProviderEntry {
                usage_url: Some("https://api.synthetic.new/v2/quotas".into()),
                search_url: Some("https://api.synthetic.new/v2/search".into()),
                ..entry("https://api.synthetic.new/openai/v1", "SYNTHETIC_API_KEY")
            },
        ),
        (
            "openai",
            entry("https://api.openai.com/v1", "OPENAI_API_KEY"),
        ),
        (
            "openai-chatgpt",
            ProviderEntry {
                api: Some(Api::OpenaiResponses),
                base_url: Some(oauth::OPENAI_API_BASE.into()),
                usage_url: Some("https://chatgpt.com/backend-api/wham/usage".into()),
                cache_affinity: Some(true),
                auth: Some(Auth::Oauth),
                oauth: Some(oauth::Flow {
                    method: Some(oauth::Method::Device),
                    authorize_url: Some(oauth::OPENAI_AUTHORIZE_URL.into()),
                    token_url: Some(oauth::OPENAI_TOKEN_URL.into()),
                    client_id: Some(oauth::OPENAI_CLIENT_ID.into()),
                    scopes: Some(oauth::OPENAI_SCOPES.into()),
                    redirect_uri: Some(oauth::OPENAI_REDIRECT_URI.into()),
                    device_redirect_uri: Some(oauth::OPENAI_DEVICE_REDIRECT_URI.into()),
                    redirect_port: Some(oauth::OPENAI_REDIRECT_PORT),
                    device_code_url: Some(oauth::OPENAI_DEVICE_CODE_URL.into()),
                    device_token_url: Some(oauth::OPENAI_DEVICE_TOKEN_URL.into()),
                    verification_url: Some(oauth::OPENAI_VERIFICATION_URL.into()),
                    ..oauth::Flow::default()
                }),
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
            "anthropic",
            ProviderEntry {
                api: Some(Api::Anthropic),
                ..entry("https://api.anthropic.com", "ANTHROPIC_API_KEY")
            },
        ),
    ])
}

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
        cache_retention: mine
            .and_then(|e| e.cache_retention)
            .or_else(|| preset.and_then(|e| e.cache_retention))
            .unwrap_or_default(),
        cache_affinity: mine
            .and_then(|e| e.cache_affinity)
            .or_else(|| preset.and_then(|e| e.cache_affinity))
            .unwrap_or(false),
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
            cache_retention: m.cache_retention.or(base.cache_retention),
            cache_affinity: m.cache_affinity.or(base.cache_affinity),
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

pub const DEFAULT_CONTEXT_WINDOW: u64 = 128_000;

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

pub fn known_window(model: &str) -> Option<u64> {
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

pub fn context_window(config: &ProviderConfig, reported: &Reported) -> u64 {
    window_and_source(config, reported).0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    Config,
    Endpoint,
    Published,
    Default,
}

impl Display for WindowSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(match self {
            WindowSource::Config => "set in config.toml",
            WindowSource::Endpoint => "stated by the provider",
            WindowSource::Published => "published for this model family",
            WindowSource::Default => "the default, as nothing knows this model",
        })
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stated {
    pub window: Option<u64>,
    pub price: Option<Price>,
}

fn openai_api(base_url: &str) -> bool {
    host_of(base_url).eq_ignore_ascii_case("api.openai.com")
}

pub fn codex_backend(base_url: &str) -> bool {
    host_of(base_url).eq_ignore_ascii_case("chatgpt.com")
}

fn host_of(base_url: &str) -> &str {
    let rest = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest);
    rest.split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
}

pub fn routes_cache(config: &ProviderConfig) -> bool {
    config.cache_affinity || openai_api(&config.base_url)
}

pub fn prompt_cache_key<'a>(config: &ProviderConfig, session: &'a str) -> Option<&'a str> {
    (routes_cache(config) && !session.is_empty()).then_some(session)
}

pub fn prompt_cache_retention(config: &ProviderConfig) -> Option<&'static str> {
    (routes_cache(config)
        && config.auth == Auth::ApiKey
        && config.cache_retention == CacheRetention::Long)
        .then_some("24h")
}

pub fn codex_originator(config: &ProviderConfig) -> Option<&'static str> {
    (config.api == Api::OpenaiResponses && codex_backend(&config.base_url))
        .then_some("codex_cli_rs")
}

pub fn affinity_headers(config: &ProviderConfig, session: &str) -> Vec<(String, String)> {
    if !routes_cache(config) || session.is_empty() {
        return Vec::new();
    }
    ["session-id", "x-client-request-id"]
        .into_iter()
        .map(|name| (name.to_owned(), session.to_owned()))
        .collect()
}

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
        .and_then(|var| env::var(var).ok())
        .filter(|k| !k.is_empty())
        .or_else(|| stored_key.filter(|k| !k.is_empty()).map(str::to_owned))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTarget {
    pub provider: String,
    pub url: String,
    pub key_env: Option<String>,
}

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
    pub response_bytes: Option<usize>,
    pub retry_requests: bool,
    window: u64,
    source: WindowSource,
    price: Option<Price>,
}

impl Provider {
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
            .or_else(|| reported.price(&config.base_url, &config.model))
            .or_else(|| openai_account::price(&config));
        Self {
            oauth: None,
            name: name.to_owned(),
            config,
            key,
            agent: http::agent(),
            response_bytes: None,
            retry_requests: true,
            window,
            source,
            price,
        }
    }

    pub fn twin(&self) -> Self {
        Self {
            name: self.name.clone(),
            config: self.config.clone(),
            key: self.key.clone(),
            oauth: self.oauth.clone(),
            agent: http::agent(),
            response_bytes: self.response_bytes,
            retry_requests: self.retry_requests,
            window: self.window,
            source: self.source,
            price: self.price,
        }
    }

    pub fn window(&self) -> u64 {
        self.window
    }

    pub fn window_source(&self) -> WindowSource {
        self.source
    }

    pub fn price(&self) -> Option<Price> {
        self.price
    }

    pub fn set_price(&mut self, price: Price) {
        if self.config.price.is_none() && price.is_known() {
            self.price = Some(price);
        }
    }

    pub fn set_effort(&mut self, effort: Option<String>) {
        self.config.effort = effort;
    }

    pub fn effort(&self) -> Option<&str> {
        self.config.effort.as_deref()
    }

    pub fn set_window(&mut self, window: u64) {
        if self.config.context_window.is_some() {
            return;
        }
        self.window = window;
        self.source = WindowSource::Endpoint;
    }

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

    pub fn with_oauth(&mut self, flow: oauth::Flow, tokens: Tokens, store: PathBuf) {
        self.oauth = Some(OAuthState {
            flow,
            tokens: Arc::new(Mutex::new(tokens)),
            store,
            provider: self.name.clone(),
        });
    }

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

    pub fn request_headers_for(&self, session: &str) -> Vec<(String, String)> {
        let mut headers = self.request_headers();
        let add = |headers: &mut Vec<(String, String)>, name: &str, value: &str| {
            if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
                headers.push((name.to_owned(), value.to_owned()));
            }
        };
        if let Some(originator) = codex_originator(&self.config) {
            add(&mut headers, "originator", originator);
        }
        for (name, value) in affinity_headers(&self.config, session) {
            add(&mut headers, &name, &value);
        }
        headers
    }

    pub fn usage(&self, url: &str) -> Result<Value, ModelsError> {
        self.refresh_tokens()
            .map_err(|e| ModelsError::Failed(format!("{e:#}")))?;
        http::fetch_usage_headers(url, &self.request_headers())
    }

    pub fn refresh_tokens(&self) -> Result<()> {
        let Some(oauth) = &self.oauth else {
            return Ok(());
        };
        let settings = oauth.flow.settings()?;
        let mut tokens = loop {
            timeout::remaining(Duration::from_secs(60))?;
            if cancel::requested() {
                return Err(Cancelled.into());
            }
            match oauth.tokens.try_lock() {
                Ok(tokens) => break tokens,
                Err(TryLockError::Poisoned(_)) => bail!("the OAuth token state was poisoned"),
                Err(TryLockError::WouldBlock) => {
                    thread::sleep(timeout::remaining(Duration::from_millis(10))?);
                }
            }
        };
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
    use serde_json::json;
    use std::path::Path;

    fn picture(name: &str, bytes: usize) -> Image {
        Image {
            media_type: "image/png".into(),
            data: "A".repeat(bytes),
            name: name.into(),
            width: 100,
            height: 50,
        }
    }

    fn attached(text: &str, pictures: &[(&str, usize)]) -> Item {
        Item::Attached {
            text: text.into(),
            images: pictures.iter().map(|(n, b)| picture(n, *b)).collect(),
        }
    }

    #[test]
    fn a_conversation_whose_pictures_fit_is_sent_unchanged() {
        let items = [attached("look", &[("a.png", 100)])];
        assert!(within_image_budget(&items, 100).is_none());
        assert!(within_image_budget(&items, 99).is_some());
    }

    #[test]
    fn the_newest_pictures_are_kept_and_the_older_ones_are_replaced_by_a_note() {
        let items = [
            attached("first", &[("old.png", 60)]),
            Item::User("and now".into()),
            attached("second", &[("new.png", 60)]),
            Item::Assistant(vec![Part::Text("ok".into())]),
        ];
        let (trimmed, omitted) = within_image_budget(&items, 60).expect("over the budget");
        assert_eq!(omitted.pictures, 1);
        assert_eq!(omitted.bytes, 60);
        match &trimmed[0] {
            Item::Attached { text, images } => {
                assert!(images.is_empty(), "the older picture is not sent");
                assert!(text.starts_with("first\n"));
                assert!(text.contains("old.png"), "{text}");
                assert!(text.contains("was not sent"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        match &trimmed[2] {
            Item::Attached { images, .. } => assert_eq!(images.len(), 1, "the newest stays"),
            other => panic!("{other:?}"),
        }
        assert_eq!(trimmed[1], Item::User("and now".into()));
        assert_eq!(trimmed.len(), items.len());
    }

    #[test]
    fn a_tool_result_that_lost_its_picture_says_so_in_its_own_text() {
        let items = [
            Item::ToolResults(vec![ToolResult {
                provenance: None,
                id: "c1".into(),
                content: "shot saved".into(),
                is_error: false,
                images: vec![picture("shot.png", 60)],
                diff: None,
            }]),
            attached("and this", &[("new.png", 60)]),
        ];
        let (trimmed, omitted) = within_image_budget(&items, 60).expect("over the budget");
        assert_eq!(omitted.pictures, 1);
        let Item::ToolResults(results) = &trimmed[0] else {
            panic!("{trimmed:?}")
        };
        assert!(results[0].images.is_empty());
        assert!(results[0].content.starts_with("shot saved\n"));
        assert!(results[0].content.contains("shot.png"));
    }

    #[test]
    fn a_body_the_provider_called_too_large_is_recognised_but_a_long_prompt_is_not() {
        assert!(is_too_large("synthetic returned 413: Payload Too Large"));
        assert!(is_too_large(
            "api returned 400: request body too large for this endpoint"
        ));
        assert!(!is_too_large(
            "api returned 400: prompt is too long: 200000 tokens"
        ));
        assert!(!is_too_large("synthetic returned 429: slow down"));
    }

    #[test]
    fn human_tool_rendering_unframes_only_with_matching_host_provenance() {
        let framed =
            "<plugin-output-ab12 plugin=\"standard-edit\">\nchanged\n</plugin-output-ab12>";
        let mut origin = Provenance {
            principal: "plugin.standard-edit".into(),
            scope: Scope::Tool,
            call_id: "c".into(),
            component_sha256: "hash".into(),
            version: "v".into(),
        };
        assert_eq!(visible_tool_text(framed, Some(&origin)), "changed");
        assert_eq!(visible_tool_text(framed, None), framed);
        origin.principal = "plugin.other".into();
        assert_eq!(visible_tool_text(framed, Some(&origin)), framed);
    }

    #[test]
    fn the_prose_in_a_reasoning_block_depends_on_the_surface_that_sent_it() {
        let anthropic = Part::Native {
            api: Api::Anthropic,
            block: json!({"type":"thinking","thinking":"check the offset","signature":"sig"}),
        };
        let redacted = Part::Native {
            api: Api::Anthropic,
            block: json!({"type":"redacted_thinking","data":"AAAA"}),
        };
        let chat = Part::Native {
            api: Api::Openai,
            block: json!({"reasoning_content":"count the rows"}),
        };
        let other_name = Part::Native {
            api: Api::Openai,
            block: json!({"reasoning":"count the columns"}),
        };
        let responses = Part::Native {
            api: Api::OpenaiResponses,
            block: json!({
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type":"summary_text","text":"one"},{"type":"summary_text","text":"two"}],
                "encrypted_content": "gAAAA",
            }),
        };
        let encrypted_only = Part::Native {
            api: Api::OpenaiResponses,
            block: json!({"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"gAAAA"}),
        };
        assert_eq!(
            reasoning_text(&anthropic).as_deref(),
            Some("check the offset")
        );
        assert_eq!(reasoning_text(&redacted), None);
        assert_eq!(reasoning_text(&chat).as_deref(), Some("count the rows"));
        assert_eq!(
            reasoning_text(&other_name).as_deref(),
            Some("count the columns")
        );
        assert_eq!(reasoning_text(&responses).as_deref(), Some("one\n\ntwo"));
        assert_eq!(reasoning_text(&encrypted_only), None);
        assert_eq!(reasoning_text(&Part::Text("not reasoning".into())), None);
    }

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
            cache_retention: CacheRetention::Short,
            cache_affinity: false,
            auth: Auth::ApiKey,
            oauth: None,
        }
    }

    #[test]
    fn a_conversation_routes_its_requests_to_one_cache_only_on_openais_own_api() {
        let openai = config("gpt", "https://api.openai.com/v1");
        assert_eq!(prompt_cache_key(&openai, "s1"), Some("s1"));
        assert_eq!(
            prompt_cache_retention(&openai),
            None,
            "short is the default: a request that says nothing takes the provider's own lifetime"
        );
        assert_eq!(
            affinity_headers(&openai, "s1"),
            vec![
                ("session-id".to_owned(), "s1".to_owned()),
                ("x-client-request-id".to_owned(), "s1".to_owned()),
            ]
        );

        let long = ProviderConfig {
            cache_retention: CacheRetention::Long,
            ..config("gpt", "https://api.openai.com/v1")
        };
        assert_eq!(prompt_cache_retention(&long), Some("24h"));
        let signed_in = ProviderConfig {
            auth: Auth::Oauth,
            ..long.clone()
        };
        assert_eq!(prompt_cache_retention(&signed_in), None);
        assert_eq!(
            prompt_cache_key(&signed_in, "s1"),
            Some("s1"),
            "the key is accepted"
        );

        for other in [
            "https://api.synthetic.new/openai/v1",
            "https://gateway.test/v1",
            "http://localhost:8080/v1",
        ] {
            let cfg = config("m", other);
            assert_eq!(prompt_cache_key(&cfg, "s1"), None, "{other}");
            assert_eq!(prompt_cache_retention(&cfg), None, "{other}");
            assert!(affinity_headers(&cfg, "s1").is_empty(), "{other}");
        }

        assert_eq!(prompt_cache_key(&openai, ""), None);
        assert!(affinity_headers(&openai, "").is_empty());

        let subscription = ProviderConfig {
            cache_affinity: true,
            ..config("m", "https://chatgpt.com/backend-api/codex")
        };
        assert_eq!(prompt_cache_key(&subscription, "s1"), Some("s1"));
        assert_eq!(affinity_headers(&subscription, "s1").len(), 2);
    }

    #[test]
    fn the_affinity_headers_ride_on_a_model_request_and_nowhere_else() {
        let p = Provider::new(
            "openai",
            config("gpt", "https://api.openai.com/v1"),
            Some("sk-1"),
            &Reported::default(),
        );
        let sent = p.request_headers_for("s1");
        assert!(sent.contains(&("session-id".to_owned(), "s1".to_owned())));
        assert!(sent.contains(&("x-client-request-id".to_owned(), "s1".to_owned())));
        assert!(
            sent.contains(&("authorization".to_owned(), "Bearer sk-1".to_owned())),
            "the credential is still there"
        );
        assert!(
            !p.request_headers()
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("session-id"))
        );

        let other = Provider::new(
            "synthetic",
            config("m", "https://api.synthetic.new/openai/v1"),
            Some("k"),
            &Reported::default(),
        );
        assert!(
            !other
                .request_headers_for("s1")
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("session-id"))
        );

        let codex = ProviderConfig {
            api: Api::OpenaiResponses,
            cache_affinity: true,
            ..config("m", "https://chatgpt.com/backend-api/codex")
        };
        let sent = Provider::new("openai-chatgpt", codex, Some("at"), &Reported::default())
            .request_headers_for("s1");
        assert!(sent.contains(&("originator".to_owned(), "codex_cli_rs".to_owned())));
        assert!(sent.contains(&("session-id".to_owned(), "s1".to_owned())));
        assert!(
            !p.request_headers_for("s1")
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("originator")),
            "the API-key endpoint does not check one"
        );

        let mut headers = BTreeMap::new();
        headers.insert("session-id".to_owned(), "mine".to_owned());
        let own = Provider::new(
            "openai",
            ProviderConfig {
                headers,
                ..config("gpt", "https://api.openai.com/v1")
            },
            Some("sk-1"),
            &Reported::default(),
        );
        let sent = own.request_headers_for("s1");
        assert_eq!(
            sent.iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case("session-id"))
                .count(),
            1
        );
        assert!(sent.contains(&("session-id".to_owned(), "mine".to_owned())));
    }

    #[test]
    fn the_long_cache_lifetime_is_a_configuration_choice() {
        let user: BTreeMap<String, ProviderEntry> = toml::from_str(
            r#"
[openai]
cache-retention = "long"
"#,
        )
        .unwrap();
        assert_eq!(
            endpoint("openai", &user).unwrap().cache_retention,
            CacheRetention::Long
        );
        assert_eq!(
            endpoint("synthetic", &BTreeMap::new())
                .unwrap()
                .cache_retention,
            CacheRetention::Short
        );
        assert!(
            endpoint("openai-chatgpt", &BTreeMap::new())
                .unwrap()
                .cache_affinity
        );
        let off: BTreeMap<String, ProviderEntry> = toml::from_str(
            r#"
[openai-chatgpt]
cache-affinity = false
"#,
        )
        .unwrap();
        assert!(!endpoint("openai-chatgpt", &off).unwrap().cache_affinity);
    }

    #[test]
    fn a_provider_says_where_its_key_and_its_own_headers_go() {
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

        let cfg = ProviderConfig {
            key_header: Some("api-key".to_owned()),
            ..config("m", "https://x.test/v1")
        };
        let sent = request_headers(&cfg, Some("secret"));
        assert_eq!(sent, vec![("api-key".to_owned(), "secret".to_owned())]);

        let cfg = ProviderConfig {
            api: Api::Anthropic,
            ..config("m", "https://x.test")
        };
        assert_eq!(
            request_headers(&cfg, Some("k")),
            vec![("x-api-key".to_owned(), "k".to_owned())]
        );

        assert!(request_headers(&config("m", "http://localhost:8080"), None).is_empty());

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
        let names = builtin_names();
        assert!(names.iter().any(|n| n == "anthropic"), "{names:?}");
        let official = endpoint("anthropic", &BTreeMap::new()).unwrap();
        assert_eq!(official.api, Api::Anthropic);
        assert_eq!(official.base_url, "https://api.anthropic.com");
        assert_eq!(official.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn the_synthetic_preset_names_its_search() {
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
        let c = endpoint("per-million", &user).unwrap();
        assert_eq!(c.price.map(|p| p.input), Some(3e-6));
        let c = endpoint("per-token", &user).unwrap();
        assert_eq!(c.price.map(|p| p.input), Some(3e-6));
    }

    #[test]
    fn an_image_is_recognised_by_its_bytes_not_its_name() {
        let png = general_purpose::STANDARD.decode(PNG_1X1).unwrap();
        let image = Image::read(Path::new("shot.txt"), png.clone()).unwrap();
        assert_eq!(image.media_type, "image/png");
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.name, "shot.txt");
        assert_eq!(general_purpose::STANDARD.decode(&image.data).unwrap(), png);

        let gif = general_purpose::STANDARD.decode(GIF_1X1).unwrap();
        let image = Image::read(Path::new("x.png"), gif).unwrap();
        assert_eq!(image.media_type, "image/gif");
        assert_eq!((image.width, image.height), (1, 1));

        let e = Image::read(Path::new("a.txt"), b"hello".to_vec()).unwrap_err();
        assert!(e.contains("not a PNG, JPEG, GIF or WebP"), "{e}");
        let mut big = png.clone();
        big.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        let e = Image::read(Path::new("big.png"), big).unwrap_err();
        assert!(e.contains("over the"), "{e}");
    }

    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const GIF_1X1: &str = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

    #[test]
    fn a_jpeg_size_is_found_by_walking_its_segments() {
        let mut d = vec![0xff, 0xd8];
        d.extend_from_slice(&[0xff, 0xe0, 0x00, 0x10]);
        d.extend_from_slice(&[0u8; 14]);
        d.extend_from_slice(&[0xff, 0xc0, 0x00, 0x11, 0x08, 0x00, 0x64, 0x00, 0xc8]);
        d.extend_from_slice(&[0u8; 4]);
        let image = Image::read(Path::new("a.jpg"), d).unwrap();
        assert_eq!(image.media_type, "image/jpeg");
        assert_eq!((image.width, image.height), (200, 100));
    }

    #[test]
    fn the_reasoning_levels_cycle_and_wrap() {
        assert_eq!(next_effort(None), Some("low".to_owned()));
        assert_eq!(next_effort(Some("low")), Some("medium".to_owned()));
        assert_eq!(next_effort(Some("medium")), Some("high".to_owned()));
        assert_eq!(next_effort(Some("high")), None);
        assert_eq!(next_effort(Some("max")), Some("low".to_owned()));
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
        let c = resolve("synthetic", &user, Some("hf:x")).unwrap();
        assert_eq!(
            c.usage_url.as_deref(),
            Some("https://api.synthetic.new/v2/quotas")
        );
        let user: BTreeMap<String, ProviderEntry> =
            toml::from_str("[synthetic]\nusage-url = \"\"\n").unwrap();
        let c = resolve("synthetic", &user, Some("hf:x")).unwrap();
        assert_eq!(c.usage_url, None);
    }

    #[test]
    fn the_most_specific_family_names_the_window() {
        assert_eq!(known_window("hf:Qwen/Qwen3-Coder-30B"), Some(262_144));
        assert_eq!(known_window("Qwen3-32B"), None);
        assert_eq!(known_window("glm-4.6"), Some(200_000));
        assert_eq!(
            known_window("hf:zai-org/GLM-5.3"),
            Some(524_288),
            "glm-5 or glm-4.6: the longer match wins, and both are known"
        );
        assert_eq!(
            known_window("hf:deepseek-ai/DeepSeek-V4.1-Flash-1017"),
            Some(524_288)
        );
        assert_eq!(known_window("gpt-oss-120b"), Some(131_072));
    }
}
