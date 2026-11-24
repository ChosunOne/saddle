//! The /resume dialog: this workspace's saved sessions, newest first, filtered as you type.

use std::time::{Duration, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::store::Summary;

pub const ROWS: usize = 12;

pub struct SessionPicker {
    pub sessions: Vec<Summary>,
    pub selected: usize,
    pub filter: String,
    /// The selected conversation waiting for a second ctrl+d, since deleting is not undoable
    /// (D55).
    pub confirming: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PickOutcome {
    Stay,
    Close,
    Resume(String),
    /// Rename the selected conversation to this (D55).
    Rename(String, String),
    /// Delete the selected conversation, after a second press of the same key (D55).
    Delete(String),
}

impl SessionPicker {
    pub fn new(sessions: Vec<Summary>) -> Self {
        Self {
            sessions,
            selected: 0,
            filter: String::new(),
            confirming: None,
        }
    }

    /// Sessions whose first prompt contains the filter, ignoring case.
    pub fn visible(&self) -> Vec<&Summary> {
        let needle = self.filter.trim().to_lowercase();
        self.sessions
            .iter()
            .filter(|s| s.title.to_lowercase().contains(&needle))
            .collect()
    }

    pub fn key(&mut self, key: KeyEvent) -> PickOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return PickOutcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shown = self.visible().len();
        if !(ctrl && key.code == KeyCode::Char('d')) {
            self.confirming = None;
        }
        match key.code {
            KeyCode::Esc => return PickOutcome::Close,
            KeyCode::Char('c') if ctrl => return PickOutcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(shown.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(ROWS),
            KeyCode::PageDown => {
                self.selected = (self.selected + ROWS).min(shown.saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(s) = self.visible().get(self.selected) {
                    return PickOutcome::Resume(s.id.clone());
                }
            }
            // Anything other than a second ctrl+d means the delete was not meant.
            KeyCode::Char('d') if ctrl => {
                if let Some(s) = self.visible().get(self.selected) {
                    let id = s.id.clone();
                    if self.confirming.as_deref() == Some(id.as_str()) {
                        return PickOutcome::Delete(id);
                    }
                    self.confirming = Some(id);
                }
            }
            KeyCode::Char('r') if ctrl => {
                if let Some(s) = self.visible().get(self.selected) {
                    let mut name = String::new();
                    name.push_str(s.name.as_deref().unwrap_or_default());
                    return PickOutcome::Rename(s.id.clone(), name);
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.selected = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        PickOutcome::Stay
    }

    pub fn paste(&mut self, text: &str) {
        self.filter.extend(text.chars().filter(|c| !c.is_control()));
        self.selected = 0;
    }
}

/// "just now", "5m ago", "3h ago", "2d ago", or a date for anything older than a month.
pub fn age(when: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(when).unwrap_or(Duration::ZERO).as_secs();
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..86_400 => format!("{}h ago", secs / 3600),
        86_400..2_592_000 => format!("{}d ago", secs / 86_400),
        _ => humantime::format_rfc3339_seconds(when).to_string()[..10].to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(title: &str) -> Summary {
        Summary {
            id: format!("id-{title}"),
            title: title.into(),
            name: None,
            prompts: 1,
            messages: 3,
            model: "mock-model".into(),
            forked_from: None,
            worker: false,
            updated: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn filter_select_and_resume() {
        let mut p = SessionPicker::new(vec![
            summary("fix the build"),
            summary("add login"),
            summary("fix tests"),
        ]);
        for c in "FIX".chars() {
            p.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(p.visible().len(), 2);
        p.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        p.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickOutcome::Resume("id-fix tests".into())
        );
        assert_eq!(
            p.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            PickOutcome::Close
        );
    }

    #[test]
    fn ages() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000);
        let ago = |s| now - Duration::from_secs(s);
        assert_eq!(age(ago(5), now), "just now");
        assert_eq!(age(ago(300), now), "5m ago");
        assert_eq!(age(ago(7200), now), "2h ago");
        assert_eq!(age(ago(3 * 86_400), now), "3d ago");
        assert_eq!(age(SystemTime::UNIX_EPOCH, now), "1970-01-01");
    }
}
