use ratatui::crossterm::terminal;
use std::io;
use std::io::IsTerminal as _;
use std::io::Read as _;
use std::io::Stdin;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ratatui::style::Color;

use super::palette::Reported;

pub const TIMEOUT: Duration = Duration::from_millis(100);

pub fn query_sequence() -> String {
    let mut out = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..16 {
        out.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    out
}

static REPORT: Mutex<Option<Reported>> = Mutex::new(None);

pub fn prime() -> Reported {
    let mut kept = match REPORT.lock() {
        Ok(kept) => kept,
        Err(poisoned) => poisoned.into_inner(),
    };
    *kept.get_or_insert_with(ask)
}

pub fn reported() -> Option<Reported> {
    let kept = match REPORT.lock() {
        Ok(kept) => kept,
        Err(poisoned) => poisoned.into_inner(),
    };
    *kept
}

fn may_ask(stdin_is_tty: bool, stdout_is_tty: bool, raw_mode: bool) -> bool {
    stdin_is_tty && stdout_is_tty && !raw_mode
}

fn ask() -> Reported {
    parse(&read_replies(&query_sequence()).unwrap_or_default())
}

pub fn discard_pending() {
    discard_pending_input();
}

fn discard_pending_input() {
    unsafe {
        tcflush(0, TCIFLUSH);
    }
}

fn read_replies(sequence: &str) -> Option<Vec<u8>> {
    let raw_mode = terminal::is_raw_mode_enabled().unwrap_or(false);
    if !may_ask(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        raw_mode,
    ) {
        return None;
    }
    let raw = terminal::enable_raw_mode().is_ok();
    let bytes = if write_query(sequence) {
        read_until_quiet()
    } else {
        Vec::new()
    };
    if raw {
        discard_pending_input();
        let _ = terminal::disable_raw_mode();
    }
    Some(bytes)
}

fn write_query(sequence: &str) -> bool {
    let mut out = io::stdout();
    out.write_all(sequence.as_bytes()).is_ok() && out.flush().is_ok()
}

fn read_until_quiet() -> Vec<u8> {
    let mut bytes = Vec::new();
    let deadline = Instant::now() + TIMEOUT;
    let mut stdin = io::stdin();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if !poll(&stdin, deadline - now) {
            break;
        }
        let mut byte = [0u8; 1];
        match stdin.read(&mut byte) {
            Ok(1) => {
                bytes.push(byte[0]);
                if byte[0] == 0x07 || (bytes.ends_with(b"\x1b\\")) {
                    continue;
                }
            }
            _ => break,
        }
    }
    bytes
}

pub const ASSUMED_CELL: (u16, u16) = (10, 20);

pub fn cell_px() -> (u16, u16) {
    measured().unwrap_or(ASSUMED_CELL)
}

pub fn cell_report() -> ((u16, u16), bool) {
    match measured() {
        Some(cell) => (cell, true),
        None => (ASSUMED_CELL, false),
    }
}

fn measured() -> Option<(u16, u16)> {
    let mut kept = match MEASURED.lock() {
        Ok(kept) => kept,
        Err(poisoned) => poisoned.into_inner(),
    };
    *kept.get_or_insert_with(measure)
}

static MEASURED: Mutex<Option<Option<(u16, u16)>>> = Mutex::new(None);

fn cell_sequence() -> String {
    "\x1b[16t\x1b[14t".to_owned()
}

fn measure() -> Option<(u16, u16)> {
    let pixels = parse_pixels(&read_replies(&cell_sequence())?);
    if let Some((width, height)) = pixels.cell {
        let cell = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
        return (cell.0 > 0 && cell.1 > 0).then_some(cell);
    }
    cell_from_window(pixels.window?, terminal::size().ok()?)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Pixels {
    cell: Option<(u32, u32)>,
    window: Option<(u32, u32)>,
}

fn parse_pixels(bytes: &[u8]) -> Pixels {
    let text = String::from_utf8_lossy(bytes);
    let mut pixels = Pixels::default();
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("\x1b[") {
        let after = &rest[at + 2..];
        let body: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ';')
            .collect();
        if body.is_empty() || !after[body.len()..].starts_with('t') {
            rest = after;
            continue;
        }
        let mut fields = body.split(';');
        let code = fields.next();
        let height = fields.next().and_then(|v| v.parse::<u32>().ok());
        let width = fields.next().and_then(|v| v.parse::<u32>().ok());
        if let (Some(height), Some(width)) = (height, width)
            && width > 0
            && height > 0
        {
            match code {
                Some("6") => pixels.cell = Some((width, height)),
                Some("4") => pixels.window = Some((width, height)),
                _ => {}
            }
        }
        rest = &after[body.len() + 1..];
    }
    pixels
}

