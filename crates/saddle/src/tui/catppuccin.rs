use ratatui::style::Color;

use super::palette::{Palette, blend, luminance, rgb};
use super::theme::Appearance;

pub const NAMES: [&str; 4] = [
    "catppuccin-latte",
    "catppuccin-frappe",
    "catppuccin-macchiato",
    "catppuccin-mocha",
];

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct Flavour {
    pub name: &'static str,
    pub appearance: Appearance,
    pub rosewater: Color,
    pub flamingo: Color,
    pub pink: Color,
    pub mauve: Color,
    pub red: Color,
    pub maroon: Color,
    pub peach: Color,
    pub yellow: Color,
    pub green: Color,
    pub teal: Color,
    pub sky: Color,
    pub sapphire: Color,
    pub blue: Color,
    pub lavender: Color,
    pub text: Color,
    pub subtext1: Color,
    pub subtext0: Color,
    pub overlay2: Color,
    pub overlay1: Color,
    pub overlay0: Color,
    pub surface2: Color,
    pub surface1: Color,
    pub surface0: Color,
    pub base: Color,
    pub mantle: Color,
    pub crust: Color,
}

const fn hex(value: &str) -> Color {
    const fn digit(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("a colour is six hex digits"),
        }
    }
    const fn byte(hi: u8, lo: u8) -> u8 {
        digit(hi) * 16 + digit(lo)
    }
    let bytes = value.as_bytes();
    let at = if bytes.len() == 7 && bytes[0] == b'#' {
        1
    } else {
        0
    };
    Color::Rgb(
        byte(bytes[at], bytes[at + 1]),
        byte(bytes[at + 2], bytes[at + 3]),
        byte(bytes[at + 4], bytes[at + 5]),
    )
}

pub const LATTE: Flavour = Flavour {
    name: "catppuccin-latte",
    appearance: Appearance::Light,
    rosewater: hex("dc8a78"),
    flamingo: hex("dd7878"),
    pink: hex("ea76cb"),
    mauve: hex("8839ef"),
    red: hex("d20f39"),
    maroon: hex("e64553"),
    peach: hex("fe640b"),
    yellow: hex("df8e1d"),
    green: hex("40a02b"),
    teal: hex("179299"),
    sky: hex("04a5e5"),
    sapphire: hex("209fb5"),
    blue: hex("1e66f5"),
    lavender: hex("7287fd"),
    text: hex("4c4f69"),
    subtext1: hex("5c5f77"),
    subtext0: hex("6c6f85"),
    overlay2: hex("7c7f93"),
    overlay1: hex("8c8fa1"),
    overlay0: hex("9ca0b0"),
    surface2: hex("acb0be"),
    surface1: hex("bcc0cc"),
    surface0: hex("ccd0da"),
    base: hex("eff1f5"),
    mantle: hex("e6e9ef"),
    crust: hex("dce0e8"),
};

pub const FRAPPE: Flavour = Flavour {
    name: "catppuccin-frappe",
    appearance: Appearance::Dark,
    rosewater: hex("f2d5cf"),
    flamingo: hex("eebebe"),
    pink: hex("f4b8e4"),
    mauve: hex("ca9ee6"),
    red: hex("e78284"),
    maroon: hex("ea999c"),
    peach: hex("ef9f76"),
    yellow: hex("e5c890"),
    green: hex("a6d189"),
    teal: hex("81c8be"),
    sky: hex("99d1db"),
    sapphire: hex("85c1dc"),
    blue: hex("8caaee"),
    lavender: hex("babbf1"),
    text: hex("c6d0f5"),
    subtext1: hex("b5bfe2"),
    subtext0: hex("a5adce"),
    overlay2: hex("949cbb"),
    overlay1: hex("838ba7"),
    overlay0: hex("737994"),
    surface2: hex("626880"),
    surface1: hex("51576d"),
    surface0: hex("414559"),
    base: hex("303446"),
    mantle: hex("292c3c"),
    crust: hex("232634"),
};

