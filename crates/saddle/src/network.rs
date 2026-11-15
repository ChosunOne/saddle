//! bounded HTTP effects with per-hop caller authority and checked DNS, never guest sockets (D143).

use saddle_limits as budgets;
use std::io::Read;
use std::sync::atomic::Ordering;
use std::time::Instant;

use anyhow::Result;
use saddle_cap::{Principal, Scope};
use saddle_policy::{Kind, Request};
use saddle_wit::saddle::host::http::{Header, Response};
use serde_json::json;
use url::Url;

use crate::cancel;
use crate::session::Session;
use crate::tools::fetch::{self, Target};

pub fn target(raw: &str) -> Result<(Url, Target), String> {
    if raw.len() > budgets::url_bytes() {
        return Err("URL exceeds 8 KiB".into());
    }
    let mut url = Url::parse(raw).map_err(|_| "invalid absolute HTTP URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("URL is not http or https".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL carries userinfo; use headers instead".into());
    }
    url.set_fragment(None);
    let host = url
        .host_str()
        .ok_or("URL has no host")?
        .trim_matches(['[', ']'])
        .to_lowercase();
    let port = url.port_or_known_default().ok_or("URL has no port")?;
    let path = format!(
        "{}{}",
        url.path(),
        url.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    let parsed = Target {
        scheme: url.scheme().into(),
        host,
        port,
        path,
    };
    Ok((url, parsed))
}

pub fn valid_headers(headers: &[Header]) -> Result<(), String> {
    if headers.len() > budgets::http_headers()
        || headers
            .iter()
            .map(|h| h.name.len() + h.value.len())
            .sum::<usize>()
            > budgets::http_header_bytes()
    {
        return Err("headers exceed count/size limit".into());
    }
    for header in headers {
        let name = header.name.to_ascii_lowercase();
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_!#$%&'*+.^`|~".contains(&c))
            || header.value.len() > budgets::header_value_bytes()
            || header.value.chars().any(char::is_control)
        {
            return Err("invalid HTTP header name/value".into());
        }
        if matches!(
            name.as_str(),
            "host"
                | "forwarded"
                | "x-forwarded-for"
                | "x-forwarded-host"
                | "x-real-ip"
                | "via"
                | "proxy-authorization"
                | "proxy-connection"
                | "connection"
                | "transfer-encoding"
                | "content-length"
                | "upgrade"
                | "te"
                | "trailer"
        ) {
            return Err("header may not alter routing or transport framing".into());
        }
    }
    Ok(())
}

pub fn permit(
    session: &mut Session,
    who: &Principal,
    call: &str,
    method: &str,
    target: &Target,
    headers: &[Header],
) -> Result<Result<bool, String>> {
    let names: Vec<String> = headers.iter().map(|h| h.name.clone()).collect();
    let request = Request::Net {
        method,
        scheme: &target.scheme,
        host: &target.host,
        port: target.port,
        path: &target.path,
        headers: &names,
    };
    let named = session.net_names_host(who, &target.host);
    if !named && let Some(why) = fetch::local(target) {
        let mut args = request.args();
        args["call_id"] = call.into();
        session.audit.event(
            Kind::Decision,
            Some(who),
            Some(Scope::Net),
            Some(false),
            args,
            &why,
        )?;
        return Ok(Err(format!("denied: {why}")));
    }
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({"operation":"http"}),
    )?;
    Ok(if decision.allow {
        Ok(named)
    } else {
        Err(format!("denied: {}", decision.explanation))
    })
}

pub struct Operation<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub headers: Vec<Header>,
    pub body: &'a str,
    pub limit: usize,
    pub deadline: Instant,
    pub redirects: bool,
}

pub fn request(
    session: &mut Session,
    who: &Principal,
    call: &str,
    operation: Operation<'_>,
) -> Result<Result<Response, String>> {
    let Operation {
        method,
        url: raw,
        mut headers,
        body,
        limit,
        deadline,
        redirects,
    } = operation;
    if let Err(why) = valid_headers(&headers) {
        return Ok(Err(why));
    }
    let mut current = raw.to_owned();
    for hop in 0..=budgets::http_redirects() {
        if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Ok(Err("network request cancelled or timed out".into()));
        }
        let (url, destination) = match target(&current) {
            Ok(parsed) => parsed,
            Err(why) => return Ok(Err(why)),
        };
        let named = match permit(session, who, call, method, &destination, &headers)? {
            Ok(named) => named,
            Err(why) => return Ok(Err(why)),
        };
        let client =
            fetch::agent_no_redirect(named, deadline.saturating_duration_since(Instant::now()));
        let response = if method == "GET" {
            let mut request = client.get(url.as_str());
            for header in &headers {
                request = request.header(&header.name, &header.value);
            }
            request.call()
        } else if method == "POST" {
            let mut request = client.post(url.as_str());
            for header in &headers {
                request = request.header(&header.name, &header.value);
            }
            request.send(body)
        } else {
            return Ok(Err("unsupported HTTP method".into()));
        };
        let mut response = match response {
            Ok(response) => response,
            Err(_) => return Ok(Err("HTTP connection failed or timed out".into())),
        };
        let status = response.status().as_u16();
        let redirect = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if redirects
            && matches!(status, 301 | 302 | 303 | 307 | 308)
            && let Some(location) = redirect
        {
            if hop == budgets::http_redirects() {
                return Ok(Err("HTTP redirect limit exceeded".into()));
            }
            let next = match url.join(&location) {
                Ok(next) => next,
                Err(_) => return Ok(Err("invalid redirect URL".into())),
            };
            if next.origin() != url.origin() {
                headers.clear();
            }
            current = next.to_string();
            continue;
        }
        let mut returned_headers = Vec::new();
        let mut header_bytes = 0;
        for (name, value) in response.headers() {
            if let Ok(value) = value.to_str() {
                header_bytes += name.as_str().len() + value.len();
                if returned_headers.len() >= budgets::http_headers()
                    || header_bytes > budgets::http_header_bytes()
                {
                    return Ok(Err("response headers exceed bounds".into()));
                }
                returned_headers.push(Header {
                    name: name.as_str().into(),
                    value: value.into(),
                });
            }
        }
        let mut bytes = Vec::new();
        if response
            .body_mut()
            .as_reader()
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return Ok(Err("HTTP body failed or timed out".into()));
        }
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        if cancel::FLAG.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return Ok(Err("network request cancelled or timed out".into()));
        }
        return Ok(Ok(Response {
            status,
            headers: returned_headers,
            body: bytes,
            truncated,
        }));
    }
    Ok(Err("HTTP redirect limit exceeded".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn urls_are_canonical_and_headers_cannot_override_the_target() {
        let (_, parsed) = target("http://2130706433/?secret=value#fragment").unwrap();
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.path, "/?secret=value");
        for url in [
            "file:///etc/passwd",
            "http://user:pass@host/",
            "http://host:invalid/",
        ] {
            assert!(target(url).is_err());
        }
        for name in [
            "host",
            "Proxy-Authorization",
            "Content-Length",
            "x-forwarded-host",
            "",
        ] {
            assert!(
                valid_headers(&[Header {
                    name: name.into(),
                    value: "value".into()
                }])
                .is_err()
            );
        }
    }
}
