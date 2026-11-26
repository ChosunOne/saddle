use anyhow::{Context, Result, bail};
use ratatui::style::Color;
use std::collections::BTreeMap;

use super::terminal;
use super::theme::{Appearance, Theme, builtin_palette, colour_value, palette};

pub const REQUIRED: &[&str] = &[
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "bashMode",
];

pub const OPTIONAL: &[(&str, &str)] = &[
    ("scrollbarTrack", "muted"),
    ("scrollbarThumb", "text"),
    ("searchMatchBg", "selectedBg"),
    ("searchMatchText", "text"),
    ("thinkingMax", "thinkingXhigh"),
];

pub fn all_roles() -> Vec<&'static str> {
    REQUIRED
        .iter()
        .copied()
        .chain(OPTIONAL.iter().map(|(name, _)| *name))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
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
    pub user_message_bg: Color,
    pub user_message_text: Color,
    pub custom_message_bg: Color,
    pub custom_message_text: Color,
    pub custom_message_label: Color,
    pub tool_pending_bg: Color,
    pub tool_success_bg: Color,
    pub tool_error_bg: Color,
    pub tool_title: Color,
    pub tool_output: Color,
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
    pub syntax_comment: Color,
    pub syntax_keyword: Color,
    pub syntax_function: Color,
    pub syntax_variable: Color,
    pub syntax_string: Color,
    pub syntax_number: Color,
    pub syntax_type: Color,
    pub syntax_operator: Color,
    pub syntax_punctuation: Color,
    pub thinking_off: Color,
    pub thinking_minimal: Color,
    pub thinking_low: Color,
    pub thinking_medium: Color,
    pub thinking_high: Color,
    pub thinking_xhigh: Color,
    pub bash_mode: Color,
    pub scrollbar_track: Color,
    pub scrollbar_thumb: Color,
    pub search_match_bg: Color,
    pub search_match_text: Color,
    pub thinking_max: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            border: Color::Cyan,
            border_accent: Color::Cyan,
            border_muted: Color::DarkGray,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            muted: Color::DarkGray,
            dim: Color::DarkGray,
            text: Color::Reset,
            thinking_text: Color::DarkGray,
            selected_bg: Color::Rgb(42, 74, 112),
            user_message_bg: Color::Reset,
            user_message_text: Color::Reset,
            custom_message_bg: Color::Rgb(40, 40, 52),
            custom_message_text: Color::Reset,
            custom_message_label: Color::Rgb(140, 140, 150),
            tool_pending_bg: Color::Rgb(40, 40, 52),
            tool_success_bg: Color::Rgb(36, 48, 40),
            tool_error_bg: Color::Rgb(62, 38, 40),
            tool_title: Color::Cyan,
            tool_output: Color::Rgb(170, 170, 178),
            md_heading: Color::Cyan,
            md_link: Color::Blue,
            md_link_url: Color::DarkGray,
            md_code: Color::Yellow,
            md_code_block: Color::Yellow,
            md_code_block_border: Color::DarkGray,
            md_quote: Color::DarkGray,
            md_quote_border: Color::DarkGray,
            md_hr: Color::DarkGray,
            md_list_bullet: Color::Cyan,
            tool_diff_added: Color::Rgb(130, 215, 140),
            tool_diff_removed: Color::Rgb(240, 130, 130),
            tool_diff_context: Color::DarkGray,
            syntax_comment: Color::Rgb(110, 118, 129),
            syntax_keyword: Color::Rgb(255, 123, 114),
            syntax_function: Color::Rgb(210, 168, 255),
            syntax_variable: Color::Rgb(230, 237, 243),
            syntax_string: Color::Rgb(165, 214, 255),
            syntax_number: Color::Rgb(121, 192, 255),
            syntax_type: Color::Rgb(126, 231, 135),
            syntax_operator: Color::Rgb(255, 123, 114),
            syntax_punctuation: Color::Rgb(139, 148, 158),
            thinking_off: Color::DarkGray,
            thinking_minimal: Color::Rgb(110, 118, 129),
            thinking_low: Color::Rgb(88, 166, 255),
            thinking_medium: Color::Rgb(210, 168, 255),
            thinking_high: Color::Rgb(255, 166, 87),
            thinking_xhigh: Color::Rgb(255, 123, 114),
            bash_mode: Color::Yellow,
            scrollbar_track: Color::DarkGray,
            scrollbar_thumb: Color::Reset,
            search_match_bg: Color::Rgb(42, 74, 112),
            search_match_text: Color::Reset,
            thinking_max: Color::Rgb(255, 123, 114),
        }
    }
}

