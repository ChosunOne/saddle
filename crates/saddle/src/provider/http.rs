//! The provider's HTTP request, with retries for overload and rate limits.

use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};

use super::{Api, Cancelled, Price, Provider, ProviderConfig, Quota, Sink, sse};

const ATTEMPTS: u32 = 4;
/// A reply larger than this is cut off rather than buffered without bound.
const BODY_LIMIT: u64 = 512 * 1024 * 1024;

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        // A local server can take minutes to read a long prompt before it answers.
        .timeout_recv_response(Some(Duration::from_secs(600)))
        .timeout_per_read(Some(Duration::from_secs(300)))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// What the request thread sends back.
enum Msg {
    Event(sse::Event),
    /// What the response's headers said about the allowance left (D36).
    Quota(Quota),
    Note(String),
    Failed(anyhow::Error),
    End,
}

/// A streamed reply read on its own thread. Waiting on it checks for a cancel every 50 ms, so a
/// cancel takes effect at once even while the network is silent: the stream is abandoned, and
/// its thread exits (closing the connection) as soon as its blocked read returns.
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

/// POSTs `body` on a new thread, retrying overload and rate limits, and streams back the
/// server-sent events of a successful response.
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
        while let Some(event) = events.next_event()? {
            if self.stopped() || self.tx.send(Msg::Event(event)).is_err() {
                break;
            }
        }
        Ok(())
    }

    fn note(&self, text: String) {
        let _ = self.tx.send(Msg::Note(text));
    }

    /// The response body once the status is a success, or `None` if the stream was abandoned.
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
                    // The allowance headers are only on the response, so they are read here and
                    // handed to the sink with the stream (D36).
                    if let Some(quota) = quota_from(resp.headers()) {
                        let _ = self.tx.send(Msg::Quota(quota));
                    }
                    let reader = resp
                        .into_body()
                        .into_with_config()
                        .limit(BODY_LIMIT)
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
                        .limit(64 * 1024)
                        .read_to_string()
                        .unwrap_or_default();
                    (status, retry_after, text)
                }
                Err(e) if transient(&e) && attempt < ATTEMPTS => {
                    let wait = backoff(attempt, None);
                    self.note(format!("{e}; retrying in {}s", wait.as_secs()));
                    self.pause(wait);
                    continue;
                }
                Err(e) => return Err(e).with_context(|| format!("POST {url}")),
            };
            let message = error_message(&text);
            if retryable(status) && attempt < ATTEMPTS {
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

    /// Sleeps for `wait`, returning early when the stream is abandoned.
    fn pause(&self, wait: Duration) {
        let end = std::time::Instant::now() + wait;
        while !self.stopped() {
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            thread::sleep(left.min(Duration::from_millis(100)));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelsError {
    /// The server refused the key (401 or 403).
    Rejected(String),
    /// Anything else: no route, no models endpoint, a server error.
    Failed(String),
}

/// Ids that are not chat models, in the lists OpenAI and others return.
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

/// One entry of a model list, with the context window and the price the server stated for it,
/// where it states them at all (D34, D36).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: Option<u64>,
    pub price: Option<Price>,
}

/// Lists the provider's chat models, newest first where the server says when each was created.
/// It also checks the key, so `/login` calls it before saving anything.
///
/// The windows it carries are where a real context limit comes from, so `/login` and `/model`
/// cache them (D34).
pub fn list_models(p: &super::Provider) -> Result<Vec<ModelInfo>, ModelsError> {
    let served = served(list_from_endpoint(p)?, p.config.api)?;
    Ok(served)
}

/// What an answer means, once it has been parsed.
///
/// Separated from the request because the interesting case is an answer with nothing in it: the
/// subscription backend gates its catalogue on the client version and replies `{"models": []}`
/// rather than an error, so silence there has a cause, and naming it is the difference between a
/// picker and a bug report (D85). Every other server is allowed to list nothing.
pub fn served(models: Vec<ModelInfo>, api: super::Api) -> Result<Vec<ModelInfo>, ModelsError> {
    if models.is_empty() && api == super::Api::OpenaiResponses {
        return Err(ModelsError::Failed(format!(
            "the backend served no models for client version {} — that version may be too old, or \
             the plan may not include any",
            super::responses::CLIENT_VERSION
        )));
    }
    Ok(models)
}

/// Where to ask what a provider serves.
///
/// One place, because the subscription backend's route is `/codex/models` *with* a version, and a
/// base URL that already ends in `/codex` makes this the easiest thing in the file to get wrong
/// (D85).
pub fn listing_url(config: &super::ProviderConfig) -> String {
    let base = config.base_url.trim_end_matches('/');
    match config.api {
        Api::Openai | Api::Anthropic => format!("{base}/models"),
        Api::OpenaiResponses => format!(
            "{base}/models?client_version={}",
            super::responses::CLIENT_VERSION
        ),
    }
}

fn list_from_endpoint(p: &super::Provider) -> Result<Vec<ModelInfo>, ModelsError> {
    let config = &p.config;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let base = &config.base_url;
    // The credential goes where the provider wants it and the provider's own headers ride along, so
    // a listing checks the same thing a turn will send: a login's token and its account (D84, D85).
    let headers = p.request_headers();
    let mut request = match config.api {
        Api::Openai => agent.get(format!("{base}/models")),
        // The subscription backend states which catalogue it serves in a query parameter, and
        // answers an empty list to a version it does not know (D85).
        Api::OpenaiResponses => agent.get(listing_url(config)),
        Api::Anthropic => agent
            .get(format!("{base}/v1/models?limit=1000"))
            .header("anthropic-version", super::anthropic::VERSION),
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

/// Fetches the provider's own usage or quota endpoint and reads what it says (D36).
///
/// A usage endpoint is not standardised, so this takes the URL from the config and reads the answer
/// for the fields in use rather than for one documented shape. The JSON is walked, so a figure
/// nested under `data`, `usage` or `quota` is found where a server puts it.
pub fn fetch_usage(
    config: &ProviderConfig,
    key: Option<&str>,
    url: &str,
) -> Result<Value, ModelsError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(20)))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut req = agent.get(url).header("accept", "application/json");
    if let Some(k) = key {
        req = match config.api {
            Api::Openai | Api::OpenaiResponses => {
                req.header("authorization", format!("Bearer {k}"))
            }
            Api::Anthropic => req
                .header("x-api-key", k)
                .header("anthropic-version", super::anthropic::VERSION),
        };
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

/// Every key whose value is a number, paired with its own name and its parents', lowercased, so a
/// field can be recognised by the words around it rather than by one fixed path.
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
        // A number sent as a string, which several billing APIs do.
        Value::String(s) => {
            if let Ok(n) = s.trim().parse::<f64>() {
                out.push((path.to_owned(), n));
            }
        }
        _ => {}
    }
}

/// The paths a usage page holds numbers at, lowercased and dot-joined, which is the schema a
/// person needs to say why a figure was not found, without printing the figures themselves (D36).
pub fn page_paths(page: &Value) -> Vec<String> {
    let mut numbers = Vec::new();
    flattened(page, "", &mut numbers);
    let mut paths: Vec<String> = numbers.into_iter().map(|(p, _)| p).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// The shape Synthetic's `/v2/quotas` states, read by name rather than by words (D49).
///
/// The page reports three pools at once, and only the first is this conversation's allowance:
/// `subscription` is the chat requests, `search.hourly` is web searches, and `planWeeklyLimit` is
/// the plan as a percentage. Matching by words anywhere in the page would take whichever came
/// first, which here is the search pool — so a page of a known shape is read at its paths.
fn known_quota(page: &Value) -> Option<Quota> {
    // The plan's own allowance, which is the one figure worth the space: the page also reports a
    // five-hour request pool and a web-search pool, and neither is what governs a week of work
    // (D49). The pools it does not report are left absent rather than shown as zero.
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
    // A plan that states no weekly figure still states its request pool, and that pool is a cap.
    // `subscription.requests` is what has been spent of it, not what is left: it reads 0 against a
    // limit of 500 with a refill hours away, and had it meant what is left the account would be
    // locked out at that moment, while the caller is talking to it (D49).
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

/// Reads a usage page into a [`Quota`] (D36). A shape that is known is read by name (D49); anything
/// else is matched by the words in its path, so `data.usage.remaining_tokens` and
/// `quota.tokens_left` are the same figure. Anything not found stays absent rather than becoming a
/// zero.
pub fn quota_from_page(page: &Value) -> (Quota, Option<f64>) {
    if let Some(quota) = known_quota(page) {
        // What was spent is not on this page, so there is nothing to add to it (D49).
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
        // A reset is a date or a duration, so it is a string rather than a number, wherever the
        // server puts it.
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

    // A page that reports usage rather than what is left is still a quota as long as it states the
    // cap: 40 used against 500 leaves 460. The cap is read first so the derivation can use it.
    let left = |left: Option<f64>, used: Option<f64>, limit: Option<u64>| -> Option<u64> {
        match (left, used, limit) {
            (None, Some(used), Some(limit)) => Some(limit.saturating_sub(used as u64)),
            (left, _, _) => left.map(|n| n as u64),
        }
    };
    // A cap is spelled `limit`, `cap` or `total`; a bare `limit` is matched too, because that is
    // what it is called next to a pool it belongs to (`{"limit": 500, "requests": 0}`), and the
    // context word keeps it from being read as the other pool's (D49).
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

/// The context window a listing states for one model, if it states one. Servers disagree on the
/// field name, so all the ones seen in the wild are read; an entry that states none leaves the
/// question to `config.toml` and to [`super::known_window`].
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
        // An aggregator reports the limit of the endpoint behind the model.
        .or_else(|| m.get("top_provider")?.get("context_length")?.as_u64())
        .or_else(|| m.get("meta")?.get("n_ctx")?.as_u64());
    // A zero or an absurdly small number is a placeholder, not a limit.
    stated.filter(|n| *n >= 1_000)
}

/// A figure, whether the wire sent it as a number or as a string. Billing APIs do both, and
/// Synthetic writes prices with a currency symbol (`"$0.0000006"`), so the symbol and any thousands
/// separators are dropped first (D36).
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

/// Looks one field up by a dotted path, so a known shape can be named rather than guessed at (D49).
fn at<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .try_fold(v, |v, key| v.get(key))
        .filter(|v| !v.is_null())
}

/// The price a listing states for one model, if it states one (D36).
///
/// Servers publish prices in two units and under several shapes: per token as a decimal string
/// ("0.000003", how the aggregators do it), or per million tokens as a number ("3.0", how a model
/// card reads). The unit is taken from a stated one where the server states it, and otherwise from
/// the magnitude, because the two are a million apart and a wrong guess is not a rounding error.
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

/// Which unit a listing's prices are in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Token,
    Million,
}

fn stated_price(m: &Value) -> Option<(Price, Unit)> {
    // The price object, wherever this server puts it.
    let p = ["pricing", "price", "cost", "costs", "billing"]
        .iter()
        .find_map(|k| m.get(*k).filter(|v| v.is_object()))?;
    // A nested `per_million` object is the unit, spelled out.
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
            // Synthetic names the cache prices in the plural, and the others in the singular.
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
    // With no stated unit, the magnitude decides: per-token prices are far below a thousandth of a
    // dollar, and per-million prices are far above it, so the gap leaves no doubt. A server that
    // states its unit is never overruled.
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
        // The subscription backend's catalogue: `{"models": [{"slug": "gpt-…"}]}` (D85).
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
    // Stable: servers that give no dates keep their own order.
    models.sort_by_key(|m| std::cmp::Reverse(m.0));
    models.dedup_by(|a, b| a.1.id == b.1.id);
    models.into_iter().map(|(_, m)| m).collect()
}

fn transient(e: &ureq::Error) -> bool {
    matches!(
        e,
        ureq::Error::Io(_) | ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed
    )
}

/// 408, 409, 429 and 5xx, including Anthropic's 529 (overloaded).
fn retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 429) || status >= 500
}

