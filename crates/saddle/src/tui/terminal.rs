//! Asking the terminal about its own colours (D73).
//!
//! pi's `system` theme derives every role from the terminal's palette, which means asking it: OSC 10
//! for the foreground, OSC 11 for the background, and OSC 4 for each ANSI colour. A terminal answers
//! on stdin, usually within a few milliseconds, so the query is written, stdin is read until the
//! replies stop arriving or 100 ms pass, and whatever came back is parsed — a terminal that says
//! nothing simply gets the fallback.
//!
//! The parsing is separated from the asking ([`parse`] takes the bytes) because a reply cannot be
//! produced in a test: it comes from a terminal, and the sandbox that runs these tests has none.
//!
//! The terminal is asked **before** the full-screen interface takes it over, and never after (D75).
//! A reply arrives on stdin, so reading one means owning stdin and the line discipline for the
//! length of the wait — which, while the interface is running, is the key reader and raw mode
//! itself. Asking from there turned raw mode off and left every keystroke echoed onto the screen
//! and the interface deaf to all of them.

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

/// How long to wait for replies, as pi does: long enough for a local terminal and a slow one over
/// ssh, short enough not to be noticed at startup.
pub const TIMEOUT: Duration = Duration::from_millis(100);

/// What to write to ask. Sent in one burst, then read until the deadline.
pub fn query_sequence() -> String {
    let mut out = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..16 {
        out.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    out
}

/// The answer this process has, so a theme rebuilt mid-session does not ask again (D73).
static REPORT: Mutex<Option<Reported>> = Mutex::new(None);

/// Asks the terminal once, before the interface starts, and keeps the answer (D73, D75).
///
/// Called from [`super::theme_selected`], which runs before the screen is taken over; later calls
/// return the kept answer and touch nothing. Raw mode is used for the length of the wait only —
/// without it the reply would be echoed over the screen and the read would wait for a newline — and
/// then put back to cooked, so the interface gets the terminal as it found it.
pub fn prime() -> Reported {
    let mut kept = match REPORT.lock() {
        Ok(kept) => kept,
        Err(poisoned) => poisoned.into_inner(),
    };
    *kept.get_or_insert_with(ask)
}

/// What the terminal reported earlier, if it has been asked (D73).
pub fn reported() -> Option<Reported> {
    let kept = match REPORT.lock() {
        Ok(kept) => kept,
        Err(poisoned) => poisoned.into_inner(),
    };
    *kept
}

/// Whether the terminal may be asked at all right now (D75).
///
/// Two refusals, both learned the hard way. A pipe has nobody to answer, so the read would sit
/// until the deadline for nothing. Raw mode being on means the full-screen interface owns the
/// terminal: it holds stdin for its key reader and it needs raw mode, so asking then means turning
/// raw mode off under it — which leaves every keystroke echoed and the interface deaf to all of
/// them. Checking is deliberate rather than trusting the caller, because trusting it broke a
/// session.
fn may_ask(stdin_is_tty: bool, stdout_is_tty: bool, raw_mode: bool) -> bool {
    stdin_is_tty && stdout_is_tty && !raw_mode
}

fn ask() -> Reported {
    parse(&read_replies(&query_sequence()).unwrap_or_default())
}

/// Throws away whatever is still waiting on stdin: a late reply to a query nobody is listening for
/// is indistinguishable from typing, and must not become a prompt (D75).
pub fn discard_pending() {
    discard_pending_input();
}

fn discard_pending_input() {
    // SAFETY: `tcflush` takes a descriptor and an integer; stdin is open for the life of the
    // process.
    unsafe {
        tcflush(0, TCIFLUSH);
    }
}

/// Writes `sequence` and reads whatever the terminal says back within [`TIMEOUT`], or `None` when
/// this process has nobody to ask (D73, D75).
///
/// Raw mode is used for the length of the wait only — without it the reply would be echoed over the
/// screen and the read would wait for a newline — and then put back to cooked, with anything still
/// in the queue thrown away so the interface does not read a late reply as a keystroke.
fn read_replies(sequence: &str) -> Option<Vec<u8>> {
    let raw_mode = terminal::is_raw_mode_enabled().unwrap_or(false);
    if !may_ask(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        raw_mode,
    ) {
        return None;
    }
    // `enable_raw_mode` says whether it *was* already on; it is not, from the check above, so this
    // always puts cooked mode back.
    let raw = terminal::enable_raw_mode().is_ok();
    let bytes = if write_query(sequence) {
        read_until_quiet()
    } else {
        Vec::new()
    };
    if raw {
        // Anything still in the queue would be read as keystrokes by the interface that is about to
        // start, and typed into the prompt (D75). Discarding it costs at most one keystroke pressed
        // in the first 100 ms of a session.
        discard_pending_input();
        let _ = terminal::disable_raw_mode();
    }
    Some(bytes)
}

/// Writes a query in one burst, or false when stdout says nothing (a pipe, a closed terminal).
fn write_query(sequence: &str) -> bool {
    let mut out = io::stdout();
    out.write_all(sequence.as_bytes()).is_ok() && out.flush().is_ok()
}

/// Reads what arrives until the terminal falls quiet or [`TIMEOUT`] passes (D73). The caller has put
/// the terminal in raw mode, so bytes arrive one at a time without a line.
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
                // Replies end with BEL or ST; once one whole sequence is in hand, keep reading
                // until a poll comes back empty.
                if byte[0] == 0x07 || (bytes.ends_with(b"\x1b\\")) {
                    continue;
                }
            }
            _ => break,
        }
    }
    bytes
}

