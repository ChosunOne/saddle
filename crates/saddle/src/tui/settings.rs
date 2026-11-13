//! The /settings dialog. Each row shows a setting's current value; enter changes it, and the
//! controller saves the change to config.toml and applies it to the running session.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Reasoning effort levels, in the order enter cycles through them. `None` sends nothing.
pub const EFFORTS: [Option<&str>; 4] = [None, Some("low"), Some("medium"), Some("high")];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Provider,
    Model,
    Effort,
    MaxTokens,
    ContextWindow,
    StatusLine,
    MaxSteps,
    AutoCompact,
    IdleCompact,
    BackgroundCompact,
    ShowReasoning,
    ExpandTools,
    EditConfig,
    EditPolicy,
    Permissions,
}

/// Current values, gathered by the controller each time the dialog opens or changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Values {
    /// Empty when no provider is chosen.
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub max_tokens: Option<u32>,
    /// `None` uses the window the provider, the cache or the table says this model takes (D34).
    pub context_window: Option<u64>,
    /// The window in force while `context_window` is unset, for the dialog to show.
    pub window: u64,
    /// The status line as set, or the default (D36).
    pub status: String,
    /// `None` is no limit (D37).
    pub max_steps: Option<usize>,
    pub auto_compact: bool,
    pub idle_compact: bool,
    pub background_compact: bool,
    pub show_reasoning: bool,
    pub expand_tools: bool,
    pub config_path: String,
    pub policy_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    OpenLogin,
    ChooseModel,
    Effort(Option<String>),
    MaxTokens(Option<u32>),
    ContextWindow(Option<u32>),
    /// The status line template; `None` returns to the default (D36).
    StatusLine(Option<String>),
    /// `None` returns to the default.
    MaxSteps(Option<usize>),
    AutoCompact(bool),
    IdleCompact(bool),
    BackgroundCompact(bool),
    ShowReasoning(bool),
    ExpandTools(bool),
    EditConfig,
    EditPolicy,
    /// Opens the permissions editor for this project (D45).
    OpenPermissions,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SettingsOutcome {
    Stay,
    Close,
    Apply(Change),
}

pub struct SettingsDialog {
    pub values: Values,
    pub selected: usize,
    /// The text being typed into a number setting.
    pub editing: Option<String>,
    pub error: Option<String>,
}

pub const ROWS: [Setting; 15] = [
    Setting::Provider,
    Setting::Model,
    Setting::Effort,
    Setting::MaxTokens,
    Setting::ContextWindow,
    Setting::StatusLine,
    Setting::MaxSteps,
    Setting::AutoCompact,
    Setting::IdleCompact,
    Setting::BackgroundCompact,
    Setting::ShowReasoning,
    Setting::ExpandTools,
    Setting::Permissions,
    Setting::EditConfig,
    Setting::EditPolicy,
];