/// The allowance left, from the response headers (D36).
///
/// Two families are in use: `x-ratelimit-*`, which OpenAI-compatible servers send and which name a
/// limit alongside the remainder, and `anthropic-ratelimit-*`, which names the resource and then
/// the field. Both are read from whichever header the server actually sent, since a server that
/// states only one of the four is common.
fn quota_from(headers: &ureq::http::HeaderMap) -> Option<Quota> {
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

/// Both APIs put the message at `error.message`.
fn error_message(text: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(text).ok();
    let message = parsed.as_ref().and_then(|v| {
        let direct = v["error"]["message"]
            .as_str()
            .or_else(|| v["message"].as_str())
            .or_else(|| v["error"].as_str())
            // FastAPI's shape, which is what the subscription backend speaks: `detail` is a string,
            // or a list of `{loc, msg}` entries that reads as a fragment of JSON if it is not
            // unpacked (D85).
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
    // A body that is not JSON at all may still be several lines of it; one line reads better in a
    // dialog that shows one.
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
        // An absent or placeholder field leaves the question open rather than answering zero.
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
        // OpenRouter publishes dollars per token as strings; a few servers send numbers.
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
        // A stated zero is not a price; it is a model nobody priced.
        assert_eq!(prices[2], None);
        assert_eq!(prices[3], None);
    }

    #[test]
    fn a_price_is_read_in_the_shape_and_unit_its_server_uses() {
        // Per token, as a decimal string: the aggregator shape.
        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "pricing": {"prompt": "0.000003", "completion": "0.000015"}}]}"#,
        )
        .unwrap();
        let p = model_entries(&v)[0].price.unwrap();
        assert_eq!(p.input, 3e-6);
        assert_eq!(p.output, 1.5e-5);

        // Per million, as numbers, which is a million-fold difference: $3 in, $15 out.
        let v: Value = serde_json::from_str(
            r#"{"data": [{"id": "a", "pricing": {"input": 3.0, "output": 15.0, "cache_read": 0.3}}]}"#,
        )
        .unwrap();
        let p = model_entries(&v)[0].price.unwrap();
        assert_eq!(p.input, 3e-6, "per million is divided down to per token");
        assert_eq!(p.output, 1.5e-5);
        assert_eq!(p.cache_read, Some(3e-7));

        // The unit stated outright beats the guess.
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

        // Nested objects and other key names still answer.
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
        // The shape Synthetic actually serves, copied from a live /models response: prices per
        // token as `$`-prefixed strings, the cache prices in the plural, and the window in
        // `context_length`.
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
        // The window was in the field the parser reads, so 524288 and not the 128k default.
        assert_eq!(m.context_window, Some(524_288));
        let price = m.price.expect("a price despite the `$`");
        assert_eq!(price.input, 6e-7, "$0.60 per million");
        assert_eq!(price.output, 1.2e-6, "$1.20 per million");
        assert_eq!(price.cache_read, Some(3e-8), "$0.03 per million");
        assert_eq!(price.cache_write, Some(0.0), "stated as free");
        // The magnitude rule leaves a per-token price per token.
        assert!((price.input * 1e6 - 0.6).abs() < 1e-9);

        // A free model states zeros, which is no price rather than a price of zero (D36).
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
        // One shape: a balance and a reset.
        let page: Value = serde_json::from_str(
            r#"{"data": {"credits": {"remaining": 12.5, "resets_at": "2026-12-01T00:00:00Z"}}}"#,
        )
        .unwrap();
        let (q, spent) = quota_from_page(&page);
        assert_eq!(q.credits, Some(12.5));
        assert_eq!(q.reset.as_deref(), Some("2026-12-01T00:00:00Z"));
        assert_eq!(spent, None);

        // Another: token and request windows with limits, plus spend.
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

        // A page with nothing recognisable is empty rather than a row of zeros.
        let page: Value = serde_json::from_str(r#"{"ok": true, "balance_updated": null}"#).unwrap();
        let (q, spent) = quota_from_page(&page);
        assert!(q.is_empty());
        assert_eq!(spent, None);
    }

    #[test]
    fn the_synthetic_quota_page_is_read_at_its_paths() {
        // The real body of `https://api.synthetic.new/v2/quotas`, with the figures as they came
        // back (D49).
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
        // The weekly allowance is the figure, and it comes with its own regen stamp.
        assert_eq!(q.percent.map(|p| p.round()), Some(94.0));
        assert_eq!(q.reset.as_deref(), Some("2026-11-12T08:26:09.000Z"));
        // The request pool refills in a five-hour window and the search pool is not this
        // conversation's allowance, so neither is read: the page reports them, and a status bar
        // shows one figure (D49). Reading them by words would have taken the *search* pool, since
        // `search` sorts before `subscription` in the page.
        assert_eq!(q.requests, None);
        assert_eq!(q.requests_limit, None);
        assert_eq!(q.tokens, None);
        assert_eq!(spent, None);
    }

    #[test]
    fn a_plan_without_a_weekly_figure_states_its_request_pool() {
        // The same page with no `planWeeklyLimit`, which is a plan sold by request count: 0 spent
        // of 500 leaves 500 (D49).
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
        // The word-matching path, for a shape nobody has named yet: 40 used of 500 is 460 left,
        // and a page that states what is left directly is not second-guessed (D49).
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
        // Usage with no cap says nothing about what is left, so it stays absent rather than zero.
        let uncapped: Value = serde_json::from_str(r#"{"data": {"tokens_used": 40}}"#).unwrap();
        assert_eq!(quota_from_page(&uncapped).0.tokens, None);
    }

    #[test]
    fn an_unrecognised_page_reports_its_field_names_not_its_figures() {
        // The diagnostic that matters when a provider's shape is new: what the numbers are called.
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
        let mut h = ureq::http::HeaderMap::new();
        h.insert("x-ratelimit-remaining-tokens", "48000".parse().unwrap());
        h.insert("x-ratelimit-limit-tokens", "1000000".parse().unwrap());
        h.insert("x-ratelimit-reset-tokens", "6m0s".parse().unwrap());
        let q = quota_from(&h).expect("a quota");
        assert_eq!(q.tokens, Some(48_000));
        assert_eq!(q.tokens_limit, Some(1_000_000));
        assert_eq!(q.reset.as_deref(), Some("6m0s"));

        // Anthropic names the resource first, and often states only one of the pair.
        let mut h = ureq::http::HeaderMap::new();
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

        // A server that states neither leaves the status line as it was.
        assert_eq!(quota_from(&ureq::http::HeaderMap::new()), None);
        let mut other = ureq::http::HeaderMap::new();
        other.insert("retry-after", "3".parse().unwrap());
        assert_eq!(quota_from(&other), None);
    }

    #[test]
    fn the_listing_url_is_the_one_the_backend_serves() {
        // D85: the route is `/codex/models` and it *requires* the version, so both halves matter
        // and both were wrong at some point.
        let mut config = super::super::ProviderConfig {
            api: super::super::Api::OpenaiResponses,
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
            auth: super::super::Auth::ApiKey,
            oauth: None,
        };
        assert_eq!(
            listing_url(&config),
            format!(
                "https://chatgpt.com/backend-api/codex/models?client_version={}",
                super::super::responses::CLIENT_VERSION
            )
        );
        // The trailing slash a user's base URL may carry is not part of the route.
        config.base_url = "https://chatgpt.com/backend-api/codex/".into();
        assert!(
            listing_url(&config).contains("/codex/models?"),
            "{}",
            listing_url(&config)
        );
        // A compatible server keeps its own, plain listing.
        config.api = super::super::Api::Openai;
        assert_eq!(
            listing_url(&config),
            "https://chatgpt.com/backend-api/codex/models"
        );
    }

    #[test]
    fn the_served_catalogue_is_read_from_slugs() {
        // D85: the subscription backend answers `{"models": [{"slug": …}]}`, not `{"data":
        // [{"id": …}]}`, and the ids are what a request has to name.
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
        // The `/v1/models` shape still reads, because a compatible server is not that backend.
        let openai: Value =
            serde_json::from_str(r#"{"data":[{"id":"gpt-5.4","created":2}]}"#).unwrap();
        assert_eq!(model_entries(&openai)[0].id, "gpt-5.4");
    }

    #[test]
    fn an_empty_served_catalogue_names_the_client_version() {
        // The backend gates its list on `client_version` and answers an empty list, not an error, so
        // silence there has a cause and the message has to say which version was asked with (D85).
        let ModelsError::Failed(why) =
            served(Vec::new(), super::super::Api::OpenaiResponses).unwrap_err()
        else {
            panic!("an empty catalogue is a failure with a reason");
        };
        assert!(
            why.contains(super::super::responses::CLIENT_VERSION),
            "{why}"
        );
        assert!(why.contains("too old"), "{why}");
        // A server that simply has no models is not that case, and says nothing.
        assert_eq!(
            served(Vec::new(), super::super::Api::Openai).unwrap(),
            vec![]
        );
    }

    #[test]
    fn the_reported_client_version_is_a_current_one() {
        // D85: the catalogue is gated on this, and an old value silently returns fewer models, so it
        // has to be a plain Codex CLI version and not something that predates the gate.
        let v = super::super::responses::CLIENT_VERSION;
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
        // The shape the subscription backend answers with, which used to reach the dialog as a
        // fragment of JSON (D85).
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
