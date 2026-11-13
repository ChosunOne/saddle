//! `websearch`: a search of the web, made by the host (D82).
//!
//! The search is asked of the **provider first**, because Synthetic serves one: `POST
//! /v2/search` on `api.synthetic.new` answers with the results, and the same key that pays for the
//! model pays for the search, so a session that can talk to its model can search without being
//! configured twice. The endpoint sits beside `/v2/quotas`, which the status line already uses.
//!
//! The provider path had to be found the hard way: the neighbouring `/v1/search` and
//! `/v1/web_search` answer `404`, so an unauthenticated probe of those routes suggests there is no
//! search at all. `/v2/search` answers `401` without a key and `404` when the path does not exist,
//! which is what distinguishes a route that wants a credential from one that is not there.
//!
//! Exa is the fallback, for a provider that does not search:
//!
//! - **with a key** (from `EXA_API_KEY`, or one stored by `/login exa`), `POST /search` on
//!   `api.exa.ai`, which returns structured results;
//! - **without one**, Exa's hosted MCP server, which answers an anonymous search.
//!
//! Without a key Exa's REST endpoint is not an option at all: it answers `402 X402_PAYMENT_REQUIRED`,
//! which is a payment challenge rather than a rate limit. That is why the anonymous path is MCP and
//! not REST.
//!
//! The provider answers `{"results": [{url, title, text, published}]}`. `text` is the whole page and
//! `published` is on some results only; the endpoint also returns Exa's `highlights`, which is a
//! prefix of that page rather than a passage chosen from it. Whichever arrives, the model is shown a
//! bounded excerpt of it, and the parser reads both spellings of the date so the fallback keeps its
//! own.
//!
//! Everything here goes through the engine before a request is made, so the `net` grant decides —
//! `api.synthetic.new`, `api.exa.ai` and `mcp.exa.ai` are the hosts that have to be granted, and the
//! same audit log records each one.

use anyhow::Result;
use saddle_cap::{Principal, Scope};
use saddle_policy::{Kind, Request};
use serde_json::{Value, json};

use super::{ToolOutput, fetch, num_field, str_field};
use crate::session::Session;

/// Results asked for when the model does not say.
const DEFAULT_RESULTS: u64 = 5;
/// The most results one call may ask for, which is Exa's own ceiling for the search endpoint.
const MAX_RESULTS: u64 = 100;
/// A reply larger than this is cut: it is the ceiling the agent puts on a tool result anyway.
const MAX_BYTES: usize = 64 * 1024;
/// How much of one quoted passage the model is shown. Exa's own highlights are short excerpts, but
/// Synthetic's search returns whole page texts under that key, so without a ceiling one result fills
/// the entire answer and the other four are lost.
const MAX_QUOTE: usize = 1000;
/// Passages per result, so one page cannot take the whole answer either.
const MAX_QUOTES: usize = 3;
/// What one answer may be before it is refused. A search of five results carries their whole text,
/// about 220 KiB in practice, so this is generous on purpose; anything near it is not an answer.
const MAX_REPLY: u64 = 1024 * 1024;

/// Which way the search is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// The provider in use searches on its own, and its own key pays for it (D82).
    Provider(crate::provider::SearchTarget),
    /// Exa's REST search, with a key.
    ExaRest,
    /// Exa's hosted MCP server, which answers without a key.
    ExaMcp,
}

/// One search result, before it is put into words for the model.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub published: Option<String>,
    pub author: Option<String>,
    /// The passages the backend chose, in its order: Exa's `highlights`, which are excerpts.
    pub highlights: Vec<String>,
    /// The result's own text — `/v2/search` documents this field and it is often the whole page,
    /// so it is the fallback excerpt when a backend sends no highlights (D82).
    pub text: Option<String>,
}

