use std::net::{Ipv4Addr, Ipv6Addr};

const HEADER: usize = 12;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

pub fn answer(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < HEADER {
        return None;
    }
    let id = &query[0..2];
    if be16(&query[4..6])? != 1 {
        return None;
    }
    let end = question_end(query, HEADER)?;
    let qtype = be16(query.get(end..end + 2)?)?;
    let qclass = be16(query.get(end + 2..end + 4)?)?;
    let question = query.get(HEADER..end + 4)?;

    let rdata: &[u8] = match (qtype, qclass) {
        (TYPE_A, CLASS_IN) => &Ipv4Addr::LOCALHOST.octets(),
        (TYPE_AAAA, CLASS_IN) => &Ipv6Addr::LOCALHOST.octets(),
        _ => &[],
    };
    let records = u16::from(!rdata.is_empty());

    let mut out = Vec::with_capacity(HEADER + question.len() + 16);
    out.extend_from_slice(id);
    out.extend_from_slice(&0x8180u16.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&records.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]);
    out.extend_from_slice(question);
    if !rdata.is_empty() {
        out.extend_from_slice(&[0xc0, 0x0c]);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        out.extend_from_slice(rdata);
    }
    Some(out)
}

fn question_end(query: &[u8], mut at: usize) -> Option<usize> {
    let mut labels = 0;
    loop {
        let len = usize::from(*query.get(at)?);
        at += 1;
        if len == 0 {
            return Some(at);
        }
        if len > 63 {
            return None;
        }
        labels += 1;
        if labels > 128 {
            return None;
        }
        at = at.checked_add(len)?;
        if at > query.len() || at > 255 + HEADER {
            return None;
        }
    }
}

fn be16(at: &[u8]) -> Option<u16> {
    let [hi, lo] = at.get(0..2)?[..] else {
        return None;
    };
    Some(u16::from_be_bytes([hi, lo]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(id: [u8; 2], name: &str, qtype: u16) -> Vec<u8> {
        let mut q = Vec::new();
        q.extend_from_slice(&id);
        q.extend_from_slice(&0x0100u16.to_be_bytes());
        q.extend_from_slice(&1u16.to_be_bytes());
        q.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn an_a_query_is_answered_with_loopback() {
        let out = answer(&query([0xab, 0xcd], "crates.io", TYPE_A)).unwrap();
        assert_eq!(&out[0..2], &[0xab, 0xcd], "the id was not carried");
        assert_eq!(be16(&out[4..6]), Some(1), "one question");
        assert_eq!(be16(&out[6..8]), Some(1), "one answer");
        assert_eq!(&out[out.len() - 4..], &[127, 0, 0, 1]);
        let ttl_at = out.len() - 10;
        assert_eq!(&out[ttl_at..ttl_at + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn an_aaaa_query_is_answered_with_ipv6_loopback() {
        let out = answer(&query([0, 1], "crates.io", TYPE_AAAA)).unwrap();
        assert_eq!(&out[out.len() - 16..], &Ipv6Addr::LOCALHOST.octets());
    }

    #[test]
    fn every_name_gets_the_same_answer() {
        for name in ["crates.io", "example.com", "a.b.c.d.e.example", "localhost"] {
            let out = answer(&query([0, 0], name, TYPE_A)).unwrap();
            assert_eq!(&out[out.len() - 4..], &[127, 0, 0, 1], "{name}");
        }
    }

    #[test]
    fn a_name_is_echoed_exactly_as_it_was_asked() {
        let q = query([7, 7], "crates.io", TYPE_A);
        let out = answer(&q).unwrap();
        assert_eq!(&out[HEADER..q.len()], &q[HEADER..]);
    }

    #[test]
    fn a_query_for_something_else_gets_no_invented_answer() {
        for qtype in [16u16, 65, 6, 12] {
            let out = answer(&query([0, 0], "crates.io", qtype)).unwrap();
            assert_eq!(be16(&out[6..8]), Some(0), "type {qtype} invented an answer");
        }
    }

    #[test]
    fn garbage_is_not_answered() {
        assert_eq!(answer(&[]), None);
        assert_eq!(answer(&[0; 11]), None);
        let mut truncated = query([0, 0], "crates.io", TYPE_A);
        truncated.truncate(HEADER + 3);
        assert_eq!(answer(&truncated), None);
        let mut two = query([0, 0], "crates.io", TYPE_A);
        two[5] = 2;
        assert_eq!(answer(&two), None);
    }

    #[test]
    fn a_compression_pointer_in_the_name_does_not_loop() {
        let mut q = vec![0u8; HEADER];
        q[5] = 1;
        q.extend_from_slice(&[0xc0, 0x0c]);
        q.extend_from_slice(&TYPE_A.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        assert_eq!(answer(&q), None);
    }
}
