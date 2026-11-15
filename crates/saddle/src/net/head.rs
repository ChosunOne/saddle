//! What a plaintext request says about the host it is for (D79).
//!
//! Three shapes arrive at the relay, and each names its host somewhere different: a `CONNECT` line
//! (`host:port`), an absolute request line (`GET http://host/path`), or an origin-form request line
//! whose host is in the `Host` header. Parsing them here rather than in the socket loop is what
//! makes the security-relevant decision testable without a network.
//!
//! A request that names no host is [`Route::Unknown`], which the proxy refuses: the guest is
//! supposed to be reaching the relay under a name, and an unnamed request is one whose host cannot
//! be checked against a grant.

use crate::net::client::Target;
use std::str;

/// A request head this large is refused rather than buffered without bound.
pub const MAX_HEAD: usize = 16 * 1024;

/// Where the bytes of a plaintext connection should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Nothing decidable has arrived yet.
    Incomplete,
    /// Not a request this proxy understands.
    Unknown,
    /// The head never ended within the size a head may be.
    TooLong,
    /// `CONNECT host:port`: the rest of the connection is carried through unchanged.
    Tunnel(Target),
    /// A request whose line carried the whole URL: forwarded in origin form to this target.
    Absolute { target: Target, head: Vec<u8> },
    /// An origin-form request naming its host in a header: forwarded to this target unchanged.
    Origin { target: Target, head: Vec<u8> },
}

/// Reads the first request of a plaintext connection.
///
/// The returned head is the part up to and including the blank line; the caller forwards it and
/// then carries whatever follows, so a later request on the same connection is read as a new head
/// rather than being spliced blindly.
pub fn route(buf: &[u8]) -> Route {
    let Some(end) = head_end(buf) else {
        return if buf.len() > MAX_HEAD {
            Route::TooLong
        } else {
            Route::Incomplete
        };
    };
    let head = &buf[..end];
    let Some(line) = first_line(head) else {
        return Route::Unknown;
    };
    let mut parts = line.split(' ');
    let (Some(method), Some(rest)) = (parts.next(), parts.next()) else {
        return Route::Unknown;
    };
    if method == "CONNECT" {
        return match connect_target(rest) {
            Some(target) => Route::Tunnel(target),
            None => Route::Unknown,
        };
    }
    // An absolute request line carries the scheme, so it is the only form whose scheme is not a
    // guess.
    if rest.contains("://") {
        let Ok(target) = Target::parse(rest) else {
            return Route::Unknown;
        };
        let rewritten = origin_form(method, &target, head);
        return Route::Absolute {
            target,
            head: rewritten,
        };
    }
    let Some(host) = header(head, "host") else {
        return Route::Unknown;
    };
    match Target::parse(&format!("http://{host}")) {
        Ok(target) => Route::Origin {
            target,
            head: head.to_vec(),
        },
        Err(_) => Route::Unknown,
    }
}

/// The scheme a `CONNECT` to `port` is for. A tunnel to 80 is plaintext HTTP and anything else is
/// TLS, which is what a grant's scheme has to match.
pub fn connect_scheme(port: u16) -> &'static str {
    if port == 80 { "http" } else { "https" }
}

/// [`Target`] for `CONNECT`, whose authority is `host:port` with no scheme of its own.
fn connect_target(authority: &str) -> Option<Target> {
    let authority = authority.trim();
    if authority.is_empty() {
        return None;
    }
    // A CONNECT without a port would leave the scheme undecidable, and the scheme is half of what
    // a grant matches on.
    let bracketed = authority.starts_with('[');
    let has_port = match authority.rfind(']') {
        Some(i) => authority[i..].starts_with("]:"),
        None => !bracketed && authority.matches(':').count() == 1,
    };
    if !has_port {
        return None;
    }
    let port = authority.rsplit_once(':')?.1.parse::<u16>().ok()?;
    Target::parse(&format!("{}://{authority}", connect_scheme(port))).ok()
}

/// Rewrites `GET http://host/p HTTP/1.1` as `GET /p HTTP/1.1`, which is what the origin expects,
/// and makes sure the head names its host.
fn origin_form(method: &str, target: &Target, head: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {} HTTP/1.1\r\n", target.path).into_bytes();
    let rest = match head.iter().position(|b| *b == b'\n') {
        Some(i) => &head[i + 1..],
        None => &[],
    };
    if !rest.is_empty() {
        out.extend_from_slice(rest);
    }
    if header(head, "host").is_none() {
        let host = if target.port == default_port(&target.scheme) {
            target.host.clone()
        } else {
            format!("{}:{}", target.host, target.port)
        };
        // After the request line, before the blank line that ends the head.
        let at = out
            .windows(1)
            .position(|w| w[0] == b'\n')
            .map(|i| i + 1)
            .unwrap_or(out.len());
        let line = format!("Host: {host}\r\n");
        out.splice(at..at, line.into_bytes());
    }
    out
}

fn default_port(scheme: &str) -> u16 {
    if scheme == "https" { 443 } else { 80 }
}

/// The byte after the blank line that ends the head.
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn first_line(head: &[u8]) -> Option<&str> {
    let end = head.windows(2).position(|w| w == b"\r\n")?;
    str::from_utf8(&head[..end]).ok()
}

