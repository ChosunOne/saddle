use crate::net;
use std::io::{self, Read, Write};
use std::thread;

use saddle_policy::Request;

use crate::net::client::Target;
use crate::net::head::{self, Route};
use crate::net::hello::{self, Peek};

const MAX_HEAD: usize = head::MAX_HEAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tls,
    Http,
    Proxy,
    NamedProxy,
}

pub struct Upstream {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
}

pub trait Peer {
    fn decide(&mut self, req: &Request<'_>) -> Result<(), String>;

    fn dial(&mut self, host: &str, port: u16) -> io::Result<Upstream>;
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub denials: Vec<String>,
}

pub fn serve(
    guest_read: &mut (impl Read + Send),
    guest_write: &mut (impl Write + Send),
    mode: Mode,
    peer: &mut dyn Peer,
) -> io::Result<Outcome> {
    let mut outcome = Outcome::default();
    let Some(plan) = negotiate(guest_read, guest_write, mode, peer, &mut outcome)? else {
        return Ok(outcome);
    };
    if let Some(reply) = plan.reply {
        guest_write.write_all(&reply)?;
        guest_write.flush()?;
    }
    let Upstream {
        mut reader,
        mut writer,
    } = plan.upstream;
    if !plan.first.is_empty() {
        writer.write_all(&plan.first)?;
        writer.flush()?;
    }
    thread::scope(|s| {
        s.spawn(|| copy(guest_read, &mut *writer));
        s.spawn(|| copy(&mut *reader, guest_write));
    });
    Ok(outcome)
}

struct Plan {
    upstream: Upstream,
    first: Vec<u8>,
    reply: Option<Vec<u8>>,
}

enum Step {
    More,
    Refuse(String),
    Go(Plan),
}

fn negotiate(
    guest_read: &mut (impl Read + Send),
    guest_write: &mut (impl Write + Send),
    mode: Mode,
    peer: &mut dyn Peer,
    outcome: &mut Outcome,
) -> io::Result<Option<Plan>> {
    let cap = if mode == Mode::Tls {
        usize::from(u16::MAX) + 5
    } else {
        MAX_HEAD
    };
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let step = if mode == Mode::Tls {
            decide_tls(&buf, peer)?
        } else {
            decide_http(&buf, mode, peer)?
        };
        match step {
            Step::More => {}
            Step::Refuse(why) => {
                refuse(mode, guest_write, &why)?;
                outcome.denials.push(why);
                return Ok(None);
            }
            Step::Go(plan) => return Ok(Some(plan)),
        }
        if buf.len() > cap {
            let why = "the first request was longer than a request head may be".to_owned();
            refuse(mode, guest_write, &why)?;
            outcome.denials.push(why);
            return Ok(None);
        }
        match guest_read.read(&mut chunk) {
            Ok(0) | Err(_) if buf.is_empty() => return Ok(None),
            Ok(0) | Err(_) => {
                let why = "the guest closed the connection before naming a host".to_owned();
                outcome.denials.push(why);
                return Ok(None);
            }
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn decide_tls(buf: &[u8], peer: &mut dyn Peer) -> io::Result<Step> {
    match hello::peek(buf) {
        Peek::Incomplete => Ok(Step::More),
        Peek::Name(name) => {
            let target = Target {
                scheme: "https".to_owned(),
                host: net::normalise_host(name),
                port: 443,
                path: "/".to_owned(),
            };
            permit("tunnel", &target, &[], buf.to_vec(), peer)
        }
        Peek::Encrypted => Ok(Step::Refuse(
            "the TLS handshake hid its server name (Encrypted Client Hello), so no grant can be \
             checked against it"
                .to_owned(),
        )),
        Peek::Unnamed => Ok(Step::Refuse(
            "the TLS handshake named no server, so no grant can be checked against it".to_owned(),
        )),
        Peek::NotTls => Ok(Step::Refuse(
            "this connection is not a TLS handshake, and a connection to :443 has to name its \
             host in one"
                .to_owned(),
        )),
    }
}

fn decide_http(buf: &[u8], mode: Mode, peer: &mut dyn Peer) -> io::Result<Step> {
    match head::route(buf) {
        Route::Incomplete => Ok(Step::More),
        Route::Unknown => Ok(Step::Refuse(
            "the request named no host, so no grant can be checked against it".to_owned(),
        )),
        Route::TooLong => Ok(Step::Refuse(
            "the request head never ended within the size a head may be".to_owned(),
        )),
        Route::Tunnel(mut target) => {
            if mode == Mode::NamedProxy && target.port == 22 {
                target.scheme = "ssh".into();
            }
            let mut step = permit("connect", &target, &[], Vec::new(), peer)?;
            if let Step::Go(plan) = &mut step {
                plan.reply = Some(b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec());
            }
            Ok(step)
        }
        Route::Absolute {
            target,
            head: rewritten,
        } => {
            let headers = head::header_names(&rewritten);
            permit(&method_of(buf), &target, &headers, rewritten, peer)
        }
        Route::Origin { target, head } => {
            let headers = head::header_names(&head);
            permit(&method_of(buf), &target, &headers, head, peer)
        }
    }
}

fn permit(
    method: &str,
    target: &Target,
    headers: &[String],
    first: Vec<u8>,
    peer: &mut dyn Peer,
) -> io::Result<Step> {
    let req = Request::Net {
        method,
        scheme: &target.scheme,
        host: &target.host,
        port: target.port,
        path: &target.path,
        headers,
    };
    if let Err(why) = peer.decide(&req) {
        return Ok(Step::Refuse(why));
    }
    match peer.dial(&target.host, target.port) {
        Ok(upstream) => Ok(Step::Go(Plan {
            upstream,
            first,
            reply: None,
        })),
        Err(e) => Ok(Step::Refuse(format!(
            "{}:{}: {e}",
            target.host, target.port
        ))),
    }
}

fn refuse(mode: Mode, guest_write: &mut (impl Write + Send), why: &str) -> io::Result<()> {
    if mode == Mode::Tls {
        return Ok(());
    }
    let body = format!("saddle: {why}\n");
    let reply = format!(
        "HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    guest_write.write_all(reply.as_bytes())?;
    guest_write.flush()
}

fn method_of(buf: &[u8]) -> String {
    let end = buf
        .iter()
        .position(|b| *b == b' ' || *b == b'\r' || *b == b'\n')
        .unwrap_or(buf.len());
    let method = String::from_utf8_lossy(&buf[..end]).into_owned();
    if method.is_empty() {
        "GET".to_owned()
    } else {
        method
    }
}

fn copy(from: &mut dyn Read, to: &mut dyn Write) {
    let _ = io::copy(from, to);
    let _ = to.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Wire {
        inbound: Arc<Mutex<(Vec<u8>, usize)>>,
        outbound: Arc<Mutex<Vec<u8>>>,
    }

    impl Wire {
        fn with(bytes: &[u8]) -> Self {
            let wire = Self::default();
            wire.inbound.lock().unwrap().0 = bytes.to_vec();
            wire
        }

        fn written(&self) -> String {
            String::from_utf8(self.outbound.lock().unwrap().clone()).unwrap()
        }

        fn read_half(&self) -> Reader {
            Reader(self.clone())
        }

        fn write_half(&self) -> Writer {
            Writer(self.clone())
        }
    }

    struct Reader(Wire);
    struct Writer(Wire);

    impl Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut state = self.0.inbound.lock().unwrap();
            let left = state.0.len() - state.1;
            let n = left.min(buf.len());
            buf[..n].copy_from_slice(&state.0[state.1..state.1 + n]);
            state.1 += n;
            Ok(n)
        }
    }

    impl Write for Writer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.outbound.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Fake {
        allowed: Result<(), String>,
        decided: Vec<String>,
        dialled: Vec<String>,
        reply: &'static [u8],
        opened: Vec<Wire>,
    }

    impl Fake {
        fn allowing() -> Self {
            Self {
                allowed: Ok(()),
                decided: Vec::new(),
                dialled: Vec::new(),
                reply: b"",
                opened: Vec::new(),
            }
        }

        fn refusing(why: &str) -> Self {
            Self {
                allowed: Err(why.to_owned()),
                ..Self::allowing()
            }
        }
    }

    impl Peer for Fake {
        fn decide(&mut self, req: &Request<'_>) -> Result<(), String> {
            if let Request::Net {
                scheme, host, port, ..
            } = req
            {
                self.decided.push(format!("{scheme}://{host}:{port}"));
            }
            self.allowed.clone()
        }

        fn dial(&mut self, host: &str, port: u16) -> io::Result<Upstream> {
            self.dialled.push(format!("{host}:{port}"));
            let wire = Wire::with(self.reply);
            self.opened.push(wire.clone());
            Ok(Upstream {
                reader: Box::new(wire.read_half()),
                writer: Box::new(wire.write_half()),
            })
        }
    }

    fn client_hello(name: Option<&str>, ech: bool) -> Vec<u8> {
        let mut exts = Vec::new();
        if let Some(name) = name {
            let mut list = vec![0u8];
            list.extend_from_slice(&(name.len() as u16).to_be_bytes());
            list.extend_from_slice(name.as_bytes());
            exts.extend_from_slice(&0u16.to_be_bytes());
            exts.extend_from_slice(&((list.len() + 2) as u16).to_be_bytes());
            exts.extend_from_slice(&(list.len() as u16).to_be_bytes());
            exts.extend_from_slice(&list);
        }
        if ech {
            exts.extend_from_slice(&0xfe0du16.to_be_bytes());
            exts.extend_from_slice(&0u16.to_be_bytes());
        }
        let mut body = vec![3, 3];
        body.extend_from_slice(&[1u8; 32]);
        body.push(0);
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&[0x13, 0x01]);
        body.push(1);
        body.push(0);
        body.extend_from_slice(&(exts.len() as u16).to_be_bytes());
        body.extend_from_slice(&exts);
        let mut handshake = vec![1u8];
        handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        handshake.extend_from_slice(&body);
        let mut record = vec![22, 3, 1];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    fn run(mode: Mode, guest: &[u8], peer: &mut Fake) -> (Outcome, String) {
        let wire = Wire::with(guest);
        let mut read = wire.read_half();
        let mut write = wire.write_half();
        let outcome = serve(&mut read, &mut write, mode, peer).unwrap();
        (outcome, wire.written())
    }

    #[test]
    fn a_tls_connection_is_decided_on_its_server_name() {
        let mut peer = Fake::allowing();
        let (outcome, _) = run(
            Mode::Tls,
            &client_hello(Some("crates.io"), false),
            &mut peer,
        );
        assert_eq!(peer.decided, ["https://crates.io:443"]);
        assert_eq!(peer.dialled, ["crates.io:443"]);
        assert!(outcome.denials.is_empty());
    }

    #[test]
    fn a_name_is_compared_without_case_or_a_root_dot() {
        let mut peer = Fake::allowing();
        run(
            Mode::Tls,
            &client_hello(Some("Crates.IO."), false),
            &mut peer,
        );
        assert_eq!(peer.decided, ["https://crates.io:443"]);
    }

    #[test]
    fn a_refused_tls_connection_is_never_dialled_and_says_nothing_in_tls() {
        let mut peer = Fake::refusing("no net grant covering https://crates.io:443");
        let (outcome, written) = run(
            Mode::Tls,
            &client_hello(Some("crates.io"), false),
            &mut peer,
        );
        assert!(peer.dialled.is_empty(), "a refused host was dialled");
        assert!(
            written.is_empty(),
            "bytes were written into a TLS handshake"
        );
        assert_eq!(
            outcome.denials,
            ["no net grant covering https://crates.io:443"]
        );
    }

    #[test]
    fn a_hidden_name_is_refused_without_a_decision() {
        let mut peer = Fake::allowing();
        let (outcome, _) = run(
            Mode::Tls,
            &client_hello(Some("cover.example"), true),
            &mut peer,
        );
        assert!(peer.decided.is_empty(), "ECH reached the policy");
        assert!(peer.dialled.is_empty());
        assert!(outcome.denials[0].contains("Encrypted Client Hello"));
    }

    #[test]
    fn a_hello_with_no_name_is_refused_without_a_decision() {
        let mut peer = Fake::allowing();
        let (outcome, _) = run(Mode::Tls, &client_hello(None, false), &mut peer);
        assert!(peer.decided.is_empty());
        assert!(outcome.denials[0].contains("named no server"));
    }

    #[test]
    fn a_hello_split_across_reads_is_still_read() {
        let hello = client_hello(Some("crates.io"), false);
        let mut peer = Fake::allowing();
        let wire = Wire::with(&hello);
        let mut read = Split(Reader(wire.clone()), 1);
        let mut write = wire.write_half();
        serve(&mut read, &mut write, Mode::Tls, &mut peer).unwrap();
        assert_eq!(peer.decided, ["https://crates.io:443"]);
    }

    struct Split(Reader, usize);

    impl Read for Split {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let cap = self.1.min(buf.len());
            self.0.read(&mut buf[..cap])
        }
    }

    #[test]
    fn the_handshake_goes_upstream_unchanged() {
        let mut peer = Fake::allowing();
        let hello = client_hello(Some("crates.io"), false);
        run(Mode::Tls, &hello, &mut peer);
        assert_eq!(peer.opened.len(), 1);
        let sent = peer.opened[0].outbound.lock().unwrap().clone();
        assert_eq!(sent, hello, "the connection was altered in flight");
    }

    #[test]
    fn a_connect_tunnel_is_answered_before_it_is_carried() {
        let mut peer = Fake::allowing();
        let (outcome, written) = run(
            Mode::Proxy,
            b"CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io:443\r\n\r\n",
            &mut peer,
        );
        assert_eq!(peer.decided, ["https://crates.io:443"]);
        assert!(written.starts_with("HTTP/1.1 200 Connection Established\r\n\r\n"));
        assert!(outcome.denials.is_empty());
    }

    #[test]
    fn only_named_proxy_connect_to_port_22_is_classified_as_ssh() {
        for (mode, request, expected) in [
            (
                Mode::NamedProxy,
                "CONNECT github.com:22 HTTP/1.1",
                "ssh://github.com:22",
            ),
            (
                Mode::Proxy,
                "CONNECT github.com:22 HTTP/1.1",
                "https://github.com:22",
            ),
            (
                Mode::NamedProxy,
                "CONNECT github.com:443 HTTP/1.1",
                "https://github.com:443",
            ),
            (
                Mode::NamedProxy,
                "GET https://github.com:22/ HTTP/1.1",
                "https://github.com:22",
            ),
        ] {
            let mut peer = Fake::refusing("test denial");
            let (outcome, reply) = run(mode, format!("{request}\r\n\r\n").as_bytes(), &mut peer);
            assert_eq!(peer.decided, [expected]);
            assert!(peer.opened.is_empty());
            assert_eq!(outcome.denials, ["test denial"]);
            assert!(reply.starts_with("HTTP/1.1 403"));
        }
    }

    #[test]
    fn a_refused_connect_is_told_why() {
        let mut peer = Fake::refusing("no net grant covering https://crates.io:443");
        let (outcome, written) = run(
            Mode::Proxy,
            b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n",
            &mut peer,
        );
        assert!(written.starts_with("HTTP/1.1 403 Forbidden"));
        assert!(written.contains("no net grant covering https://crates.io:443"));
        assert_eq!(outcome.denials.len(), 1);
    }

    #[test]
    fn an_absolute_request_is_forwarded_in_origin_form() {
        let mut peer = Fake::allowing();
        run(
            Mode::Proxy,
            b"GET http://crates.io/x HTTP/1.1\r\nHost: crates.io\r\n\r\n",
            &mut peer,
        );
        assert_eq!(peer.decided, ["http://crates.io:80"]);
        assert_eq!(
            peer.opened[0].written(),
            "GET /x HTTP/1.1\r\nHost: crates.io\r\n\r\n"
        );
    }

    #[test]
    fn a_plaintext_request_on_80_is_decided_on_its_host_header() {
        let mut peer = Fake::allowing();
        run(
            Mode::Http,
            b"GET /x HTTP/1.1\r\nHost: example.com\r\n\r\n",
            &mut peer,
        );
        assert_eq!(peer.decided, ["http://example.com:80"]);
        assert_eq!(peer.dialled, ["example.com:80"]);
    }

    #[test]
    fn a_request_with_no_host_is_refused_and_told_so() {
        let mut peer = Fake::allowing();
        let (outcome, written) = run(Mode::Http, b"GET /x HTTP/1.1\r\n\r\n", &mut peer);
        assert!(peer.decided.is_empty());
        assert!(written.starts_with("HTTP/1.1 403"));
        assert!(outcome.denials[0].contains("named no host"));
    }

    #[test]
    fn a_guest_that_says_nothing_is_not_dialled() {
        let mut peer = Fake::allowing();
        let (outcome, _) = run(Mode::Tls, b"", &mut peer);
        assert!(peer.dialled.is_empty());
        assert!(
            outcome.denials.is_empty(),
            "a silent guest is not a refusal"
        );
    }

    #[test]
    fn a_head_that_never_ends_is_refused() {
        let mut peer = Fake::allowing();
        let endless = vec![b'A'; MAX_HEAD + 8192];
        let (outcome, _) = run(Mode::Http, &endless, &mut peer);
        assert!(peer.dialled.is_empty());
        assert!(outcome.denials.iter().any(|d| d.contains("never ended")));
    }
}
