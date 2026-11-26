use std::collections::BTreeMap;
use std::env;

use anyhow::{Context, Result, bail};
use ratatui::style::Color;

use super::catppuccin;
use super::palette::{self, Palette, SyntaxColours};
use super::terminal;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    pub appearance: Appearance,
    pub accent: Color,
    pub border: Color,
    pub border_accent: Color,
    pub border_muted: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub muted: Color,
    pub dim: Color,
    pub text: Color,
    pub thinking_text: Color,
    pub selected_bg: Color,
    pub selection_fg: Color,
    pub user_message_text: Color,
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    pub tool_title: Color,
    pub tool_output: Color,
    pub tool_error_text: Color,
    pub compact_done_bg: Color,
    pub md_heading: Color,
    pub md_link: Color,
    pub md_link_url: Color,
    pub md_code: Color,
    pub md_code_block: Color,
    pub md_code_block_border: Color,
    pub md_quote: Color,
    pub md_quote_border: Color,
    pub md_hr: Color,
    pub md_list_bullet: Color,
    pub tool_diff_added: Color,
    pub tool_diff_removed: Color,
    pub tool_diff_context: Color,
    pub bash_mode: Color,
    pub syntax: SyntaxColours,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    #[default]
    Dark,
    Light,
}

impl Appearance {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "dark" => Ok(Self::Dark),
            "light" => Ok(Self::Light),
            other => bail!("`{other}` is not an appearance; use \"dark\" or \"light\""),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Palette::default().to_theme("default", Appearance::Dark)
    }
}

pub fn builtin_palette(name: &str) -> Option<Palette> {
    if let Some(flavour) = catppuccin::flavour(name) {
        return Some(flavour.palette());
    }
    match name {
        "default" | "dark" => Some(Palette::default()),
        "light" => Some(Palette {
            accent: Color::Blue,
            border: Color::Blue,
            border_accent: Color::Blue,
            border_muted: Color::Gray,
            warning: Color::Rgb(160, 100, 0),
            muted: Color::Gray,
            dim: Color::Gray,
            thinking_text: Color::Gray,
            user_message_bg: Color::Rgb(248, 248, 250),
            custom_message_bg: Color::Rgb(235, 235, 240),
            custom_message_label: Color::Gray,
            tool_pending_bg: Color::Rgb(235, 235, 240),
            tool_success_bg: Color::Rgb(226, 240, 228),
            tool_error_bg: Color::Rgb(248, 228, 228),
            tool_title: Color::Blue,
            tool_output: Color::Rgb(60, 60, 68),
            selected_bg: Color::Rgb(190, 210, 240),
            md_heading: Color::Blue,
            md_code: Color::Magenta,
            md_code_block: Color::Magenta,
            md_code_block_border: Color::Gray,
            md_quote: Color::Gray,
            md_quote_border: Color::Gray,
            md_hr: Color::Gray,
            md_link: Color::Blue,
            md_link_url: Color::Gray,
            md_list_bullet: Color::Blue,
            tool_diff_added: Color::Rgb(20, 120, 40),
            tool_diff_removed: Color::Rgb(170, 30, 40),
            tool_diff_context: Color::Gray,
            syntax_comment: Color::Rgb(110, 110, 120),
            syntax_keyword: Color::Rgb(175, 0, 90),
            syntax_function: Color::Rgb(90, 40, 160),
            syntax_variable: Color::Rgb(20, 20, 30),
            syntax_string: Color::Rgb(10, 100, 60),
            syntax_number: Color::Rgb(20, 80, 160),
            syntax_type: Color::Rgb(10, 110, 110),
            syntax_operator: Color::Rgb(175, 0, 90),
            syntax_punctuation: Color::Rgb(90, 90, 100),
            bash_mode: Color::Rgb(160, 100, 0),
            ..Palette::default()
        }),
        "mono" => Some(Palette {
            accent: Color::Reset,
            border: Color::Reset,
            border_accent: Color::Reset,
            border_muted: Color::Gray,
            warning: Color::Reset,
            muted: Color::Gray,
            dim: Color::Gray,
            thinking_text: Color::Gray,
            custom_message_bg: Color::Reset,
            custom_message_label: Color::Gray,
            tool_pending_bg: Color::Reset,
            tool_success_bg: Color::Reset,
            tool_error_bg: Color::Reset,
            tool_title: Color::Reset,
            tool_output: Color::Reset,
            selected_bg: Color::Rgb(60, 60, 60),
            md_heading: Color::Reset,
            md_link: Color::Reset,
            md_link_url: Color::Gray,
            md_code: Color::Reset,
            md_code_block: Color::Reset,
            md_code_block_border: Color::Gray,
            md_quote: Color::Gray,
            md_quote_border: Color::Gray,
            md_hr: Color::Gray,
            md_list_bullet: Color::Reset,
            tool_diff_added: Color::Reset,
            tool_diff_removed: Color::Reset,
            tool_diff_context: Color::Gray,
            syntax_comment: Color::Gray,
            syntax_keyword: Color::Reset,
            syntax_function: Color::Reset,
            syntax_variable: Color::Reset,
            syntax_string: Color::Reset,
            syntax_number: Color::Reset,
            syntax_type: Color::Reset,
            syntax_operator: Color::Reset,
            syntax_punctuation: Color::Gray,
            bash_mode: Color::Reset,
            ..Palette::default()
        }),
        _ => None,
    }
}