impl Palette {
    pub fn set(&mut self, role: &str, colour: Color) -> Result<()> {
        let mut applied = true;
        match role {
            "accent" => self.accent = colour,
            "border" => self.border = colour,
            "borderAccent" => self.border_accent = colour,
            "borderMuted" => self.border_muted = colour,
            "success" => self.success = colour,
            "error" => self.error = colour,
            "warning" => self.warning = colour,
            "muted" => self.muted = colour,
            "dim" => self.dim = colour,
            "text" => self.text = colour,
            "thinkingText" => self.thinking_text = colour,
            "selectedBg" => self.selected_bg = colour,
            "userMessageBg" => self.user_message_bg = colour,
            "userMessageText" => self.user_message_text = colour,
            "customMessageBg" => self.custom_message_bg = colour,
            "customMessageText" => self.custom_message_text = colour,
            "customMessageLabel" => self.custom_message_label = colour,
            "toolPendingBg" => self.tool_pending_bg = colour,
            "toolSuccessBg" => self.tool_success_bg = colour,
            "toolErrorBg" => self.tool_error_bg = colour,
            "toolTitle" => self.tool_title = colour,
            "toolOutput" => self.tool_output = colour,
            "mdHeading" => self.md_heading = colour,
            "mdLink" => self.md_link = colour,
            "mdLinkUrl" => self.md_link_url = colour,
            "mdCode" => self.md_code = colour,
            "mdCodeBlock" => self.md_code_block = colour,
            "mdCodeBlockBorder" => self.md_code_block_border = colour,
            "mdQuote" => self.md_quote = colour,
            "mdQuoteBorder" => self.md_quote_border = colour,
            "mdHr" => self.md_hr = colour,
            "mdListBullet" => self.md_list_bullet = colour,
            "toolDiffAdded" => self.tool_diff_added = colour,
            "toolDiffRemoved" => self.tool_diff_removed = colour,
            "toolDiffContext" => self.tool_diff_context = colour,
            "syntaxComment" => self.syntax_comment = colour,
            "syntaxKeyword" => self.syntax_keyword = colour,
            "syntaxFunction" => self.syntax_function = colour,
            "syntaxVariable" => self.syntax_variable = colour,
            "syntaxString" => self.syntax_string = colour,
            "syntaxNumber" => self.syntax_number = colour,
            "syntaxType" => self.syntax_type = colour,
            "syntaxOperator" => self.syntax_operator = colour,
            "syntaxPunctuation" => self.syntax_punctuation = colour,
            "thinkingOff" => self.thinking_off = colour,
            "thinkingMinimal" => self.thinking_minimal = colour,
            "thinkingLow" => self.thinking_low = colour,
            "thinkingMedium" => self.thinking_medium = colour,
            "thinkingHigh" => self.thinking_high = colour,
            "thinkingXhigh" => self.thinking_xhigh = colour,
            "bashMode" => self.bash_mode = colour,
            "scrollbarTrack" => self.scrollbar_track = colour,
            "scrollbarThumb" => self.scrollbar_thumb = colour,
            "searchMatchBg" => self.search_match_bg = colour,
            "searchMatchText" => self.search_match_text = colour,
            "thinkingMax" => self.thinking_max = colour,
            other => {
                applied = false;
                let _ = other;
            }
        }
        if !applied {
            if let Some(role) = alias(role) {
                return self.set(role, colour);
            }
            bail!(
                "`{role}` is not a colour role; the roles are {}",
                all_roles().join(", ")
            );
        }
        Ok(())
    }

    fn fill_fallbacks(&mut self, given: &[String]) {
        let has = |name: &str| given.iter().any(|g| g == name);
        if !has("scrollbarTrack") {
            self.scrollbar_track = self.muted;
        }
        if !has("scrollbarThumb") {
            self.scrollbar_thumb = self.text;
        }
        if !has("searchMatchBg") {
            self.search_match_bg = self.selected_bg;
        }
        if !has("searchMatchText") {
            self.search_match_text = self.text;
        }
        if !has("thinkingMax") {
            self.thinking_max = self.thinking_xhigh;
        }
    }

