use saddle_limits as budgets;
use saddle_sdk::SearchSnapshot;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::configure_limits;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, Output, Tool, last_search, search_web};
use serde::Deserialize;
use serde_json::Value;
#[cfg(target_arch = "wasm32")]
use serde_json::json;

pub struct Websearch;
pub trait Transport {
    fn search(&mut self, query: &str, count: u32) -> Result<SearchSnapshot, String>;
    fn last(&mut self) -> Result<Option<SearchSnapshot>, String>;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    query: Option<String>,
    num_results: Option<u64>,
    result: Option<u64>,
}

pub fn search_with(arguments: &str, transport: &mut impl Transport) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.result == Some(0) || args.num_results == Some(0) {
        return Err("result and num_results must be a positive whole number".into());
    }
    let query = args.query.unwrap_or_default().trim().to_owned();
    let count = args
        .num_results
        .unwrap_or(budgets::web_default_results() as u64)
        .clamp(1, budgets::web_results() as u64) as u32;
    let snapshot = if args.result.is_some() {
        match transport.last()? {
            Some(last) if query.is_empty() || last.query == query => last,
            _ if !query.is_empty() => {
                transport.search(&query, budgets::web_default_results() as u32)?
            }
            _ => {
                return Err(
                    "no search to expand in this session; call websearch with a `query` first"
                        .into(),
                );
            }
        }
    } else {
        if query.is_empty() {
            return Err("`query` is empty; to read a result pass `result`".into());
        }
        transport.search(&query, count)?
    };
    if snapshot.status != 200 {
        return Err(if snapshot.via == "provider" {
            provider_explain(snapshot.status, &snapshot.body, &snapshot.provider)
        } else {
            explain(snapshot.status, &snapshot.body)
        });
    }
    if snapshot.via == "exa-mcp" {
        let answer = parse_mcp(&snapshot.body).ok_or("Exa's MCP server sent no result")?;
        if let Some(error) = answer.get("error") {
            return Err(format!("Exa answered: {error}"));
        }
        let text = answer
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if args.result.is_some() {
            return Err("this anonymous search returned unstructured text, not numbered full-page results; use fetch with a result URL".into());
        }
        return Ok(format!(
            "{}\n\n(Exa's MCP server, searched without a key; set EXA_API_KEY for more)",
            cut(text)
        ));
    }
    let mut hits = parse_exa(&snapshot.body)?;
    hits.truncate(snapshot.count as usize);
    let last = LastSearch {
        query: snapshot.query.clone(),
        via: if snapshot.via == "provider" {
            "provider"
        } else {
            "exa"
        },
        hits,
    };
    if let Some(index) = args.result {
        let hit = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| last.hits.get(i))
            .ok_or_else(|| {
                format!(
                    "`result` is {index} and the search of `{}` returned {}; numbering starts at 1",
                    last.query,
                    last.hits.len()
                )
            })?;
        return Ok(one_in_full(hit, index, last.hits.len(), &last));
    }
    if snapshot.via == "provider" {
        let asked = if count > budgets::web_default_results() as u32 {
            format!(
                " ({count} asked, {} returned: the endpoint answers {})",
                last.hits.len(),
                budgets::web_default_results()
            )
        } else {
            String::new()
        };
        Ok(format!(
            "{} searched for `{}` and answered:\n\n{}{}",
            snapshot.provider,
            last.query,
            format_hits(&last.hits),
            asked
        ))
    } else {
        Ok(format!(
            "{}\n\n({count} asked of Exa, {})",
            format_hits(&last.hits),
            describe(&snapshot.body)
        ))
    }
}