pub const MACCHIATO: Flavour = Flavour {
    name: "catppuccin-macchiato",
    appearance: Appearance::Dark,
    rosewater: hex("f4dbd6"),
    flamingo: hex("f0c6c6"),
    pink: hex("f5bde6"),
    mauve: hex("c6a0f6"),
    red: hex("ed8796"),
    maroon: hex("ee99a0"),
    peach: hex("f5a97f"),
    yellow: hex("eed49f"),
    green: hex("a6da95"),
    teal: hex("8bd5ca"),
    sky: hex("91d7e3"),
    sapphire: hex("7dc4e4"),
    blue: hex("8aadf4"),
    lavender: hex("b7bdf8"),
    text: hex("cad3f5"),
    subtext1: hex("b8c0e0"),
    subtext0: hex("a5adcb"),
    overlay2: hex("939ab7"),
    overlay1: hex("8087a2"),
    overlay0: hex("6e738d"),
    surface2: hex("5b6078"),
    surface1: hex("494d64"),
    surface0: hex("363a4f"),
    base: hex("24273a"),
    mantle: hex("1e2030"),
    crust: hex("181926"),
};

pub const MOCHA: Flavour = Flavour {
    name: "catppuccin-mocha",
    appearance: Appearance::Dark,
    rosewater: hex("f5e0dc"),
    flamingo: hex("f2cdcd"),
    pink: hex("f5c2e7"),
    mauve: hex("cba6f7"),
    red: hex("f38ba8"),
    maroon: hex("eba0ac"),
    peach: hex("fab387"),
    yellow: hex("f9e2af"),
    green: hex("a6e3a1"),
    teal: hex("94e2d5"),
    sky: hex("89dceb"),
    sapphire: hex("74c7ec"),
    blue: hex("89b4fa"),
    lavender: hex("b4befe"),
    text: hex("cdd6f4"),
    subtext1: hex("bac2de"),
    subtext0: hex("a6adc8"),
    overlay2: hex("9399b2"),
    overlay1: hex("7f849c"),
    overlay0: hex("6c7086"),
    surface2: hex("585b70"),
    surface1: hex("45475a"),
    surface0: hex("313244"),
    base: hex("1e1e2e"),
    mantle: hex("181825"),
    crust: hex("11111b"),
};

pub const FLAVOURS: [Flavour; 4] = [LATTE, FRAPPE, MACCHIATO, MOCHA];

pub fn flavour(name: &str) -> Option<Flavour> {
    FLAVOURS.iter().find(|f| f.name == name).copied()
}

