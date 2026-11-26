use std::fmt::Write as _;
use std::str;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Kitty,
    Iterm2,
    Text,
}

pub fn detect(setting: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Protocol {
    match setting {
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

pub fn reason(protocol: Protocol, env: &dyn Fn(&str) -> Option<String>) -> String {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    match protocol {
        Protocol::Kitty => {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub full: Fit,
    pub from: u16,
    pub rows: u16,
}

impl Window {
    #[cfg(test)]
    pub fn all(full: Fit) -> Self {
        Self {
            full,
            from: 0,
            rows: full.rows,
        }
    }
}

pub fn band(first: usize, rows: u16, top: usize, height: usize) -> Option<(u16, u16)> {
    let start = first;
    let end = start + rows as usize;
    let first = start.max(top);
    let last = end.min(top + height);
    if last <= first {
        return None;
    }
    Some(((first - start) as u16, (last - first) as u16))
}

pub fn transmit(protocol: Protocol, data: &str, id: u32) -> Option<String> {
    match protocol {
        Protocol::Kitty => Some(kitty_transmit(data, id)),
        Protocol::Iterm2 | Protocol::Text => None,
    }
}

pub fn draw(
    protocol: Protocol,
    data: &str,
    pixels: (u32, u32),
    window: &Window,
    id: u32,
) -> Option<String> {
    match protocol {
        Protocol::Kitty => Some(kitty_place(pixels, window, id)),
        Protocol::Iterm2 => Some(iterm2(data, window.full.cols, window.rows)),
        Protocol::Text => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub cols: u16,
    pub rows: u16,
    pub scaled: bool,
}

const ASSUMED_CELL: (u32, u32) = (10, 20);

static CELL_PX: OnceLock<(u32, u32)> = OnceLock::new();

pub fn set_cell_px(size: (u16, u16)) {
    if size.0 > 0 && size.1 > 0 {
        let _ = CELL_PX.set((size.0 as u32, size.1 as u32));
    }
}

pub fn cell_px() -> (u32, u32) {
    CELL_PX.get().copied().unwrap_or(ASSUMED_CELL)
}

pub const MAX_ROWS: u16 = 24;

pub fn fit(pixels: (u32, u32), room: (u16, u16)) -> Fit {
    fit_in(pixels, room, cell_px())
}

pub fn fit_in(pixels: (u32, u32), room: (u16, u16), cell: (u32, u32)) -> Fit {
    let (w, h) = pixels;
    let (cell_w, cell_h) = (cell.0.max(1), cell.1.max(1));
    let (room_cols, room_rows) = (room.0.max(1) as f64, room.1.max(1) as f64);
    let at_one = (
        w.div_ceil(cell_w).max(1) as f64,
        h.div_ceil(cell_h).max(1) as f64,
    );
    let k = (room_cols / at_one.0).min(room_rows / at_one.1).min(1.0);
    Fit {
        cols: ((at_one.0 * k).floor() as u32).clamp(1, room_cols as u32) as u16,
        rows: ((at_one.1 * k).floor() as u32).clamp(1, room_rows as u32) as u16,
        scaled: k < 1.0,
    }
}

fn kitty_transmit(data: &str, id: u32) -> String {
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(4096)
        .map(|c| str::from_utf8(c).unwrap_or_default())
        .collect();
    let n = chunks.len();
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let params = if i == 0 {
            format!("a=t,f=100,q=2,i={id},")
        } else {
            String::new()
        };
        let more = if i + 1 < n { "m=1" } else { "m=0" };
        let _ = write!(out, "\x1b_G{params}{more};{chunk}\x1b\\");
    }
    out
}

const PLACEMENT: u32 = 1;

fn kitty_place(pixels: (u32, u32), window: &Window, id: u32) -> String {
    let geometry = source(pixels, window);
    let comma = if geometry.is_empty() { "" } else { "," };
    format!("\x1b_Ga=p,i={id},p={PLACEMENT},q=2,C=1{comma}{geometry}\x1b\\")
}

fn source(pixels: (u32, u32), window: &Window) -> String {
    let band = crop(pixels, window);
    if !window.full.scaled {
        return band;
    }
    format!("c={},r={}{band}", window.full.cols, window.rows)
}

fn crop(pixels: (u32, u32), window: &Window) -> String {
    if window.rows >= window.full.rows && window.from == 0 {
        return String::new();
    }
    let h_px = pixels.1 as f64;
    let full = window.full.rows.max(1) as f64;
    let y = ((window.from as f64 / full) * h_px).round() as u32;
    let h = (((window.rows as f64 / full) * h_px).round() as u32)
        .max(1)
        .min(pixels.1.saturating_sub(y).max(1));
    format!(",y={y},h={h}")
}

fn iterm2(data: &str, cols: u16, rows: u16) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=1:{data}\x07",
        file_size(data)
    )
}

fn file_size(data: &str) -> usize {
    let padding = data.bytes().rev().take_while(|b| *b == b'=').count();
    data.len() / 4 * 3 - padding.min(2)
}

pub fn fallback(name: &str, width: u32, height: u32) -> String {
    format!("[image {name} {width}×{height}]")
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;

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
        assert_eq!(
            band(11, 4, 0, 24),
            Some((0, 4)),
            "whole, at the top of the area"
        );
        assert_eq!(band(11, 4, 2, 24), Some((0, 4)));
        assert_eq!(band(11, 4, 13, 24), Some((2, 2)));
        assert_eq!(band(11, 4, 14, 24), Some((3, 1)), "the last row only");
        assert_eq!(
            band(11, 4, 0, 11),
            None,
            "the picture starts below the last visible line"
        );
        assert_eq!(band(11, 4, 15, 24), None);
        assert_eq!(band(11, 4, 0, 0), None, "no rows at all");
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
        assert_eq!(detect(None, &env(&[("TERM", "xterm")])), Protocol::Text);
    }

    #[test]
    fn a_picture_asked_for_in_cells_carries_its_box_in_the_placement() {
        let pixels = (133, 61);
        let room = (80, MAX_ROWS);
        let natural = fit_in(pixels, room, (10, 20));
        assert!(!natural.scaled, "an attachment of this size is one to one");
        let pinned = Fit {
            scaled: true,
            ..natural
        };
        let window = Window {
            full: pinned,
            from: 0,
            rows: pinned.rows,
        };
        let escape = kitty_place(pixels, &window, 1);
        assert!(escape.contains("c=14,r=4"), "{escape:?}");
        let plain = kitty_place(
            pixels,
            &Window {
                full: Fit {
                    scaled: false,
                    ..natural
                },
                from: 0,
                rows: natural.rows,
            },
            1,
        );
        assert!(!plain.contains("c="), "{plain:?}");
    }

    #[test]
    fn an_explicit_setting_beats_the_environment_both_ways() {
        let iterm = env(&[("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(detect(Some("kitty"), &iterm), Protocol::Kitty);
        assert_eq!(detect(Some("text"), &iterm), Protocol::Text);
        assert_eq!(detect(Some("false"), &iterm), Protocol::Text);
        assert_eq!(detect(Some("sixel"), &iterm), Protocol::Iterm2);
    }

    #[test]
    fn a_kitty_image_is_chunked_and_continues_while_there_is_more() {
        let data = STANDARD.encode(vec![7u8; 10 * 1024]);
        let out = transmit(Protocol::Kitty, &data, 1).unwrap();
        let chunks: Vec<&str> = out.split("\x1b_G").skip(1).collect();
        assert_eq!(chunks.len(), 4, "{}", out.len());
        assert!(
            chunks[0].starts_with("a=t,f=100,q=2,i=1,m=1;"),
            "{}",
            chunks[0]
        );
        for chunk in &chunks[1..3] {
            assert!(chunk.starts_with("m=1;"), "{}", chunk);
        }
        assert!(chunks[3].starts_with("m=0;"), "{}", chunks[3]);
        for chunk in &chunks {
            assert!(chunk.ends_with("\x1b\\"), "{}", chunk);
            let payload = chunk.split_once(';').map(|(_, p)| p).unwrap();
            assert!(payload.len() <= 4096 + "\x1b\\".len(), "{}", payload.len());
        }
        assert!(transmit(Protocol::Iterm2, &data, 1).is_none());
    }

    #[test]
    fn a_picture_is_placed_rather_than_transmitted_again() {
        let f = fit((640, 480), (20, 6));
        let out = draw(Protocol::Kitty, "BwcH", (640, 480), &Window::all(f), 7).unwrap();
        assert!(out.starts_with("\x1b_Ga=p,i=7,p=1,q=2,C=1,"), "{out}");
        assert!(out.contains(&format!("c={},r={}", f.cols, f.rows)), "{out}");
        assert!(out.ends_with("\x1b\\"), "{out}");
        assert!(!out.contains("m="), "a placement is one escape: {out}");
    }

    #[test]
    fn an_iterm_image_is_one_escape_with_its_cell_size() {
        let png = b"\x89PNG\r\n\x1a\nfake";
        let f = fit((300, 80), (40, 8));
        assert_eq!((f.cols, f.rows, f.scaled), (30, 4, false));
        let data = STANDARD.encode(png);
        let out = draw(Protocol::Iterm2, &data, (300, 80), &Window::all(f), 1).unwrap();
        assert!(
            out.starts_with("\x1b]1337;File=inline=1;size=12;width=30;height=4;"),
            "{out}"
        );
        assert!(out.contains(&data), "{out}");
        assert!(out.contains("preserveAspectRatio=1:"), "{out}");
        assert!(out.ends_with('\x07'), "an OSC ends with BEL");
        assert!(
            !out.contains("a="),
            "the kitty parameters are not in an iTerm2 escape"
        );
        assert_eq!(file_size(&STANDARD.encode(png)), png.len());
    }

    #[test]
    fn only_the_visible_band_of_a_picture_that_fits_is_sent() {
        let full = fit((320, 400), (80, 24));
        assert_eq!((full.cols, full.rows, full.scaled), (32, 20, false));
        let window = Window {
            full,
            from: 5,
            rows: 10,
        };
        let out = draw(Protocol::Kitty, "BwcH", (320, 400), &window, 1).unwrap();
        assert!(out.contains(",y=100,h=200"), "{out}");
        assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
        let top = Window {
            full,
            from: 0,
            rows: 4,
        };
        let out = draw(Protocol::Kitty, "BwcH", (320, 400), &top, 1).unwrap();
        assert!(out.contains(",y=0,h=80"), "{out}");
        assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
    }

    #[test]
    fn a_band_does_not_change_the_size_of_a_picture_that_fits() {
        let full = fit((1000, 400), (120, 24));
        assert!(!full.scaled);
        let escapes: Vec<String> = [(0, 20), (0, 7), (3, 17), (19, 1)]
            .into_iter()
            .map(|(from, rows)| {
                let window = Window { full, from, rows };
                draw(Protocol::Kitty, "BwcH", (1000, 400), &window, 1).unwrap()
            })
            .collect();
        for out in &escapes {
            assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
        }
        let big = fit((1000, 1200), (120, 24));
        assert!(big.scaled);
        for (from, rows) in [(0, 24), (0, 9), (15, 9), (23, 1)] {
            let window = Window {
                full: big,
                from,
                rows,
            };
            let out = draw(Protocol::Kitty, "BwcH", (1000, 1200), &window, 1).unwrap();
            assert!(out.contains(&format!("c={},r={}", big.cols, rows)), "{out}");
        }
    }

    #[test]
    fn a_tall_screenshot_is_scaled_then_cropped_to_the_band_on_screen() {
        let full = fit((1200, 2400), (80, MAX_ROWS));
        assert!(full.scaled);
        assert_eq!((full.cols, full.rows), (24, 24));
        let top = Window {
            full,
            from: 0,
            rows: 16,
        };
        let out = draw(Protocol::Kitty, "BwcH", (1200, 2400), &top, 1).unwrap();
        assert!(out.contains("c=24,r=16,y=0,h=1600"), "{out}");
        let middle = Window {
            full,
            from: 8,
            rows: 16,
        };
        let out = draw(Protocol::Kitty, "BwcH", (1200, 2400), &middle, 1).unwrap();
        assert!(out.contains("c=24,r=16,y=800,h=1600"), "{out}");
        let band = 1200.0 / 1600.0;
        let (cell_w, cell_h) = cell_px();
        let cell_box = (24.0 * cell_w as f64) / (16.0 * cell_h as f64);
        assert!((band - cell_box).abs() < 1e-9, "{band} vs {cell_box}");
    }

    #[test]
    fn a_wide_screenshot_uses_the_width_it_has() {
        let f = fit((2560, 1440), (80, MAX_ROWS));
        assert!(f.scaled);
        assert_eq!((f.cols, f.rows), (80, 22));
        let out = draw(Protocol::Kitty, "BwcH", (2560, 1440), &Window::all(f), 1).unwrap();
        assert!(out.contains("c=80,r=22"), "{out}");
        assert!(!out.contains(",y="), "{out}");
    }

    #[test]
    fn a_picture_shown_whole_sends_no_source_rectangle() {
        let full = fit((64, 64), (80, 24));
        let out = draw(Protocol::Kitty, "BwcH", (64, 64), &Window::all(full), 1).unwrap();
        assert!(!out.contains(",y=") && !out.contains(",h="), "{out}");
        assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
    }

    #[test]
    fn a_picture_that_fits_is_drawn_one_pixel_to_one_pixel() {
        let f = fit((64, 64), (80, 20));
        assert!(!f.scaled);
        assert_eq!((f.cols, f.rows), (7, 4));
        let out = draw(Protocol::Kitty, "BwcH", (1920, 1080), &Window::all(f), 1).unwrap();
        assert!(!out.contains(",c=") && !out.contains(",r="), "{out}");
    }

    #[test]
    fn the_terminal_s_own_cell_size_decides_what_fits() {
        let assumed = fit_in((900, 400), (80, MAX_ROWS), ASSUMED_CELL);
        assert!(assumed.scaled, "{assumed:?}");
        let measured = fit_in((900, 400), (80, MAX_ROWS), (12, 20));
        assert!(!measured.scaled, "{measured:?}");
        assert_eq!((measured.cols, measured.rows), (75, 20));
        assert_eq!(fit_in((400, 400), (80, MAX_ROWS), (10, 20)).rows, 20);
        assert_eq!(fit_in((400, 400), (80, MAX_ROWS), (10, 25)).rows, 16);
    }

    #[test]
    fn a_picture_too_big_for_its_room_is_scaled_not_dropped() {
        let f = fit((1920, 1080), (80, 20));
        assert!(f.scaled);
        assert!(f.cols <= 80 && f.rows <= 20, "{f:?}");
        let ratio = f.cols as f64 / (f.rows as f64 * 2.0);
        assert!((ratio - 16.0 / 9.0).abs() < 0.35, "{f:?} ratio {ratio}");
        let out = draw(Protocol::Kitty, "BwcH", (1920, 1080), &Window::all(f), 1).unwrap();
        assert!(out.contains(&format!("c={},r={}", f.cols, f.rows)), "{out}");
    }

    #[test]
    fn two_tall_pictures_both_fit_because_each_is_scaled() {
        for _ in 0..2 {
            let f = fit((320, 200), (80, 8));
            assert!(f.scaled);
            assert!(f.rows <= 8, "{f:?}");
        }
        assert!(!fit((64, 64), (80, 8)).scaled);
    }

    #[test]
    fn the_diagnostic_names_the_variable_that_decided() {
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
                "BwcH",
                (10, 3),
                &Window::all(fit((10, 3), (10, 3))),
                1
            ),
            None
        );
        assert_eq!(fallback("shot.png", 1280, 720), "[image shot.png 1280×720]");
    }
}