pub fn search(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let query = match str_field(input, "query") {
        Ok(q) => q.trim().to_owned(),
        Err(out) => return Ok(out),
    };
    if query.is_empty() {
        return Ok(ToolOutput::err("`query` is empty"));
    }
    let count = match num_field(input, "num_results") {
        Ok(n) => n.unwrap_or(DEFAULT_RESULTS).clamp(1, MAX_RESULTS),
        Err(out) => return Ok(out),
    };
    let backend = match choose(session) {
        Ok(backend) => backend,
        Err(out) => return Ok(out),
    };
    // The query is not a secret, and a search log that does not say what was searched cannot be
    // reviewed. Header values still never reach a record (D48).
    session.audit.event(
        Kind::Decision,
        Some(&Principal::Agent),
        Some(Scope::Net),
        None,
        json!({ "tool": "websearch", "query": query, "results": count, "via": backend_name(&backend) }),
        format!("searched the web for `{query}`"),
    )?;
    match backend {
        Backend::Provider(target) => provider_search(session, &target, &query, count),
        Backend::ExaRest => exa_rest(session, &query, count),
        Backend::ExaMcp => exa_mcp(session, &query, count),
    }
}

fn backend_name(backend: &Backend) -> &'static str {
    match backend {
        Backend::Provider(_) => "provider",
        Backend::ExaRest => "exa",
        Backend::ExaMcp => "exa-mcp",
    }
}

/// Chooses the way the search is asked: the provider in use when it searches, else Exa with a key,
/// else Exa's MCP.
fn choose(session: &mut Session) -> Result<Backend, ToolOutput> {
    if let Some(target) = session.search.clone() {
        return Ok(Backend::Provider(target));
    }
    // A key makes the REST endpoint usable; without one it is a payment challenge, not a limit.
    if exa_key(session).is_some() {
        return Ok(Backend::ExaRest);
    }
    Ok(Backend::ExaMcp)
}

/// The provider's own search: one POST, the same JSON Exa answers with, paid for by the provider's
/// key (D82).
///
/// The body is the query and nothing else. `numResults` is accepted and ignored — the server answers
/// five whatever is asked — so the count is applied here, to what came back, rather than sent as a
/// wish. A body that names a field the server does not know is not rejected either, which is why the
/// absence of a documented schema is survivable.
fn provider_search(
    session: &mut Session,
    target: &crate::provider::SearchTarget,
    query: &str,
    count: u64,
) -> Result<ToolOutput> {
    let url = match fetch::Target::parse(&target.url) {
        Ok(url) => url,
        Err(why) => return Ok(ToolOutput::err(why)),
    };
    // The gate is asked before the key is looked for: whether this session may search at all is a
    // policy question, and a missing key must not hide the answer to it (D82).
    if let Err(denied) = permit(session, "POST", &url, &["content-type", "authorization"])? {
        return Ok(denied);
    }
    let Some(key) = provider_key(session, target) else {
        let env = target.key_env.as_deref().unwrap_or("its API key");
        return Ok(ToolOutput::err(format!(
            "`{}` searches the web, but no key is set for it: set {env}, or store one with \
             `saddle login {}`",
            target.provider, target.provider
        )));
    };
    let body = json!({ "query": query }).to_string();
    let headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        ("authorization".to_owned(), format!("Bearer {key}")),
    ];
    let (status, text) = match fetch::post(&url, true, &headers, &body, MAX_REPLY) {
        Ok(answer) => answer,
        Err(why) => return Ok(ToolOutput::err(why)),
    };
    if status != 200 {
        return Ok(ToolOutput::err(provider_explain(
            status,
            &text,
            &target.provider,
        )));
    }
    let mut hits = parse_exa(&text)?;
    hits.truncate(usize::try_from(count).unwrap_or(hits.len()));
    let asked = if count > DEFAULT_RESULTS {
        format!(
            " ({count} asked, {} returned: the endpoint answers {})",
            hits.len(),
            DEFAULT_RESULTS
        )
    } else {
        String::new()
    };
    Ok(ToolOutput::ok(format!(
        "{} searched for `{query}` and answered:\n\n{}{}",
        target.provider,
        format_hits(&hits),
        asked
    )))
}

