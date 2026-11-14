//! Reading a hostname out of a TLS client hello before deciding the connection (D79).
//!
//! The name a guest is connecting to is in the clear in the server name extension (SNI, RFC 6066),
//! which arrives in the first hundred bytes of the connection. Reading it is a walk over two
//! length fields, not a negotiation, and no key is involved: the proxy checks the name, then
//! passes the bytes through untouched.
//!
//! Two cases are refused rather than guessed at. Encrypted Client Hello hides the name inside a
//! second, encrypted hello, so the outer name may be a decoy and a grant that matches it proves
//! nothing. A hello whose structure does not parse might be hiding the extension that would have
//! stopped us, so an unparseable hello is not "no name" but "not decidable".
//!
//! Nothing here can fail in a way that allows: an incomplete buffer says so, and every other
//! answer either names a host or names nothing.

/// What the first bytes of a connection say about the host it is for.
use std::str;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peek<'a> {
    /// More bytes are needed before this can be answered.
    Incomplete,
    /// A client hello that names this host.
    Name(&'a str),
    /// A client hello with no readable name: the extension is absent, or its list holds no host.
    Unnamed,
    /// A client hello that hides its name with Encrypted Client Hello (RFC 9849).
    Encrypted,
    /// Not a client hello at all.
    NotTls,
}

/// Reads the server name from the start of a TLS connection, without consuming anything.
///
/// A client hello that arrives split across records is reported as [`Peek::Incomplete`] until the
/// record carrying it is whole; a hello larger than one record is not reassembled, because
/// refusing it costs a retry over the proxy and guessing costs a policy decision.
pub fn peek(buf: &[u8]) -> Peek<'_> {
    let Some(&kind) = buf.first() else {
        return Peek::Incomplete;
    };
    // A handshake record is type 22 with a 3.x version.
    if kind != 22 {
        return Peek::NotTls;
    }
    let Some(&major) = buf.get(1) else {
        return Peek::Incomplete;
    };
    if major != 3 {
        return Peek::NotTls;
    }
    let Some([_, _, _, hi, lo]) = buf.get(0..5) else {
        return Peek::Incomplete;
    };
    let len = usize::from(u16::from_be_bytes([*hi, *lo]));
    let Some(record) = buf.get(5..5 + len) else {
        return Peek::Incomplete;
    };
    let Some(&msg) = record.first() else {
        return Peek::NotTls;
    };
    // The only handshake message a client sends first is client_hello.
    if msg != 1 {
        return Peek::NotTls;
    }
    parse(record)
}

/// `client_hello { type(1) length(3) body }` from inside the handshake record.
fn parse(record: &[u8]) -> Peek<'_> {
    let Some(len) = be24(record.get(1..4)) else {
        return Peek::Incomplete;
    };
    let Some(body) = record.get(4..4 + len) else {
        return Peek::Incomplete;
    };
    // version(2) random(32) session_id(1 + n) cipher_suites(2 + n) compression(1 + n)
    let mut at = 34;
    let Some(&sid) = body.get(at) else {
        return Peek::Incomplete;
    };
    at += 1 + usize::from(sid);
    let Some(cs) = be16(body.get(at..)) else {
        return Peek::Incomplete;
    };
    at += 2 + usize::from(cs);
    let Some(&cm) = body.get(at) else {
        return Peek::Incomplete;
    };
    at += 1 + usize::from(cm);
    // The extensions block is optional, and absent in a hello that offers only a session.
    if body.len() <= at {
        return Peek::Unnamed;
    }
    let Some(exts) = be16(body.get(at..)) else {
        return Peek::Incomplete;
    };
    let start = at + 2;
    let Some(block) = body.get(start..start + usize::from(exts)) else {
        return Peek::Incomplete;
    };
    let mut name = None;
    let mut at = 0;
    while at + 4 <= block.len() {
        let kind = u16::from_be_bytes([block[at], block[at + 1]]);
        let len = usize::from(u16::from_be_bytes([block[at + 2], block[at + 3]]));
        let Some(data) = block.get(at + 4..at + 4 + len) else {
            return Peek::Incomplete;
        };
        match kind {
            0 => name = server_name(data).or(name),
            // Encrypted Client Hello: the outer name is a cover, so matching it proves nothing.
            0xfe0d => return Peek::Encrypted,
            _ => {}
        }
        at += 4 + len;
    }
    match name {
        Some(name) => Peek::Name(name),
        None => Peek::Unnamed,
    }
}