pub fn builtin_names() -> Vec<&'static str> {
    let mut names = vec!["default", "dark", "light", "mono"];
    names.extend(catppuccin::NAMES);
    names.push("system");
    names
}

pub type ThemeTables = BTreeMap<String, toml::Value>;

pub fn build(selected: &str, tables: &ThemeTables) -> Result<Theme> {
    build_for(selected, tables, terminal_appearance())
}

pub fn build_for(selected: &str, tables: &ThemeTables, appearance: Appearance) -> Result<Theme> {
    match selected.split_once('/') {
        Some((light, dark)) if !light.is_empty() && !dark.is_empty() => {
            let name = match appearance {
                Appearance::Light => light,
                Appearance::Dark => dark,
            };
            let mut theme = build_one(name, tables)?;
            theme.appearance = appearance;
            Ok(theme)
        }
        _ => build_one(selected, tables),
    }
}

fn build_one(selected: &str, tables: &ThemeTables) -> Result<Theme> {
    let mut seen = Vec::new();
    let (palette, appearance) = resolve(selected, tables, &mut seen)?;
    Ok(palette.to_theme(selected, appearance))
}

fn resolve(
    name: &str,
    tables: &ThemeTables,
    seen: &mut Vec<String>,
) -> Result<(Palette, Appearance)> {
    if seen.iter().any(|s| s == name) {
        bail!(
            "theme `{name}` extends itself ({}); a theme cannot be its own base",
            seen.join(" -> ")
        );
    }
    seen.push(name.to_owned());
    let table = tables.get(name);
    let extends = table
        .and_then(|t| t.get("extends"))
        .and_then(|v| v.as_str());
    let (mut palette, mut appearance) = match extends {
        Some(base) => resolve(base, tables, seen)?,
        None if name == "system" => {
            let terminal = Palette::from_terminal();
            (terminal.palette, terminal.appearance)
        }
        None => match builtin_palette(name) {
            Some(palette) => (palette, appearance_of(name)),
            None => (Palette::default(), Appearance::Dark),
        },
    };
    let Some(value) = table else {
        if name != "system" && builtin_palette(name).is_none() {
            bail!(
                "there is no theme `{name}`; built in: {}. Define one with [themes.{name}] in \
                 config.toml",
                builtin_names().join(", ")
            );
        }
        return Ok((palette, appearance));
    };
    let Some(table) = value.as_table() else {
        bail!("[themes.{name}] must be a table of colour roles");
    };
    if let Some(given) = table.get("appearance") {
        let Some(given) = given.as_str() else {
            bail!("[themes.{name}].appearance must be \"dark\" or \"light\"");
        };
        appearance =
            Appearance::parse(given).with_context(|| format!("[themes.{name}].appearance"))?;
    }
    palette = Palette::from_table(table, palette).with_context(|| format!("[themes.{name}]"))?;
    Ok((palette, appearance))
}

fn appearance_of(name: &str) -> Appearance {
    if let Some(flavour) = catppuccin::flavour(name) {
        return flavour.appearance;
    }
    match name {
        "light" => Appearance::Light,
        _ => Appearance::Dark,
    }
}