impl Flavour {
    pub fn palette(&self) -> Palette {
        let muted = self.ink(self.subtext0, 4.5);
        let subtle = self.ink(self.overlay2, 3.0);
        let faint = self.ink(self.overlay1, 2.5);
        let (quiet, selection) = match self.appearance {
            Appearance::Dark => (self.surface0, self.surface1),
            Appearance::Light => (
                blend(self.base, self.mauve, 0.06),
                blend(self.base, self.mauve, 0.20),
            ),
        };
        let tint = match self.appearance {
            Appearance::Dark => 0.18,
            Appearance::Light => 0.12,
        };
        let pass = blend(self.base, self.green, tint);
        let fail = blend(self.base, self.red, tint);
        let (match_bg, match_text) = match self.appearance {
            Appearance::Dark => (self.yellow, self.base),
            Appearance::Light => (blend(self.base, self.yellow, 0.35), self.text),
        };
        Palette {
            accent: self.ink(self.mauve, 4.5),
            border: self.mauve,
            border_accent: self.mauve,
            border_muted: self.surface2,
            success: self.ink(self.green, 4.5),
            error: self.ink(self.red, 4.5),
            warning: self.ink(self.yellow, 4.5),
            muted,
            dim: subtle,
            text: self.text,
            thinking_text: subtle,
            selected_bg: selection,
            user_message_bg: quiet,
            user_message_text: self.text,
            custom_message_bg: quiet,
            custom_message_text: self.text,
            custom_message_label: muted,
            tool_pending_bg: quiet,
            tool_success_bg: pass,
            tool_error_bg: fail,
            tool_title: self.ink(self.mauve, 4.5),
            tool_output: self.ink(self.subtext1, 4.5),
            md_heading: self.ink(self.mauve, 4.5),
            md_link: self.ink(self.blue, 4.5),
            md_link_url: subtle,
            md_code: self.ink(self.peach, 4.5),
            md_code_block: self.ink(self.peach, 4.5),
            md_code_block_border: self.surface2,
            md_quote: muted,
            md_quote_border: self.surface2,
            md_hr: self.surface2,
            md_list_bullet: self.mauve,
            tool_diff_added: self.ink(self.green, 4.5),
            tool_diff_removed: self.ink(self.red, 4.5),
            tool_diff_context: subtle,
            syntax_comment: subtle,
            syntax_keyword: self.ink(self.mauve, 4.5),
            syntax_function: self.ink(self.blue, 4.5),
            syntax_variable: self.text,
            syntax_string: self.ink(self.green, 4.5),
            syntax_number: self.ink(self.peach, 4.5),
            syntax_type: self.ink(self.yellow, 4.5),
            syntax_operator: self.ink(self.sky, 4.5),
            syntax_punctuation: faint,
            thinking_off: faint,
            thinking_minimal: self.teal,
            thinking_low: self.blue,
            thinking_medium: self.mauve,
            thinking_high: self.peach,
            thinking_xhigh: self.maroon,
            thinking_max: self.red,
            bash_mode: self.ink(self.peach, 4.5),
            scrollbar_track: self.surface1,
            scrollbar_thumb: self.overlay0,
            search_match_bg: match_bg,
            search_match_text: match_text,
        }
    }

    fn ink(&self, colour: Color, floor: f64) -> Color {
        if contrast(colour, self.base) >= floor {
            return colour;
        }
        let (mut low, mut high) = (0.0, 1.0);
        let mut best = self.text;
        for _ in 0..24 {
            let t = (low + high) / 2.0;
            let moved = blend(colour, self.text, t);
            if contrast(moved, self.base) >= floor {
                best = moved;
                high = t;
            } else {
                low = t;
            }
        }
        best
    }
}

