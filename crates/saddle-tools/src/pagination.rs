//! streaming D51 pagination: bounded storage, complete line counts and UTF-8 validation (D135).

use memchr::memchr_iter;
use std::str;

use crate::{Arguments, PAGE_BYTES, PAGE_LINES, SCAN_BYTES};

const RANGE_BYTES: u32 = 65536;

struct Page {
    first: u64,
    want: u64,
    total: u64,
    selected: u64,
    last: u64,
    stopped: bool,
    truncated: bool,
    line_bytes: u64,
    line_last: Option<u8>,
    prefix: Vec<u8>,
    body: Vec<u8>,
}
impl Page {
    fn segment(&mut self, bytes: &[u8]) {
        self.line_bytes += bytes.len() as u64;
        if let Some(last) = bytes.last() {
            self.line_last = Some(*last);
        }
        if !self.stopped && self.total + 1 >= self.first {
            let room = (PAGE_BYTES + 1).saturating_sub(self.prefix.len());
            self.prefix
                .extend_from_slice(&bytes[..bytes.len().min(room)]);
        }
    }
    fn end_line(&mut self, newline: bool) -> Result<(), String> {
        self.total += 1;
        let len = self.line_bytes - u64::from(newline && self.line_last == Some(b'\r'));
        if !self.stopped && self.total >= self.first {
            if self.selected >= self.want {
                self.stopped = true;
            } else if self.body.len() as u64 + len + 1 > PAGE_BYTES as u64 {
                self.stopped = true;
                if self.selected == 0 {
                    self.prefix.truncate(PAGE_BYTES.min(len as usize));
                    let valid = match str::from_utf8(&self.prefix) {
                        Ok(_) => self.prefix.len(),
                        Err(error) if error.error_len().is_none() => error.valid_up_to(),
                        Err(_) => return Err("file is not UTF-8".into()),
                    };
                    self.body.extend_from_slice(&self.prefix[..valid]);
                    self.body.extend_from_slice(
                        format!(
                            "\n[line {} is {len} bytes; the first {valid} are shown]",
                            self.first
                        )
                        .as_bytes(),
                    );
                    self.last = self.total;
                    self.selected = 1;
                    self.truncated = true;
                }
            } else {
                if self.selected != 0 {
                    self.body.push(b'\n');
                }
                self.body.extend_from_slice(&self.prefix[..len as usize]);
                self.selected += 1;
                self.last = self.total;
            }
        }
        self.line_bytes = 0;
        self.line_last = None;
        self.prefix.clear();
        Ok(())
    }
}

/// reads a D51 text page while retaining only the selected lines and one bounded line prefix.
pub fn read_with(
    arguments: &str,
    mut read: impl FnMut(&str, &str, u64, u32) -> Result<(Vec<u8>, bool), String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.offset == 0 || args.limit == 0 {
        return Err("offset and limit must be positive integers".into());
    }
    let mut page = Page {
        first: args.offset,
        want: args.limit.min(PAGE_LINES),
        total: 0,
        selected: 0,
        last: args.offset - 1,
        stopped: false,
        truncated: false,
        line_bytes: 0,
        line_last: None,
        prefix: Vec::new(),
        body: Vec::new(),
    };
    let mut offset = 0;
    // Preserve exact bytes only when the entire file fits, including its original line endings.
    let mut original = Vec::new();
    let mut pending = Vec::new();
    let mut ended_newline = false;
    loop {
        if offset >= SCAN_BYTES {
            return Err(
                "read would scan more than 64 MiB; use a narrower file or byte-range reader".into(),
            );
        }
        let want = RANGE_BYTES.min((SCAN_BYTES - offset) as u32);
        let (bytes, eof) = read(&args.root, &args.path, offset, want)?;
        if bytes.len() > want as usize || (bytes.is_empty() && !eof) {
            return Err("invalid reply from range service".into());
        }
        offset += bytes.len() as u64;
        if bytes.contains(&0) {
            return Err("file contains NUL bytes; this tool reads text only".into());
        }
        // Keep at most three trailing bytes between chunks; validation covers even unselected data.
        pending.extend_from_slice(&bytes);
        match str::from_utf8(&pending) {
            Ok(_) => pending.clear(),
            Err(error) if error.error_len().is_none() => {
                let tail = pending[error.valid_up_to()..].to_vec();
                pending.clear();
                pending.extend_from_slice(&tail);
            }
            Err(_) => return Err("file is not UTF-8".into()),
        }
        let room = PAGE_BYTES.saturating_sub(original.len());
        original.extend_from_slice(&bytes[..bytes.len().min(room)]);
        let mut start = 0;
        for newline in memchr_iter(b'\n', &bytes) {
            page.segment(&bytes[start..newline]);
            page.end_line(true)?;
            start = newline + 1;
        }
        page.segment(&bytes[start..]);
        if let Some(last) = bytes.last() {
            ended_newline = *last == b'\n';
        }
        if eof {
            break;
        }
    }
    if !pending.is_empty() {
        return Err("file is not UTF-8".into());
    }
    if page.line_bytes != 0 {
        page.end_line(false)?;
    }
    if page.total == 0 {
        return Ok("\n\nthe file is empty".into());
    }
    if page.first > page.total {
        return Ok(format!(
            "\n\noffset {} is past the end of the file, which has {} lines",
            page.first, page.total
        ));
    }
    let more = page.last < page.total;
    if !more && page.first == 1 && !page.truncated && offset <= PAGE_BYTES as u64 {
        return String::from_utf8(original).map_err(|_| "file is not UTF-8".into());
    }
    if !more && ended_newline {
        page.body.push(b'\n');
    }
    let mut body = String::from_utf8(page.body).map_err(|_| "file page is not UTF-8".to_owned())?;
    if more {
        body.push_str(&format!(
            "\n\n[showing lines {}-{} of {}; call read again with offset={} for more]",
            page.first,
            page.last,
            page.total,
            page.last + 1
        ));
    }
    Ok(body)
}