/// The provider key: the variable it names, then the store `/login` writes.
fn provider_key(session: &Session, target: &crate::provider::SearchTarget) -> Option<String> {
    if let Some(var) = &target.key_env
        && let Ok(key) = std::env::var(var)
        && !key.trim().is_empty()
    {
        return Some(key);
    }
    let store = crate::auth::AuthStore::load(&session.setup.paths.auth_store()).ok()?;
    store
        .key(&target.provider)
        .filter(|k| !k.trim().is_empty())
        .map(str::to_owned)
}

/// What a failed provider search means, in a sentence the model can act on.
fn provider_explain(status: u16, body: &str, provider: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(200).collect());
    match status {
        401 | 403 => format!(
            "`{provider}` refused the key ({status}): {detail}. The search uses the same key as the \
             model, so `saddle login {provider}` fixes both"
        ),
        400 | 500 => format!(
            "`{provider}` could not answer this search ({status}): {detail}. An empty query is \
             refused; a query that is too long or too short is too"
        ),
        429 => format!("`{provider}` is rate limiting this session ({status}): {detail}"),
        _ => format!("`{provider}` answered {status}: {detail}"),
    }
}

/// The Exa key: the environment first, so a session can be given one for a single run, then the
/// store `/login` writes.
pub(crate) fn exa_key(session: &Session) -> Option<String> {
    if let Ok(key) = std::env::var("EXA_API_KEY")
        && !key.trim().is_empty()
    {
        return Some(key);
    }
    let store = crate::auth::AuthStore::load(&session.setup.paths.auth_store()).ok()?;
    store.key("exa").map(str::to_owned)
}

/// The REST search: one request, structured results.
fn exa_rest(session: &mut Session, query: &str, count: u64) -> Result<ToolOutput> {
    let key = exa_key(session).unwrap_or_default();
    let host = "api.exa.ai";
    let target = fetch::Target {
        scheme: "https".to_owned(),
        host: host.to_owned(),
        port: 443,
        path: "/search".to_owned(),
    };
    let body = json!({
        "query": query,
        // `auto` is Exa's own default and the balanced mode; the cheaper and deeper modes are not
        // a decision this tool should make for the model.
        "type": "auto",
        "numResults": count,
        "contents": { "highlights": true },
    })
    .to_string();
    let headers = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "x-api-key".to_owned(),
            key, // a name is audited, a value never is (D48)
        ),
    ];
    let allowed = permit(session, "POST", &target, &["content-type", "x-api-key"])?;
    if let Err(denied) = allowed {
        return Ok(denied);
    }
    let (status, text) = match fetch::post(&target, true, &headers, &body, MAX_REPLY) {
        Ok(answer) => answer,
        Err(why) => return Ok(ToolOutput::err(why)),
    };
    if status != 200 {
        return Ok(ToolOutput::err(explain(status, &text)));
    }
    Ok(ToolOutput::ok(format!(
        "{}\n\n({count} asked of Exa, {})",
        format_hits(&parse_exa(&text)?),
        describe(&text)
    )))
}