    pub fn to_theme(&self, name: &str, appearance: Appearance) -> Theme {
        Theme {
            name: name.to_owned(),
            appearance,
            accent: self.accent,
            border: self.border,
            border_accent: self.border_accent,
            border_muted: self.border_muted,
            success: self.success,
            error: self.error,
            warning: self.warning,
            muted: self.muted,
            dim: self.dim,
            text: self.text,
            thinking_text: self.thinking_text,
            selected_bg: self.selected_bg,
            selection_fg: contrast_on(self.selected_bg, self.text),
            user_message_text: self.user_message_text,
            tool_pending_bg: self.tool_pending_bg,
            tool_success_bg: self.tool_success_bg,
            tool_error_bg: self.tool_error_bg,
            tool_title: self.tool_title,
            tool_output: self.tool_output,
            tool_error_text: self.error,
            compact_done_bg: self.tool_pending_bg,
            md_heading: self.md_heading,
            md_link: self.md_link,
            md_link_url: self.md_link_url,
            md_code: self.md_code,
            md_code_block: self.md_code_block,
            md_code_block_border: self.md_code_block_border,
            md_quote: self.md_quote,
            md_quote_border: self.md_quote_border,
            md_hr: self.md_hr,
            md_list_bullet: self.md_list_bullet,
            tool_diff_added: self.tool_diff_added,
            tool_diff_removed: self.tool_diff_removed,
            tool_diff_context: self.tool_diff_context,
            syntax: SyntaxColours::from_palette(self),
            bash_mode: self.bash_mode,
        }
    }

    pub fn from_table(table: &toml::Table, base: Palette) -> Result<Self> {
        let mut palette = base;
        let mut given = Vec::new();
        let mut vars: BTreeMap<String, String> = BTreeMap::new();
        if let Some(table) = table.get("vars") {
            let Some(table) = table.as_table() else {
                bail!("`vars` must be a table of colours");
            };
            for (key, value) in table {
                let Some(value) = value.as_str() else {
                    bail!("vars.{key} must be a colour string");
                };
                vars.insert(key.clone(), value.to_owned());
            }
        }
        for (role, value) in table {
            if matches!(role.as_str(), "extends" | "vars" | "appearance" | "name") {
                continue;
            }
            let colour = match value {
                toml::Value::String(text) => {
                    colour_value(text, &vars).with_context(|| role.clone())?
                }
                toml::Value::Integer(index) => {
                    palette_index(*index).with_context(|| role.clone())?
                }
                _ => bail!(
                    "{role} must be a colour string or a palette index 0-255, not {}",
                    value.type_str()
                ),
            };
            palette.set(role, colour)?;
            given.push(role.clone());
        }
        palette.fill_fallbacks(&given);
        Ok(palette)
    }
}

fn palette_index(index: i64) -> Result<Color> {
    palette(index)
}

fn alias(role: &str) -> Option<&'static str> {
    Some(match role {
        "border-accent" => "borderAccent",
        "border-muted" => "borderMuted",
        "warn" => "warning",
        "thinking-text" => "thinkingText",
        "selection-bg" => "selectedBg",
        "selection-fg" => "selectedBg",
        "code" => "mdCode",
        "link" => "mdLink",
        "tool-text" => "toolOutput",
        "tool-error-text" => "error",
        "tool-ok-bg" => "toolSuccessBg",
        "tool-pending-bg" => "toolPendingBg",
        "tool-error-bg" => "toolErrorBg",
        "tool-title" => "toolTitle",
        "compact-done-bg" => "toolPendingBg",
        "diff-added" => "toolDiffAdded",
        "diff-removed" => "toolDiffRemoved",
        "diff-context" => "toolDiffContext",
        "bash-mode" => "bashMode",
        "user-message-text" => "userMessageText",
        "md-heading" => "mdHeading",
        "md-link" => "mdLink",
        "md-link-url" => "mdLinkUrl",
        "md-code" => "mdCode",
        "md-code-block" => "mdCodeBlock",
        "md-code-block-border" => "mdCodeBlockBorder",
        "md-quote" => "mdQuote",
        "md-quote-border" => "mdQuoteBorder",
        "md-hr" => "mdHr",
        "md-list-bullet" => "mdListBullet",
        _ => return None,
    })
}

