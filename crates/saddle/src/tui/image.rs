//! Drawing an attached image in the terminal (D59).
//!
//! Three protocols matter and they are not interchangeable: the Kitty graphics protocol, iTerm2's
//! OSC 1337, and Sixel. The first two are implemented here; anything else falls back to a line of
//! text saying what the picture is, which is what a terminal that cannot draw one can honestly do.
//!
//! Detection is by environment, and getting it wrong is harmless in the same way a wrong `TERM` is:
//! the escape is ignored or printed, and the fallback line is there either way. Nothing here reads
//! the image's pixels — the terminal scales it — so saddle needs no image decoder.

use std::fmt::Write as _;
use std::str;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

/// How a terminal can draw a picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Kitty and the terminals that implement its graphics protocol.
    Kitty,
    /// iTerm2's inline images, also spoken by WezTerm and others.
    Iterm2,
    /// No protocol known: draw a line of text instead.
    Text,
}

/// What the environment says about the terminal, following the same order pi uses: an explicit
/// setting first, then what the terminal calls itself, then what it can be inferred from.
pub fn detect(setting: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Protocol {
    match setting {
        // An explicit choice is honoured, including turning drawing off.
        Some("kitty") => return Protocol::Kitty,
        Some("iterm2") => return Protocol::Iterm2,
        Some("none") | Some("text") | Some("false") => return Protocol::Text,
        _ => {}
    }
    let get = |k: &str| env(k).unwrap_or_default();
    let term = get("TERM");
    let program = get("TERM_PROGRAM");
    if !get("KITTY_WINDOW_ID").is_empty()
        || term.contains("kitty")
        || term.contains("ghostty")
        || program == "ghostty"
        || program == "WezTerm"
    {
        return Protocol::Kitty;
    }
    if program == "iTerm.app" || !get("ITERM_SESSION_ID").is_empty() {
        return Protocol::Iterm2;
    }
    Protocol::Text
}

/// Why a protocol was chosen, for `saddle check`: the variable that decided it, or none when the
/// environment said nothing and the fallback applies (D59).
pub fn reason(protocol: Protocol, env: &dyn Fn(&str) -> Option<String>) -> String {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    match protocol {
        Protocol::Kitty => {
            // The window id only has to exist; the other two have to say what they are.
            if let Some(v) = get("KITTY_WINDOW_ID") {
                return format!("KITTY_WINDOW_ID={v}");
            }
            for key in ["TERM", "TERM_PROGRAM"] {
                if let Some(v) = get(key)
                    && (v.contains("kitty") || v.contains("ghostty") || v == "WezTerm")
                {
                    return format!("{key}={v}");
                }
            }
            "kitty".to_owned()
        }
        Protocol::Iterm2 => match (get("TERM_PROGRAM"), get("ITERM_SESSION_ID")) {
            (Some(p), _) if p == "iTerm.app" => "TERM_PROGRAM=iTerm.app".to_owned(),
            (_, Some(v)) => format!("ITERM_SESSION_ID={}", v.chars().take(8).collect::<String>()),
            _ => "iTerm2".to_owned(),
        },
        Protocol::Text => "no kitty or iTerm2 marker in the environment".to_owned(),
    }
}

/// Which part of a picture to put on the screen (D59).
///
/// A picture behaves like text: when the transcript scrolls, the part that is off screen is simply
/// not drawn. Requiring the whole of it to fit is what made a second image vanish, since two large
/// screenshots never both fit a real transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The picture's full size in cells.
    pub full: Fit,
    /// The first row of the picture that is on screen, and how many of its rows are.
    pub from: u16,
    pub rows: u16,
}

impl Window {
    /// The whole picture, which is what a test draws when the point is the encoding, not the crop.
    #[cfg(test)]
    pub fn all(full: Fit) -> Self {
        Self {
            full,
            from: 0,
            rows: full.rows,
        }
    }
}

/// Which part of a picture is on screen, as `(from, rows)` in rows of the picture (D59).
///
/// The picture occupies the line after its marker and the `rows` lines following that, and the
/// screen shows `height` lines from `top`. Text above a picture scrolls away while its lower part is
/// still visible, so a picture has to behave the same way: the marker line being off the top of the
/// screen is not the same thing as the picture being off it.
pub fn band(marker: usize, rows: u16, top: usize, height: usize) -> Option<(u16, u16)> {
    let start = marker.saturating_add(1);
    let end = start + rows as usize;
    let first = start.max(top);
    let last = end.min(top + height);
    if last <= first {
        return None;
    }
    Some(((first - start) as u16, (last - first) as u16))
}