/// What a terminal is assumed to use for a cell when it will not say. Ten pixels wide and twenty
/// tall is the usual shape at an ordinary font size; it is what the picture layout was built on
/// before the terminal was asked (D59).
pub const ASSUMED_CELL: (u16, u16) = (10, 20);

/// Asks the terminal how many pixels one cell is, once, before the interface starts (D59).
///
/// The answer decides how many rows the transcript reserves for a picture and whether it is drawn at
/// its own size or in a cell box, which is why it is worth a question: a cell is not in the window
/// size (its pixel fields are zero in practice), and a page of pictures laid out for a cell the
/// terminal does not use is a page of pictures that overlap the text. A terminal that says nothing —
/// or reports zeroes, as a pty with no window does — keeps [`ASSUMED_CELL`].
pub fn cell_px() -> (u16, u16) {
    measured().unwrap_or(ASSUMED_CELL)
}

/// One cell's pixels, and whether the terminal is the one that said so: `saddle check` prints both,
/// because a guess is otherwise indistinguishable from an answer (D59).
pub fn cell_report() -> ((u16, u16), bool) {
    match measured() {
        Some(cell) => (cell, true),
        None => (ASSUMED_CELL, false),
    }
}

/// What the terminal said, asking it once and never twice (D59).
fn measured() -> Option<(u16, u16)> {
    let mut kept = match MEASURED.lock() {
        Ok(kept) => kept,
        Err(poisoned) => poisoned.into_inner(),
    };
    *kept.get_or_insert_with(measure)
}

/// The answer this process has, so nothing asks twice (D59); the outer `None` is "not asked yet".
static MEASURED: Mutex<Option<Option<(u16, u16)>>> = Mutex::new(None);

/// What to write: `CSI 16 t` for the cell itself, `CSI 14 t` for the window it is a fraction of
/// (D59). Both are asked, because terminals differ about which one they know.
fn cell_sequence() -> String {
    "\x1b[16t\x1b[14t".to_owned()
}

fn measure() -> Option<(u16, u16)> {
    let pixels = parse_pixels(&read_replies(&cell_sequence())?);
    if let Some((width, height)) = pixels.cell {
        // The cell reply is the answer itself; the window's is divided by the cell count.
        let cell = (u16::try_from(width).ok()?, u16::try_from(height).ok()?);
        return (cell.0 > 0 && cell.1 > 0).then_some(cell);
    }
    cell_from_window(pixels.window?, terminal::size().ok()?)
}

/// What the two pixel replies said, each `(width, height)` in pixels (D59).
#[derive(Debug, Default, PartialEq, Eq)]
struct Pixels {
    /// `CSI 6 t`: one cell.
    cell: Option<(u32, u32)>,
    /// `CSI 4 t`: the window the cells are counted in.
    window: Option<(u32, u32)>,
}