fn contrast(a: Color, b: Color) -> f64 {
    let lum = |c: Color| {
        let (r, g, b) = rgb(c).expect("a colour");
        luminance((r, g, b))
    };
    let (x, y) = (lum(a), lum(b));
    let (hi, lo) = if x > y { (x, y) } else { (y, x) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_the_published_one() {
        assert_eq!(LATTE.base, Color::Rgb(0xef, 0xf1, 0xf5));
        assert_eq!(LATTE.mauve, Color::Rgb(0x88, 0x39, 0xef));
        assert_eq!(LATTE.text, Color::Rgb(0x4c, 0x4f, 0x69));
        assert_eq!(FRAPPE.base, Color::Rgb(0x30, 0x34, 0x46));
        assert_eq!(FRAPPE.peach, Color::Rgb(0xef, 0x9f, 0x76));
        assert_eq!(MACCHIATO.base, Color::Rgb(0x24, 0x27, 0x3a));
        assert_eq!(MACCHIATO.surface0, Color::Rgb(0x36, 0x3a, 0x4f));
        assert_eq!(MOCHA.base, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(MOCHA.crust, Color::Rgb(0x11, 0x11, 0x1b));
        assert_eq!(MOCHA.green, Color::Rgb(0xa6, 0xe3, 0xa1));
        for f in [FRAPPE, MACCHIATO, MOCHA] {
            assert!(luminance_of(f.text) > luminance_of(f.base), "{}", f.name);
            assert!(
                luminance_of(f.surface0) > luminance_of(f.base),
                "{}",
                f.name
            );
        }
        assert!(luminance_of(LATTE.text) < luminance_of(LATTE.base));
        assert!(luminance_of(LATTE.surface0) < luminance_of(LATTE.base));
    }

    fn luminance_of(c: Color) -> f64 {
        let (r, g, b) = rgb(c).expect("a colour");
        luminance((r, g, b))
    }

    #[test]
    fn each_flavour_is_26_distinct_colours_and_a_ramp_that_runs_one_way() {
        for f in FLAVOURS {
            let all = [
                ("rosewater", f.rosewater),
                ("flamingo", f.flamingo),
                ("pink", f.pink),
                ("mauve", f.mauve),
                ("red", f.red),
                ("maroon", f.maroon),
                ("peach", f.peach),
                ("yellow", f.yellow),
                ("green", f.green),
                ("teal", f.teal),
                ("sky", f.sky),
                ("sapphire", f.sapphire),
                ("blue", f.blue),
                ("lavender", f.lavender),
                ("text", f.text),
                ("subtext1", f.subtext1),
                ("subtext0", f.subtext0),
                ("overlay2", f.overlay2),
                ("overlay1", f.overlay1),
                ("overlay0", f.overlay0),
                ("surface2", f.surface2),
                ("surface1", f.surface1),
                ("surface0", f.surface0),
                ("base", f.base),
                ("mantle", f.mantle),
                ("crust", f.crust),
            ];
            assert_eq!(all.len(), 26, "26 colours per flavour");
            let mut seen: Vec<(String, Color)> = Vec::new();
            for (name, colour) in all {
                if let Some((other, _)) = seen.iter().find(|(_, c)| *c == colour) {
                    panic!("{}: {name} and {other} are both {colour:?}", f.name);
                }
                seen.push((name.to_owned(), colour));
            }
            let ramp = [
                f.text, f.subtext1, f.subtext0, f.overlay2, f.overlay1, f.overlay0, f.surface2,
                f.surface1, f.surface0, f.base,
            ];
            let light = f.appearance == Appearance::Light;
            for pair in ramp.windows(2) {
                let (a, b) = (luminance_of(pair[0]), luminance_of(pair[1]));
                assert!(
                    if light { a < b } else { a > b },
                    "{}: {} then {} does not run from text to base",
                    f.name,
                    pair[0],
                    pair[1]
                );
            }
            assert!(
                luminance_of(f.base) > luminance_of(f.mantle)
                    && luminance_of(f.mantle) > luminance_of(f.crust),
                "{}: base, mantle and crust are not a ramp",
                f.name
            );
        }
    }

    #[test]
    fn a_hex_is_read_the_way_the_table_writes_it() {
        assert_eq!(hex("11111b"), Color::Rgb(17, 17, 27));
        assert_eq!(hex("#11111b"), Color::Rgb(17, 17, 27));
        assert_eq!(hex("FFFFFF"), Color::Rgb(255, 255, 255));
        assert_eq!(hex("000000"), Color::Rgb(0, 0, 0));
    }

    #[test]
    fn every_flavour_is_found_by_its_name_and_by_no_other() {
        for name in NAMES {
            let f = flavour(name).unwrap_or_else(|| panic!("{name} is missing"));
            assert_eq!(f.name, name);
            assert_eq!(
                f.appearance,
                if name == "catppuccin-latte" {
                    Appearance::Light
                } else {
                    Appearance::Dark
                }
            );
        }
        assert!(flavour("catppuccin").is_none(), "the flavours are named");
        assert!(
            flavour("mocha").is_none(),
            "and prefixed, so a name says what it is"
        );
        assert_eq!(FLAVOURS.len(), NAMES.len());
    }

    #[test]
    fn text_reads_on_the_background_it_is_drawn_on() {
        for f in FLAVOURS {
            let p = f.palette();
            for (role, colour, floor) in [
                ("text", p.text, 4.5),
                ("muted", p.muted, 3.0),
                ("dim", p.dim, 2.0),
                ("accent", p.accent, 4.5),
                ("success", p.success, 4.5),
                ("error", p.error, 4.5),
                ("warning", p.warning, 4.5),
                ("toolOutput", p.tool_output, 4.5),
                ("mdLink", p.md_link, 4.5),
                ("mdCode", p.md_code, 4.5),
                ("mdHeading", p.md_heading, 4.5),
                ("mdQuote", p.md_quote, 3.0),
                ("mdLinkUrl", p.md_link_url, 3.0),
                ("toolDiffAdded", p.tool_diff_added, 4.5),
                ("toolDiffRemoved", p.tool_diff_removed, 4.5),
                ("syntaxComment", p.syntax_comment, 3.0),
                ("syntaxKeyword", p.syntax_keyword, 4.5),
                ("syntaxFunction", p.syntax_function, 4.5),
                ("syntaxString", p.syntax_string, 4.5),
                ("syntaxNumber", p.syntax_number, 4.5),
                ("syntaxType", p.syntax_type, 4.5),
                ("syntaxOperator", p.syntax_operator, 3.0),
                ("syntaxPunctuation", p.syntax_punctuation, 2.0),
            ] {
                let ratio = contrast(colour, f.base);
                assert!(
                    ratio >= floor,
                    "{}: {role} is {ratio:.2}:1 on base, under {floor}: {colour:?}",
                    f.name
                );
            }
        }
    }

    #[test]
    fn the_text_drawn_in_a_panel_reads_on_that_panel() {
        let cases = |p: &Palette, f: &Flavour| {
            vec![
                (
                    "selectedBg",
                    p.selected_bg,
                    vec![("text", p.text, 4.5), ("muted", p.muted, 3.0)],
                ),
                (
                    "userMessageBg",
                    p.user_message_bg,
                    vec![("text", p.text, 4.5)],
                ),
                (
                    "customMessageBg",
                    p.custom_message_bg,
                    vec![("label", p.custom_message_label, 3.0)],
                ),
                (
                    "toolPendingBg",
                    p.tool_pending_bg,
                    vec![("toolOutput", p.tool_output, 4.5)],
                ),
                (
                    "toolSuccessBg",
                    p.tool_success_bg,
                    vec![("toolOutput", p.tool_output, 4.5)],
                ),
                (
                    "toolErrorBg",
                    p.tool_error_bg,
                    vec![("toolOutput", p.tool_output, 4.5)],
                ),
                (
                    "searchMatchBg",
                    p.search_match_bg,
                    vec![("searchMatchText", p.search_match_text, 4.5)],
                ),
                (
                    "base",
                    f.base,
                    vec![("text", p.text, 4.5), ("muted", p.muted, 3.0)],
                ),
            ]
        };
        for f in FLAVOURS {
            let p = f.palette();
            for (panel, bg, roles) in cases(&p, &f) {
                for (role, colour, floor) in roles {
                    let ratio = contrast(colour, bg);
                    assert!(
                        ratio >= floor,
                        "{}: {} is {:.2}:1 on {}, under {}: {:?} on {:?}",
                        f.name,
                        role,
                        ratio,
                        panel,
                        floor,
                        colour,
                        bg
                    );
                }
            }
        }
    }

    #[test]
    fn a_panel_is_told_apart_from_the_background_it_sits_on() {
        for f in FLAVOURS {
            let p = f.palette();
            for (panel, bg) in [
                ("selectedBg", p.selected_bg),
                ("userMessageBg", p.user_message_bg),
                ("toolSuccessBg", p.tool_success_bg),
                ("toolErrorBg", p.tool_error_bg),
            ] {
                let ratio = contrast(bg, f.base);
                assert!(ratio > 1.05, "{}: {panel} is invisible: {ratio:.3}", f.name);
            }
            assert!(
                contrast(p.tool_success_bg, p.tool_error_bg) > 1.05,
                "{}: a pass and a fail look alike",
                f.name
            );
        }
    }
}
