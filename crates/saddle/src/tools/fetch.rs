//! `fetch`: one HTTP GET, made by the host rather than by a child process (D39).
//!
//! The host makes the connection, checks the grant against the URL and logs the decision (D10).
//! `websearch` also makes host-side requests (D82); commands reach the host's policy-checking proxy
//! through their own network namespace (D79), never through unrestricted host sockets.
//!
//! D28's address rules are shared with the command network bridge: loopback, RFC 1918,
//! link-local, unique-local, CGNAT and the cloud metadata address are refused unless a grant names
//! that address itself. ureq receives checked addresses rather than resolving the name again, so
//! DNS rebinding cannot swap one underneath.

use saddle_limits as budgets;
use std::net::Ipv4Addr;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::result::Result as StdResult;
use std::time::Duration;
use ureq::config::Config;

use anyhow::Result;
use saddle_cap::Principal;
use serde_json::Value;

#[cfg(test)]
use serde_json::json;
use ureq::http::Uri;
use ureq::unversioned::resolver::{ArrayVec, DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use super::ToolOutput;
use crate::network::{self, Operation};
use crate::session::Session;
use saddle_sdk::{Header as SdkHeader, HttpResponse};
use saddle_tools::fetch::fetch_with;
use saddle_wit::saddle::host::http::Header as HttpHeader;
use std::time::Instant;

/// A URL as the policy and the request need it: the parts are separate because a grant matches on
/// scheme, host and port rather than on the whole string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path and query together, as they go on the request line.
    pub path: String,
}

impl Target {
    /// Parses `scheme://host[:port][/path]`. Only http and https are known; userinfo is refused
    /// rather than stripped, because `https://user@host/` reads as one host and means another.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| format!("`{raw}` has no scheme; write https://host/path"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!("`{scheme}` is not http or https"));
        }
        if rest.contains('@') {
            return Err(format!(
                "`{raw}` carries userinfo; put the key in a header instead"
            ));
        }
        let default = if scheme == "https" { 443 } else { 80 };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], rest[i..].to_owned()),
            None => (rest, "/".to_owned()),
        };
        if authority.is_empty() {
            return Err(format!("`{raw}` has no host"));
        }
        // An IPv6 literal is bracketed, and its colons are not a port.
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| format!("`{raw}` has an unclosed IPv6 address"))?;
            match after.strip_prefix(':') {
                Some(p) => (host.to_owned(), p.parse::<u16>().ok()),
                None => (host.to_owned(), None),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h.to_owned(), p.parse::<u16>().ok()),
                None => (authority.to_owned(), None),
            }
        };
        if host.is_empty() {
            return Err(format!("`{raw}` has no host"));
        }
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port: port.unwrap_or(default),
            path,
        })
    }

    /// The address as it goes in the audit record: the query is redacted by the engine (D24).
    pub(crate) fn request_path(&self) -> &str {
        &self.path
    }
}

pub fn fetch(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let mut fatal = None;
    let result = fetch_with(&input.to_string(), |request| {
        let headers = request
            .headers
            .into_iter()
            .map(|header| HttpHeader {
                name: header.name,
                value: header.value,
            })
            .collect();
        match network::request(
            session,
            &Principal::Agent,
            "native-fetch",
            Operation {
                method: "GET",
                url: &request.url,
                headers,
                body: "",
                limit: budgets::http_capture_bytes(),
                deadline: Instant::now() + Duration::from_secs(30),
                redirects: true,
            },
        ) {
            Ok(result) => result.map(|response| HttpResponse {
                status: response.status,
                headers: response
                    .headers
                    .into_iter()
                    .map(|header| SdkHeader {
                        name: header.name,
                        value: header.value,
                    })
                    .collect(),
                body: response.body,
                truncated: response.truncated,
            }),
            Err(error) => {
                fatal = Some(error);
                Err("network host failure".into())
            }
        }
    });
    if let Some(error) = fatal {
        return Err(error);
    }
    Ok(match result {
        Ok(text) => ToolOutput::ok(text),
        Err(why) => ToolOutput::err(why),
    })
}

