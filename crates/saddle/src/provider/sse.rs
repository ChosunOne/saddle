use std::io::{self, BufRead};

use anyhow::Result;

use super::Sink;

pub trait Events {
    fn next(&mut self, sink: &mut dyn Sink) -> Result<Option<Event>>;
}

impl<R: BufRead> Events for Reader<R> {
    fn next(&mut self, _sink: &mut dyn Sink) -> Result<Option<Event>> {
        Ok(self.next_event()?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub event: String,
    pub data: String,
}

pub struct Reader<R> {
    inner: R,
    line: Vec<u8>,
}

impl<R: BufRead> Reader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            line: Vec::new(),
        }
    }

    pub fn next_event(&mut self) -> io::Result<Option<Event>> {
        let mut event = String::new();
        let mut data: Option<String> = None;
        loop {
            self.line.clear();
            if self.inner.read_until(b'\n', &mut self.line)? == 0 {
                return Ok(data.map(|data| Event { event, data }));
            }
            let line = String::from_utf8_lossy(&self.line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if let Some(data) = data {
                    return Ok(Some(Event { event, data }));
                }
                event.clear();
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "event" => event = value.to_owned(),
                "data" => match &mut data {
                    Some(d) => {
                        d.push('\n');
                        d.push_str(value);
                    }
                    None => data = Some(value.to_owned()),
                },
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_comments_crlf_and_multiline_data() {
        let text = ": keep-alive\r\nevent: a\r\ndata: one\r\n\r\nevent: lonely\n\ndata:two\ndata: three\n\ndata: tail";
        let mut r = Reader::new(text.as_bytes());
        let ev = |e: &str, d: &str| Event {
            event: e.into(),
            data: d.into(),
        };
        assert_eq!(r.next_event().unwrap(), Some(ev("a", "one")));
        assert_eq!(r.next_event().unwrap(), Some(ev("", "two\nthree")));
        assert_eq!(r.next_event().unwrap(), Some(ev("", "tail")));
        assert_eq!(r.next_event().unwrap(), None);
    }
}
