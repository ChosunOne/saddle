//! The `/theme` list: every theme by name, drawn in the theme it offers (D74).
//!
//! A theme is chosen by looking at it, so the list is a picker rather than a printed set of names:
//! the highlighted entry is applied to the whole screen as the selection moves, and escape puts back
//! what was there. Enter is what makes the choice stick, by writing it to `config.toml`.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tui::theme::{Appearance, Theme, ThemeTables, build, builtin_names};

/// How many entries are on screen at once; the rest scroll, as the message tree does.
pub const ROWS: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A theme that ships with this build.
    Builtin,
    /// A `[themes.<name>]` table in `config.toml`.
    Config,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Builtin => "built in",
            Self::Config => "config.toml",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub source: Source,
    /// The theme as it builds, or why it does not: a broken palette stays in the list, so the
    /// mistake is visible where it was made instead of only at startup.
    pub theme: Result<Theme, String>,
}

impl Entry {
    /// What the list says about the entry beyond its name.
    pub fn note(&self) -> String {
        match &self.theme {
            Ok(theme) => match theme.appearance {
                Appearance::Light => "light".into(),
                Appearance::Dark => "dark".into(),
            },
            Err(e) => format!("does not build: {e}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    /// Escape: put back the theme that was in force when the list opened.
    Cancel,
    /// Enter: keep the highlighted theme and save it.
    Choose(String),
}

pub struct ThemePicker {
    pub entries: Vec<Entry>,
    pub selected: usize,
    /// The theme in force before the list opened, by name, so escape can name it in a note.
    pub original: String,
}

impl ThemePicker {
    /// Every theme this build can show, the one in force first so the list opens on it (D74).
    pub fn collect(tables: &ThemeTables, current: &str) -> Self {
        let mut names: Vec<(String, Source)> = builtin_names()
            .into_iter()
            .map(|n| (n.to_owned(), Source::Builtin))
            .collect();
        for name in tables.keys() {
            match names.iter_mut().find(|(n, _)| n == name) {
                // A `[themes.mono]` table overrides the built-in rather than duplicating it, and the
                // list must say so: `config.toml` is where that theme is now edited.
                Some((_, source)) => *source = Source::Config,
                None => names.push((name.clone(), Source::Config)),
            }
        }
        let entries: Vec<Entry> = names
            .into_iter()
            .map(|(name, source)| {
                let theme = build(&name, tables).map_err(|e| format!("{e:#}"));
                Entry {
                    name,
                    source,
                    theme,
                }
            })
            .collect();
        // Open on the theme in force, so the first thing seen is where you are. A pair
        // (`light/dark`) has no entry of its own: the half in force is the one to stand on.
        let selected = entries
            .iter()
            .position(|e| e.name == current)
            .or_else(|| entries.iter().position(|e| e.theme.is_ok()))
            .unwrap_or(0);
        Self {
            entries,
            selected,
            original: current.to_owned(),
        }
    }

    pub fn current(&self) -> &Entry {
        &self.entries[self.selected]
    }

    /// The theme to draw with while this entry is highlighted, if it builds.
    pub fn preview(&self) -> Option<&Theme> {
        self.current().theme.as_ref().ok()
    }

    /// The first entry drawn, kept so the selection is always on screen.
    pub fn offset(&self) -> usize {
        let last = self.entries.len().saturating_sub(1);
        if self.selected < ROWS {
            0
        } else {
            (self.selected + 1 - ROWS).min(last.saturating_sub(ROWS - 1))
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Outcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.entries.len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if ctrl => return Outcome::Cancel,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(last),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(ROWS),
            KeyCode::PageDown => self.selected = (self.selected + ROWS).min(last),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = last,
            KeyCode::Enter | KeyCode::Char(' ') => {
                // A theme that does not build cannot be chosen; the reason is already on screen.
                return match &self.current().theme {
                    Ok(_) => Outcome::Choose(self.current().name.clone()),
                    Err(_) => Outcome::Stay,
                };
            }
            _ => {}
        }
        Outcome::Stay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ThemeTables` is the contents of `[themes]`, so these are the tables inside it.
    fn tables(text: &str) -> ThemeTables {
        toml::from_str(text).unwrap()
    }

    fn press(p: &mut ThemePicker, code: KeyCode) -> Outcome {
        p.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn the_list_offers_every_built_in_and_every_theme_in_the_config() {
        let p = ThemePicker::collect(&tables("[mine]\n[other]\n"), "default");
        let names: Vec<&str> = p.entries.iter().map(|e| e.name.as_str()).collect();
        for builtin in builtin_names() {
            assert!(names.contains(&builtin), "{builtin} missing from {names:?}");
        }
        assert!(names.contains(&"mine"), "{names:?}");
        assert!(names.contains(&"other"), "{names:?}");
        // No name twice, even when a table shadows a built-in.
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicates in {names:?}");
    }

    #[test]
    fn a_theme_defined_here_shadows_the_built_in_of_the_same_name() {
        let p = ThemePicker::collect(&tables("[mono]\naccent = \"#ff0000\"\n"), "default");
        let mono: Vec<&Entry> = p.entries.iter().filter(|e| e.name == "mono").collect();
        assert_eq!(mono.len(), 1);
        assert_eq!(mono[0].source, Source::Config, "the config wins");
    }

    #[test]
    fn the_list_opens_on_the_theme_in_force() {
        let p = ThemePicker::collect(&tables(""), "light");
        assert_eq!(p.current().name, "light");
        // A name that is not in the list (a pair, or a theme since deleted) opens on the first
        // entry that can be drawn rather than on nothing.
        let p = ThemePicker::collect(&tables(""), "light/dark");
        assert!(p.preview().is_some(), "opened on a theme that builds");
    }

    #[test]
    fn moving_previews_enter_chooses_and_escape_cancels() {
        let mut p = ThemePicker::collect(&tables(""), "default");
        let first = p.current().name.clone();
        assert_eq!(press(&mut p, KeyCode::Down), Outcome::Stay);
        assert_ne!(p.current().name, first, "the highlight moved");
        assert!(p.preview().is_some(), "so there is something to preview");
        let chosen = p.current().name.clone();
        assert_eq!(press(&mut p, KeyCode::Enter), Outcome::Choose(chosen));
        assert_eq!(press(&mut p, KeyCode::Esc), Outcome::Cancel);
        // The ends are ends, not wrap-arounds: a list you can overshoot by holding a key is worse.
        press(&mut p, KeyCode::Up);
        press(&mut p, KeyCode::Up);
        press(&mut p, KeyCode::Up);
        assert_eq!(p.selected, 0);
        press(&mut p, KeyCode::End);
        assert_eq!(p.selected, p.entries.len() - 1);
        press(&mut p, KeyCode::Down);
        assert_eq!(p.selected, p.entries.len() - 1, "no past the end");
    }

    #[test]
    fn a_theme_that_does_not_build_stays_in_the_list_and_cannot_be_chosen() {
        // A broken palette is a mistake to be shown, not hidden: it is where the person made it.
        let mut p =
            ThemePicker::collect(&tables("[broken]\naccent = \"not-a-colour\"\n"), "default");
        let at = p.entries.iter().position(|e| e.name == "broken").unwrap();
        p.selected = at;
        assert!(p.preview().is_none(), "nothing to draw");
        assert!(
            p.current().note().starts_with("does not build"),
            "{}",
            p.current().note()
        );
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            Outcome::Stay,
            "refused, not chosen"
        );
    }

    #[test]
    fn the_selection_stays_on_screen_in_a_long_list() {
        let mut many = String::new();
        for i in 0..40 {
            many.push_str(&format!("[t{i}]\n"));
        }
        let mut p = ThemePicker::collect(&tables(&many), "default");
        assert_eq!(p.offset(), 0);
        press(&mut p, KeyCode::End);
        let top = p.offset();
        assert!(top > 0, "scrolled");
        assert!(p.selected >= top, "the selection is inside the window");
        assert!(p.selected < top + ROWS, "and not below it");
        press(&mut p, KeyCode::Home);
        assert_eq!(p.offset(), 0, "back to the top");
    }

    #[test]
    fn the_catppuccin_flavours_are_offered_and_say_which_terminal_they_want() {
        // A flavour is picked by looking at it, so it has to be in the list, and the note has to
        // say which kind of terminal it belongs on — Latte on a dark terminal is unreadable (D76).
        let p = ThemePicker::collect(&tables(""), "default");
        for name in crate::tui::catppuccin::NAMES {
            let entry = p
                .entries
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("{name} is not in the list"));
            assert_eq!(entry.source, Source::Builtin);
            let want = if name == "catppuccin-latte" {
                "light"
            } else {
                "dark"
            };
            assert_eq!(entry.note(), want, "{name}");
            assert!(entry.theme.is_ok(), "{name} does not build");
        }
    }

    #[test]
    fn the_note_says_what_a_theme_looks_like() {
        let p = ThemePicker::collect(&tables(""), "default");
        let light = p.entries.iter().find(|e| e.name == "light").unwrap();
        assert_eq!(light.note(), "light");
        let dark = p.entries.iter().find(|e| e.name == "dark").unwrap();
        assert_eq!(dark.note(), "dark");
    }
}