/// The `headers` object, checked: a name is a token, and a value is opaque but bounded. A header
/// that would let the request reach somewhere else (`host`, and the proxy headers) is refused,
/// since a grant is checked against the URL and a header must not be able to change what that URL
/// means (D48).
#[cfg(test)]
fn headers_field(input: &Value) -> Result<Vec<(String, String)>, ToolOutput> {
    let Some(value) = input.get("headers") else {
        return Ok(Vec::new());
    };
    let Some(object) = value.as_object() else {
        return Err(ToolOutput::err(
            "`headers` must be an object of name to value",
        ));
    };
    let mut out = Vec::with_capacity(object.len());
    for (name, value) in object {
        let Some(value) = value.as_str() else {
            return Err(ToolOutput::err(format!("header `{name}` is not a string")));
        };
        let lower = name.to_ascii_lowercase();
        if !lower
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(ToolOutput::err(format!("`{name}` is not a header name")));
        }
        if matches!(
            lower.as_str(),
            "host" | "x-forwarded-for" | "x-forwarded-host" | "x-real-ip" | "forwarded" | "via"
        ) {
            return Err(ToolOutput::err(format!(
                "`{name}` is refused: a header may not change which host the request reaches, and \
                 the grant was checked against the URL"
            )));
        }
        if value.chars().any(|c| c.is_control()) {
            return Err(ToolOutput::err(format!(
                "header `{name}` carries a control character"
            )));
        }
        if value.len() > 8192 {
            return Err(ToolOutput::err(format!(
                "header `{name}` is longer than 8 KiB"
            )));
        }
        out.push((name.clone(), value.to_owned()));
    }
    Ok(out)
}

/// Refuses a target that is this machine or the network behind it, saying which grant would let it
/// through (D40).
///
/// The answer names the address rather than the name that resolved to it, because that is what the
/// grant has to say: `{ host = "127.0.0.1", port = 8080 }`. A wildcard cannot stand in, since it
/// covers only public space.
pub(crate) fn local(target: &Target) -> Option<String> {
    // `localhost` is loopback by definition (RFC 6761), so it is recognised without a lookup.
    // Anything else is decided after the address is resolved, and only once the policy has allowed
    // the request, so a denied host is never looked up.
    let host = target.host.trim_end_matches('.').to_ascii_lowercase();
    let what = match host.parse::<IpAddr>() {
        Ok(ip) => private(ip)?,
        Err(_) if host == "localhost" => "loopback",
        Err(_) => return None,
    };
    // The grant has to name the host as it was written: a grant for `127.0.0.1` does not name
    // `localhost`, though both reach the same server.
    // An IPv6 literal is written back from the parsed address, so `[::1]` becomes `::1`.
    let name = match target.host.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => target.host.clone(),
    };
    let mut grant = format!("{{ host = \"{name}\"");

    if !matches!(
        (target.scheme.as_str(), target.port),
        ("https", 443) | ("http", 80)
    ) {
        grant.push_str(&format!(", port = {}", target.port));
    }
    if target.scheme == "http" {
        grant.push_str(", scheme = \"http\"");
    }
    grant.push_str(&format!(
        ", why = \"reaches the local server on {}:{}\" }}",
        target.host, target.port
    ));
    Some(format!(
        "{what} is not reachable by a grant for a name or for every host: reaching it takes a net \
         grant that names the address itself. Add \n  net = [{grant}]\nto [agent] in this \
         workspace's policy file (`saddle check` prints its path), then start a new session. A \
         local server is outside every grant's reach on purpose, \
         since {what} is the machine saddle runs on rather than the task's business."
    ))
}

/// The client every host-side request is made with: a resolver that pins the addresses that were
/// checked, so a name cannot be swapped between the decision and the connection (D28).
/// The same agent with a deadline of the caller's choosing, which is what an MCP server's
/// `[mcp.<name>] timeout` is (D113).
pub(crate) fn agent_for(named: bool, timeout: Duration) -> ureq::Agent {
    agent_configured(named, timeout, false)
}

/// manual redirect handling permits the caller to decide every hop (D143).
pub(crate) fn agent_no_redirect(named: bool, timeout: Duration) -> ureq::Agent {
    agent_configured(named, timeout, true)
}

fn agent_configured(named: bool, timeout: Duration, no_redirect: bool) -> ureq::Agent {
    // `with_parts` is the only way to install a resolver. The connector is the default one, so TLS
    // and pools are unchanged.
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .user_agent(concat!("saddle/", env!("CARGO_PKG_VERSION")));
    let config = if no_redirect {
        config.max_redirects(0)
    } else {
        config
    }
    .build();
    ureq::Agent::with_parts(
        config,
        DefaultConnector::new(),
        Checked {
            inner: DefaultResolver::default(),
            // A grant that names an address may reach it; anything else resolves to public space
            // only.
            named,
        },
    )
}