#[cfg(target_arch = "wasm32")]
struct HostTransport;
#[cfg(target_arch = "wasm32")]
impl Transport for HostTransport {
    fn search(&mut self, query: &str, count: u32) -> Result<SearchSnapshot, String> {
        search_web(query, count)
    }
    fn last(&mut self) -> Result<Option<SearchSnapshot>, String> {
        last_search()
    }
}
#[cfg(target_arch = "wasm32")]
impl Tool for Websearch {
    fn describe() -> Definition {
        Definition {name:"websearch".into(),description:"search the web with the configured provider or Exa fallback, under this plugin's delegated net grants. Returns numbered excerpts; pass result to read a page from this plugin's last search without another network request. Query with result searches only if it differs from the cached query. Credentials stay in the core. Response and presentation byte budgets are host-configurable. Anonymous MCP answers may be unstructured.".into(),input_schema:json!({"type":"object","properties":{"query":{"type":"string"},"num_results":{"type":"integer","minimum":1},"result":{"type":"integer","minimum":1}},"additionalProperties":false}).to_string()}
    }
    fn invoke(arguments: String) -> Output {
        if let Err(text) = configure_limits() {
            return Output {
                text,
                is_error: true,
            };
        }
        match search_with(&arguments, &mut HostTransport) {
            Ok(text) => Output {
                text,
                is_error: false,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub published: Option<String>,
    pub author: Option<String>,
    pub highlights: Vec<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastSearch {
    pub query: String,
    pub via: &'static str,
    pub hits: Vec<Hit>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    FromCache,
    Fresh,
    Nothing,
}

#[cfg(test)]
fn plan(last: Option<&LastSearch>, query: &str) -> Plan {
    match (last, query.is_empty()) {
        (Some(_), true) => Plan::FromCache,
        (Some(last), false) if last.query == query => Plan::FromCache,
        (_, true) => Plan::Nothing,
        (_, false) => Plan::Fresh,
    }
}

fn one_in_full(hit: &Hit, index: u64, of: usize, last: &LastSearch) -> String {
    let mut out = format!("{index}. {}\n   {}\n", title_of(hit), hit.url);
    if let Some(when) = &hit.published {
        out.push_str(&format!("   published {}\n", date_of(when)));
    }
    let body = hit.text.as_deref().map(str::trim).filter(|t| !t.is_empty());
    match body {
        Some(text) => {
            out.push('\n');
            out.push_str(&whole_text(text, &hit.url));
            out.push_str(&format!(
                "\n\n(result {index} of {of}, in full, from the search of `{}` via {})",
                last.query, last.via
            ));
        }
        None => out.push_str(
            "\n   (this result came without its text; the page itself is at the URL above, for \
             `fetch`)",
        ),
    }
    out
}

fn whole_text(text: &str, url: &str) -> String {
    if text.len() <= budgets::presentation_bytes() {
        return text.to_owned();
    }
    let mut end = budgets::presentation_bytes();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n(showing the first {end} of {} bytes; the page itself is at {url})",
        &text[..end],
        text.len()
    )
}

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

fn parse_exa(body: &str) -> Result<Vec<Hit>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("Exa's answer is not JSON: {e}"))?;
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| "Exa's answer has no results".to_owned())?;
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
        for passage in h.excerpts().iter().take(budgets::web_quotes()) {
            out.push_str(&format!("   > {}\n", passage_for_model(passage)));
        }
    }
    out.push_str("\n(full text of one result: call websearch with {\"result\": N})");
    cut(out.trim_end())
}

fn passage_for_model(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= budgets::web_quote_chars() {
        return one_line;
    }
    let kept: String = one_line.chars().take(budgets::web_quote_chars()).collect();
    format!("{kept}…")
}

impl Hit {
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

fn cut(text: &str) -> String {
    if text.len() <= budgets::presentation_bytes() {
        return text.to_owned();
    }
    let mut end = budgets::presentation_bytes();
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n(cut at {} bytes)",
        &text[..end],
        budgets::presentation_bytes()
    )
}

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