fn contrast_on(background: Color, preferred: Color) -> Color {
    let Some(bg) = rgb(background) else {
        return preferred;
    };
    let ratio = |fg: (f64, f64, f64)| {
        let (a, b) = (luminance(fg), luminance(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    };
    if let Some(fg) = rgb(preferred)
        && ratio(fg) >= 4.5
    {
        return preferred;
    }
    if ratio((1.0, 1.0, 1.0)) >= ratio((0.0, 0.0, 0.0)) {
        Color::White
    } else {
        Color::Black
    }
}

pub fn rgb(colour: Color) -> Option<(f64, f64, f64)> {
    let channel = |v: u8| f64::from(v) / 255.0;
    match colour {
        Color::Rgb(r, g, b) => Some((channel(r), channel(g), channel(b))),
        Color::Black => Some((0.0, 0.0, 0.0)),
        Color::White => Some((1.0, 1.0, 1.0)),
        Color::Red => Some((0.80, 0.0, 0.0)),
        Color::Green => Some((0.0, 0.80, 0.0)),
        Color::Yellow => Some((0.80, 0.80, 0.0)),
        Color::Blue => Some((0.0, 0.0, 0.80)),
        Color::Magenta => Some((0.80, 0.0, 0.80)),
        Color::Cyan => Some((0.0, 0.80, 0.80)),
        Color::Gray => Some((0.75, 0.75, 0.75)),
        Color::DarkGray => Some((0.35, 0.35, 0.35)),
        _ => None,
    }
}

pub fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
    let lin = |v: f64| {
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

pub fn blend(from: Color, to: Color, fraction: f64) -> Color {
    let (Some(a), Some(b)) = (rgb(from), rgb(to)) else {
        return from;
    };
    srgb((
        lerp(a.0, b.0, fraction),
        lerp(a.1, b.1, fraction),
        lerp(a.2, b.2, fraction),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyntaxColours {
    pub comment: Color,
    pub keyword: Color,
    pub function: Color,
    pub variable: Color,
    pub string: Color,
    pub number: Color,
    pub kind: Color,
    pub operator: Color,
    pub punctuation: Color,
}

impl SyntaxColours {
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            comment: p.syntax_comment,
            keyword: p.syntax_keyword,
            function: p.syntax_function,
            variable: p.syntax_variable,
            string: p.syntax_string,
            number: p.syntax_number,
            kind: p.syntax_type,
            operator: p.syntax_operator,
            punctuation: p.syntax_punctuation,
        }
    }
}

#[cfg(test)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PiTheme {
    #[allow(dead_code)]
    pub name: Option<String>,
    pub appearance: Option<String>,
    #[serde(default)]
    pub vars: BTreeMap<String, serde_json::Value>,
    pub colors: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    pub export: Option<serde_json::Value>,
}

#[cfg(test)]
impl PiTheme {
    pub fn palette(&self, base: Palette) -> Result<Palette> {
        let mut palette = base;
        let mut vars = BTreeMap::new();
        for (key, value) in &self.vars {
            match value {
                serde_json::Value::String(text) => {
                    vars.insert(key.clone(), text.clone());
                }
                other => bail!("vars.{key} must be a colour string, not {other}"),
            }
        }
        let mut given = Vec::new();
        for (role, value) in &self.colors {
            let colour = match value {
                serde_json::Value::String(text) => {
                    colour_value(text, &vars).with_context(|| role.clone())?
                }
                serde_json::Value::Number(n) => match n.as_i64() {
                    Some(index) => palette_index(index).with_context(|| role.clone())?,
                    None => bail!("colors.{role} must be an integer index 0-255"),
                },
                other => bail!("colors.{role} must be a colour, not {other}"),
            };
            palette.set(role, colour)?;
            given.push(role.clone());
        }
        palette.fill_fallbacks(&given);
        Ok(palette)
    }

    pub fn appearance(&self) -> Option<Appearance> {
        self.appearance
            .as_deref()
            .and_then(|a| Appearance::parse(a).ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reported {
    pub foreground: Option<Color>,
    pub background: Option<Color>,
    pub ansi: [Option<Color>; 16],
}

impl Reported {
    pub fn nothing() -> Self {
        Self {
            foreground: None,
            background: None,
            ansi: [None; 16],
        }
    }

    pub fn has_any(&self) -> bool {
        self.foreground.is_some()
            || self.background.is_some()
            || self.ansi.iter().any(Option::is_some)
    }
}

pub struct Derived {
    pub palette: Palette,
    pub appearance: Appearance,
}

impl Palette {
    pub fn from_report(report: &Reported) -> Derived {
        let background = report.background;
        let appearance = match background.and_then(rgb) {
            Some(bg) => {
                if luminance(bg) > 0.35 {
                    Appearance::Light
                } else {
                    Appearance::Dark
                }
            }
            None => Appearance::Dark,
        };
        if !report.has_any() {
            return Derived {
                palette: Palette::fallback(appearance),
                appearance,
            };
        }
        let base = match appearance {
            Appearance::Dark => Palette::default(),
            Appearance::Light => builtin_palette("light").unwrap_or_default(),
        };
        let mut palette = base.clone();
        let text = report.foreground.unwrap_or(Color::Reset);
        palette.text = text;
        palette.muted = place(base.muted, background, appearance, 3.0);
        palette.dim = place(base.dim, background, appearance, 2.0);
        palette.accent = hue_or(report, 6, base.accent, background, appearance);
        palette.error = hue_or(report, 1, base.error, background, appearance);
        palette.success = hue_or(report, 2, base.success, background, appearance);
        palette.warning = hue_or(report, 3, base.warning, background, appearance);
        palette.md_link = hue_or(report, 4, base.md_link, background, appearance);
        palette.md_heading = palette.accent;
        palette.md_list_bullet = palette.accent;
        palette.md_code = hue_or(report, 5, base.md_code, background, appearance);
        palette.md_code_block = palette.md_code;
        palette.tool_title = palette.accent;
        palette.md_quote = palette.muted;
        palette.md_quote_border = palette.muted;
        palette.md_hr = palette.muted;
        palette.md_link_url = palette.muted;
        palette.md_code_block_border = palette.muted;
        palette.tool_output = place(base.tool_output, background, appearance, 4.0);
        palette.tool_diff_added = hue_or(report, 2, base.tool_diff_added, background, appearance);
        palette.tool_diff_removed =
            hue_or(report, 1, base.tool_diff_removed, background, appearance);
        palette.tool_diff_context = palette.muted;
        palette.syntax_comment = palette.muted;
        palette.syntax_keyword = palette.error;
        palette.syntax_function = hue_or(report, 5, base.syntax_function, background, appearance);
        palette.syntax_variable = text;
        palette.syntax_string = hue_or(report, 2, base.syntax_string, background, appearance);
        palette.syntax_number = hue_or(report, 4, base.syntax_number, background, appearance);
        palette.syntax_type = hue_or(report, 6, base.syntax_type, background, appearance);
        palette.syntax_operator = palette.error;
        palette.syntax_punctuation = palette.muted;
        let shade = |toward: f64| mix(background.unwrap_or(Color::Reset), appearance, toward);
        palette.tool_pending_bg = shade(0.06);
        palette.tool_success_bg = shade(0.06);
        palette.tool_error_bg = shade(0.08);
        palette.selected_bg = shade(0.22);
        palette.custom_message_bg = shade(0.05);
        palette.user_message_bg = background.unwrap_or(Color::Reset);
        palette.custom_message_label = palette.muted;
        palette.thinking_text = palette.dim;
        palette.bash_mode = palette.warning;
        palette.fill_fallbacks(&[]);
        Derived {
            palette,
            appearance,
        }
    }

    fn fallback(_appearance: Appearance) -> Palette {
        Palette {
            accent: Color::Cyan,
            border: Color::Cyan,
            border_accent: Color::Cyan,
            border_muted: Color::DarkGray,
            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            muted: Color::DarkGray,
            dim: Color::DarkGray,
            text: Color::Reset,
            thinking_text: Color::DarkGray,
            selected_bg: Color::Rgb(60, 60, 60),
            user_message_bg: Color::Reset,
            user_message_text: Color::Reset,
            custom_message_bg: Color::Reset,
            custom_message_text: Color::Reset,
            custom_message_label: Color::DarkGray,
            tool_pending_bg: Color::Reset,
            tool_success_bg: Color::Reset,
            tool_error_bg: Color::Reset,
            tool_title: Color::Cyan,
            tool_output: Color::Reset,
            md_heading: Color::Cyan,
            md_link: Color::Blue,
            md_link_url: Color::DarkGray,
            md_code: Color::Yellow,
            md_code_block: Color::Yellow,
            md_code_block_border: Color::DarkGray,
            md_quote: Color::DarkGray,
            md_quote_border: Color::DarkGray,
            md_hr: Color::DarkGray,
            md_list_bullet: Color::Cyan,
            tool_diff_added: Color::Green,
            tool_diff_removed: Color::Red,
            tool_diff_context: Color::DarkGray,
            syntax_comment: Color::DarkGray,
            syntax_keyword: Color::Red,
            syntax_function: Color::Magenta,
            syntax_variable: Color::Reset,
            syntax_string: Color::Green,
            syntax_number: Color::Blue,
            syntax_type: Color::Cyan,
            syntax_operator: Color::Red,
            syntax_punctuation: Color::DarkGray,
            bash_mode: Color::Yellow,
            ..Palette::default()
        }
    }

    pub fn from_terminal() -> Derived {
        Self::from_report(&terminal::prime())
    }
}

fn hue_or(
    report: &Reported,
    index: usize,
    fallback: Color,
    background: Option<Color>,
    appearance: Appearance,
) -> Color {
    match report.ansi.get(index).copied().flatten() {
        Some(colour) => place(colour, background, appearance, 4.5),
        None => place(fallback, background, appearance, 4.5),
    }
}

fn place(colour: Color, background: Option<Color>, appearance: Appearance, ratio: f64) -> Color {
    let (Some(bg), Some(fg)) = (background.and_then(rgb), rgb(colour)) else {
        return colour;
    };
    let ratio_of = |c: (f64, f64, f64)| {
        let (a, b) = (luminance(c), luminance(bg));
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    };
    let ratio = ratio * 1.02;
    if ratio_of(fg) >= ratio {
        return srgb(fg);
    }
    let target = match appearance {
        Appearance::Dark => 1.0,
        Appearance::Light => 0.0,
    };
    let (mut low, mut high) = (0.0, 1.0);
    let mut best = fg;
    for _ in 0..18 {
        let t = (low + high) / 2.0;
        let moved = (
            lerp(fg.0, target, t),
            lerp(fg.1, target, t),
            lerp(fg.2, target, t),
        );
        if ratio_of(moved) >= ratio {
            best = moved;
            high = t;
        } else {
            low = t;
        }
    }
    srgb(best)
}

fn lerp(from: f64, to: f64, t: f64) -> f64 {
    from + (to - from) * t
}

fn mix(background: Color, appearance: Appearance, amount: f64) -> Color {
    let Some(bg) = rgb(background) else {
        return background;
    };
    let target = match appearance {
        Appearance::Dark => 1.0,
        Appearance::Light => 0.0,
    };
    srgb((
        lerp(bg.0, target, amount),
        lerp(bg.1, target, amount),
        lerp(bg.2, target, amount),
    ))
}

fn srgb((r, g, b): (f64, f64, f64)) -> Color {
    let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::Rgb(c(r), c(g), c(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_error_text_uses_the_error_foreground_not_the_block_background() {
        let palette = Palette {
            error: Color::Rgb(240, 110, 120),
            tool_error_bg: Color::Rgb(62, 38, 40),
            ..Palette::default()
        };
        let theme = palette.to_theme("custom", Appearance::Dark);
        assert_eq!(theme.tool_error_text, palette.error);
        assert_eq!(theme.tool_error_bg, palette.tool_error_bg);
    }

    #[test]
    fn legacy_tool_error_text_alias_changes_foreground_not_background() {
        let mut palette = Palette::default();
        let background = palette.tool_error_bg;
        palette.set("tool-error-text", Color::Yellow).unwrap();
        let theme = palette.to_theme("legacy", Appearance::Dark);
        assert_eq!(theme.tool_error_text, Color::Yellow);
        assert_eq!(theme.tool_error_bg, background);
    }

    #[test]
    fn every_role_in_the_list_can_be_set_on_a_palette() {
        for role in all_roles() {
            let mut p = Palette::default();
            p.set(role, Color::Cyan)
                .unwrap_or_else(|e| panic!("{role}: {e}"));
        }
        assert_eq!(REQUIRED.len(), 51, "every role in pi's required list");
        assert_eq!(OPTIONAL.len(), 5);
    }

    #[test]
    fn the_optional_roles_fall_back_as_pi_says() {
        let mut p = Palette {
            muted: Color::Rgb(1, 2, 3),
            text: Color::Rgb(4, 5, 6),
            selected_bg: Color::Rgb(7, 8, 9),
            thinking_xhigh: Color::Rgb(10, 11, 12),
            ..Palette::default()
        };
        p.fill_fallbacks(&[]);
        assert_eq!(p.scrollbar_track, Color::Rgb(1, 2, 3), "from muted");
        assert_eq!(p.scrollbar_thumb, Color::Rgb(4, 5, 6), "from text");
        assert_eq!(p.search_match_bg, Color::Rgb(7, 8, 9), "from selectedBg");
        assert_eq!(p.search_match_text, Color::Rgb(4, 5, 6), "from text");
        assert_eq!(p.thinking_max, Color::Rgb(10, 11, 12), "from thinkingXhigh");
        p.fill_fallbacks(&["scrollbarTrack".into()]);
        assert_eq!(p.scrollbar_track, Color::Rgb(1, 2, 3));
    }

    #[test]
    fn a_pi_json_theme_is_understood_whole() {
        let json = r##"{
            "name": "from-pi",
            "appearance": "dark",
            "vars": { "b": "#0af" },
            "colors": {
                "accent": "b",
                "border": 240,
                "error": "oklch(62% 0.2 29)",
                "syntaxKeyword": "#f00",
                "scrollbarThumb": ""
            },
            "export": { "pageBg": "#111" }
        }"##;
        let theme: PiTheme = serde_json::from_str(json).unwrap();
        let palette = theme.palette(Palette::default()).unwrap();
        assert_eq!(palette.accent, Color::Rgb(0, 0xaa, 0xff), "a var reference");
        assert_eq!(palette.border, Color::Indexed(240), "a palette index");
        assert!(matches!(palette.syntax_keyword, Color::Rgb(255, 0, 0)));
        assert_eq!(
            palette.scrollbar_thumb,
            Color::Reset,
            "empty is the terminal"
        );
        assert!(matches!(palette.error, Color::Rgb(r, _, _) if r > 150));
        assert_eq!(theme.appearance(), Some(Appearance::Dark));
        let bad = r##"{"colors": {"nonsense": "#fff"}}"##;
        let theme: PiTheme = serde_json::from_str(bad).unwrap();
        assert!(theme.palette(Palette::default()).is_err());
        let unknown = r#"{"colors": {}, "extra": 1}"#;
        assert!(serde_json::from_str::<PiTheme>(unknown).is_err());
    }

    fn contrast(a: Color, b: Color) -> f64 {
        let lum = |c: Color| {
            let Color::Rgb(r, g, b) = c else {
                panic!("not a colour: {c:?}")
            };
            let f = |v: u8| {
                let v = v as f64 / 255.0;
                if v <= 0.039_28 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
        };
        let (x, y) = (lum(a), lum(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    fn report(background: Color, foreground: Color) -> Reported {
        Reported {
            foreground: Some(foreground),
            background: Some(background),
            ansi: [None; 16],
        }
    }

    fn text_roles(p: &Palette) -> Vec<(&'static str, Color, f64)> {
        vec![
            ("text", p.text, 4.5),
            ("muted", p.muted, 3.0),
            ("dim", p.dim, 2.0),
            ("accent", p.accent, 4.5),
            ("success", p.success, 4.5),
            ("error", p.error, 4.5),
            ("warning", p.warning, 4.5),
            ("mdHeading", p.md_heading, 4.5),
            ("mdLink", p.md_link, 4.5),
            ("mdCode", p.md_code, 4.5),
            ("mdLinkUrl", p.md_link_url, 3.0),
            ("toolOutput", p.tool_output, 4.5),
            ("toolDiffAdded", p.tool_diff_added, 4.5),
            ("toolDiffRemoved", p.tool_diff_removed, 4.5),
        ]
    }

    #[test]
    fn the_derived_roles_clear_the_contrast_floor_on_a_terminal_background() {
        for (bg, fg) in [
            (Color::Rgb(0x1e, 0x1e, 0x1e), Color::Rgb(0xd4, 0xd4, 0xd4)),
            (Color::Rgb(0xfd, 0xfd, 0xfd), Color::Rgb(0x1a, 0x1a, 0x1a)),
        ] {
            let derived = Palette::from_report(&report(bg, fg));
            for (role, colour, floor) in text_roles(&derived.palette) {
                if colour == Color::Reset {
                    continue;
                }
                let ratio = contrast(colour, bg);
                assert!(
                    ratio >= floor,
                    "{role} is {ratio:.2}:1 on {bg:?}: {colour:?}"
                );
            }
        }
    }

    #[test]
    fn a_nearly_invisible_terminal_colour_is_moved_until_it_reads() {
        let bg = Color::Rgb(0x28, 0x28, 0x28);
        let derived = Palette::from_report(&report(bg, Color::Rgb(0xff, 0xff, 0xff)));
        assert!(
            contrast(derived.palette.muted, bg) >= 3.0,
            "{:?}",
            derived.palette.muted
        );
        assert!(
            contrast(derived.palette.dim, bg) >= 2.0,
            "dim is faint on purpose, but not invisible: {:?}",
            derived.palette.dim
        );
        assert_eq!(
            derived.palette.text,
            Color::Rgb(0xff, 0xff, 0xff),
            "the reported foreground is used as it is"
        );
    }

    #[test]
    fn the_appearance_follows_the_reported_background() {
        let dark = Palette::from_report(&report(
            Color::Rgb(0x10, 0x10, 0x10),
            Color::Rgb(0xff, 0xff, 0xff),
        ));
        assert_eq!(dark.appearance, Appearance::Dark);
        let light =
            Palette::from_report(&report(Color::Rgb(0xfa, 0xfa, 0xfa), Color::Rgb(0, 0, 0)));
        assert_eq!(light.appearance, Appearance::Light);
        assert!(contrast(dark.palette.selected_bg, Color::Rgb(0x10, 0x10, 0x10)) > 1.1);
        assert!(contrast(light.palette.selected_bg, Color::Rgb(0xfa, 0xfa, 0xfa)) > 1.1);
    }

    #[test]
    fn a_terminal_that_reports_a_palette_colours_the_roles_from_it() {
        let mut r = report(Color::Rgb(0x18, 0x18, 0x18), Color::Rgb(0xee, 0xee, 0xee));
        r.ansi[1] = Some(Color::Rgb(0xff, 0x55, 0x55));
        r.ansi[4] = Some(Color::Rgb(0x55, 0x88, 0xff));
        let derived = Palette::from_report(&r);
        let red = |c: Color| matches!(c, Color::Rgb(x, _, _) if x > 0xf0);
        let blue = |c: Color| matches!(c, Color::Rgb(_, _, z) if z > 0xf0);
        assert!(
            red(derived.palette.error),
            "error from ANSI red: {:?}",
            derived.palette.error
        );
        assert!(
            blue(derived.palette.md_link),
            "link from ANSI blue: {:?}",
            derived.palette.md_link
        );
    }

    #[test]
    fn a_silent_terminal_gets_the_ansi_fallback() {
        let derived = Palette::from_report(&Reported::nothing());
        let p = &derived.palette;
        assert_eq!(p.accent, Color::Cyan);
        assert_eq!(p.error, Color::Red);
        assert_eq!(p.text, Color::Reset);
        for (role, colour) in [
            ("toolPendingBg", p.tool_pending_bg),
            ("toolErrorBg", p.tool_error_bg),
            ("userMessageBg", p.user_message_bg),
        ] {
            assert_eq!(colour, Color::Reset, "{role} must stay unpainted");
        }
    }

    #[test]
    fn a_derived_palette_builds_a_theme_that_paints_syntax() {
        let derived = Palette::from_report(&report(
            Color::Rgb(0x10, 0x10, 0x10),
            Color::Rgb(0xee, 0xee, 0xee),
        ));
        let theme = derived.palette.to_theme("system", derived.appearance);
        assert_ne!(theme.syntax.keyword, Color::Reset);
        assert_ne!(theme.syntax.comment, Color::Reset);
        assert_eq!(theme.appearance, Appearance::Dark);
    }

    #[test]
    fn a_selection_foreground_is_chosen_by_contrast() {
        let pale_bg = Color::Rgb(240, 240, 240);
        assert_eq!(contrast_on(pale_bg, Color::White), Color::Black);
        let dark_bg = Color::Rgb(20, 20, 30);
        assert_eq!(contrast_on(dark_bg, Color::Black), Color::White);
        assert_eq!(contrast_on(dark_bg, Color::White), Color::White);
        assert_eq!(contrast_on(Color::Reset, Color::White), Color::White);
    }
}