/// The authority for the request line: the default port is left out, and an IPv6 literal is
/// bracketed again.
pub(crate) fn authority(host: &str, port: u16, https: bool) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    if (https && port == 443) || (!https && port == 80) {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Refuses addresses that reach the machine saddle runs on or the network behind it (D28).
///
/// The exemption is decided by the caller from the *grants*: a private address is reachable only
/// when a grant spells that address out (`session.net_names_host`), so `http://127.0.0.1:8080/`
/// works when someone wrote that address down, and a wildcard grant still does not open loopback
/// (D40). Deciding it from the shape of the URL instead would make `*` mean the local machine.
pub(crate) fn private(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                Some("loopback")
            } else if v4.is_private() {
                Some("a private network address")
            } else if v4.is_link_local() {
                Some("a link-local address")
            } else if v4.is_broadcast() || v4.is_unspecified() {
                Some("an unusable address")
            } else if o[0] == 169 && o[1] == 254 {
                // The cloud metadata endpoint, which is what an SSRF is usually after.
                Some("169.254.0.0/16, where cloud metadata lives")
            } else if o[0] == 100 && (64..128).contains(&o[1]) {
                Some("a carrier-grade NAT address")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                Some("loopback")
            } else if v6.is_unspecified() {
                Some("an unusable address")
            } else if (v6.segments()[0] & 0xfe00) == 0xfc00 {
                Some("a unique-local address")
            } else if (v6.segments()[0] & 0xffc0) == 0xfe80 {
                Some("a link-local address")
            } else if let Some(v4) = v6.to_ipv4_mapped() {
                // ::ffff:127.0.0.1 is loopback however it is spelled.
                private(IpAddr::V4(v4))
            } else {
                None
            }
        }
    }
}

/// Checks where a name resolves before a request is built, so a refusal can say why (D28).
pub(crate) fn resolve_public(host: &str, port: u16) -> Result<(), String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host} did not resolve: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} did not resolve"));
    }
    let public: Vec<&SocketAddr> = addrs.iter().filter(|a| private(a.ip()).is_none()).collect();
    if public.is_empty() {
        let first = addrs.first().copied();
        let what = first
            .and_then(|a| private(a.ip()))
            .unwrap_or("an address this workspace may not reach");
        // Named by the address it resolved to, which is what the grant has to say.
        let host_ip = first.map(|a| a.ip().to_string()).unwrap_or_default();
        return Err(format!(
            "{host} resolves to {what} ({host_ip}), which no grant for a name or for every host \
             can reach. Add a net grant that names the address itself, as in\n  net = [{{ host = \
             \"{host_ip}\"{port} }}]\nto [agent] in this workspace's policy file (`saddle check` \
             prints its path), then start a new session.",
            port = if port == 443 || port == 80 {
                String::new()
            } else {
                format!(", port = {port}")
            }
        ));
    }
    Ok(())
}

/// A resolver that drops the checked addresses before ureq connects, so the address that was
/// approved is the address that is dialled (D28, DNS rebinding).
#[derive(Debug)]
struct Checked {
    inner: DefaultResolver,
    /// The grant named the address, so a private one may be dialled.
    named: bool,
}

