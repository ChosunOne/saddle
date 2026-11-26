use std::cmp::Reverse;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use ureq::http::HeaderMap;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};

use super::retry::Transient;
use super::{
    Api, Cancelled, Price, Provider, ProviderConfig, Quota, Sink, anthropic, openai_account,
    responses, sse,
};

const ATTEMPTS: u32 = 4;
const BODY_LIMIT: u64 = 512 * 1024 * 1024;

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .timeout_per_read(Some(Duration::from_secs(300)))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

pub(crate) enum Msg {
    Event(sse::Event),
    Quota(Quota),
    Note(String),
    Failed(anyhow::Error),
    End,
}

pub(crate) fn event_stream(rx: Receiver<Msg>, stop: Arc<AtomicBool>) -> EventStream {
    EventStream { rx, stop }
}

pub struct EventStream {
    rx: Receiver<Msg>,
    stop: Arc<AtomicBool>,
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl sse::Events for EventStream {
    fn next(&mut self, sink: &mut dyn Sink) -> Result<Option<sse::Event>> {
        loop {
            if sink.cancelled() {
                self.stop.store(true, Ordering::SeqCst);
                return Err(Cancelled.into());
            }
            match self.rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Msg::Event(e)) => return Ok(Some(e)),
                Ok(Msg::Note(note)) => sink.note(&note),
                Ok(Msg::Quota(quota)) => sink.quota(&quota),
                Ok(Msg::Failed(e)) => return Err(e),
                Ok(Msg::End) | Err(RecvTimeoutError::Disconnected) => return Ok(None),
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

pub fn stream(
    p: &Provider,
    url: String,
    headers: Vec<(String, String)>,
    body: &Value,
) -> Result<EventStream> {
    let bytes = serde_json::to_vec(body)?;
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let request = Request {
        agent: p.agent.clone(),
        body_limit: p.response_bytes.map_or(BODY_LIMIT, |bytes| bytes as u64),
        attempts: if p.retry_requests { ATTEMPTS } else { 1 },
        name: p.name.clone(),
        url,
        headers,
        bytes,
        tx: tx.clone(),
        stop: stop.clone(),
    };
    thread::Builder::new()
        .name("saddle-http".into())
        .spawn(move || {
            let last = match request.run() {
                Ok(()) => Msg::End,
                Err(e) => Msg::Failed(e),
            };
            let _ = tx.send(last);
        })
        .context("starting the request thread")?;
    Ok(EventStream { rx, stop })
}

struct Request {
    attempts: u32,
    body_limit: u64,
    agent: ureq::Agent,
    name: String,
    url: String,
    headers: Vec<(String, String)>,
    bytes: Vec<u8>,
    tx: Sender<Msg>,
    stop: Arc<AtomicBool>,
}

impl Request {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn run(&self) -> Result<()> {
        let Some(body) = self.send()? else {
            return Ok(());
        };
        let mut events = sse::Reader::new(body);
        while let Some(event) = events.next_event().map_err(|error| {
            Transient::new(format!("reading the response stream: {error}"))
                .with_reason("HTTP response read failed")
        })? {
            if self.stopped() || self.tx.send(Msg::Event(event)).is_err() {
                break;
            }
        }
        Ok(())
    }

    fn note(&self, text: String) {
        let _ = self.tx.send(Msg::Note(text));
    }

    fn send(&self) -> Result<Option<impl BufRead + use<>>> {
        let url = &self.url;
        let mut attempt = 0;
        loop {
            if self.stopped() {
                return Ok(None);
            }
            attempt += 1;
            let mut req = self
                .agent
                .post(url)
                .header("content-type", "application/json");
            for (k, v) in &self.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            let (status, retry_after, text) = match req.send(&self.bytes[..]) {
                Ok(resp) if resp.status().is_success() => {
                    if let Some(quota) = quota_from(resp.headers()) {
                        let _ = self.tx.send(Msg::Quota(quota));
                    }
                    let reader = resp
                        .into_body()
                        .into_with_config()
                        .limit(self.body_limit)
                        .reader();
                    return Ok(Some(BufReader::new(reader)));
                }
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok());
                    let text = resp
                        .body_mut()
                        .with_config()
                        .limit(self.body_limit.min(64 * 1024))
                        .read_to_string()
                        .unwrap_or_default();
                    (status, retry_after, text)
                }
                Err(e) if transient(&e) && attempt < self.attempts => {
                    let wait = backoff(attempt, None);
                    self.note(format!("{e}; retrying in {}s", wait.as_secs()));
                    self.pause(wait);
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("POST {url}")),
            };
            let message = error_message(&text);
            if retryable(status) && attempt < self.attempts {
                let wait = backoff(attempt, retry_after);
                self.note(format!(
                    "{} returned {status} ({message}); retrying in {}s",
                    self.name,
                    wait.as_secs()
                ));
                self.pause(wait);
                continue;
            }
            bail!("{} returned {status}: {message}", self.name);
        }
    }

    fn pause(&self, wait: Duration) {
        let end = Instant::now() + wait;
        while !self.stopped() {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            thread::sleep(left.min(Duration::from_millis(100)));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    Rejected(String),
    Failed(String),
}

impl ModelsError {
    pub fn reason(&self) -> &str {
        match self {
            Self::Rejected(why) | Self::Failed(why) => why,
        }
    }
}

const NOT_CHAT: &[&str] = &[
    "embed",
    "tts",
    "whisper",
    "dall-e",
    "moderation",
    "transcribe",
    "image",
    "audio",
    "realtime",
    "search",
    "babbage",
    "davinci",
    "sora",
];

#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u64>,
    pub price: Option<Price>,
}

pub fn list_models(p: &Provider) -> Result<Vec<ModelInfo>, ModelsError> {
    let served = served(list_from_endpoint(p)?, p.config.api)?;
    Ok(served)
}

pub fn served(models: Vec<ModelInfo>, api: Api) -> Result<Vec<ModelInfo>, ModelsError> {
    if models.is_empty() && api == Api::OpenaiResponses {
        return Err(ModelsError::Failed(format!(
            "the backend served no models for client version {} — that version may be too old, or \
             the plan may not include any",
            responses::CLIENT_VERSION
        )));
    }
    Ok(models)
}

pub fn listing_url(config: &ProviderConfig) -> String {
    let base = config.base_url.trim_end_matches('/');
    match config.api {
        Api::Openai | Api::Anthropic => format!("{base}/models"),
        Api::OpenaiResponses => {
            format!("{base}/models?client_version={}", responses::CLIENT_VERSION)
        }
    }
}

fn list_from_endpoint(p: &Provider) -> Result<Vec<ModelInfo>, ModelsError> {
    let config = &p.config;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let base = &config.base_url;
    let headers = p.request_headers();
    let mut request = match config.api {
        Api::Openai => agent.get(format!("{base}/models")),
        Api::OpenaiResponses => agent.get(listing_url(config)),
        Api::Anthropic => agent
            .get(format!("{base}/v1/models?limit=1000"))
            .header("anthropic-version", anthropic::VERSION),
    };
    for (name, value) in &headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let mut resp = request
        .call()
        .map_err(|e| ModelsError::Failed(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(8 * 1024 * 1024)
        .read_to_string()
        .map_err(|e| ModelsError::Failed(e.to_string()))?;
    match status {
        401 | 403 => return Err(ModelsError::Rejected(error_message(&text))),
        200..300 => {}
        _ => {
            return Err(ModelsError::Failed(format!(
                "{status}: {}",
                error_message(&text)
            )));
        }
    }
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|_| ModelsError::Failed("the model list is not JSON".into()))?;
    Ok(model_entries(&parsed))
}

pub(super) fn fetch_usage_headers(
    url: &str,
    headers: &[(String, String)],
) -> Result<Value, ModelsError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut req = agent.get(url).header("accept", "application/json");
    for (name, value) in headers {
        req = req.header(name.as_str(), value.as_str());
    }
    let mut resp = req.call().map_err(|e| ModelsError::Failed(e.to_string()))?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|e| ModelsError::Failed(e.to_string()))?;
    match status {
        401 | 403 => return Err(ModelsError::Rejected(error_message(&text))),
        200..300 => {}
        _ => {
            return Err(ModelsError::Failed(format!(
                "{status}: {}",
                error_message(&text)
            )));
        }
    }
    serde_json::from_str(&text)
        .map_err(|_| ModelsError::Failed("the usage page is not JSON".into()))
}

fn flattened(v: &Value, path: &str, out: &mut Vec<(String, f64)>) {
    match v {
        Value::Object(map) => {
            for (k, v) in map {
                let lower = k.to_ascii_lowercase();
                let here = if path.is_empty() {
                    lower.clone()
                } else {
                    format!("{path}.{lower}")
                };
                flattened(v, &here, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                flattened(item, path, out);
            }
        }
        Value::Number(n) => {
            if let Some(n) = n.as_f64() {
                out.push((path.to_owned(), n));
            }
        }
        Value::String(s) => {
            if let Ok(n) = s.trim().parse::<f64>() {
                out.push((path.to_owned(), n));
            }
        }
        _ => {}
    }
}

pub fn page_paths(page: &Value) -> Vec<String> {
    let mut numbers = Vec::new();
    flattened(page, "", &mut numbers);
    let mut paths: Vec<String> = numbers.into_iter().map(|(p, _)| p).collect();
    paths.sort();
    paths.dedup();
    paths
}

fn known_quota(page: &Value) -> Option<Quota> {
    if let Some(percent) = at(page, "planWeeklyLimit.percentRemaining")
        .or_else(|| at(page, "planWeeklyLimit.percent_remaining"))
        .and_then(number)
    {
        return Some(Quota {
            percent: Some(percent),
            reset: at(page, "planWeeklyLimit.nextRegenAt")
                .or_else(|| at(page, "planWeeklyLimit.next_regen_at"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            ..Quota::default()
        });
    }
    let limit = at(page, "subscription.limit").and_then(number)?;
    let used = at(page, "subscription.requests")
        .and_then(number)
        .unwrap_or(0.0);
    Some(Quota {
        requests: Some((limit - used).max(0.0) as u64),
        requests_limit: Some(limit as u64),
        reset: at(page, "subscription.renewsAt")
            .or_else(|| at(page, "subscription.renews_at"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        ..Quota::default()
    })
}

pub fn quota_from_page(page: &Value) -> (Quota, Option<f64>) {
    if let Some(quota) = openai_account::quota(page) {
        return (quota, None);
    }
    if let Some(quota) = known_quota(page) {
        return (quota, None);
    }
    let mut numbers = Vec::new();
    flattened(page, "", &mut numbers);
    let has = |path: &str, word: &str| path.split('.').any(|p| p.contains(word));
    let find = |words: &[&str], context: &[&str]| -> Option<f64> {
        numbers
            .iter()
            .find(|(path, _)| {
                words.iter().any(|w| {
                    path.split('.')
                        .next_back()
                        .is_some_and(|last| last.contains(w))
                }) && (context.is_empty() || context.iter().any(|c| has(path, c)))
            })
            .map(|(_, n)| *n)
    };
    let limit_of = |words: &[&str], context: &[&str]| -> Option<u64> {
        numbers
            .iter()
            .find(|(path, _)| {
                words.iter().any(|w| {
                    path.split('.')
                        .next_back()
                        .is_some_and(|last| last.contains(w))
                }) && (context.is_empty() || context.iter().any(|c| has(path, c)))
            })
            .map(|(_, n)| *n as u64)
    };
    let text = |words: &[&str]| -> Option<String> {
        fn walk(v: &Value, words: &[&str], found: &mut Option<String>) {
            match v {
                Value::Object(map) => {
                    for (k, v) in map {
                        let lower = k.to_ascii_lowercase();
                        if found.is_none()
                            && words.iter().any(|w| lower.contains(w))
                            && let Some(s) = v.as_str()
                        {
                            *found = Some(s.to_owned());
                        }
                        walk(v, words, found);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        walk(item, words, found);
                    }
                }
                _ => {}
            }
        }
        let mut found = None;
        walk(page, words, &mut found);
        found
    };

    let left = |left: Option<f64>, used: Option<f64>, limit: Option<u64>| -> Option<u64> {
        match (left, used, limit) {
            (None, Some(used), Some(limit)) => Some(limit.saturating_sub(used as u64)),
            (left, _, _) => left.map(|n| n as u64),
        }
    };
    let tokens_limit = limit_of(
        &[
            "limit_tokens",
            "token_limit",
            "max_tokens",
            "tokens_total",
            "limit",
            "cap",
        ],
        &["token"],
    );
    let requests_limit = limit_of(
        &[
            "limit_requests",
            "request_limit",
            "requests_total",
            "limit",
            "cap",
        ],
        &["request"],
    );

    let quota = Quota {
        windows: Vec::new(),
        tokens: left(
            find(
                &["remaining_tokens", "tokens_remaining", "tokens_left"],
                &["token"],
            ),
            find(&["used_tokens", "tokens_used", "used"], &["token"]),
            tokens_limit,
        ),
        tokens_limit,
        requests: left(
            find(
                &["remaining_requests", "requests_remaining", "requests_left"],
                &["request"],
            ),
            find(&["used_requests", "requests_used", "used"], &["request"]),
            requests_limit,
        ),
        requests_limit,
        reset: text(&["reset"]),
        credits: find(
            &["remaining", "balance", "credits", "left"],
            &["credit", "balance", "quota", "dollar", "usd"],
        ),
        percent: None,
    };
    let spent = find(
        &["spent", "total_cost", "cost", "usage"],
        &["cost", "spent", "dollar", "usd"],
    );
    (quota, spent)
}

fn stated_window(m: &Value) -> Option<u64> {
    const FIELDS: &[&str] = &[
        "context_window",
        "context_length",
        "max_context_length",
        "max_model_len",
        "max_input_tokens",
        "n_ctx",
    ];
    let stated = FIELDS
        .iter()
        .find_map(|f| m.get(f).and_then(Value::as_u64))
        .or_else(|| m.get("top_provider")?.get("context_length")?.as_u64())
        .or_else(|| m.get("meta")?.get("n_ctx")?.as_u64());
    stated.filter(|n| *n >= 1_000)
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s
            .trim()
            .trim_start_matches(['$', '€', '£'])
            .trim()
            .replace(',', "")
            .parse::<f64>()
            .ok(),
        _ => None,
    }
}

fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(v, |v, key| v.get(key))
        .filter(|v| !v.is_null())
}

fn listing_price(m: &Value) -> Option<Price> {
    stated_price(m).map(|(price, unit)| match unit {
        Unit::Token => price,
        Unit::Million => Price {
            input: price.input / 1e6,
            output: price.output / 1e6,
            cache_read: price.cache_read.map(|p| p / 1e6),
            cache_write: price.cache_write.map(|p| p / 1e6),
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Token,
    Million,
}

fn stated_price(m: &Value) -> Option<(Price, Unit)> {
    let p = ["pricing", "price", "cost", "costs", "billing"]
        .iter()
        .find_map(|k| m.get(*k).filter(|v| v.is_object()))?;
    let nested = |k: &str| p.get(k).filter(|v| v.is_object());
    let (table, mut unit) = match (nested("per_million"), nested("per_token")) {
        (Some(t), _) => (t, Unit::Million),
        (None, Some(t)) => (t, Unit::Token),
        _ => (p, Unit::Token),
    };
    let mut stated = false;
    if let Some(text) = p
        .get("unit")
        .or_else(|| p.get("per"))
        .and_then(Value::as_str)
    {
        let text = text.to_ascii_lowercase();
        if text.contains("million") || text.contains("1m") {
            unit = Unit::Million;
            stated = true;
        } else if text.contains("token") {
            unit = Unit::Token;
            stated = true;
        }
    }
    let field = |names: &[&str]| names.iter().find_map(|n| table.get(*n).and_then(number));
    let price = Price {
        input: field(&[
            "input",
            "prompt",
            "input_price",
            "prompt_price",
            "price_in",
            "in",
        ])
        .unwrap_or(0.0),
        output: field(&[
            "output",
            "completion",
            "output_price",
            "completion_price",
            "price_out",
            "out",
        ])
        .unwrap_or(0.0),
        cache_read: field(&[
            "input_cache_reads",
            "input_cache_read",
            "cache_read",
            "cached_input",
            "cache_read_price",
            "cache_hit",
        ]),
        cache_write: field(&[
            "input_cache_writes",
            "input_cache_write",
            "cache_write",
            "cache_creation",
            "cache_write_price",
        ]),
    };
    if !price.is_known() {
        return None;
    }
    if !stated && unit == Unit::Token {
        let largest = [
            Some(price.input),
            Some(price.output),
            price.cache_read,
            price.cache_write,
        ]
        .into_iter()
        .flatten()
        .fold(0.0f64, f64::max);
        if largest >= 0.001 {
            unit = Unit::Million;
        }
    }
    Some((price, unit))
}

fn model_entries(v: &Value) -> Vec<ModelInfo> {
    let items = v["data"]
        .as_array()
        .or_else(|| v["models"].as_array())
        .or_else(|| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut models: Vec<(u64, ModelInfo)> = items
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str().or_else(|| m["slug"].as_str())?;
            let lower = id.to_ascii_lowercase();
            if NOT_CHAT.iter().any(|n| lower.contains(n)) {
                return None;
            }
            Some((
                m["created"].as_u64().unwrap_or(0),
                ModelInfo {
                    id: id.to_owned(),
                    context_window: stated_window(m),
                    price: listing_price(m),
                },
            ))
        })
        .collect();
    models.sort_by_key(|m| Reverse(m.0));
    models.dedup_by(|a, b| a.1.id == b.1.id);
    models.into_iter().map(|(_, m)| m).collect()
}

fn transient(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed
    )
}

fn retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 429) || status >= 500
}

fn quota_from(headers: &HeaderMap) -> Option<Quota> {
    let mut primary = serde_json::Map::new();
    let mut secondary = serde_json::Map::new();
    for (prefix, window) in [("primary", &mut primary), ("secondary", &mut secondary)] {
        for (suffix, key, multiplier) in [
            ("used-percent", "used_percent", 1.0),
            ("window-minutes", "limit_window_seconds", 60.0),
            ("reset-at", "reset_at", 1.0),
        ] {
            if let Some(n) = headers
                .get(format!("x-codex-{prefix}-{suffix}"))
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|n| n.is_finite() && *n >= 0.0)
            {
                let value = if key == "used_percent" {
                    json!(n)
                } else {
                    json!((n * multiplier) as u64)
                };
                window.insert(key.into(), value);
            }
        }
    }
    if let Some(q) = openai_account::quota(&json!({"rate_limit": {
        "primary_window": primary, "secondary_window": secondary
    }}))
    .filter(|q| !q.is_empty())
    {
        return Some(q);
    }
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let count = |name: &str| get(name).and_then(|v| v.parse::<u64>().ok());
    let first = |names: &[&str]| names.iter().find_map(|n| count(n));
    let reset = |names: &[&str]| names.iter().find_map(|n| get(n)).map(str::to_owned);
    let quota = Quota {
        windows: Vec::new(),
        tokens: first(&[
            "x-ratelimit-remaining-tokens",
            "anthropic-ratelimit-tokens-remaining",
            "anthropic-ratelimit-input-tokens-remaining",
            "x-ratelimit-remaining-token",
        ]),
        tokens_limit: first(&[
            "x-ratelimit-limit-tokens",
            "anthropic-ratelimit-tokens-limit",
            "anthropic-ratelimit-input-tokens-limit",
        ]),
        requests: first(&[
            "x-ratelimit-remaining-requests",
            "anthropic-ratelimit-requests-remaining",
        ]),
        requests_limit: first(&[
            "x-ratelimit-limit-requests",
            "anthropic-ratelimit-requests-limit",
        ]),
        credits: None,
        percent: None,
        reset: reset(&[
            "x-ratelimit-reset-tokens",
            "anthropic-ratelimit-tokens-reset",
            "anthropic-ratelimit-input-tokens-reset",
            "x-ratelimit-reset-requests",
            "anthropic-ratelimit-requests-reset",
        ]),
    };
    (!quota.is_empty()).then_some(quota)
}

fn backoff(attempt: u32, retry_after: Option<u64>) -> Duration {
    let secs = retry_after.unwrap_or(1 << (attempt - 1)).min(60);
    Duration::from_secs(secs)
}

fn error_message(text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let message = parsed.as_ref().and_then(|v| {
        let direct = v["error"]["message"]
            .as_str()
            .or_else(|| v["message"].as_str())
            .or_else(|| v["error"].as_str())
            .or_else(|| v["detail"].as_str());
        if let Some(m) = direct {
            return Some(m.to_owned());
        }
        let first = &v["detail"][0];
        let msg = first["msg"].as_str()?;
        let field = first["loc"]
            .as_array()
            .and_then(|l| l.last())
            .and_then(|f| f.as_str())
            .unwrap_or_default();
        Some(if field.is_empty() {
            msg.to_owned()
        } else {
            format!("{field}: {msg}")
        })
    });
    let mut m = message.unwrap_or_else(|| text.trim().to_owned());
    if m.contains('\n') && m.lines().count() > 1 {
        m = m.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    if m.len() > 2000 {
        let mut cut = 2000;
        while !m.is_char_boundary(cut) {
            cut -= 1;
        }
        m.truncate(cut);
        m.push('…');
    }
    if m.is_empty() { "no message".into() } else { m }
}

#[cfg(test)]
mod tests {
    use super::super::{Api, Auth, ProviderConfig};
    use super::responses::CLIENT_VERSION;
    use super::*;

    #[test]
    fn model_lists_drop_non_chat_models_and_put_new_ones_first() {
        let v: Value = serde_json::from_str(
            r#"{"data": [
                {"id": "gpt-old", "created": 1},
                {"id": "text-embedding-3-large", "created": 9},
                {"id": "gpt-new", "created": 5},
                {"id": "whisper-1", "created": 3}
            ]}"#,
        )
        .unwrap();
        let ids =
            |v: &Value| -> Vec<String> { model_entries(v).into_iter().map(|m| m.id).collect() };
        assert_eq!(ids(&v), ["gpt-new", "gpt-old"]);
        let undated: Value =
            serde_json::from_str(r#"{"data": [{"id": "hf:b"}, {"id": "hf:a"}]}"#).unwrap();
        assert_eq!(ids(&undated), ["hf:b", "hf:a"]);
    }

    #[test]
    fn a_listing_carries_the_window_its_server_states() {
        let v: Value = serde_json::from_str(
            r#"{"data": [
                {"id": "a", "context_length": 262144},
                {"id": "b", "top_provider": {"context_length": 200000}},
                {"id": "c", "meta": {"n_ctx": 131072}},
                {"id": "d", "max_model_len": 32768},
                {"id": "e", "context_window": 0},
                {"id": "f"}
            ]}"#,
        )
        .unwrap();
        let windows: Vec<Option<u64>> = model_entries(&v)
            .into_iter()
            .map(|m| m.context_window)
            .collect();
        assert_eq!(
            windows,
            [
                Some(262_144),
                Some(200_000),
                Some(131_072),
                Some(32_768),
                None,
                None
            ]
        );
    }