pub fn terminal_appearance() -> Appearance {
    if let Some(colour) = terminal::reported().and_then(|r| r.background)
        && let Some(rgb) = palette::rgb(colour)
    {
        return if palette::luminance(rgb) > 0.35 {
            Appearance::Light
        } else {
            Appearance::Dark
        };
    }
    if let Ok(value) = env::var("COLORFGBG") {
        if let Some(bg) = value.rsplit(';').next()
            && let Ok(index) = bg.trim().parse::<u8>()
        {
            return match index {
                7 => Appearance::Light,
                9..=15 => Appearance::Light,
                _ => Appearance::Dark,
            };
        }
    }
    Appearance::Dark
}

pub fn palette(index: i64) -> Result<Color> {
    if !(0..=255).contains(&index) {
        bail!("a palette index is 0 to 255, not {index}");
    }
    Ok(Color::Indexed(index as u8))
}

pub fn colour_value(text: &str, vars: &BTreeMap<String, String>) -> Result<Color> {
    let value = text.trim();
    if value.is_empty() {
        return Ok(Color::Reset);
    }
    if let Some(lower) = value
        .strip_prefix("oklch(")
        .and_then(|v| v.strip_suffix(')'))
    {
        return parse_oklch(lower);
    }
    if let Some(lower) = value
        .strip_prefix("okhsl(")
        .and_then(|v| v.strip_suffix(')'))
    {
        return parse_okhsl(lower);
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex, text);
    }
    if let Ok(index) = value.parse::<u16>() {
        return palette(i64::from(index));
    }
    if let Some(ansi) = ansi(value) {
        return Ok(ansi);
    }
    let mut name = value.to_owned();
    let mut hops = 0;
    while let Some(next) = vars.get(&name) {
        hops += 1;
        if hops > vars.len() + 1 {
            bail!("`{value}` refers to itself through vars; a variable cannot be its own value");
        }
        if next.trim() == name {
            bail!("`{value}` refers to itself in vars");
        }
        name = next.trim().to_owned();
        if !vars.contains_key(&name) {
            return colour_value(&name, &BTreeMap::new());
        }
    }
    if vars.contains_key(value) {
        return colour_value(&name, &BTreeMap::new());
    }
    bail!(
        "`{value}` is not a colour or a var; use an ANSI name, #rgb, #rrggbb, oklch(…), okhsl(…), a \
         palette index 0-255, \"\" for the terminal default, or the name of a `vars` entry"
    )
}

fn parse_hex(hex: &str, whole: &str) -> Result<Color> {
    let digits = |s: &str| -> Result<u8> {
        u8::from_str_radix(s, 16).map_err(|_| anyhow::anyhow!("`{s}` is not hex"))
    };
    match hex.len() {
        3 => {
            let r = digits(&hex[0..1])?;
            let g = digits(&hex[1..2])?;
            let b = digits(&hex[2..3])?;
            Ok(Color::Rgb(r * 17, g * 17, b * 17))
        }
        6 => Ok(Color::Rgb(
            digits(&hex[0..2])?,
            digits(&hex[2..4])?,
            digits(&hex[4..6])?,
        )),
        _ => bail!("`{whole}` is not a colour: hex is #rgb or #rrggbb"),
    }
}