#[cfg(test)]
fn mcp_session(body: &str) -> Option<String> {
    body.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("mcp-session-id")
            .then(|| value.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

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
    use std::str;

    #[test]
    fn a_rest_answer_becomes_numbered_results() {
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
        assert!(text.contains("2. https://docs.rs/syntect"), "{text}");
        assert!(describe(body).contains("request abc") && describe(body).contains("$0.0050"));
    }

    #[test]
    fn the_documented_search_shape_is_read() {
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
        assert!(text.contains("published 2025-11-05\n"), "{text}");
        assert!(!text.contains("2025-11-05T00:00"), "{text}");
        assert_eq!(date_of("not a date"), "not a date");
        assert_eq!(date_of("2025-11-05"), "2025-11-05");
        let bare = Hit {
            title: "t".into(),
            url: "https://t/".into(),
            ..Hit::default()
        };
        assert!(!format_hits(&[bare]).contains("> "), "quoted nothing");
        let empty = Hit {
            title: "t".into(),
            url: "https://t/".into(),
            highlights: vec![String::new(), "  ".into()],
            text: Some("real text".into()),
            ..Hit::default()
        };
        assert!(format_hits(&[empty]).contains("> real text"));
    }

    fn last_search(hits: Vec<Hit>) -> LastSearch {
        LastSearch {
            query: "rust atomics".to_owned(),
            via: "provider",
            hits,
        }
    }

    #[test]
    fn a_result_can_be_read_in_full_from_the_search_already_made() {
        let hit = Hit {
            title: "Atomics".into(),
            url: "https://doc.rust-lang.org/nomicon/atomics.html".into(),
            published: Some("2025-06-26T00:00:00.000Z".into()),
            text: Some("the whole page, not an excerpt".into()),
            ..Hit::default()
        };
        let held = last_search(vec![hit.clone(), hit]);
        let text = one_in_full(&held.hits[0], 1, held.hits.len(), &held);
        assert!(
            text.starts_with("1. Atomics\n   https://doc.rust-lang.org"),
            "{text}"
        );
        assert!(text.contains("published 2025-06-26"), "{text}");
        assert!(text.contains("the whole page, not an excerpt"), "{text}");
        assert!(
            text.contains(
                "(result 1 of 2, in full, from the search of `rust atomics` via provider)"
            ),
            "{text}"
        );
        let bare = Hit {
            title: "t".into(),
            url: "https://t/".into(),
            ..Hit::default()
        };
        let held = last_search(vec![bare]);
        let text = one_in_full(&held.hits[0], 1, 1, &held);
        assert!(text.contains("came without its text"), "{text}");
        assert!(text.contains("`fetch`"), "{text}");
    }

    #[test]
    fn a_long_page_is_cut_at_what_a_tool_result_carries() {
        let page = "word ".repeat(60_000);
        let text = whole_text(&page, "https://example.test/");
        assert!(text.len() < page.len(), "not cut");
        assert!(
            text.contains(&format!("of {} bytes", page.len())),
            "no total"
        );
        assert!(
            text.contains("https://example.test/"),
            "no address to fetch the rest"
        );
        assert!(
            text.len() <= budgets::presentation_bytes() + 200,
            "{} bytes",
            text.len()
        );
        assert_eq!(whole_text("short", "https://x/"), "short");
        let multibyte = "é".repeat(budgets::presentation_bytes());
        assert!(str::from_utf8(whole_text(&multibyte, "https://x/").as_bytes()).is_ok());
    }

    #[test]
    fn an_expansion_reads_the_search_it_names_or_searches_once_more() {
        let held = last_search(vec![]);
        assert_eq!(
            plan(Some(&held), ""),
            Plan::FromCache,
            "no query, use the held"
        );
        assert_eq!(plan(Some(&held), "rust atomics"), Plan::FromCache);
        assert_eq!(plan(Some(&held), "something else"), Plan::Fresh);
        assert_eq!(plan(None, "rust atomics"), Plan::Fresh);
        assert_eq!(plan(None, ""), Plan::Nothing);
    }

    #[test]
    fn the_results_say_how_to_read_one_in_full() {
        let hit = Hit {
            title: "t".into(),
            url: "https://t/".into(),
            text: Some("x".into()),
            ..Hit::default()
        };
        let text = format_hits(&[hit]);
        assert!(text.contains("result\": N"), "{text}");
        assert_eq!(format_hits(&[]), "no results");
    }

    #[test]
    fn an_answer_with_no_results_says_so() {
        assert_eq!(format_hits(&[]), "no results");
        assert_eq!(parse_exa(r#"{"results": []}"#).unwrap(), []);
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
        assert!(explain(500, "<html>oops</html>").contains("<html>oops</html>"));
    }

    #[test]
    fn a_passage_is_one_line_of_bounded_length() {
        let page = "word ".repeat(5000);
        let quoted = passage_for_model(&page);
        assert_eq!(
            quoted.chars().count(),
            budgets::web_quote_chars() + 1,
            "kept or ellipsis wrong"
        );
        assert!(quoted.ends_with('…'), "{quoted:.20}");
        assert_eq!(passage_for_model("one\n two\n\nthree"), "one two three");
        let hit = Hit {
            title: "a".into(),
            url: "https://a/".into(),
            highlights: (0..10).map(|i| format!("passage {i}")).collect(),
            ..Hit::default()
        };
        let text = format_hits(&[hit]);
        assert_eq!(
            text.matches("   > ").count(),
            budgets::web_quotes(),
            "{text}"
        );
        let hits: Vec<Hit> = (0..5)
            .map(|i| Hit {
                title: format!("t{i}"),
                url: format!("https://a/{i}"),
                highlights: vec![page.clone()],
                ..Hit::default()
            })
            .collect();
        assert!(
            format_hits(&hits).len() < budgets::presentation_bytes(),
            "not bounded"
        );
    }

    #[test]
    fn a_reply_is_cut_to_what_a_tool_result_may_carry() {
        let long = "x".repeat(budgets::presentation_bytes() + 100);
        let shortened = cut(&long);
        assert!(
            shortened.len() < budgets::presentation_bytes() + 100,
            "not cut"
        );
        assert!(shortened.ends_with("bytes)"), "{shortened:?}");
        assert_eq!(cut("short"), "short");
        let multibyte = "é".repeat(budgets::presentation_bytes());
        assert!(str::from_utf8(cut(&multibyte).as_bytes()).is_ok());
    }
}