fn cell_from_window(window: (u32, u32), cells: (u16, u16)) -> Option<(u16, u16)> {
    let (width, height) = window;
    let (cols, rows) = (cells.0 as u32, cells.1 as u32);
    if cols == 0 || rows == 0 {
        return None;
    }
    let cell = (
        u16::try_from(width / cols).ok()?,
        u16::try_from(height / rows).ok()?,
    );
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

fn poll(stdin: &Stdin, limit: Duration) -> bool {
    let mut fd = MaybeUninit::<libc_pollfd>::zeroed();
    unsafe {
        let p = fd.as_mut_ptr();
        (*p).fd = stdin.as_raw_fd();
        (*p).events = 1;
        (*p).revents = 0;
        let ms = limit.as_millis().min(i32::MAX as u128) as i32;
        poll_fd(p, 1, ms) > 0
    }
}

#[repr(C)]
struct libc_pollfd {
    fd: i32,
    events: i16,
    revents: i16,
}

const TCIFLUSH: i32 = 0;

unsafe extern "C" {
    #[link_name = "poll"]
    fn poll_fd(fds: *mut libc_pollfd, nfds: u64, timeout: i32) -> i32;
    fn tcflush(fd: i32, queue: i32) -> i32;
}

pub fn parse(bytes: &[u8]) -> Reported {
    let mut report = Reported::nothing();
    let text = String::from_utf8_lossy(bytes);
    let mut rest = text.as_ref();
    while let Some(at) = rest.find("\x1b]") {
        let after = &rest[at + 2..];
        let end = after
            .find('\x07')
            .or_else(|| after.find("\x1b\\"))
            .unwrap_or(after.len());
        let body = &after[..end];
        apply(&mut report, body);
        rest = &after[end..];
    }
    report
}

fn apply(report: &mut Reported, body: &str) {
    let mut parts = body.splitn(3, ';');
    let (Some(code), Some(second)) = (parts.next(), parts.next()) else {
        return;
    };
    match code {
        "4" => {
            let Some(value) = parts.next() else {
                return;
            };
            let Ok(index) = second.parse::<usize>() else {
                return;
            };
            if index < 16
                && let Some(colour) = parse_colour(value)
            {
                report.ansi[index] = Some(colour);
            }
        }
        "10" | "11" => {
            if let Some(colour) = parse_colour(second) {
                if code == "10" {
                    report.foreground = Some(colour);
                } else {
                    report.background = Some(colour);
                }
            }
        }
        _ => {}
    }
}

pub fn parse_colour(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let channel = |s: Option<&str>| -> Option<u8> {
            let s = s?;
            let s = &s[..s.len().min(4)];
            let v = u32::from_str_radix(s, 16).ok()?;
            let max = (16u32.pow(s.len() as u32)) - 1;
            Some(((v * 255) / max.max(1)) as u8)
        };
        let (r, g, b) = (
            channel(parts.next()),
            channel(parts.next()),
            channel(parts.next()),
        );
        return Some(Color::Rgb(r?, g?, b?));
    }
    let hex = value.strip_prefix('#')?;
    if hex.len() == 6 {
        let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some(Color::Rgb(c(0)?, c(2)?, c(4)?));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminal_is_never_asked_once_the_interface_owns_it() {
        assert!(
            !may_ask(true, true, true),
            "raw mode on: the interface owns the terminal"
        );
        assert!(may_ask(true, true, false));
        assert!(
            !may_ask(false, true, false),
            "stdin is a pipe: nobody answers"
        );
        assert!(
            !may_ask(true, false, false),
            "stdout is a pipe: nothing to write to"
        );
        assert!(!may_ask(false, false, false));
    }

    #[test]
    fn a_reply_in_the_usual_form_is_read() {
        let bytes = b"\x1b]10;rgb:ffff/ffff/ffff\x07\x1b]11;rgb:0000/0000/0000\x07";
        let report = parse(&bytes[..]);
        assert_eq!(report.foreground, Some(Color::Rgb(255, 255, 255)));
        assert_eq!(report.background, Some(Color::Rgb(0, 0, 0)));
        assert!(report.has_any());
    }

    #[test]
    fn the_ansi_colours_are_read_by_index() {
        let bytes = b"\x1b]4;1;rgb:cccc/0000/0000\x07\x1b]4;4;rgb:0000/0000/cccc\x07";
        let report = parse(&bytes[..]);
        assert_eq!(report.ansi[1], Some(Color::Rgb(0xcc, 0, 0)));
        assert_eq!(report.ansi[4], Some(Color::Rgb(0, 0, 0xcc)));
        assert_eq!(report.ansi[0], None, "not reported");
    }

    #[test]
    fn hex_and_short_fields_are_accepted_too() {
        let bytes = b"\x1b]11;#1a2b3c\x07\x1b]10;rgb:ff/ff/ff\x07";
        let report = parse(&bytes[..]);
        assert_eq!(report.background, Some(Color::Rgb(0x1a, 0x2b, 0x3c)));
        assert_eq!(report.foreground, Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn noise_around_the_replies_is_ignored() {
        let bytes = b"abc\x1b]11;rgb:1010/1010/1010\x07\x1b[A\x1b]4;9;?\x07junk";
        let report = parse(&bytes[..]);
        assert_eq!(report.background, Some(Color::Rgb(0x10, 0x10, 0x10)));
    }

    #[test]
    fn silence_is_silence() {
        let report = parse(b"");
        assert!(!report.has_any());
        assert_eq!(report, Reported::nothing());
    }

    #[test]
    fn the_cell_size_is_read_from_either_reply() {
        let pixels = parse_pixels(b"\x1b[6;34;17t");
        assert_eq!(pixels.cell, Some((17, 34)), "width, height");
        assert_eq!(pixels.window, None);
        let pixels = parse_pixels(b"\x1b[4;480;1920t");
        assert_eq!(pixels.cell, None);
        assert_eq!(pixels.window, Some((1920, 480)));
        assert_eq!(parse_pixels(b"\x1b[6;0;0t\x1b[4;0;0t"), Pixels::default());
        let pixels = parse_pixels(b"\x1b[?1;2c\x1b[6;34;17t\x1b[12;40R\x1b[4;480;1920t");
        assert_eq!(pixels.cell, Some((17, 34)));
        assert_eq!(pixels.window, Some((1920, 480)));
        assert_eq!(parse_pixels(b""), Pixels::default());
    }

    #[test]
    fn a_window_is_divided_into_the_cells_across_and_down_it() {
        assert_eq!(cell_from_window((1920, 480), (240, 30)), Some((8, 16)));
        assert_eq!(cell_from_window((7, 480), (240, 30)), None);
        assert_eq!(cell_from_window((1920, 480), (0, 30)), None);
        assert_eq!(cell_from_window((1920, 480), (240, 0)), None);
    }

    #[test]
    fn the_query_asks_for_the_foreground_background_and_sixteen_colours() {
        let query = query_sequence();
        assert!(query.contains("\x1b]10;?\x07"));
        assert!(query.contains("\x1b]11;?\x07"));
        for index in 0..16 {
            assert!(query.contains(&format!("\x1b]4;{index};?\x07")), "{index}");
        }
    }

    #[test]
    fn the_cell_size_is_asked_for_as_a_cell_and_as_a_window() {
        let sequence = cell_sequence();
        assert!(sequence.contains("\x1b[16t"), "{sequence}");
        assert!(sequence.contains("\x1b[14t"), "{sequence}");
    }
}