impl Setting {
    pub fn label(self) -> &'static str {
        match self {
            Setting::Provider => "Provider",
            Setting::Model => "Model",
            Setting::Effort => "Reasoning effort",
            Setting::MaxTokens => "Max output tokens",
            Setting::ContextWindow => "Context window",
            Setting::StatusLine => "Status line",
            Setting::AutoCompact => "Compact automatically",
            Setting::IdleCompact => "Compact while idle",
            Setting::BackgroundCompact => "Compact in the background",
            Setting::MaxSteps => "Model requests per prompt",
            Setting::ShowReasoning => "Show reasoning",
            Setting::ExpandTools => "Show full tool output",
            Setting::Permissions => "Permissions for this project",
            Setting::EditConfig => "Edit config.toml",
            Setting::EditPolicy => "Edit the default policy",
        }
    }

    /// What enter does, shown under the list for the selected row.
    pub fn help(self) -> &'static str {
        match self {
            Setting::Provider => "enter opens /login to choose a provider or add a key",
            Setting::Model => "enter lists the provider's models",
            Setting::Effort => "enter cycles default, low, medium, high; default sends nothing",
            Setting::MaxTokens => "enter edits; leave it empty for the provider's default",
            Setting::ContextWindow => {
                "the model's window in tokens; compaction starts 16k below it (empty: 128k)"
            }
            Setting::StatusLine => {
                "placeholders such as {cost}, {quota}, {cachehit}; empty uses the default"
            }
            Setting::AutoCompact => "summarize older messages when the context is nearly full",
            Setting::IdleCompact => {
                "past half full and left alone for two minutes, so coming back is not a full window"
            }
            Setting::BackgroundCompact => {
                "past three quarters, summarize while the turn carries on instead of stopping it"
            }
            Setting::MaxSteps => "model requests one prompt may take; leave it empty for no limit",
            Setting::ShowReasoning => "the start-up default; ctrl+t changes it for this session",
            Setting::ExpandTools => "the start-up default; ctrl+o changes it for this session",
            Setting::Permissions => {
                "the grants this project has; exclusions and scopes, applied next session"
            }
            Setting::EditConfig => "opens $EDITOR; roots and sandbox changes apply next session",
            Setting::EditPolicy => "the global policy every project starts from",
        }
    }

    fn needs_provider(self) -> bool {
        matches!(
            self,
            Setting::Effort | Setting::MaxTokens | Setting::ContextWindow
        )
    }
}

fn on_off(b: bool) -> String {
    if b { "on".into() } else { "off".into() }
}

impl SettingsDialog {
    pub fn new(values: Values) -> Self {
        Self {
            values,
            selected: 0,
            editing: None,
            error: None,
        }
    }

    pub fn current(&self) -> Setting {
        ROWS[self.selected]
    }