    #[test]
    fn a_listing_carries_the_price_its_server_states() {
        let v: Value = serde_json::from_str(
            r#"{"data": [
                {"id": "a", "pricing": {"prompt": "0.000003", "completion": "0.000015",
                                        "input_cache_read": "0.0000003"}},
                {"id": "b", "pricing": {"input": 0.000001, "output": 0.000002}},
                {"id": "c", "pricing": {"prompt": "0", "completion": "0"}},
                {"id": "d"}
            ]}"#,
        )
        .unwrap();
        let prices: Vec<Option<Price>> = model_entries(&v).into_iter().map(|m| m.price).collect();
        assert_eq!(prices[0].unwrap().input, 3e-6);
        assert_eq!(prices[0].unwrap().output, 1.5e-5);
        assert_eq!(prices[0].unwrap().cache_read, Some(3e-7));
        assert_eq!(prices[1].unwrap().input, 1e-6);
        assert_eq!(prices[2], None);
        assert_eq!(prices[3], None);
    }

    #[test]
    fn a_price_is_read_in_the_shape_and_unit_its_server_uses() {
        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "pricing": {"prompt": "0.000003", "completion": "0.000015"}}]}"#,
        )
        .unwrap();
        let p = model_entries(&v)[0].price.unwrap();
        assert_eq!(p.input, 3e-6);
        assert_eq!(p.output, 1.5e-5);

        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "pricing": {"input": 3.0, "output": 15.0, "cache_read": 0.3}}]}"#,
        )
        .unwrap();
        let p = model_entries(&v)[0].price.unwrap();
        assert_eq!(p.input, 3e-6, "per million is divided down to per token");
        assert_eq!(p.output, 1.5e-5);
        assert_eq!(p.cache_read, Some(3e-7));

        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "pricing": {"unit": "per_million_tokens", "input": 2.0, "output": 4.0}}]}"#,
        )
        .unwrap();
        assert_eq!(model_entries(&v)[0].price.unwrap().input, 2e-6);
        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "pricing": {"unit": "per_token", "input": 0.002, "output": 0.004}}]}"#,
        )
        .unwrap();
        assert_eq!(
            model_entries(&v)[0].price.unwrap().input,
            0.002,
            "a stated unit is not overruled by the magnitude"
        );

        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "cost": {"per_million": {"input_price": 1.0, "output_price": 2.0}}}]}"#,
        )
        .unwrap();
        let p = model_entries(&v)[0].price.unwrap();
        assert_eq!(p.input, 1e-6);
        assert_eq!(p.output, 2e-6);
    }

    #[test]
    fn a_listing_priced_with_currency_symbols_is_read() {
        let v: Value = serde_json::from_str(
            r#"{"data": [{
                "id": "hf:deepseek-ai/DeepSeek-V4.1-Flash-1017",
                "provider": "synthetic",
                "display_name": "DeepSeek V4.1 Flash 10/17",
                "context_length": 524288,
                "max_output_length": 65536,
                "pricing": {
                    "prompt": "$0.0000006",
                    "completion": "$0.0000012",
                    "image": "0",
                    "request": "0",
                    "input_cache_reads": "$0.00000003",
                    "input_cache_writes": "0"
                }
            }]}"#,
        )
        .unwrap();
        let m = &model_entries(&v)[0];
        assert_eq!(m.context_window, Some(524_288));
        let price = m.price.expect("a price despite the `$`");
        assert_eq!(price.input, 6e-7, "$0.60 per million");
        assert_eq!(price.output, 1.2e-6, "$1.20 per million");
        assert_eq!(price.cache_read, Some(3e-8), "$0.03 per million");
        assert_eq!(price.cache_write, Some(0.0), "stated as free");
        assert!((price.input * 1e6 - 0.6).abs() < 1e-9);

        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "infinite:small:vision", "context_length": 524288,
                "pricing": {"prompt": "0", "completion": "0", "input_cache_reads": "0"}}]}"#,
        )
        .unwrap();
        let m = &model_entries(&v)[0];
        assert_eq!(m.price, None);
        assert_eq!(m.context_window, Some(524_288));
    }

    #[test]
    fn a_usage_page_is_read_wherever_its_figures_sit() {
        let page: Value = serde_json::from_str(
            r#"{"data": {"credits": {"remaining": 12.5, "resets_at": "2026-12-01T00:00:00Z"}}}"#,
        )
        .unwrap();
        let (q, spent) = quota_from_page(&page);
        assert_eq!(q.credits, Some(12.5));
        assert_eq!(q.reset.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert_eq!(spent, None);

        let page: Value = serde_json::from_str(
            r#"{"usage": {"limits": [
                {"remaining_tokens": 48000, "limit_tokens": 1000000},
                {"remaining_requests": 9, "limit_requests": 100}
            ], "total_cost": "0.4231"}}"#,
        )
        .unwrap();
        let (q, spent) = quota_from_page(&page);
        assert_eq!(q.tokens, Some(48_000));
        assert_eq!(q.tokens_limit, Some(1_000_000));
        assert_eq!(q.requests, Some(9));
        assert_eq!(q.requests_limit, Some(100));
        assert_eq!(spent, Some(0.4231));

        let page: Value = serde_json::from_str(r#"{"ok": true, "balance_updated": null}"#).unwrap();
        let (q, spent) = quota_from_page(&page);
        assert!(q.is_empty());
        assert_eq!(spent, None);
    }

    #[test]
    fn the_synthetic_quota_page_is_read_at_its_paths() {
        let page: Value = serde_json::from_str(
            r#"{"subscription":{"limit":500,"requests":0,
                    "renewsAt":"2026-11-12T13:18:26.671Z"},
                "search":{"hourly":{"limit":250,"requests":0,
                    "renewsAt":"2026-11-12T09:18:26.671Z"}},
                "planWeeklyLimit":{"nextRegenAt":"2026-11-12T08:26:09.000Z",
                    "percentRemaining":94.43746622040817,"nextRegenPercent":2}}"#,
        )
        .unwrap();
        let (q, spent) = quota_from_page(&page);
        assert_eq!(q.percent.map(|p| p.round()), Some(94.0));
        assert_eq!(q.reset.as_deref(), Some("2026-11-12T08:26:09.000Z"));
        assert_eq!(q.requests, None);
        assert_eq!(q.requests_limit, None);
        assert_eq!(q.tokens, None);
        assert_eq!(spent, None);
    }

    #[test]
    fn a_plan_without_a_weekly_figure_states_its_request_pool() {
        let page: Value = serde_json::from_str(
            r#"{"subscription":{"limit":500,"requests":0,
                "renewsAt":"2026-11-12T13:18:26.671Z"},
                "search":{"hourly":{"limit":250,"requests":0}}}"#,
        )
        .unwrap();
        let q = quota_from_page(&page).0;
        assert_eq!(q.requests, Some(500));
        assert_eq!(q.requests_limit, Some(500));
        assert_eq!(q.percent, None);
        assert_eq!(q.reset.as_deref(), Some("2026-11-12T13:18:26.671Z"));
    }

    #[test]
    fn a_page_that_reports_usage_yields_what_is_left() {
        let used: Value =
            serde_json::from_str(r#"{"data": {"requests_used": 40, "requests_limit": 500}}"#)
                .unwrap();
        assert_eq!(quota_from_page(&used).0.requests, Some(460));
        let left: Value = serde_json::from_str(
            r#"{"data": {"requests_remaining": 460, "requests_used": 40,
                "requests_limit": 500}}"#,
        )
        .unwrap();
        assert_eq!(quota_from_page(&left).0.requests, Some(460));
        let uncapped: Value = serde_json::from_str(r#"{"data": {"tokens_used": 40}}"#).unwrap();
        assert_eq!(quota_from_page(&uncapped).0.tokens, None);
    }

    #[test]
    fn an_unrecognised_page_reports_its_field_names_not_its_figures() {
        let page: Value = serde_json::from_str(
            r#"{"data": {"monthly": {"tokens_used": 1234567, "cap": 5000000},
                "reset_at": "2026-12-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let paths = page_paths(&page);
        assert_eq!(paths, ["data.monthly.cap", "data.monthly.tokens_used"]);
        assert!(!paths.iter().any(|p| p.contains("1234567")));
    }

    #[test]
    fn the_quota_headers_are_read_in_both_families() {
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining-tokens", "48000".parse().unwrap());
        h.insert("x-ratelimit-limit-tokens", "1000000".parse().unwrap());
        h.insert("x-ratelimit-reset-tokens", "6m0s".parse().unwrap());
        let q = quota_from(&h).expect("a quota");
        assert_eq!(q.tokens, Some(48_000));
        assert_eq!(q.tokens_limit, Some(1_000_000));
        assert_eq!(q.reset.as_deref(), Some("6m0s"));

        let mut h = HeaderMap::new();
        h.insert(
            "anthropic-ratelimit-input-tokens-remaining",
            "1200".parse().unwrap(),
        );
        h.insert(
            "anthropic-ratelimit-requests-remaining",
            "9".parse().unwrap(),
        );
        let q = quota_from(&h).expect("a quota");
        assert_eq!(q.tokens, Some(1_200));
        assert_eq!(q.requests, Some(9));
        assert_eq!(q.tokens_limit, None);

        assert_eq!(quota_from(&HeaderMap::new()), None);
        let mut other = HeaderMap::new();
        other.insert("retry-after", "3".parse().unwrap());
        assert_eq!(quota_from(&other), None);
    }

    #[test]
    fn codex_headers_preserve_both_windows() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1800000000"),
            ("x-codex-secondary-used-percent", "100"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-at", "1800600000"),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        let quota = quota_from(&headers).unwrap();
        assert_eq!(quota.windows[0].percent, 87.5);
        assert_eq!(quota.windows[0].label, "5h");
        assert_eq!(quota.windows[1].percent, 0.0);
        assert_eq!(quota.windows[1].label, "weekly");
        assert_ne!(quota.windows[0].reset, quota.windows[1].reset);
    }

    #[test]
    fn the_listing_url_is_the_one_the_backend_serves() {
        let mut config = ProviderConfig {
            api: Api::OpenaiResponses,
            base_url: "https://chatgpt.com/backend-api/codex".into(),
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
            auth: Auth::ApiKey,
            oauth: None,
        };
        assert_eq!(
            listing_url(&config),
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                CLIENT_VERSION
            )
        );
        config.base_url = "https://chatgpt.com/backend-api/codex/".into();
        assert!(
            listing_url(&config).contains("/codex/models?"),
            "{}",
            listing_url(&config)
        );
        config.api = Api::Openai;
        assert_eq!(
            listing_url(&config),
            "https://chatgpt.com/backend-api/codex/models"
        );
    }

    #[test]
    fn the_served_catalogue_is_read_from_slugs() {
        let served: Value = serde_json::from_str(
            r#"{"models":[
                {"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol"},
                {"slug":"gpt-5.6-luna","display_name":"GPT-5.6 Luna"}
            ]}"#,
        )
        .unwrap();
        let ids: Vec<String> = model_entries(&served).into_iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            ["gpt-6.1-sol", "gpt-5.6-luna"],
            "the served order is kept"
        );
        let openai: Value =
            serde_json::from_str(r#"{"data":[{"id":"gpt-5.4","created":2}]}"#).unwrap();
        assert_eq!(model_entries(&openai)[0].id, "gpt-5.4");
    }

    #[test]
    fn an_empty_served_catalogue_names_the_client_version() {
        let ModelsError::Failed(why) = served(Vec::new(), Api::OpenaiResponses).unwrap_err() else {
            panic!("an empty catalogue is a failure with a reason");
        };
        assert!(why.contains(CLIENT_VERSION), "{why}");
        assert!(why.contains("too old"), "{why}");
        assert_eq!(served(Vec::new(), Api::Openai).unwrap(), vec![]);
    }

    #[test]
    fn the_reported_client_version_is_a_current_one() {
        let v = CLIENT_VERSION;
        let parts: Vec<&str> = v.split('.').collect();
        assert_eq!(parts.len(), 3, "{v}");
        assert!(parts.iter().all(|p| p.parse::<u32>().is_ok()), "{v}");
        let minor: u32 = parts[1].parse().unwrap();
        assert!(
            minor >= 140,
            "{v} predates the versions known to serve a full list"
        );
    }

    #[test]
    fn error_messages_and_retry_rules() {
        assert_eq!(
            error_message(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
            ),
            "Overloaded"
        );
        assert_eq!(error_message("upstream down"), "upstream down");
        assert_eq!(
            error_message(
                r#"{"detail":[{"loc":["body","client_version"],"msg":"Field required","input":null}]}"#
            ),
            "client_version: Field required"
        );
        assert_eq!(error_message(r#"{"detail":"Not Found"}"#), "Not Found");
        assert!(retryable(529) && retryable(429) && !retryable(400) && !retryable(401));
        assert_eq!(backoff(1, None), Duration::from_secs(1));
        assert_eq!(backoff(3, None), Duration::from_secs(4));
        assert_eq!(backoff(1, Some(120)), Duration::from_secs(60));
    }
}