/// The escape that draws the part of `png` that `window` names, or `None` when this terminal cannot
/// draw pictures at all.
pub fn draw(
    protocol: Protocol,
    png: &[u8],
    pixels: (u32, u32),
    window: &Window,
    id: u32,
) -> Option<String> {
    match protocol {
        Protocol::Kitty => Some(kitty(png, pixels, window, id)),
        // iTerm2 has no source rectangle, so it is given the rows there are and it fits the whole
        // picture into them with `preserveAspectRatio`. A partly visible picture is therefore small
        // rather than cropped, which is honest even if it is not what kitty does.
        Protocol::Iterm2 => Some(iterm2(png, window.full.cols, window.rows)),
        Protocol::Text => None,
    }
}

/// The cell box an image needs, and whether the terminal has to scale it to fit (D59).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub cols: u16,
    pub rows: u16,
    /// True when the picture is bigger than the room it has, so the terminal must shrink it. False
    /// means draw at one pixel of the file to one pixel of the screen.
    pub scaled: bool,
}

/// Cell sizes assumed when working out how much room a picture needs. A terminal does not tell us
/// its real ones, so these are the usual values; they decide the layout, and the terminal decides
/// the actual pixels when it paints (D59).
const CELL_PX: (u32, u32) = (10, 20);

/// The tallest a picture is allowed to be, so one screenshot cannot fill the whole transcript.
pub const MAX_ROWS: u16 = 24;

/// How an image of `pixels` fits in `room` cells, with its aspect ratio kept.
///
/// At one to one it keeps its own pixel size. If that is bigger than the room, it is scaled down.
pub fn fit(pixels: (u32, u32), room: (u16, u16)) -> Fit {
    let (w, h) = pixels;
    let (room_cols, room_rows) = (room.0.max(1) as f64, room.1.max(1) as f64);
    let at_one = (
        w.div_ceil(CELL_PX.0).max(1) as f64,
        h.div_ceil(CELL_PX.1).max(1) as f64,
    );
    let k = (room_cols / at_one.0).min(room_rows / at_one.1).min(1.0);
    Fit {
        cols: ((at_one.0 * k).floor() as u32).clamp(1, room_cols as u32) as u16,
        rows: ((at_one.1 * k).floor() as u32).clamp(1, room_rows as u32) as u16,
        scaled: k < 1.0,
    }
}

/// The Kitty graphics protocol: a sequence of escapes, each carrying at most 4096 bytes of base64,
/// the first with the parameters and the rest continuing it.
///
/// No `c`/`r` when the whole picture is shown at its own size: those ask kitty to **fill a cell
/// box**, which stretches it, and without them a 64×64 PNG is 64×64 pixels of screen. They are sent
/// when it has to be scaled, and `y`/`h` name the band of the source that is on screen (D59).
fn kitty(png: &[u8], pixels: (u32, u32), window: &Window, id: u32) -> String {
    let b64 = STANDARD.encode(png);
    // Base64 is ASCII, so a 4096-byte split is always a character boundary.
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| str::from_utf8(c).unwrap_or_default())
        .collect();
    let n = chunks.len();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        // `a=T` transmits and displays, `f=100` is PNG, `q=2` silences the terminal's replies,
        // which would otherwise land in the middle of the transcript. `C=1` keeps the cursor where
        // it is instead of moving it past the picture.
        // Every parameter list ends with a comma, because `m=` follows it inside the same escape.
        let params = if i == 0 {
            let geometry = source(pixels, window);
            let comma = if geometry.is_empty() { "" } else { "," };
            format!("a=T,f=100,q=2,i={id},C=1{comma}{geometry},")
        } else {
            String::new()
        };
        let more = if i + 1 < n { "m=1" } else { "m=0" };
        let _ = write!(out, "\x1b_G{params}{more};{chunk}\x1b\\");
    }
    out
}