/// `server_name_list` from inside extension 0, taking the first `host_name` entry.
fn server_name(data: &[u8]) -> Option<&str> {
    let len = usize::from(be16(Some(data))?);
    let list = data.get(2..2 + len)?;
    let mut at = 0;
    while at + 3 <= list.len() {
        let kind = list[at];
        let n = usize::from(u16::from_be_bytes([list[at + 1], list[at + 2]]));
        let name = list.get(at + 3..at + 3 + n)?;
        // A name that is not text is not a hostname, so it names nothing.
        if kind == 0 {
            return str::from_utf8(name).ok();
        }
        at += 3 + n;
    }
    None
}

fn be16(at: Option<&[u8]>) -> Option<u16> {
    let [hi, lo] = at?.get(0..2)?[..] else {
        return None;
    };
    Some(u16::from_be_bytes([hi, lo]))
}

fn be24(at: Option<&[u8]>) -> Option<usize> {
    let [a, b, c] = at?.get(0..3)?[..] else {
        return None;
    };
    Some(usize::from(a) << 16 | usize::from(b) << 8 | usize::from(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net;

    /// A syntactically valid client hello, so the parser is tested against the shape it will see
    /// rather than against bytes chosen to make it pass.
    fn client_hello(sni: Option<&str>, ech: bool) -> Vec<u8> {
        let mut exts = Vec::new();
        if let Some(name) = sni {
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
        body.extend_from_slice(&[7u8; 32]);
        body.push(0); // no session id
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

    #[test]
    fn the_name_comes_out_of_a_client_hello() {
        let hello = client_hello(Some("crates.io"), false);
        assert_eq!(peek(&hello), Peek::Name("crates.io"));
    }

    #[test]
    fn a_hello_without_the_extension_names_nothing() {
        assert_eq!(peek(&client_hello(None, false)), Peek::Unnamed);
        // An extension list that is empty rather than absent is the same answer.
        let mut hello = client_hello(None, false);
        let at = hello.len() - 2;
        hello[at..].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(peek(&hello), Peek::Unnamed);
    }

    #[test]
    fn a_hidden_name_is_refused_rather_than_taken_from_the_cover() {
        // The outer name is what a middlebox sees; it is not the name the client asked for, so a
        // grant matching it would be a grant for something else.
        assert_eq!(
            peek(&client_hello(Some("cover.example"), true)),
            Peek::Encrypted
        );
    }

    #[test]
    fn something_that_is_not_a_client_hello_names_nothing() {
        assert_eq!(peek(b"GET / HTTP/1.1\r\n"), Peek::NotTls);
        assert_eq!(peek(b"CONNECT crates.io:443 HTTP/1.1"), Peek::NotTls);
        // Right record type, wrong handshake message (server_hello is 2).
        let mut hello = client_hello(Some("crates.io"), false);
        hello[5] = 2;
        assert_eq!(peek(&hello), Peek::NotTls);
    }

    #[test]
    fn a_short_buffer_is_never_a_name() {
        let hello = client_hello(Some("crates.io"), false);
        for n in 0..hello.len() {
            let seen = peek(&hello[..n]);
            assert!(
                matches!(seen, Peek::Incomplete | Peek::NotTls | Peek::Unnamed),
                "a {n}-byte prefix was read as {seen:?}"
            );
        }
    }

    #[test]
    fn a_length_that_overruns_its_container_is_incomplete_not_a_name() {
        // Every length field enlarged in turn: the parser must notice it does not fit rather than
        // walk off the end.
        let hello = client_hello(Some("crates.io"), false);
        for at in [3, 6, 7, hello.len() - 1] {
            let mut bent = hello.clone();
            bent[at] = 0xff;
            assert!(
                !matches!(peek(&bent), Peek::Name(_)),
                "byte {at} was enlarged"
            );
        }
        // A record that claims more bytes than arrived is waiting for the rest of them.
        let mut whole = client_hello(Some("crates.io"), false);
        whole.truncate(whole.len() - 1);
        assert_eq!(peek(&whole), Peek::Incomplete);
    }

    #[test]
    fn a_name_that_is_not_text_names_nothing() {
        let mut hello = client_hello(Some("crates.io"), false);
        // Overwrite the last byte of the name with an invalid UTF-8 lead byte.
        let at = hello.len() - 2;
        hello[at] = 0xff;
        assert_eq!(peek(&hello), Peek::Unnamed);
    }

    #[test]
    fn a_host_is_compared_without_its_root_dot_or_its_case() {
        assert_eq!(net::normalise_host("Crates.IO."), "crates.io");
        assert_eq!(net::normalise_host("crates.io"), "crates.io");
    }
}