/// The anonymous search: Exa's MCP server, which needs a session before it will answer a tool call.
fn exa_mcp(session: &mut Session, query: &str, count: u64) -> Result<ToolOutput> {
    let host = "mcp.exa.ai";
    let target = fetch::Target {
        scheme: "https".to_owned(),
        host: host.to_owned(),
        port: 443,
        path: "/mcp".to_owned(),
    };
    let accept = vec![
        ("content-type".to_owned(), "application/json".to_owned()),
        (
            "accept".to_owned(),
            "application/json, text/event-stream".to_owned(),
        ),
    ];
    if let Err(denied) = permit(session, "POST", &target, &["content-type", "accept"])? {
        return Ok(denied);
    }
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "saddle", "version": env!("CARGO_PKG_VERSION") },
        },
    })
    .to_string();
    let (status, text) = match fetch::post(&target, true, &accept, &init, MAX_REPLY) {
        Ok(answer) => answer,
        Err(why) => return Ok(ToolOutput::err(why)),
    };
    if status != 200 {
        return Ok(ToolOutput::err(explain(status, &text)));
    }
    // The session header is what binds the next call to the initialised session; a server that
    // sends none is answering statelessly, which is also fine.
    let session_id = mcp_session(&text);
    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "web_search_exa", "arguments": { "query": query, "numResults": count } },
    })
    .to_string();
    let mut headers = accept;
    if let Some(id) = &session_id {
        headers.push(("mcp-session-id".to_owned(), id.clone()));
    }
    if let Err(denied) = permit(session, "POST", &target, &header_names(&headers))? {
        return Ok(denied);
    }
    let (status, text) = match fetch::post(&target, true, &headers, &call, MAX_REPLY) {
        Ok(answer) => answer,
        Err(why) => return Ok(ToolOutput::err(why)),
    };
    if status != 200 {
        return Ok(ToolOutput::err(explain(status, &text)));
    }
    let answer =
        parse_mcp(&text).ok_or_else(|| anyhow::anyhow!("Exa's MCP server sent no result"))?;
    if let Some(error) = answer.get("error") {
        return Ok(ToolOutput::err(format!("Exa answered: {error}")));
    }
    let text = answer
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if text.trim().is_empty() {
        return Ok(ToolOutput::ok(format!("no results for `{query}`")));
    }
    Ok(ToolOutput::ok(format!(
        "{}\n\n(Exa's MCP server, searched without a key; set EXA_API_KEY for more)",
        cut(text)
    )))
}

/// Asks the policy about one request, and returns the denial to show if it refuses.
fn permit(
    session: &mut Session,
    method: &str,
    target: &fetch::Target,
    headers: &[&str],
) -> Result<std::result::Result<(), ToolOutput>> {
    let names: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    let d = session.decide(
        &Principal::Agent,
        &Request::Net {
            method,
            scheme: &target.scheme,
            host: &target.host,
            port: target.port,
            path: &target.path,
            headers: &names,
        },
    )?;
    Ok(if d.allow {
        Ok(())
    } else {
        Err(ToolOutput::denied(&d.explanation))
    })
}

fn header_names(headers: &[(String, String)]) -> Vec<&str> {
    headers.iter().map(|(name, _)| name.as_str()).collect()
}

/// The results of a REST search, as the model reads them.
fn parse_exa(body: &str) -> Result<Vec<Hit>> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| anyhow::anyhow!("Exa's answer is not JSON: {e}"))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Exa's answer has no results"))?;
    Ok(results.iter().map(hit).collect())
}

fn hit(result: &Value) -> Hit {
    let text = |key: &str| {
        result
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    };
    Hit {
        title: text("title").unwrap_or_default(),
        url: text("url").unwrap_or_default(),
        // `/v2/search` documents `published`; Exa's REST search calls the same thing
        // `publishedDate`, so one parser reads both rather than one backend losing its dates.
        published: text("published").or_else(|| text("publishedDate")),
        author: text("author"),
        highlights: result
            .get("highlights")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        text: text("text"),
    }
}

/// The results as the model reads them: one numbered entry each, then what Exa quoted from it.
pub fn format_hits(hits: &[Hit]) -> String {
    if hits.is_empty() {
        return "no results".to_owned();
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, title_of(h), h.url));
        let mut line: Vec<String> = Vec::new();
        if let Some(when) = &h.published {
            line.push(format!("published {}", date_of(when)));
        }
        if let Some(who) = &h.author {
            line.push(format!("by {who}"));
        }
        if !line.is_empty() {
            out.push_str(&format!("   {}\n", line.join(", ")));
        }
        for passage in h.excerpts().iter().take(MAX_QUOTES) {
            out.push_str(&format!("   > {}\n", passage_for_model(passage)));
        }
    }
    cut(out.trim_end())
}