/// The geometry parameters for one picture: the cell box when it has to be scaled, and the band of
/// the source when only part of it is on screen (D59).
fn source(pixels: (u32, u32), window: &Window) -> String {
    let partial = window.rows < window.full.rows || window.from > 0;
    if !window.full.scaled && !partial {
        // Exactly the picture's own pixels, one to one.
        return String::new();
    }
    let mut params = format!("c={},r={}", window.full.cols, window.rows);
    if partial {
        // Which band of the file to show: the rows scrolled off the top, and the rows on screen,
        // converted back to pixels so the crop is exact.
        let h_px = pixels.1 as f64;
        let full = window.full.rows.max(1) as f64;
        let y = ((window.from as f64 / full) * h_px).round() as u32;
        let h = (((window.rows as f64 / full) * h_px).round() as u32)
            .max(1)
            .min(pixels.1.saturating_sub(y).max(1));
        let _ = write!(params, ",y={y},h={h}");
    }
    params
}

/// iTerm2's inline image: one OSC 1337, the size in cells, and the file inline.
fn iterm2(png: &[u8], cols: u16, rows: u16) -> String {
    let b64 = STANDARD.encode(png);
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=1:{b64}\x07",
        png.len()
    )
}

/// What a terminal with no protocol shows: the name and size, in the transcript, where the picture
/// would have been.
pub fn fallback(name: &str, width: u32, height: u32) -> String {
    format!("[image {name} {width}×{height}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn the_band_follows_the_picture_past_the_top_of_the_screen() {
        // The picture after a marker on line 10 occupies 11..15. What is on screen is the
        // intersection with the rows the transcript is showing.
        assert_eq!(
            band(10, 4, 0, 24),
            Some((0, 4)),
            "whole, at the top of the area"
        );
        // Scrolled by two: still whole, higher up.
        assert_eq!(band(10, 4, 2, 24), Some((0, 4)));
        // Scrolled past the marker, which is the case that used to draw nothing: two rows of the
        // picture are above the fold and two are on screen.
        assert_eq!(band(10, 4, 13, 24), Some((2, 2)));
        assert_eq!(band(10, 4, 14, 24), Some((3, 1)), "the last row only");
        // Below the bottom of the screen, or scrolled fully past it, there is nothing to draw.
        assert_eq!(
            band(10, 4, 0, 11),
            None,
            "the marker is the last visible line"
        );
        assert_eq!(band(10, 4, 15, 24), None);
        assert_eq!(band(10, 4, 0, 0), None, "no rows at all");
    }

    #[test]
    fn the_terminal_is_recognised_from_what_it_calls_itself() {
        assert_eq!(
            detect(None, &env(&[("KITTY_WINDOW_ID", "1")])),
            Protocol::Kitty
        );
        assert_eq!(
            detect(None, &env(&[("TERM", "xterm-kitty")])),
            Protocol::Kitty
        );
        assert_eq!(
            detect(None, &env(&[("TERM", "xterm-ghostty")])),
            Protocol::Kitty
        );
        assert_eq!(
            detect(None, &env(&[("TERM_PROGRAM", "WezTerm")])),
            Protocol::Kitty
        );
        assert_eq!(
            detect(None, &env(&[("TERM_PROGRAM", "iTerm.app")])),
            Protocol::Iterm2
        );
        // A terminal that says nothing gets the fallback rather than a guess that prints escapes.
        assert_eq!(detect(None, &env(&[("TERM", "xterm")])), Protocol::Text);
    }

    #[test]
    fn an_explicit_setting_beats_the_environment_both_ways() {
        let iterm = env(&[("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(detect(Some("kitty"), &iterm), Protocol::Kitty);
        assert_eq!(detect(Some("text"), &iterm), Protocol::Text);
        assert_eq!(detect(Some("false"), &iterm), Protocol::Text);
        // An unknown value falls back to detection rather than breaking drawing.
        assert_eq!(detect(Some("sixel"), &iterm), Protocol::Iterm2);
    }

    #[test]
    fn a_kitty_image_is_chunked_and_continues_while_there_is_more() {
        // 10 KiB of bytes is 14 KiB of base64, so four chunks of 4096.
        let png = vec![7u8; 10 * 1024];
        // 640×480 does not fit 20×6 cells, so it is scaled into them.
        let fit = fit((640, 480), (20, 6));
        assert!(fit.scaled);
        let out = draw(Protocol::Kitty, &png, (640, 480), &Window::all(fit), 1).unwrap();
        let chunks: Vec<&str> = out.split("\x1b_G").skip(1).collect();
        assert_eq!(chunks.len(), 4, "{}", out.len());
        // The first chunk carries the parameters and the id.
        // Scaled, so the cell box is sent with the aspect-preserving size (D59).
        assert!(
            chunks[0].starts_with(&format!(
                "a=T,f=100,q=2,i=1,C=1,c={},r={},m=1;",
                fit.cols, fit.rows
            )),
            "{}",
            chunks[0]
        );
        // Every chunk that has a successor says so, and the last one says it is the end.
        for chunk in &chunks[1..3] {
            assert!(chunk.starts_with("m=1;"), "{}", chunk);
        }
        assert!(chunks[3].starts_with("m=0;"), "{}", chunks[3]);
        // Each chunk is a complete escape.
        for chunk in &chunks {
            assert!(chunk.ends_with("\x1b\\"), "{}", chunk);
            let payload = chunk.split_once(';').map(|(_, p)| p).unwrap();
            assert!(payload.len() <= 4096 + "\x1b\\".len(), "{}", payload.len());
        }
    }

    #[test]
    fn an_iterm_image_is_one_escape_with_its_cell_size() {
        let png = b"\x89PNG\r\n\x1a\nfake";
        // 300×80 at 10×20 cells is 30×4, which fits the room it is given, so no scaling.
        let f = fit((300, 80), (40, 8));
        assert_eq!((f.cols, f.rows, f.scaled), (30, 4, false));
        let out = draw(Protocol::Iterm2, png, (300, 80), &Window::all(f), 1).unwrap();
        assert!(
            out.starts_with("\x1b]1337;File=inline=1;size=12;width=30;height=4;"),
            "{out}"
        );
        assert!(out.contains("preserveAspectRatio=1:"), "{out}");
        assert!(out.ends_with('\x07'), "an OSC ends with BEL");
        assert!(
            !out.contains("a=T"),
            "the kitty parameters are not in an iTerm2 escape"
        );
    }

    #[test]
    fn only_the_visible_band_of_a_tall_picture_is_sent() {
        // What a partly scrolled picture should do: the rows above the window are not drawn, and
        // the source rectangle names the band that is (D59).
        // 320×400 is exactly 32 cells by 20, so it fits the room unchanged.
        let full = fit((320, 400), (80, 24));
        assert_eq!((full.cols, full.rows, full.scaled), (32, 20, false));
        // Rows 5..15 of those 20, so the middle of the file.
        let window = Window {
            full,
            from: 5,
            rows: 10,
        };
        let out = draw(Protocol::Kitty, b"x", (320, 400), &window, 1).unwrap();
        assert!(out.contains("r=10"), "{out}");
        // Halfway down a 400-pixel file is 100 pixels, and ten of twenty rows is 200 pixels.
        assert!(out.contains("y=100"), "{out}");
        assert!(out.contains("h=200"), "{out}");
        // The top band starts at the top of the file.
        let top = Window {
            full,
            from: 0,
            rows: 4,
        };
        let out = draw(Protocol::Kitty, b"x", (320, 400), &top, 1).unwrap();
        assert!(out.contains("y=0"), "{out}");
        assert!(out.contains("h=80"), "{out}");
    }

    #[test]
    fn a_tall_screenshot_is_scaled_then_cropped_to_the_band_on_screen() {
        // A phone screenshot, 1200×2400, in an 80-column transcript. At its own size it would need
        // 120×120 cells, so it is scaled into the 24-row cap instead: 24×24 cells (D59). That is
        // the ordinary case, and the one no other test covers — scaled *and* only partly visible.
        let full = fit((1200, 2400), (80, MAX_ROWS));
        assert!(full.scaled);
        assert_eq!((full.cols, full.rows), (24, 24));
        // A transcript with only 16 rows below the marker line shows the top 16 of the 24.
        let top = Window {
            full,
            from: 0,
            rows: 16,
        };
        let out = draw(Protocol::Kitty, b"x", (1200, 2400), &top, 1).unwrap();
        assert!(out.contains("c=24,r=16,y=0,h=1600"), "{out}");
        // Scrolled eight rows, the band is the middle 16 rows: 800..2400 of the file's pixels.
        let middle = Window {
            full,
            from: 8,
            rows: 16,
        };
        let out = draw(Protocol::Kitty, b"x", (1200, 2400), &middle, 1).unwrap();
        assert!(out.contains("c=24,r=16,y=800,h=1600"), "{out}");
        // The band and the cell box it is put in must have the same shape. Kitty letterboxes when
        // they do not, which would leave a margin and look like the picture had slipped; this is
        // the assertion that makes the assumed cell size load-bearing (D59).
        let band = 1200.0 / 1600.0;
        let cell_box = (24.0 * CELL_PX.0 as f64) / (16.0 * CELL_PX.1 as f64);
        assert!((band - cell_box).abs() < 1e-9, "{band} vs {cell_box}");
    }

    #[test]
    fn a_wide_screenshot_uses_the_width_it_has() {
        // 2560×1440 in the same transcript: 256 cells wide at its own size, scaled to the width,
        // which leaves it 22 rows tall.
        let f = fit((2560, 1440), (80, MAX_ROWS));
        assert!(f.scaled);
        assert_eq!((f.cols, f.rows), (80, 22));
        let out = draw(Protocol::Kitty, b"x", (2560, 1440), &Window::all(f), 1).unwrap();
        assert!(out.contains("c=80,r=22"), "{out}");
        // Whole and unscrolled, so no source rectangle — cropping a picture that fits would be a
        // bug, not a crop.
        assert!(!out.contains(",y="), "{out}");
    }

    #[test]
    fn a_picture_shown_whole_sends_no_source_rectangle() {
        // The one-pixel-to-one-pixel case must stay free of y/h, or kitty would crop it.
        let full = fit((64, 64), (80, 24));
        let out = draw(Protocol::Kitty, b"x", (64, 64), &Window::all(full), 1).unwrap();
        assert!(!out.contains(",y=") && !out.contains(",h="), "{out}");
        assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
    }

    #[test]
    fn a_picture_that_fits_is_drawn_one_pixel_to_one_pixel() {
        // What was asked for: 64×64 is 64×64 pixels of screen, no cell box at all (D59).
        let f = fit((64, 64), (80, 20));
        assert!(!f.scaled);
        assert_eq!((f.cols, f.rows), (7, 4));
        let out = draw(Protocol::Kitty, b"x", (1920, 1080), &Window::all(f), 1).unwrap();
        assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
    }

    #[test]
    fn a_picture_too_big_for_its_room_is_scaled_not_dropped() {
        // Before this, a second large screenshot simply was not drawn, which read as "only one
        // image renders" (D59).
        let f = fit((1920, 1080), (80, 20));
        assert!(f.scaled);
        assert!(f.cols <= 80 && f.rows <= 20, "{f:?}");
        // The aspect ratio survives: 1920×1080 is 16:9, and so is the box.
        let ratio = f.cols as f64 / (f.rows as f64 * 2.0);
        assert!((ratio - 16.0 / 9.0).abs() < 0.35, "{f:?} ratio {ratio}");
        let out = draw(Protocol::Kitty, b"x", (1920, 1080), &Window::all(f), 1).unwrap();
        assert!(out.contains(&format!("c={},r={}", f.cols, f.rows)), "{out}");
    }

    #[test]
    fn two_tall_pictures_both_fit_because_each_is_scaled() {
        // The exact case that failed: two 320×200 images in a 16-row transcript.
        for _ in 0..2 {
            let f = fit((320, 200), (80, 8));
            assert!(f.scaled);
            assert!(f.rows <= 8, "{f:?}");
        }
        // A small icon next to them still keeps its exact pixels.
        assert!(!fit((64, 64), (80, 8)).scaled);
    }

    #[test]
    fn the_diagnostic_names_the_variable_that_decided() {
        // What `saddle check` reports, so a terminal that draws nothing can be told apart from one
        // that was never asked (D59).
        assert_eq!(
            reason(Protocol::Kitty, &env(&[("TERM", "xterm-kitty")])),
            "TERM=xterm-kitty"
        );
        assert_eq!(
            reason(Protocol::Iterm2, &env(&[("TERM_PROGRAM", "iTerm.app")])),
            "TERM_PROGRAM=iTerm.app"
        );
        assert!(
            reason(Protocol::Text, &env(&[("TERM", "xterm-256color")]))
                .contains("no kitty or iTerm2 marker")
        );
    }

    #[test]
    fn a_terminal_with_no_protocol_gets_nothing_to_escape_and_a_line_instead() {
        assert_eq!(
            draw(
                Protocol::Text,
                b"x",
                (10, 3),
                &Window::all(fit((10, 3), (10, 3))),
                1
            ),
            None
        );
        assert_eq!(fallback("shot.png", 1280, 720), "[image shot.png 1280×720]");
    }
}