impl Resolver for Checked {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> StdResult<ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.inner.resolve(uri, config, timeout)?;
        let mut out: ResolvedSocketAddrs =
            ArrayVec::from_fn(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        for addr in addrs.iter() {
            if (self.named || private(addr.ip()).is_none()) && out.try_push(*addr).is_err() {
                break;
            }
        }
        if out.is_empty() {
            // Every address was refused, so there is nothing to dial. The reason is reported by
            // `resolve_public` before the request is built; this is the backstop for a resolver
            // whose answer changed between the check and here.
            return Err(ureq::Error::HostNotFound);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_becomes_the_parts_a_grant_matches() {
        let t = Target::parse("https://api.synthetic.new/openai/v1/models?x=1").unwrap();
        assert_eq!(t.scheme, "https");
        assert_eq!(t.host, "api.synthetic.new");
        assert_eq!(t.port, 443);
        assert_eq!(t.path, "/openai/v1/models?x=1");

        let t = Target::parse("http://127.0.0.1:8080/v1/").unwrap();
        assert_eq!(t.port, 8080);
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(authority(&t.host, t.port, false), "127.0.0.1:8080");

        // No path means the root, and the default port is not written out.
        let t = Target::parse("https://Example.COM").unwrap();
        assert_eq!(t.path, "/");
        assert_eq!(t.host, "example.com");
        assert_eq!(authority(&t.host, t.port, true), "example.com");

        // An IPv6 literal keeps its brackets and its colons are not a port.
        let t = Target::parse("https://[2001:db8::1]:8443/x").unwrap();
        assert_eq!(t.host, "2001:db8::1");
        assert_eq!(t.port, 8443);
        assert_eq!(authority(&t.host, t.port, true), "[2001:db8::1]:8443");
    }

    #[test]
    fn a_url_that_is_not_plain_http_is_refused_before_anything_is_sent() {
        assert!(
            Target::parse("file:///etc/passwd")
                .unwrap_err()
                .contains("not http")
        );
        assert!(
            Target::parse("gopher://x")
                .unwrap_err()
                .contains("not http")
        );
        assert!(Target::parse("api.x/v1").unwrap_err().contains("no scheme"));
        assert!(Target::parse("https://").unwrap_err().contains("no host"));
        // userinfo reads as one host and means another, so it is refused rather than stripped.
        assert!(
            Target::parse("https://user@evil.test/")
                .unwrap_err()
                .contains("userinfo")
        );
    }

    #[test]
    fn a_local_server_is_refused_with_the_grant_that_would_allow_it() {
        let t = Target::parse("http://127.0.0.1:8080/v1/models").unwrap();
        let why = local(&t).expect("a local address");
        // It names the exact grant, including the port and the scheme this URL needs.
        assert!(
            why.contains(r#"net = [{ host = "127.0.0.1", port = 8080, scheme = "http""#),
            "{why}"
        );
        assert!(why.contains("policy file"), "{why}");
        assert!(why.contains("loopback"), "{why}");

        // A default port and https need neither spelled out.
        let t = Target::parse("https://10.0.0.5/x").unwrap();
        let why = local(&t).expect("a private address");
        assert!(
            why.contains(r#"net = [{ host = "10.0.0.5""#) && !why.contains("port ="),
            "{why}"
        );

        // `localhost` is loopback by definition, and the grant it suggests names it as written.
        let t = Target::parse("http://localhost:8080/").unwrap();
        let why = local(&t).expect("localhost");
        assert!(
            why.contains(r#"net = [{ host = "localhost", port = 8080, scheme = "http""#),
            "{why}"
        );

        // A public host is not this function's business.
        assert_eq!(local(&Target::parse("https://example.com/").unwrap()), None);
    }

    #[test]
    fn headers_are_checked_and_a_value_is_carried_but_a_host_cannot_be() {
        let parse = |v: Value| headers_field(&v);
        // A key goes in a header, and it is carried as written.
        let headers = parse(json!({"headers": {"authorization": "Bearer sk-1"}})).unwrap();
        assert_eq!(
            headers,
            [("authorization".to_owned(), "Bearer sk-1".to_owned())]
        );
        assert_eq!(parse(json!({"url": "https://x/"})).unwrap(), vec![]);

        // What a header may not do is change where the request goes.
        for name in ["host", "Host", "x-forwarded-for", "forwarded"] {
            let e = parse(json!({"headers": {name: "evil.test"}})).unwrap_err();
            assert!(
                e.content.contains("may not change which host"),
                "{name}: {}",
                e.content
            );
        }
        // A name that is not a name, a value with a newline, and an oversized value.
        assert!(parse(json!({"headers": {"bad name": "v"}})).is_err());
        assert!(parse(json!({"headers": {"x": "a\r\nInjected: 1"}})).is_err());
        assert!(parse(json!({"headers": {"x": "a".repeat(9000)}})).is_err());
        assert!(parse(json!({"headers": "not an object"})).is_err());
        assert!(parse(json!({"headers": {"x": 1}})).is_err());
    }

    #[test]
    fn the_addresses_an_ssrf_wants_are_refused() {
        for ip in [
            "127.0.0.1",
            "::1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "fe80::1",
            "fd00::1",
            "0.0.0.0",
            "::ffff:127.0.0.1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(private(ip).is_some(), "{ip} should be refused");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(private(ip).is_none(), "{ip} should be allowed");
        }
    }
}
