use std::str;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peek<'a> {
    Incomplete,
    Name(&'a str),
    Unnamed,
    Encrypted,
    NotTls,
}

pub fn peek(buf: &[u8]) -> Peek<'_> {
    let Some(&kind) = buf.first() else {
        return Peek::Incomplete;
    };
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
    if msg != 1 {
        return Peek::NotTls;
    }
    parse(record)
}

fn parse(record: &[u8]) -> Peek<'_> {
    let Some(len) = be24(record.get(1..4)) else {
        return Peek::Incomplete;
    };
    let Some(body) = record.get(4..4 + len) else {
        return Peek::Incomplete;
    };
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

fn server_name(data: &[u8]) -> Option<&str> {
    let len = usize::from(be16(Some(data))?);
    let list = data.get(2..2 + len)?;
    let mut at = 0;
    while at + 3 <= list.len() {
        let kind = list[at];
        let n = usize::from(u16::from_be_bytes([list[at + 1], list[at + 2]]));
        let name = list.get(at + 3..at + 3 + n)?;
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

    #[test]
    fn the_name_comes_out_of_a_client_hello() {
        let hello = client_hello(Some("crates.io"), false);
        assert_eq!(peek(&hello), Peek::Name("crates.io"));
    }

    #[test]
    fn a_hello_without_the_extension_names_nothing() {
        assert_eq!(peek(&client_hello(None, false)), Peek::Unnamed);
        let mut hello = client_hello(None, false);
        let at = hello.len() - 2;
        hello[at..].copy_from_slice(&0u16.to_be_bytes());
        assert_eq!(peek(&hello), Peek::Unnamed);
    }

    #[test]
    fn a_hidden_name_is_refused_rather_than_taken_from_the_cover() {
        assert_eq!(
            peek(&client_hello(Some("cover.example"), true)),
            Peek::Encrypted
        );
    }

    #[test]
    fn something_that_is_not_a_client_hello_names_nothing() {
        assert_eq!(peek(b"GET / HTTP/1.1\r\n"), Peek::NotTls);
        assert_eq!(peek(b"CONNECT crates.io:443 HTTP/1.1"), Peek::NotTls);
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
        let hello = client_hello(Some("crates.io"), false);
        for at in [3, 6, 7, hello.len() - 1] {
            let mut bent = hello.clone();
            bent[at] = 0xff;
            assert!(
                !matches!(peek(&bent), Peek::Name(_)),
                "byte {at} was enlarged"
            );
        }
        let mut whole = client_hello(Some("crates.io"), false);
        whole.truncate(whole.len() - 1);
        assert_eq!(peek(&whole), Peek::Incomplete);
    }

    #[test]
    fn a_name_that_is_not_text_names_nothing() {
        let mut hello = client_hello(Some("crates.io"), false);
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
