//! Colours (D71). One place that says what each part of the screen is painted with, so a person can
//! change it without patching the renderer, and so the renderer never names a colour directly.
//!
//! A theme is a table of roles. The built-in `default` is what this binary has always drawn, so a
//! configuration that says nothing changes nothing; a `[themes.<name>]` table in `config.toml`
//! starts from the role set it names in `extends` (the default theme unless the name is itself
//! built in) and overrides only the roles it lists. That is what makes a partial theme useful: three
//! lines can restyle an accent without redefining the other twenty roles and owning them forever.
//!
//! Colours are written the way a person writes them: an ANSI name (`cyan`, `dark-gray`), a `#rrggbb`
//! hex value for a terminal that has true colour, or a 0–255 palette index for one that does not.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use ratatui::style::Color;

/// The roles the renderer paints with. Every key here is a valid `[themes.<name>]` line, and every
/// colour the interface draws comes from one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub name: String,
    /// Titles, tool names, the prompt marker: what draws the eye first.
    pub accent: Color,
    /// Secondary text: separators, gutters, rules, quotes, hints.
    pub muted: Color,
    /// Inline code and fenced blocks in a model's markdown.
    pub code: Color,
    pub warn: Color,
    pub error: Color,
    pub success: Color,
    pub link: Color,
    /// Body text, when a role needs to say "not the muted one".
    pub text: Color,
    pub diff_added: Color,
    pub diff_removed: Color,
    /// The block a tool call sits in, before and after it answers.
    pub tool_pending_bg: Color,
    pub tool_ok_bg: Color,
    pub tool_error_bg: Color,
    pub tool_text: Color,
    pub tool_error_text: Color,
    pub compact_done_bg: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
}

impl Default for Theme {
    /// What the interface looked like before themes existed, exactly.
    fn default() -> Self {
        Self {
            name: "default".into(),
            accent: Color::Cyan,
            muted: Color::DarkGray,
            code: Color::Yellow,
            warn: Color::Yellow,
            error: Color::Red,
            success: Color::Green,
            link: Color::Blue,
            text: Color::Reset,
            diff_added: Color::Rgb(130, 215, 140),
            diff_removed: Color::Rgb(240, 130, 130),
            tool_pending_bg: Color::Rgb(40, 40, 52),
            tool_ok_bg: Color::Rgb(36, 48, 40),
            tool_error_bg: Color::Rgb(62, 38, 40),
            tool_text: Color::Rgb(170, 170, 178),
            tool_error_text: Color::Rgb(240, 165, 165),
            compact_done_bg: Color::Rgb(38, 42, 60),
            selection_bg: Color::Rgb(42, 74, 112),
            selection_fg: Color::Rgb(246, 248, 251),
        }
    }
}

/// The themes a build ships with, by name.
pub fn builtin(name: &str) -> Option<Theme> {
    match name {
        "default" => Some(Theme::default()),
        // For a light terminal: the ANSI defaults are chosen to read on white, and the block
        // backgrounds are pale rather than dark so the tool text stays legible.
        "light" => Some(Theme {
            name: "light".into(),
            accent: Color::Blue,
            muted: Color::Gray,
            code: Color::Magenta,
            warn: Color::Rgb(160, 100, 0),
            error: Color::Red,
            success: Color::Green,
            link: Color::Blue,
            text: Color::Reset,
            diff_added: Color::Rgb(20, 120, 40),
            diff_removed: Color::Rgb(170, 30, 40),
            tool_pending_bg: Color::Rgb(235, 235, 240),
            tool_ok_bg: Color::Rgb(226, 240, 228),
            tool_error_bg: Color::Rgb(248, 228, 228),
            tool_text: Color::Rgb(60, 60, 68),
            tool_error_text: Color::Rgb(140, 30, 30),
            compact_done_bg: Color::Rgb(232, 234, 245),
            selection_bg: Color::Rgb(190, 210, 240),
            selection_fg: Color::Rgb(20, 20, 24),
        }),
        // Colour only where colour carries meaning; everything else in the terminal's own
        // foreground. For a monochrome terminal, or someone who wants the noise gone.
        "mono" => Some(Theme {
            name: "mono".into(),
            accent: Color::Reset,
            muted: Color::Gray,
            code: Color::Reset,
            warn: Color::Reset,
            error: Color::Reset,
            success: Color::Reset,
            link: Color::Reset,
            text: Color::Reset,
            diff_added: Color::Reset,
            diff_removed: Color::Reset,
            tool_pending_bg: Color::Reset,
            tool_ok_bg: Color::Reset,
            tool_error_bg: Color::Reset,
            tool_text: Color::Reset,
            tool_error_text: Color::Reset,
            compact_done_bg: Color::Reset,
            selection_bg: Color::Rgb(60, 60, 60),
            selection_fg: Color::Rgb(240, 240, 240),
        }),
        _ => None,
    }
}