/// One quoted passage as a single line, cut to [`MAX_QUOTE`] characters.
///
/// Cuts on a character boundary and says so, because half a word at the end of an excerpt reads as
/// a typo rather than as an excerpt.
fn passage_for_model(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= MAX_QUOTE {
        return one_line;
    }
    let kept: String = one_line.chars().take(MAX_QUOTE).collect();
    format!("{kept}…")
}

impl Hit {
    /// What to quote from this result: the passages the backend chose, or the result's own text.
    ///
    /// Empty strings are dropped rather than quoted, so a result with `"highlights": [""]` still
    /// shows its text instead of a blank line.
    fn excerpts(&self) -> Vec<&str> {
        let chosen: Vec<&str> = self
            .highlights
            .iter()
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .collect();
        if !chosen.is_empty() {
            return chosen;
        }
        self.text
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|t| vec![t])
            .unwrap_or_default()
    }
}

/// An ISO timestamp as the date it states, since that is the part a reader can use.
fn date_of(stamp: &str) -> &str {
    let bytes = stamp.as_bytes();
    let iso = bytes.len() >= 10
        && bytes[..10].iter().enumerate().all(|(i, b)| match i {
            4 | 7 => *b == b'-',
            _ => b.is_ascii_digit(),
        });
    if iso { &stamp[..10] } else { stamp }
}

fn title_of(hit: &Hit) -> &str {
    if hit.title.is_empty() {
        &hit.url
    } else {
        &hit.title
    }
}

/// A reply cut to what a tool result may carry, saying so rather than ending mid-word.
fn cut(text: &str) -> String {
    if text.len() <= MAX_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n(cut at {MAX_BYTES} bytes)", &text[..end])
}

/// `x-request-id`, `costDollars` and `searchTime`, when the answer says: enough for a record to be
/// reconciled against a bill without the model having to care.
fn describe(body: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return "no request id".to_owned();
    };
    let id = value
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or("no request id");
    let cost = value
        .pointer("/costDollars/total")
        .and_then(Value::as_f64)
        .map(|c| format!("${c:.4}"))
        .unwrap_or_else(|| "cost not stated".to_owned());
    format!("request {id}, {cost}")
}

/// What the status and body mean, in a sentence the model can act on.
fn explain(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
        })
        .unwrap_or_else(|| body.chars().take(200).collect());
    match status {
        401 | 403 => format!(
            "Exa refused the key ({status}): {detail}. A key goes in EXA_API_KEY, or store one with \
             `saddle login exa`"
        ),
        402 => format!(
            "Exa wants payment for this request ({status}): {detail}. The anonymous path through \
             Exa's MCP server needs no key, so unset EXA_API_KEY to use it"
        ),
        429 => format!("Exa is rate limiting this session ({status}): {detail}. A key raises it"),
        400 => format!("Exa refused the query ({status}): {detail}"),
        _ => format!("Exa answered {status}: {detail}"),
    }
}

