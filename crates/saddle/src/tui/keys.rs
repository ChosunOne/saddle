use std::fmt;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;
use serde::Deserializer;
use serde::de::Error as DeError;

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Keys {
    pub tree_next_end: Option<Stroke>,
    pub tree_previous_end: Option<Stroke>,
}

impl Keys {
    pub fn ends(&self) -> Option<String> {
        match (&self.tree_next_end, &self.tree_previous_end) {
            (None, None) => None,
            (Some(next), None) => Some(format!("{next} next branch end")),
            (None, Some(previous)) => Some(format!("{previous} previous branch end")),
            (Some(next), Some(previous)) => Some(format!("{next}/{previous} branch ends")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stroke {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Stroke {
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("a key is empty; write one like ctrl+n or f5".to_owned());
        }
        let parts: Vec<&str> = text.split('+').filter(|part| !part.is_empty()).collect();
        let Some((last, named)) = parts.split_last() else {
            return Err(format!(
                "`{text}` is not a key; write one like ctrl+n or f5"
            ));
        };
        let mut mods = KeyModifiers::empty();
        for part in named {
            match part.trim().to_lowercase().as_str() {
                "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
                "alt" | "opt" | "option" | "meta" => mods |= KeyModifiers::ALT,
                "shift" => mods |= KeyModifiers::SHIFT,
                other => {
                    return Err(format!(
                        "`{other}` is not a modifier; write ctrl, alt or shift"
                    ));
                }
            }
        }
        let (code, mods) = Self::code(last.trim(), mods)?;
        Ok(Self { code, mods })
    }

    fn code(name: &str, mods: KeyModifiers) -> Result<(KeyCode, KeyModifiers), String> {
        let lower = name.to_lowercase();
        let named = match lower.as_str() {
            "tab" if mods.contains(KeyModifiers::SHIFT) => {
                return Ok((KeyCode::BackTab, KeyModifiers::empty()));
            }
            "tab" => KeyCode::Tab,
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "space" => KeyCode::Char(' '),
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "page-up" | "pageup" | "pgup" => KeyCode::PageUp,
            "page-down" | "pagedown" | "pgdn" => KeyCode::PageDown,
            other => {
                if let Some(number) = other.strip_prefix('f')
                    && let Ok(number) = number.parse::<u8>()
                    && (1..=24).contains(&number)
                {
                    return Ok((KeyCode::F(number), mods));
                }
                let mut chars = name.chars();
                return match (chars.next(), chars.next()) {
                    (Some(ch), None) => {
                        let ch = if mods.contains(KeyModifiers::SHIFT) {
                            ch.to_ascii_uppercase()
                        } else {
                            ch.to_ascii_lowercase()
                        };
                        Ok((KeyCode::Char(ch), mods))
                    }
                    _ => Err(format!(
                        "`{name}` is not a key; write one like ctrl+n, alt+j, f5 or a single character"
                    )),
                };
            }
        };
        Ok((named, mods))
    }

    pub fn is(&self, key: &KeyEvent) -> bool {
        key.code == self.code && key.modifiers.contains(self.mods)
    }

    #[cfg(test)]
    pub fn keystroke(&self) -> KeyEvent {
        KeyEvent::new(self.code, self.mods)
    }
}

impl fmt::Display for Stroke {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        for (flag, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
        ] {
            if self.mods.contains(flag) {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&Self::name(self.code));
        f.write_str(&out)
    }
}

impl Stroke {
    fn name(code: KeyCode) -> String {
        match code {
            KeyCode::Char(' ') => "space".to_owned(),
            KeyCode::Char(ch) => ch.to_string(),
            KeyCode::Tab => "tab".to_owned(),
            KeyCode::BackTab => "shift+tab".to_owned(),
            KeyCode::Enter => "enter".to_owned(),
            KeyCode::Esc => "esc".to_owned(),
            KeyCode::Backspace => "backspace".to_owned(),
            KeyCode::Delete => "delete".to_owned(),
            KeyCode::Insert => "insert".to_owned(),
            KeyCode::Up => "up".to_owned(),
            KeyCode::Down => "down".to_owned(),
            KeyCode::Left => "left".to_owned(),
            KeyCode::Right => "right".to_owned(),
            KeyCode::Home => "home".to_owned(),
            KeyCode::End => "end".to_owned(),
            KeyCode::PageUp => "page-up".to_owned(),
            KeyCode::PageDown => "page-down".to_owned(),
            KeyCode::F(number) => format!("f{number}"),
            other => format!("{other:?}").to_lowercase(),
        }
    }
}

impl<'de> Deserialize<'de> for Stroke {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(DeError::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_read_the_way_a_person_writes_it() {
        for (text, code, mods) in [
            ("ctrl+n", KeyCode::Char('n'), KeyModifiers::CONTROL),
            ("alt+j", KeyCode::Char('j'), KeyModifiers::ALT),
            (
                "control+shift+p",
                KeyCode::Char('P'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            ("opt+f5", KeyCode::F(5), KeyModifiers::ALT),
            ("f12", KeyCode::F(12), KeyModifiers::empty()),
            ("page-up", KeyCode::PageUp, KeyModifiers::empty()),
            ("enter", KeyCode::Enter, KeyModifiers::empty()),
            ("esc", KeyCode::Esc, KeyModifiers::empty()),
            ("space", KeyCode::Char(' '), KeyModifiers::empty()),
            ("x", KeyCode::Char('x'), KeyModifiers::empty()),
        ] {
            let stroke = Stroke::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(stroke.code, code, "{text}");
            assert_eq!(stroke.mods, mods, "{text}");
            assert!(stroke.is(&KeyEvent::new(code, mods)), "{text}");
        }
        assert_eq!(
            Stroke::parse("shift+tab"),
            Ok(Stroke {
                code: KeyCode::BackTab,
                mods: KeyModifiers::empty()
            })
        );
        assert_eq!(
            Stroke::parse("shift+tab").unwrap().keystroke().code,
            KeyCode::BackTab
        );
    }

    #[test]
    fn a_stroke_says_itself_back() {
        for text in [
            "ctrl+n",
            "alt+j",
            "f5",
            "page-down",
            "space",
            "enter",
            "shift+tab",
        ] {
            let stroke = Stroke::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            assert_eq!(stroke.to_string(), text);
        }
        assert_eq!(Stroke::parse("Control + N").unwrap().to_string(), "ctrl+n");
        let shifted = Stroke::parse("shift+n").expect("a key");
        assert_eq!(shifted.to_string(), "shift+N");
        assert!(shifted.is(&KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT)));
    }

    #[test]
    fn a_key_this_file_does_not_know_is_an_error_rather_than_a_key_that_never_fires() {
        for bad in ["ctrln", "", "hyper+k", "f99", "ctrl+", "not a key"] {
            assert!(Stroke::parse(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(
            Stroke::parse("hyper+k")
                .unwrap_err()
                .contains("not a modifier")
        );
        assert!(Stroke::parse("shiftkey").unwrap_err().contains("not a key"));
        assert!(Stroke::parse("f99").unwrap_err().contains("f99"));
    }

    #[test]
    fn the_footer_says_the_keys_only_when_there_are_some() {
        let mut keys = Keys::default();
        assert_eq!(keys.ends(), None);
        keys.tree_next_end = Some(press_stroke("ctrl+n"));
        assert_eq!(keys.ends().as_deref(), Some("ctrl+n next branch end"));
        keys.tree_previous_end = Some(press_stroke("ctrl+p"));
        assert_eq!(keys.ends().as_deref(), Some("ctrl+n/ctrl+p branch ends"));
    }

    fn press_stroke(text: &str) -> Stroke {
        Stroke::parse(text).expect("a key")
    }
}