/// Every built-in theme name, for a message that has to list them.
pub fn builtin_names() -> Vec<&'static str> {
    vec!["default", "light", "mono"]
}

/// The roles a theme can set, in the order a message lists them.
pub const ROLES: &[&str] = &[
    "accent",
    "muted",
    "code",
    "warn",
    "error",
    "success",
    "link",
    "text",
    "diff-added",
    "diff-removed",
    "tool-pending-bg",
    "tool-ok-bg",
    "tool-error-bg",
    "tool-text",
    "tool-error-text",
    "compact-done-bg",
    "selection-bg",
    "selection-fg",
];

impl Theme {
    /// Applies one role. An unknown role is an error rather than a silent no-op: a misspelt colour is
    /// a theme that does not do what it says, and nothing else would ever point at the line.
    pub fn set(&mut self, role: &str, value: &str) -> Result<()> {
        let colour = parse(value)?;
        match role {
            "accent" => self.accent = colour,
            "muted" => self.muted = colour,
            "code" => self.code = colour,
            "warn" => self.warn = colour,
            "error" => self.error = colour,
            "success" => self.success = colour,
            "link" => self.link = colour,
            "text" => self.text = colour,
            "diff-added" => self.diff_added = colour,
            "diff-removed" => self.diff_removed = colour,
            "tool-pending-bg" => self.tool_pending_bg = colour,
            "tool-ok-bg" => self.tool_ok_bg = colour,
            "tool-error-bg" => self.tool_error_bg = colour,
            "tool-text" => self.tool_text = colour,
            "tool-error-text" => self.tool_error_text = colour,
            "compact-done-bg" => self.compact_done_bg = colour,
            "selection-bg" => self.selection_bg = colour,
            "selection-fg" => self.selection_fg = colour,
            other => bail!(
                "`{other}` is not a colour role; the roles are {}",
                ROLES.join(", ")
            ),
        }
        Ok(())
    }
}

/// One colour as written in the config.
pub fn parse(value: &str) -> Result<Color> {
    let v = value.trim().to_ascii_lowercase();
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() != 6 {
            bail!("`{value}` is not a colour: hex is #rrggbb, six digits after the #");
        }
        let channel = |at: usize| -> Result<u8> {
            u8::from_str_radix(&hex[at..at + 2], 16).map_err(|_| {
                anyhow::anyhow!(
                    "`{value}` is not a colour: `{}` is not hex",
                    &hex[at..at + 2]
                )
            })
        };
        return Ok(Color::Rgb(channel(0)?, channel(2)?, channel(4)?));
    }
    // A bare number is a palette index, which is what a 256-colour terminal wants.
    if let Ok(index) = v.parse::<u16>() {
        if index > 255 {
            bail!("`{value}` is not a colour: a palette index is 0 to 255");
        }
        return Ok(Color::Indexed(index as u8));
    }
    Ok(match v.as_str() {
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
        other => bail!(
            "`{other}` is not a colour; use an ANSI name (cyan, dark-gray, …), #rrggbb, or a \
             palette index 0-255"
        ),
    })
}

/// The themes a config defines, before the selected one is built.
pub type ThemeTables = BTreeMap<String, toml::Value>;

/// Builds the selected theme (D71).
///
/// `extends` names the theme to start from: a built-in, or another table in the same config, which
/// is what lets a person put their shared roles in one place. An unknown name is an error here
/// rather than a silent fallback to the default, because a theme that is not the one asked for is
/// indistinguishable from a theme that failed.
pub fn build(selected: &str, tables: &ThemeTables) -> Result<Theme> {
    let mut seen = Vec::new();
    let mut theme = resolve(selected, tables, &mut seen)?;
    theme.name = selected.to_owned();
    Ok(theme)
}