/// The session id from an MCP answer's headers, which arrive here as part of the body's framing.
fn mcp_session(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("mcp-session-id")
            .then(|| value.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// The JSON-RPC answer inside an MCP reply, which is either a JSON body or a server-sent event
/// stream whose `data:` lines each carry one message.
fn parse_mcp(body: &str) -> Option<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body)
        && value.get("result").is_some()
    {
        return Some(value);
    }
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(payload.trim())
            && (value.get("result").is_some() || value.get("error").is_some())
        {
            return Some(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rest_answer_becomes_numbered_results() {
        // The shape is Exa's own, trimmed to the fields this tool reads.
        let body = r#"{
            "requestId": "abc",
            "costDollars": {"total": 0.005},
            "results": [
                {"title": "trishume/syntect", "url": "https://github.com/trishume/syntect/",
                 "publishedDate": "2024-01-02T00:00:00.000Z", "author": null,
                 "highlights": ["Rust library for syntax highlighting"]},
                {"url": "https://docs.rs/syntect",
                 "highlights": []}
            ]
        }"#;
        let hits = parse_exa(body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "trishume/syntect");
        assert_eq!(hits[1].title, "", "a result with no title keeps none");
        let text = format_hits(&hits);
        assert!(
            text.starts_with("1. trishume/syntect\n   https://github.com"),
            "{text}"
        );
        assert!(text.contains("published 2024-01-02"), "{text}");
        assert!(
            text.contains("> Rust library for syntax highlighting"),
            "{text}"
        );
        // A result with no title is named by its address, so it is still distinguishable.
        assert!(text.contains("2. https://docs.rs/syntect"), "{text}");
        assert!(describe(body).contains("request abc") && describe(body).contains("$0.0050"));
    }

    #[test]
    fn the_documented_search_shape_is_read() {
        // D82: `POST /v2/search` documents `url`, `title`, `text` and `published` — no highlights,
        // no author. A parser that only read Exa's `highlights` would show titles with no content,
        // so this is the shape that matters most and the one the fallback must not depend on.
        let body = r#"{
            "results": [{
                "url": "https://curl.se/docs/projdocs.html",
                "title": "curl — Project Documentation",
                "text": "Project Documentation. The curl project is ...",
                "published": "2025-11-05T00:00:00.000Z"
            }]
        }"#;
        let hits = parse_exa(body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].published.as_deref(),
            Some("2025-11-05T00:00:00.000Z")
        );
        assert!(hits[0].highlights.is_empty());
        let text = format_hits(&hits);
        assert!(text.contains("1. curl — Project Documentation"), "{text}");
        assert!(
            text.contains("> Project Documentation. The curl project is ..."),
            "the text is the excerpt: {text}"
        );
        // A timestamp is shown as its date, and anything that is not one is shown as it came.
        assert!(text.contains("published 2025-11-05\n"), "{text}");
        assert!(!text.contains("2025-11-05T00:00"), "{text}");
        assert_eq!(date_of("not a date"), "not a date");
        assert_eq!(date_of("2025-11-05"), "2025-11-05");
        // A result with neither highlights nor text is a link rather than a blank quote.
        let bare = Hit {
            title: "t".into(),
            url: "https://t/".into(),
            ..Hit::default()
        };
        assert!(!format_hits(&[bare]).contains("> "), "quoted nothing");
        // An empty highlight falls back to the text rather than quoting a blank line.
        let empty = Hit {
            title: "t".into(),
            url: "https://t/".into(),
            highlights: vec![String::new(), "  ".into()],
            text: Some("real text".into()),
            ..Hit::default()
        };
        assert!(format_hits(&[empty]).contains("> real text"));
    }

    #[test]
    fn an_answer_with_no_results_says_so() {
        assert_eq!(format_hits(&[]), "no results");
        assert_eq!(parse_exa(r#"{"results": []}"#).unwrap(), []);
        // A body that is not what Exa sends is an error rather than an empty success.
        assert!(parse_exa("not json").is_err());
        assert!(parse_exa(r#"{"nope": 1}"#).is_err());
    }

    #[test]
    fn a_highlight_that_spans_lines_stays_on_one() {
        let hit = Hit {
            title: "a".into(),
            url: "https://a/".into(),
            highlights: vec!["one\ntwo".into()],
            ..Hit::default()
        };
        let text = format_hits(&[hit]);
        assert!(text.contains("> one two"), "{text}");
        assert!(!text.contains("one\ntwo"), "{text}");
    }

    #[test]
    fn an_mcp_answer_is_found_in_a_stream_or_a_body() {
        // The transport is one POST and a line filter: the server may answer either way, and both
        // are the same JSON-RPC message.
        let streamed = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"Title: x\"}]}}\n\n";
        assert_eq!(
            parse_mcp(streamed)
                .unwrap()
                .pointer("/result/content/0/text")
                .and_then(Value::as_str),
            Some("Title: x")
        );
        let plain = r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"y"}]}}"#;
        assert!(parse_mcp(plain).is_some());
        // An error message is a result to report, not a parse failure.
        let failed =
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"code\":-32602,\"message\":\"bad\"}}";
        assert_eq!(
            parse_mcp(failed)
                .unwrap()
                .pointer("/error/message")
                .and_then(Value::as_str),
            Some("bad")
        );
        assert!(parse_mcp("nothing here").is_none());
    }

    #[test]
    fn a_session_id_is_read_from_whatever_framing_it_arrives_in() {
        assert_eq!(
            mcp_session("mcp-session-id: abc-123\r\ncontent-type: text/event-stream\r\n"),
            Some("abc-123".to_owned())
        );
        assert_eq!(mcp_session("content-type: application/json"), None);
    }

    #[test]
    fn the_denial_for_a_key_explains_what_to_do() {
        // The two cases people hit: a key that is wrong, and a key whose team is out of credit.
        let text = explain(
            401,
            r#"{"error":"Invalid API key","tag":"INVALID_API_KEY"}"#,
        );
        assert!(text.contains("EXA_API_KEY"), "{text}");
        assert!(text.contains("Invalid API key"), "{text}");
        let text = explain(
            402,
            r#"{"error":"Payment required","tag":"X402_PAYMENT_REQUIRED"}"#,
        );
        assert!(text.contains("no key"), "{text}");
        assert!(text.contains("unset EXA_API_KEY"), "{text}");
        let text = explain(429, r#"{"error":"slow down"}"#);
        assert!(text.contains("rate limiting"), "{text}");
        // A body that is not JSON is reported rather than dropped.
        assert!(explain(500, "<html>oops</html>").contains("<html>oops</html>"));
    }

    #[test]
    fn a_passage_is_one_line_of_bounded_length() {
        // Synthetic returns whole pages under `highlights`; a model gets an excerpt (D82).
        let page = "word ".repeat(5000);
        let quoted = passage_for_model(&page);
        assert_eq!(
            quoted.chars().count(),
            MAX_QUOTE + 1,
            "kept or ellipsis wrong"
        );
        assert!(quoted.ends_with('…'), "{quoted:.20}");
        // A short passage is untouched apart from its line breaks.
        assert_eq!(passage_for_model("one\n two\n\nthree"), "one two three");
        // No more than a few passages per result, whatever the server sends.
        let hit = Hit {
            title: "a".into(),
            url: "https://a/".into(),
            highlights: (0..10).map(|i| format!("passage {i}")).collect(),
            ..Hit::default()
        };
        let text = format_hits(&[hit]);
        assert_eq!(text.matches("   > ").count(), MAX_QUOTES, "{text}");
        // Five whole pages still fit in one answer.
        let hits: Vec<Hit> = (0..5)
            .map(|i| Hit {
                title: format!("t{i}"),
                url: format!("https://a/{i}"),
                highlights: vec![page.clone()],
                ..Hit::default()
            })
            .collect();
        assert!(format_hits(&hits).len() < MAX_BYTES, "not bounded");
    }

    #[test]
    fn a_reply_is_cut_to_what_a_tool_result_may_carry() {
        let long = "x".repeat(MAX_BYTES + 100);
        let shortened = cut(&long);
        assert!(shortened.len() < MAX_BYTES + 100, "not cut");
        assert!(shortened.ends_with("bytes)"), "{shortened:?}");
        // Short text is untouched.
        assert_eq!(cut("short"), "short");
        // A cut never splits a character.
        let multibyte = "é".repeat(MAX_BYTES);
        assert!(std::str::from_utf8(cut(&multibyte).as_bytes()).is_ok());
    }
}
