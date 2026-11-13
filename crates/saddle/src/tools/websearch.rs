//! `websearch`: a search of the web, made by the host (D82).
//!
//! The search is asked of Exa, because the configured provider cannot do it: Synthetic serves chat
//! completions only, and its `/v1/responses`, `/v1/search` and `/v1/web_search` all answer 404, so
//! there is no provider-side search to call. The seam for one is [`Backend::Provider`], which is
//! what a provider that gains the capability would add — the choice lives in one function, so that
//! is an edit rather than a second code path.
//!
//! There are two ways to reach Exa, and which one is used is not a matter of taste:
//!
//! - **with a key** (from `EXA_API_KEY`, or one stored by `/login exa`), `POST /search` on
//!   `api.exa.ai`, which returns structured results;
//! - **without one**, Exa's hosted MCP server, which answers an anonymous search. A key raises the
//!   rate limit, so the keyed path is preferred when there is one.
//!
//! Without a key the REST endpoint is not an option at all: it answers `402 X402_PAYMENT_REQUIRED`,
//! which is a payment challenge rather than a rate limit. That is why the anonymous path is MCP and
//! not REST.
//!
//! Everything here goes through the engine before a request is made, so the `net` grant decides —
//! `api.exa.ai` and `mcp.exa.ai` are the hosts that have to be granted, and the same audit log
//! records each one.

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

/// Which way the search is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// A provider that searches on its own. No provider in this build has the capability — the
    /// variant exists so that adding one is a variant and a match arm, not a second tool (D82).
    Provider { provider: String, host: String },
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
    /// The passages Exa thought were the answer, in its order.
    pub highlights: Vec<String>,
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
        Backend::ExaRest => exa_rest(session, &query, count),
        Backend::ExaMcp => exa_mcp(session, &query, count),
        Backend::Provider { provider, host } => Ok(ToolOutput::err(format!(
            "{provider} reports a search capability at {host}, and this build has no request for \
             it; unset it to use Exa instead"
        ))),
    }
}

fn backend_name(backend: &Backend) -> &'static str {
    match backend {
        Backend::Provider { .. } => "provider",
        Backend::ExaRest => "exa",
        Backend::ExaMcp => "exa-mcp",
    }
}

/// Chooses the way the search is asked: a provider that can, else Exa with a key, else Exa's MCP.
fn choose(session: &mut Session) -> Result<Backend, ToolOutput> {
    if let Some((provider, host)) = crate::provider::searching_provider() {
        return Ok(Backend::Provider {
            provider: provider.to_owned(),
            host: host.to_owned(),
        });
    }
    // A key makes the REST endpoint usable; without one it is a payment challenge, not a limit.
    if exa_key(session).is_some() {
        return Ok(Backend::ExaRest);
    }
    Ok(Backend::ExaMcp)
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
    let (status, text) = match fetch::post(&target, true, &headers, &body) {
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
    let (status, text) = match fetch::post(&target, true, &accept, &init) {
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
    let (status, text) = match fetch::post(&target, true, &headers, &call) {
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
        published: text("publishedDate"),
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
            line.push(format!("published {when}"));
        }
        if let Some(who) = &h.author {
            line.push(format!("by {who}"));
        }
        if !line.is_empty() {
            out.push_str(&format!("   {}\n", line.join(", ")));
        }
        for quote in &h.highlights {
            out.push_str(&format!("   > {}\n", quote.replace('\n', " ")));
        }
    }
    cut(out.trim_end())
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