/// The two replies, in pixels: `CSI 6 ; height ; width t` is a cell and `CSI 4 ; height ; width t`
/// is the window (D59).
///
/// Only a reply that is nothing but numbers and the closing `t` is read: a terminal answers a burst
/// of questions in one stream, so a device-attributes reply or a keypress may sit between the two
/// this is looking for, and reading the first `t` in the buffer would swallow the real answer.
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

/// A cell size from the window's pixels and the number of cells across and down it (D59).
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

/// Whether a byte is waiting within `limit`. `poll(2)` on stdin, so nothing is consumed by asking.
fn poll(stdin: &Stdin, limit: Duration) -> bool {
    let mut fd = MaybeUninit::<libc_pollfd>::zeroed();
    // SAFETY: `fd` is a plain struct of integers, and the descriptor is borrowed for the call.
    unsafe {
        let p = fd.as_mut_ptr();
        (*p).fd = stdin.as_raw_fd();
        (*p).events = 1; // POLLIN
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

/// `TCIFLUSH`: discard input that has been received but not read.
const TCIFLUSH: i32 = 0;

unsafe extern "C" {
    #[link_name = "poll"]
    fn poll_fd(fds: *mut libc_pollfd, nfds: u64, timeout: i32) -> i32;
    fn tcflush(fd: i32, queue: i32) -> i32;
}

/// Reads the replies out of `bytes` (D73).
///
/// Accepts the two forms a terminal answers with: `\x1b]11;rgb:rrrr/gggg/bbbb\x07` and the `#rrggbb`
/// spelling some use. Anything else in the buffer is ignored, since a terminal may be reporting a key
/// or a mouse event in the middle of the reply.
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
    // `10;rgb:…` for the foreground, `11;rgb:…` for the background, `4;N;rgb:…` for ANSI colour N.
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

/// One colour as a terminal states it: `rgb:rrrr/gggg/bbbb` or `#rrggbb`.
pub fn parse_colour(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let channel = |s: Option<&str>| -> Option<u8> {
            let s = s?;
            // The fields are 1 to 4 hex digits; a 4-digit field is more precision than 8 bits holds.
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
        // The regression that cost a session: asking while the interface was running turned raw
        // mode off, so every keystroke was echoed and none arrived (D75).
        assert!(
            !may_ask(true, true, true),
            "raw mode on: the interface owns the terminal"
        );
        // Only a real terminal on both ends, and only while it is still the shell's.
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
        // A one-digit-per-channel field is scaled, not truncated.
        assert_eq!(report.foreground, Some(Color::Rgb(255, 255, 255)));
    }

    #[test]
    fn noise_around_the_replies_is_ignored() {
        // A key press, a partial sequence, then a real reply: only the reply counts.
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
        // `CSI 16 t` is the cell itself; the reply is height then width, in pixels (D59).
        let pixels = parse_pixels(b"\x1b[6;34;17t");
        assert_eq!(pixels.cell, Some((17, 34)), "width, height");
        assert_eq!(pixels.window, None);
        // `CSI 14 t` is the window, which is divided by the cell count.
        let pixels = parse_pixels(b"\x1b[4;480;1920t");
        assert_eq!(pixels.cell, None);
        assert_eq!(pixels.window, Some((1920, 480)));
        // A terminal with no window reports zeroes, which is not an answer.
        assert_eq!(parse_pixels(b"\x1b[6;0;0t\x1b[4;0;0t"), Pixels::default());
        // Other replies in the same burst are skipped, not read as one: a device-attributes reply
        // and a cursor position report sit between the two this looks for.
        let pixels = parse_pixels(b"\x1b[?1;2c\x1b[6;34;17t\x1b[12;40R\x1b[4;480;1920t");
        assert_eq!(pixels.cell, Some((17, 34)));
        assert_eq!(pixels.window, Some((1920, 480)));
        assert_eq!(parse_pixels(b""), Pixels::default());
    }

    #[test]
    fn a_window_is_divided_into_the_cells_across_and_down_it() {
        assert_eq!(cell_from_window((1920, 480), (240, 30)), Some((8, 16)));
        // A cell narrower than a pixel, or a terminal that says it has no cells, is no answer.
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
        // Two questions because terminals differ about which they know: the cell itself (16 t) and
        // the window it is a fraction of (14 t) (D59).
        let sequence = cell_sequence();
        assert!(sequence.contains("\x1b[16t"), "{sequence}");
        assert!(sequence.contains("\x1b[14t"), "{sequence}");
    }
}