/// One header value by lowercase name. The first occurrence wins, as HTTP requires for `Host`.
fn header(head: &[u8], name: &str) -> Option<String> {
    let mut lines = head.split(|b| *b == b'\n').skip(1);
    lines.find_map(|line| {
        let line = str::from_utf8(line.strip_suffix(b"\r")?).ok()?;
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
}

/// Every header name in a head, lowercased, for the audit record: a value is where a key lives,
/// so it is never part of what a decision or a record carries (D48).
pub fn header_names(head: &[u8]) -> Vec<String> {
    head.split(|b| *b == b'\n')
        .skip(1)
        .filter_map(|line| {
            let line = str::from_utf8(line.strip_suffix(b"\r")?).ok()?;
            let (key, _) = line.split_once(':')?;
            Some(key.trim().to_ascii_lowercase())
        })
        .filter(|name| !name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(head: &str) -> Route {
        route(head.as_bytes())
    }

    #[test]
    fn a_connect_line_names_its_host_and_port() {
        let Route::Tunnel(target) =
            r("CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io:443\r\n\r\n")
        else {
            panic!("not a tunnel");
        };
        assert_eq!(target.scheme, "https");
        assert_eq!(target.host, "crates.io");
        assert_eq!(target.port, 443);
        // A tunnel to 80 is plaintext, so a grant has to have been written for http.
        let Route::Tunnel(plain) = r("CONNECT example.com:80 HTTP/1.1\r\n\r\n") else {
            panic!("not a tunnel");
        };
        assert_eq!(plain.scheme, "http");
    }

    #[test]
    fn a_connect_without_a_port_is_refused() {
        assert_eq!(r("CONNECT crates.io HTTP/1.1\r\n\r\n"), Route::Unknown);
        assert_eq!(r("CONNECT :443 HTTP/1.1\r\n\r\n"), Route::Unknown);
        assert_eq!(r("CONNECT HTTP/1.1\r\n\r\n"), Route::Unknown);
    }

    #[test]
    fn an_absolute_line_is_rewritten_to_origin_form() {
        let Route::Absolute { target, head } =
            r("GET http://crates.io/api?q=1 HTTP/1.1\r\nHost: crates.io\r\n\r\n")
        else {
            panic!("not absolute");
        };
        assert_eq!(target.host, "crates.io");
        assert_eq!(target.path, "/api?q=1");
        // The origin is told the path, never the whole URL.
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "GET /api?q=1 HTTP/1.1\r\nHost: crates.io\r\n\r\n"
        );
    }

    #[test]
    fn an_absolute_line_without_a_host_header_gets_one() {
        let Route::Absolute { head, .. } =
            r("GET https://crates.io/x HTTP/1.1\r\nAccept: */*\r\n\r\n")
        else {
            panic!("not absolute");
        };
        let head = String::from_utf8(head).unwrap();
        assert_eq!(
            head,
            "GET /x HTTP/1.1\r\nHost: crates.io\r\nAccept: */*\r\n\r\n"
        );
        // A non-default port is carried in the header, or the origin would look on 443.
        let Route::Absolute { head, .. } = r("GET https://127.0.0.1:8443/x HTTP/1.1\r\n\r\n")
        else {
            panic!("not absolute");
        };
        assert!(
            String::from_utf8(head)
                .unwrap()
                .contains("Host: 127.0.0.1:8443")
        );
    }

    #[test]
    fn an_origin_form_request_takes_its_host_from_the_header() {
        let Route::Origin { target, .. } =
            r("GET /index.html HTTP/1.0\r\nhost:   Example.COM \r\n\r\n")
        else {
            panic!("not origin form");
        };
        assert_eq!(target.scheme, "http");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 80);
        // The header may carry the port, and an IPv6 literal is bracketed.
        let Route::Origin { target, .. } = r("GET / HTTP/1.1\r\nHost: [::1]:3000\r\n\r\n") else {
            panic!("not origin form");
        };
        assert_eq!(target.host, "::1");
        assert_eq!(target.port, 3000);
    }

    #[test]
    fn a_request_that_names_no_host_is_refused() {
        assert_eq!(
            r("GET /index.html HTTP/1.1\r\nAccept: */*\r\n\r\n"),
            Route::Unknown
        );
        // HTTP/2's connection preface is not something this proxy parses.
        assert_eq!(r("PRI * HTTP/2.0\r\n\r\n"), Route::Unknown);
        assert_eq!(r("\r\n\r\n"), Route::Unknown);
    }

    #[test]
    fn a_partial_head_waits_and_an_endless_one_is_refused() {
        assert_eq!(r("CONNECT crate"), Route::Incomplete);
        assert_eq!(route(&vec![b'G'; MAX_HEAD + 1]), Route::TooLong);
    }

    #[test]
    fn a_head_with_a_non_text_line_is_refused() {
        let mut bytes = b"GET / HTTP/1.1\r\nHost: a\r\n\r\n".to_vec();
        bytes[4] = 0xff;
        assert_eq!(route(&bytes), Route::Unknown);
    }
    #[test]
    fn the_names_of_the_headers_are_kept_but_not_their_values() {
        let head = b"GET / HTTP/1.1\r\nHost: crates.io\r\nAuthorization: Bearer s3cret\r\n\r\n";
        let names = header_names(head);
        assert_eq!(names, ["host", "authorization"]);
        assert!(!names.iter().any(|n| n.contains("s3cret")));
    }
}
