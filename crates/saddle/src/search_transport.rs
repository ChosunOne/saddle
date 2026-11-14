//! protected search-provider communication and per-principal bounded response snapshots (D143).

use std::env;
use std::time::Instant;

use anyhow::Result;
use saddle_cap::{Principal, Scope};
use saddle_policy::Kind;
use saddle_wit::saddle::host::http::{Header, Response};
use saddle_wit::saddle::host::web_search::Snapshot;
use serde_json::{Value, json};

use crate::auth::AuthStore;
use crate::network::{self, MAX_SEARCH_RESPONSE, Operation};
use crate::provider::SearchTarget;
use crate::session::Session;

#[derive(Clone)]
pub struct CachedSearch {
    pub snapshot: Snapshot,
    pub source: String,
}

fn key(session: &Session, provider: &str, variable: Option<&str>) -> Option<String> {
    variable
        .and_then(|name| env::var(name).ok())
        .filter(|key| !key.trim().is_empty())
        .or_else(|| {
            AuthStore::load(&session.setup.paths.auth_store())
                .ok()?
                .key(provider)
                .map(str::to_owned)
        })
}

fn provider_key(session: &Session, target: &SearchTarget) -> Option<String> {
    key(session, &target.provider, target.key_env.as_deref())
}

pub fn search(
    session: &mut Session,
    who: &Principal,
    call: &str,
    query: String,
    count: u32,
    deadline: Instant,
) -> Result<Result<Snapshot, String>> {
    if query.trim().is_empty() || query.len() > 16384 || !(1..=100).contains(&count) {
        return Ok(Err("invalid search query/count or exceeds 16 KiB".into()));
    }
    let provider = session.search.clone();
    let exa_key = if provider.is_none() {
        key(session, "exa", Some("EXA_API_KEY"))
    } else {
        None
    };
    let (via, label, url) = if let Some(target) = &provider {
        ("provider", target.provider.clone(), target.url.clone())
    } else if exa_key.is_some() {
        ("exa", "Exa".into(), "https://api.exa.ai/search".into())
    } else {
        ("exa-mcp", "Exa".into(), "https://mcp.exa.ai/mcp".into())
    };
    let (_, target) = match network::target(&url) {
        Ok(target) => target,
        Err(why) => return Ok(Err(why)),
    };
    let mut headers = vec![Header {
        name: "content-type".into(),
        value: "application/json".into(),
    }];
    if let Err(why) = network::permit(session, who, call, "POST", &target, &headers)? {
        return Ok(Err(why));
    }
    // Only this fixed provider protocol can carry protected credentials. No endpoint/header/key
    // selector is accepted from the guest, and redirects are never followed with these headers.
    let secret = if let Some(target) = &provider {
        let Some(secret) = provider_key(session, target) else {
            return Ok(Err(format!(
                "`{}` searches the web, but no key is set: set {}, or store one with `saddle login {}`",
                target.provider,
                target.key_env.as_deref().unwrap_or("its API key"),
                target.provider
            )));
        };
        headers.push(Header {
            name: "authorization".into(),
            value: format!("Bearer {secret}"),
        });
        Some(secret)
    } else if let Some(secret) = exa_key {
        headers.push(Header {
            name: "x-api-key".into(),
            value: secret.clone(),
        });
        Some(secret)
    } else {
        None
    };
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::Net),
        None,
        json!({"call_id":call,"operation":"search","query":query,"results":count,"via":via}),
        "search transport requested",
    )?;
    let payload = match via {
        "provider" => json!({"query":query}),
        "exa" => {
            json!({"query":query,"type":"auto","numResults":count,"contents":{"highlights":true}})
        }
        _ => {
            headers.push(Header {
                name: "accept".into(),
                value: "application/json, text/event-stream".into(),
            });
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"saddle","version":env!("CARGO_PKG_VERSION")}}})
        }
    };
    let mut response = match post(
        session,
        who,
        call,
        &url,
        headers.clone(),
        &payload,
        deadline,
    )? {
        Ok(response) => response,
        Err(why) => return Ok(Err(why)),
    };
    if via == "exa-mcp" && response.status == 200 {
        if let Some(id) = response
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("mcp-session-id"))
        {
            headers.push(Header {
                name: "mcp-session-id".into(),
                value: id.value.clone(),
            });
        }
        let payload = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"web_search_exa","arguments":{"query":query,"numResults":count}}});
        response = match post(session, who, call, &url, headers, &payload, deadline)? {
            Ok(response) => response,
            Err(why) => return Ok(Err(why)),
        };
    }
    if response.truncated {
        return Ok(Err("search response exceeds 1 MiB".into()));
    }
    let mut body = match String::from_utf8(response.body) {
        Ok(body) => body,
        Err(_) => return Ok(Err("search response is not UTF-8".into())),
    };
    if let Some(secret) = secret.filter(|secret| !secret.is_empty()) {
        body = body.replace(&secret, "[redacted credential]");
    }
    if body.len() > MAX_SEARCH_RESPONSE {
        return Ok(Err("search response exceeds 1 MiB".into()));
    }
    let snapshot = Snapshot {
        query,
        count,
        via: via.into(),
        provider: label,
        status: response.status,
        body,
    };
    if response.status == 200 {
        if session.search_snapshots.len() >= 17
            && !session.search_snapshots.contains_key(who)
            && let Some(oldest) = session.search_snapshots.keys().next().cloned()
        {
            session.search_snapshots.remove(&oldest);
        }
        session.search_snapshots.insert(
            who.clone(),
            CachedSearch {
                snapshot: snapshot.clone(),
                source: url,
            },
        );
    }
    Ok(Ok(snapshot))
}

fn post(
    session: &mut Session,
    who: &Principal,
    call: &str,
    url: &str,
    headers: Vec<Header>,
    payload: &Value,
    deadline: Instant,
) -> Result<Result<Response, String>> {
    network::request(
        session,
        who,
        call,
        Operation {
            method: "POST",
            url,
            headers,
            body: &payload.to_string(),
            limit: MAX_SEARCH_RESPONSE,
            deadline,
            redirects: false,
        },
    )
}

pub fn last(
    session: &mut Session,
    who: &Principal,
    call: &str,
) -> Result<Result<Option<Snapshot>, String>> {
    let Some(cached) = session.search_snapshots.get(who).cloned() else {
        return Ok(Ok(None));
    };
    if matches!(who, Principal::Plugin(_)) {
        let (_, target) = match network::target(&cached.source) {
            Ok(target) => target,
            Err(why) => return Ok(Err(why)),
        };
        if let Err(why) = network::permit(session, who, call, "POST", &target, &[])? {
            return Ok(Err(why));
        }
    }
    Ok(Ok(Some(cached.snapshot)))
}