    pub fn value(&self, s: Setting) -> String {
        let v = &self.values;
        let none = v.provider.is_empty();
        match s {
            Setting::Provider if none => "none chosen".into(),
            Setting::Provider => v.provider.clone(),
            Setting::Model if none => "-".into(),
            Setting::Model => v.model.clone(),
            _ if none && s.needs_provider() => "choose a provider first".into(),
            Setting::Effort => v.effort.clone().unwrap_or_else(|| "default".into()),
            Setting::MaxTokens => v
                .max_tokens
                .map_or_else(|| "default".into(), |n| n.to_string()),
            Setting::ContextWindow => v
                .context_window
                .map_or_else(|| format!("auto ({})", v.window), |n| n.to_string()),
            Setting::StatusLine => v.status.clone(),
            Setting::MaxSteps => v
                .max_steps
                .map_or_else(|| "unlimited".into(), |n| n.to_string()),
            Setting::AutoCompact => on_off(v.auto_compact),
            Setting::IdleCompact => on_off(v.idle_compact),
            Setting::BackgroundCompact => on_off(v.background_compact),
            Setting::ShowReasoning => on_off(v.show_reasoning),
            Setting::ExpandTools => on_off(v.expand_tools),
            // The value column says what the row does, since the grants themselves are a screen
            // of their own.
            Setting::Permissions => "open the editor".into(),
            Setting::EditConfig => v.config_path.clone(),
            Setting::EditPolicy => v.policy_path.clone(),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> SettingsOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return SettingsOutcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // The status line is the one setting edited as text; the others take digits only.
        let text_mode = self.current() == Setting::StatusLine;
        if let Some(text) = &mut self.editing {
            match key.code {
                KeyCode::Esc => {
                    self.editing = None;
                    self.error = None;
                }
                KeyCode::Enter => return self.finish_edit(),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if c.is_ascii_digit() || (text_mode && !c.is_control()) => {
                    text.push(c);
                }
                _ => {}
            }
            return SettingsOutcome::Stay;
        }
        match key.code {
            KeyCode::Esc => return SettingsOutcome::Close,
            KeyCode::Char('c') if ctrl => return SettingsOutcome::Close,
            KeyCode::Up => self.selected = self.selected.checked_sub(1).unwrap_or(ROWS.len() - 1),
            KeyCode::Down => self.selected = (self.selected + 1) % ROWS.len(),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => return self.activate(1),
            KeyCode::Left => return self.activate(-1),
            _ => {}
        }
        SettingsOutcome::Stay
    }

    fn activate(&mut self, step: isize) -> SettingsOutcome {
        self.error = None;
        let s = self.current();
        if s.needs_provider() && self.values.provider.is_empty() {
            self.error = Some("choose a provider first".into());
            return SettingsOutcome::Stay;
        }
        let v = &self.values;
        let change = match s {
            Setting::Provider => Change::OpenLogin,
            Setting::Model => Change::ChooseModel,
            Setting::Effort => {
                let at = EFFORTS
                    .iter()
                    .position(|e| e.map(str::to_owned) == v.effort)
                    .unwrap_or(0) as isize;
                let next = (at + step).rem_euclid(EFFORTS.len() as isize) as usize;
                Change::Effort(EFFORTS[next].map(str::to_owned))
            }
            Setting::MaxTokens => {
                self.editing = Some(v.max_tokens.map(|n| n.to_string()).unwrap_or_default());
                return SettingsOutcome::Stay;
            }
            Setting::ContextWindow => {
                self.editing = Some(v.context_window.map(|n| n.to_string()).unwrap_or_default());
                return SettingsOutcome::Stay;
            }
            Setting::StatusLine => {
                self.editing = Some(v.status.clone());
                return SettingsOutcome::Stay;
            }
            Setting::MaxSteps => {
                self.editing = Some(v.max_steps.map(|n| n.to_string()).unwrap_or_default());
                return SettingsOutcome::Stay;
            }
            Setting::AutoCompact => Change::AutoCompact(!v.auto_compact),
            Setting::IdleCompact => Change::IdleCompact(!v.idle_compact),
            Setting::BackgroundCompact => Change::BackgroundCompact(!v.background_compact),
            Setting::ShowReasoning => Change::ShowReasoning(!v.show_reasoning),
            Setting::ExpandTools => Change::ExpandTools(!v.expand_tools),
            Setting::Permissions => Change::OpenPermissions,
            Setting::EditConfig => Change::EditConfig,
            Setting::EditPolicy => Change::EditPolicy,
        };
        SettingsOutcome::Apply(change)
    }

    fn finish_edit(&mut self) -> SettingsOutcome {
        let text = self.editing.take().unwrap_or_default();
        // The status line is text, not a number: it is checked when it is saved.
        if self.current() == Setting::StatusLine {
            let text = text.trim().to_owned();
            return SettingsOutcome::Apply(Change::StatusLine((!text.is_empty()).then_some(text)));
        }
        let number = if text.is_empty() {
            None
        } else {
            match text.parse::<u32>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    self.error = Some("enter a whole number above zero".into());
                    self.editing = Some(text);
                    return SettingsOutcome::Stay;
                }
            }
        };
        SettingsOutcome::Apply(match self.current() {
            Setting::MaxTokens => Change::MaxTokens(number),
            Setting::ContextWindow => Change::ContextWindow(number),
            _ => Change::MaxSteps(number.map(|n| n as usize)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            provider: "synthetic".into(),
            model: "hf:zai-org/GLM-5.3".into(),
            effort: None,
            max_tokens: None,
            context_window: None,
            window: 262_144,
            status: crate::tui::status::DEFAULT_STATUS.into(),
            max_steps: None,
            auto_compact: true,
            idle_compact: true,
            background_compact: true,
            show_reasoning: true,
            expand_tools: false,
            config_path: "~/.config/saddle/config.toml".into(),
            policy_path: "~/.config/saddle/policy.toml".into(),
        }
    }

    fn press(d: &mut SettingsDialog, code: KeyCode) -> SettingsOutcome {
        d.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn go_to(d: &mut SettingsDialog, s: Setting) {
        while d.current() != s {
            press(d, KeyCode::Down);
        }
    }

    #[test]
    fn the_request_limit_reads_as_unlimited_and_clears_to_it() {
        let mut d = SettingsDialog::new(values());
        go_to(&mut d, Setting::MaxSteps);
        assert_eq!(d.value(Setting::MaxSteps), "unlimited");
        press(&mut d, KeyCode::Enter);
        assert_eq!(d.editing.as_deref(), Some(""), "nothing to type over");
        for c in "12".chars() {
            d.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::MaxSteps(Some(12)))
        );
        // Emptying it asks for no limit again.
        go_to(&mut d, Setting::MaxSteps);
        press(&mut d, KeyCode::Enter);
        for _ in 0..2 {
            press(&mut d, KeyCode::Backspace);
        }
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::MaxSteps(None))
        );
    }

    #[test]
    fn the_status_line_is_edited_as_text_and_can_be_emptied() {
        let mut d = SettingsDialog::new(values());
        go_to(&mut d, Setting::StatusLine);
        assert!(
            d.value(Setting::StatusLine).contains("{cost}"),
            "shows what is set"
        );
        assert_eq!(press(&mut d, KeyCode::Enter), SettingsOutcome::Stay);
        assert!(d.editing.is_some(), "goes into an editing prompt");
        for _ in 0..d.editing.clone().unwrap().chars().count() {
            press(&mut d, KeyCode::Backspace);
        }
        for c in "{percent} of {window}".chars() {
            d.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::StatusLine(Some("{percent} of {window}".into())))
        );
        // An empty prompt asks for the default rather than for an empty line.
        go_to(&mut d, Setting::StatusLine);
        press(&mut d, KeyCode::Enter);
        for _ in 0..d.editing.clone().unwrap().chars().count() {
            press(&mut d, KeyCode::Backspace);
        }
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::StatusLine(None))
        );
    }

    #[test]
    fn cycles_toggles_and_numbers() {
        let mut d = SettingsDialog::new(values());
        go_to(&mut d, Setting::Effort);
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::Effort(Some("low".into())))
        );
        assert_eq!(
            press(&mut d, KeyCode::Left),
            SettingsOutcome::Apply(Change::Effort(Some("high".into())))
        );
        go_to(&mut d, Setting::MaxTokens);
        press(&mut d, KeyCode::Enter);
        for c in "8x000".chars() {
            press(&mut d, KeyCode::Char(c));
        }
        assert_eq!(d.editing.as_deref(), Some("8000"), "only digits are taken");
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::MaxTokens(Some(8000)))
        );
        go_to(&mut d, Setting::MaxSteps);
        press(&mut d, KeyCode::Enter);
        d.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::MaxSteps(None))
        );
        go_to(&mut d, Setting::ShowReasoning);
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::ShowReasoning(false))
        );
        assert_eq!(press(&mut d, KeyCode::Esc), SettingsOutcome::Close);
    }

    #[test]
    fn the_compaction_toggles_are_reachable_from_the_dialog() {
        // D77 and D78 are config keys a person should not have to know about to find.
        for (setting, want) in [
            (Setting::AutoCompact, Change::AutoCompact(false)),
            (Setting::IdleCompact, Change::IdleCompact(false)),
            (Setting::BackgroundCompact, Change::BackgroundCompact(false)),
        ] {
            let mut d = SettingsDialog::new(values());
            go_to(&mut d, setting);
            assert_eq!(d.value(setting), "on", "on by default");
            assert_eq!(press(&mut d, KeyCode::Enter), SettingsOutcome::Apply(want));
        }
        // And each says what it does, rather than only its name.
        assert!(Setting::IdleCompact.help().contains("half full"));
        assert!(Setting::BackgroundCompact.help().contains("three quarters"));
    }

    #[test]
    fn provider_settings_wait_for_a_provider() {
        let mut d = SettingsDialog::new(Values {
            provider: String::new(),
            ..values()
        });
        assert_eq!(d.value(Setting::Effort), "choose a provider first");
        go_to(&mut d, Setting::Effort);
        assert_eq!(press(&mut d, KeyCode::Enter), SettingsOutcome::Stay);
        assert!(d.error.is_some());
        go_to(&mut d, Setting::Provider);
        assert_eq!(
            press(&mut d, KeyCode::Enter),
            SettingsOutcome::Apply(Change::OpenLogin)
        );
    }
}
