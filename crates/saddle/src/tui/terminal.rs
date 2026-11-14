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
    let raw_mode = terminal::is_raw_mode_enabled().unwrap_or(false);
    if !may_ask(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        raw_mode,
    ) {
        return Reported::nothing();
    }
    // `enable_raw_mode` says whether it *was* already on; it is not, from the check above, so this
    // always puts cooked mode back.
    let raw = terminal::enable_raw_mode().is_ok();
    let report = query();
    if raw {
        // Anything still in the queue would be read as keystrokes by the interface that is about to
        // start, and typed into the prompt (D75). Discarding it costs at most one keystroke pressed
        // in the first 100 ms of a session.
        discard_pending_input();
        let _ = terminal::disable_raw_mode();
    }
    report
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

/// Asks this process's terminal, waiting at most [`TIMEOUT`] (D73).
///
/// Only called for the `system` theme, and only when stdin is a terminal: a pipe has nobody to answer
/// and the read would sit until the deadline for nothing.
pub fn query() -> Reported {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Reported::nothing();
    }
    let mut out = io::stdout();
    if out.write_all(query_sequence().as_bytes()).is_err() || out.flush().is_err() {
        return Reported::nothing();
    }
    // A reply arrives as an OSC sequence; read until they stop, or the deadline passes. The caller
    // has already put the terminal in raw mode, so a byte at a time arrives without a line.
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
    parse(&bytes)
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
    fn the_query_asks_for_the_foreground_background_and_sixteen_colours() {
        let query = query_sequence();
        assert!(query.contains("\x1b]10;?\x07"));
        assert!(query.contains("\x1b]11;?\x07"));
        for index in 0..16 {
            assert!(query.contains(&format!("\x1b]4;{index};?\x07")), "{index}");
        }
    }
}