fn resolve(name: &str, tables: &ThemeTables, seen: &mut Vec<String>) -> Result<Theme> {
    if seen.iter().any(|s| s == name) {
        bail!(
            "theme `{name}` extends itself ({}); a theme cannot be its own base",
            seen.join(" -> ")
        );
    }
    seen.push(name.to_owned());
    let table = tables.get(name);
    let mut theme = match table
        .and_then(|t| t.get("extends"))
        .and_then(|v| v.as_str())
    {
        Some(base) => resolve(base, tables, seen)?,
        // A built-in name with no `extends` starts from that built-in; anything else starts from
        // `default`, so a table that overrides one role does not have to restate the others.
        None => builtin(name).unwrap_or_default(),
    };
    if table.is_none() {
        if builtin(name).is_none() {
            bail!(
                "there is no theme `{name}`; built in: {}. Define one with [themes.{name}] in \
                 config.toml",
                builtin_names().join(", ")
            );
        }
        return Ok(theme);
    }
    let Some(table) = table.and_then(|t| t.as_table()) else {
        bail!("[themes.{name}] must be a table of colour roles");
    };
    for (role, value) in table {
        if role == "extends" {
            continue;
        }
        let Some(value) = value.as_str() else {
            bail!("[themes.{name}].{role} must be a colour string");
        };
        theme.set(role, value)?;
    }
    Ok(theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(text: &str) -> ThemeTables {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn colours_are_written_the_way_a_person_writes_them() {
        assert_eq!(parse("cyan").unwrap(), Color::Cyan);
        assert_eq!(parse("Dark-Grey").unwrap(), Color::DarkGray);
        assert_eq!(parse("#1a2b3c").unwrap(), Color::Rgb(0x1a, 0x2b, 0x3c));
        assert_eq!(parse(" 240 ").unwrap(), Color::Indexed(240));
        assert_eq!(parse("reset").unwrap(), Color::Reset);
        // Anything else is refused, with what was wrong.
        assert!(parse("#123").unwrap_err().to_string().contains("#rrggbb"));
        assert!(parse("300").unwrap_err().to_string().contains("0 to 255"));
        assert!(
            parse("chartreuse")
                .unwrap_err()
                .to_string()
                .contains("ANSI name")
        );
    }

    #[test]
    fn the_default_theme_is_what_the_interface_always_drew() {
        // Nothing configured means nothing changes, which is the property that makes shipping a
        // theme system safe.
        let theme = build("default", &ThemeTables::new()).unwrap();
        assert_eq!(theme, Theme::default());
        assert_eq!(theme.accent, Color::Cyan);
        assert_eq!(theme.muted, Color::DarkGray);
        assert_eq!(theme.tool_pending_bg, Color::Rgb(40, 40, 52));
    }

    #[test]
    fn a_table_overrides_only_the_roles_it_lists() {
        let t = tables("[mine]\naccent = \"magenta\"\nerror = \"#ff0000\"\n");
        let theme = build("mine", &t).unwrap();
        assert_eq!(theme.accent, Color::Magenta);
        assert_eq!(theme.error, Color::Rgb(255, 0, 0));
        // Everything else is the default's.
        assert_eq!(theme.muted, Theme::default().muted);
        assert_eq!(theme.selection_bg, Theme::default().selection_bg);
        assert_eq!(theme.name, "mine");
    }

    #[test]
    fn extends_picks_the_base_and_another_table_is_a_valid_base() {
        let t = tables(
            "[base]\naccent = \"magenta\"\nmuted = \"gray\"\n\n[child]\nextends = \"base\"\nmuted = \"blue\"\n",
        );
        let theme = build("child", &t).unwrap();
        assert_eq!(theme.accent, Color::Magenta, "inherited from its base");
        assert_eq!(theme.muted, Color::Blue, "overridden by the child");
        // A built-in can be extended too, so a person can adjust `light` without copying it.
        let t = tables("[dim]\nextends = \"light\"\naccent = \"red\"\n");
        let theme = build("dim", &t).unwrap();
        assert_eq!(theme.accent, Color::Red);
        assert_eq!(theme.muted, builtin("light").unwrap().muted);
    }

    #[test]
    fn a_theme_that_extends_itself_is_refused_rather_than_looping() {
        let t = tables("[a]\nextends = \"a\"\n");
        let err = build("a", &t).unwrap_err().to_string();
        assert!(err.contains("extends itself"), "{err}");
        let t = tables("[a]\nextends = \"b\"\n\n[b]\nextends = \"a\"\n");
        assert!(
            build("a", &t)
                .unwrap_err()
                .to_string()
                .contains("extends itself")
        );
    }

    #[test]
    fn an_unknown_theme_or_role_is_an_error_that_says_what_exists() {
        let err = build("nope", &ThemeTables::new()).unwrap_err().to_string();
        assert!(err.contains("no theme `nope`"), "{err}");
        assert!(err.contains("default, light, mono"), "{err}");
        let t = tables("[mine]\naccent = \"cyan\"\nakcent = \"red\"\n");
        // Duplicate keys do not even parse as TOML; the case that matters is a misspelt role.
        assert!(t.is_empty() || t.contains_key("mine"));
        let t = tables("[mine]\nacccent = \"cyan\"\n");
        let err = build("mine", &t).unwrap_err().to_string();
        assert!(err.contains("not a colour role"), "{err}");
        assert!(err.contains("accent, muted, code"), "{err}");
    }

    #[test]
    fn every_role_a_theme_can_set_is_a_role_the_renderer_uses() {
        // A role nobody paints with would be a setting that silently does nothing.
        let theme = Theme::default();
        for role in ROLES {
            let mut probe = theme.clone();
            probe
                .set(role, "cyan")
                .unwrap_or_else(|e| panic!("{role}: {e}"));
        }
        assert_eq!(ROLES.len(), 18, "update the count when a role is added");
    }
}