fn ansi(value: &str) -> Option<Color> {
    Some(match value.trim().to_ascii_lowercase().as_str() {
        "reset" | "default" | "terminal" => Color::Reset,
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "dark-gray" | "dark-grey" => Color::DarkGray,
        "light-red" => Color::LightRed,
        "light-green" => Color::LightGreen,
        "light-yellow" => Color::LightYellow,
        "light-blue" => Color::LightBlue,
        "light-magenta" => Color::LightMagenta,
        "light-cyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}

fn parse_oklch(args: &str) -> Result<Color> {
    let (lightness, chroma, hue) = three(args, "oklch")?;
    let lightness = if lightness.ends_with('%') {
        number(&lightness[..lightness.len() - 1])? / 100.0
    } else {
        number(&lightness)?
    };
    let chroma = number(&chroma)?;
    let hue = number(hue.trim_end_matches("deg"))?;
    let srgb = oklch_to_srgb(lightness, chroma, hue);
    Ok(srgb_colour(srgb))
}

fn parse_okhsl(args: &str) -> Result<Color> {
    let (hue, saturation, lightness) = three(args, "okhsl")?;
    let hue = number(hue.trim_end_matches("deg"))?;
    let saturation = percent_or_number(&saturation)?;
    let lightness = percent_or_number(&lightness)?;
    let max = max_chroma(lightness, hue);
    let srgb = oklch_to_srgb(lightness, (saturation * max).clamp(0.0, max), hue);
    Ok(srgb_colour(srgb))
}

fn three(args: &str, form: &str) -> Result<(String, String, String)> {
    let parts: Vec<String> = args
        .replace(',', " ")
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    match parts.len() {
        3 => Ok((parts[0].clone(), parts[1].clone(), parts[2].clone())),
        n => bail!("`{form}(…)` takes three values, not {n}"),
    }
}

fn number(text: &str) -> Result<f64> {
    text.trim()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("`{}` is not a number", text.trim()))
}

fn percent_or_number(text: &str) -> Result<f64> {
    let text = text.trim();
    match text.strip_suffix('%') {
        Some(rest) => Ok((number(rest)? / 100.0).clamp(0.0, 1.0)),
        None => Ok(number(text)?.clamp(0.0, 1.0)),
    }
}

fn oklch_to_srgb(lightness: f64, chroma: f64, hue: f64) -> [f64; 3] {
    let hue = hue.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();
    let l_ = lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m_ = lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s_ = lightness - 0.089_484_177_5 * a - 1.291_485_548_0 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    [
        4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
        -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
        -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701_0 * s,
    ]
}

fn max_chroma(lightness: f64, hue: f64) -> f64 {
    let in_gamut = |c: f64| {
        let rgb = oklch_to_srgb(lightness, c, hue);
        rgb.iter().all(|v| (-1e-4..=1.0 + 1e-4).contains(v))
    };
    let (mut low, mut high) = (0.0, 0.5);
    if in_gamut(high) {
        return high;
    }
    for _ in 0..24 {
        let mid = (low + high) / 2.0;
        if in_gamut(mid) {
            low = mid;
        } else {
            high = mid;
        }
    }
    low
}

fn srgb_colour(linear: [f64; 3]) -> Color {
    let encode = |v: f64| -> u8 {
        let v = v.clamp(0.0, 1.0);
        let s = if v <= 0.003_130_8 {
            12.92 * v
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        };
        (s * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(encode(linear[0]), encode(linear[1]), encode(linear[2]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::catppuccin;
    use std::collections::BTreeSet;

    fn tables(text: &str) -> ThemeTables {
        toml::from_str(text).unwrap()
    }

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn colours_are_written_the_way_pi_writes_them() {
        let no_vars: BTreeMap<String, String> = BTreeMap::new();
        assert_eq!(colour_value("cyan", &no_vars).unwrap(), Color::Cyan);
        assert_eq!(
            colour_value("Dark-Grey", &no_vars).unwrap(),
            Color::DarkGray
        );
        assert_eq!(
            colour_value("#00aaff", &no_vars).unwrap(),
            Color::Rgb(0, 0xaa, 0xff)
        );
        assert_eq!(
            colour_value("#0af", &no_vars).unwrap(),
            Color::Rgb(0, 0xaa, 0xff)
        );
        assert_eq!(colour_value("240", &no_vars).unwrap(), Color::Indexed(240));
        assert_eq!(colour_value("", &no_vars).unwrap(), Color::Reset);
        assert_eq!(colour_value("  ", &no_vars).unwrap(), Color::Reset);
        for bad in ["#12", "chartreuse", "300"] {
            assert!(
                colour_value(bad, &no_vars).is_err(),
                "{bad} should be refused"
            );
        }
    }

    #[test]
    fn oklch_produces_the_colour_it_names() {
        assert_eq!(parse_oklch("100% 0 0").unwrap(), Color::Rgb(255, 255, 255));
        assert_eq!(parse_oklch("0% 0 0").unwrap(), Color::Rgb(0, 0, 0));
        let Color::Rgb(r, g, b) = parse_oklch("62% 0.1 264").unwrap() else {
            panic!("rgb");
        };
        assert!(b > r && b > g, "expected blue, got {r},{g},{b}");
        let Color::Rgb(r, _, _) = parse_oklch("62% 0.2 29").unwrap() else {
            panic!("rgb");
        };
        assert!(r > 150, "expected red-dominant, got r={r}");
        assert_eq!(parse_oklch("1 0 0").unwrap(), Color::Rgb(255, 255, 255));
    }

    #[test]
    fn okhsl_saturation_is_relative_to_the_gamut() {
        let light = parse_okhsl("250 0% 55%").unwrap();
        let Color::Rgb(r, g, b) = light else {
            panic!("rgb")
        };
        assert!(
            r.abs_diff(g) <= 1 && g.abs_diff(b) <= 1,
            "grey, got {r},{g},{b}"
        );
        let Color::Rgb(r, g, b) = parse_okhsl("250 100% 55%").unwrap() else {
            panic!("rgb");
        };
        assert!(
            r == 0 || r == 255 || g == 0 || g == 255 || b == 0 || b == 255,
            "expected a gamut edge, got {r},{g},{b}"
        );
        for s in [10, 30, 50, 70, 90] {
            assert!(parse_okhsl(&format!("250 {s}% 55%")).is_ok());
        }
    }

    #[test]
    fn vars_reference_other_vars_and_a_cycle_is_refused() {
        let v = vars(&[("base", "cyan"), ("accent2", "base")]);
        assert_eq!(colour_value("accent2", &v).unwrap(), Color::Cyan);
        assert_eq!(colour_value("base", &v).unwrap(), Color::Cyan);
        let v = vars(&[("a", "a")]);
        assert!(format!("{:#}", colour_value("a", &v).unwrap_err()).contains("itself"));
        let v = vars(&[("a", "b"), ("b", "a")]);
        assert!(colour_value("a", &v).is_err(), "a cycle must not loop");
        let err = colour_value("nope", &BTreeMap::new())
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a colour or a var"), "{err}");
    }

    #[test]
    fn the_default_theme_is_what_the_interface_always_drew() {
        let theme = build("default", &ThemeTables::new()).unwrap();
        assert_eq!(theme, Theme::default());
        assert_eq!(theme.accent, Color::Cyan);
        assert_eq!(theme.muted, Color::DarkGray);
        assert_eq!(theme.tool_pending_bg, Color::Rgb(40, 40, 52));
    }

    #[test]
    fn a_table_overrides_only_the_roles_it_lists_and_may_use_vars() {
        let t = tables(
            "[mine]\naccent = \"magenta\"\nerror = \"#f00\"\n[mine.vars]\nshared = \"#0af\"\n",
        );
        let t = match t.get("mine").and_then(|v| v.as_table()) {
            Some(_) => {
                let mut parsed: ThemeTables = toml::from_str(
                    "[mine]\naccent = \"magenta\"\nerror = \"#f00\"\n\n[mine.vars]\nshared = \"#0af\"\n",
                )
                .unwrap();
                parsed.remove("mine.vars");
                parsed
            }
            None => t,
        };
        let theme = build("mine", &t).unwrap();
        assert_eq!(theme.accent, Color::Magenta);
        assert_eq!(theme.error, Color::Rgb(255, 0, 0));
        assert_eq!(
            theme.muted,
            Theme::default().muted,
            "the rest is the default's"
        );
    }

    #[test]
    fn a_var_can_be_used_by_a_role_in_the_same_table() {
        let t = tables("[mine.vars]\nshared = \"#0af\"\n\n[mine]\naccent = \"shared\"\n");
        let theme = build("mine", &t).unwrap();
        assert_eq!(theme.accent, Color::Rgb(0, 0xaa, 0xff));
    }

    #[test]
    fn extends_picks_the_base_and_a_cycle_is_named() {
        let t = tables(
            "[base]\naccent = \"magenta\"\nmuted = \"gray\"\n\n[child]\nextends = \"base\"\nmuted = \"blue\"\n",
        );
        let theme = build("child", &t).unwrap();
        assert_eq!(theme.accent, Color::Magenta);
        assert_eq!(theme.muted, Color::Blue);
        let t = tables("[a]\nextends = \"a\"\n");
        assert!(
            build("a", &t)
                .unwrap_err()
                .to_string()
                .contains("extends itself")
        );
    }

    #[test]
    fn every_built_in_theme_builds_and_the_catppuccin_flavours_are_themselves() {
        for name in builtin_names() {
            let theme = build(name, &ThemeTables::new())
                .unwrap_or_else(|e| panic!("{name} does not build: {e:#}"));
            assert_eq!(theme.name, name);
        }
        let mocha = build("catppuccin-mocha", &ThemeTables::new()).unwrap();
        assert_eq!(mocha.text, Color::Rgb(0xcd, 0xd6, 0xf4), "Mocha's own text");
        assert_eq!(mocha.accent, Color::Rgb(0xcb, 0xa6, 0xf7), "Mocha's mauve");
        assert_eq!(mocha.appearance, Appearance::Dark);
        let latte = build("catppuccin-latte", &ThemeTables::new()).unwrap();
        assert_eq!(latte.text, Color::Rgb(0x4c, 0x4f, 0x69), "Latte's own text");
        assert_eq!(latte.appearance, Appearance::Light);
        let accents: Vec<String> = catppuccin::NAMES
            .iter()
            .map(|n| format!("{:?}", build(n, &ThemeTables::new()).unwrap().accent))
            .collect();
        let distinct: BTreeSet<&String> = accents.iter().collect();
        assert_eq!(distinct.len(), 4, "{accents:?}");
    }

    #[test]
    fn a_config_table_can_have_the_last_word_over_a_flavour() {
        let t = tables("[catppuccin-mocha]\naccent = \"#00ff00\"\n");
        let theme = build("catppuccin-mocha", &t).unwrap();
        assert_eq!(theme.accent, Color::Rgb(0, 255, 0), "the table wins");
        assert_eq!(
            theme.text,
            Color::Rgb(0xcd, 0xd6, 0xf4),
            "the flavour still shows"
        );
    }

    #[test]
    fn an_unknown_theme_a_misspelt_role_and_an_unpainted_role_all_explain_themselves() {
        let err = build("nope", &ThemeTables::new()).unwrap_err().to_string();
        assert!(err.contains("no theme `nope`"), "{err}");
        assert!(err.contains("mono, catppuccin-latte"), "{err}");
        assert!(err.contains("catppuccin-mocha, system"), "{err}");

        let t = tables("[mine]\nacccent = \"cyan\"\n");
        let err = format!("{:#}", build("mine", &t).unwrap_err());
        assert!(err.contains("not a colour role"), "{err}");

        let t = tables("[mine]\nsyntaxKeyword = \"#f00\"\n");
        assert!(build("mine", &t).is_ok());
    }

    #[test]
    fn a_light_dark_pair_picks_the_right_half() {
        let t = tables("[l]\naccent = \"red\"\n\n[d]\naccent = \"blue\"\n");
        let light = build_for("l/d", &t, Appearance::Light).unwrap();
        assert_eq!(light.name, "l");
        assert_eq!(light.accent, Color::Red);
        assert_eq!(light.appearance, Appearance::Light);
        let dark = build_for("l/d", &t, Appearance::Dark).unwrap();
        assert_eq!(dark.name, "d");
        assert_eq!(dark.accent, Color::Blue);
        assert_eq!(dark.appearance, Appearance::Dark);
        assert_eq!(build_for("l", &t, Appearance::Light).unwrap().name, "l");
        assert!(build_for("/d", &t, Appearance::Light).is_err());
        assert!(build_for("l/", &t, Appearance::Light).is_err());
    }

    #[test]
    fn colorfgbg_decides_the_appearance() {
        let decide = |value: &str| match value.rsplit(';').next().unwrap().parse::<u8>() {
            Ok(7) | Ok(9..=15) => Appearance::Light,
            _ => Appearance::Dark,
        };
        assert_eq!(decide("15;0"), Appearance::Dark);
        assert_eq!(decide("0;15"), Appearance::Light);
        assert_eq!(decide("0;7"), Appearance::Light);
        assert_eq!(decide("0;8"), Appearance::Dark);
    }

    #[test]
    fn a_palette_index_may_be_a_toml_integer() {
        let t = tables("[mine]\nmdCode = 39\n");
        assert_eq!(build("mine", &t).unwrap().md_code, Color::Indexed(39));
        let t = tables("[mine]\nmdCode = 256\n");
        let err = format!("{:#}", build("mine", &t).unwrap_err());
        assert!(err.contains("0 to 255"), "{err}");
    }
}
