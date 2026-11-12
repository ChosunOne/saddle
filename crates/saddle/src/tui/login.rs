//! The /login dialog: pick a provider, enter its key (or a new server's address), let the host
//! check the key by listing the provider's models, then pick a model.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::provider::ModelsError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    /// The model, or the server address, shown beside the name.
    pub detail: String,
    /// "logged in", "key from $VAR", "no key needed", or empty.
    pub status: String,
    /// The provider names a key variable, so it cannot run without a key.
    pub needs_key: bool,
    /// A key is already available, from the environment or the key store.
    pub has_key: bool,
    /// The configured model, selected first in the model list.
    pub model: Option<String>,
    /// The entry that adds a new OpenAI-compatible server.
    pub custom: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Name,
    BaseUrl,
    Key,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub kind: FieldKind,
    pub label: &'static str,
    pub value: String,
    pub optional: bool,
}

impl Field {
    fn new(kind: FieldKind, label: &'static str, value: &str, optional: bool) -> Self {
        Self {
            kind,
            label,
            value: value.to_owned(),
            optional,
        }
    }

    pub fn masked(&self) -> bool {
        self.kind == FieldKind::Key
    }
}

/// What the host needs to list models: the provider, and a key or address entered here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub provider: String,
    /// `None` keeps the key the provider already has, or uses none.
    pub key: Option<String>,
    /// Set for a new OpenAI-compatible server.
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginRequest {
    pub provider: String,
    pub key: Option<String>,
    pub model: String,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Pick,
    Fill,
    /// Waiting for the model list; `id` matches the answer to this request.
    Checking {
        id: u64,
    },
    Models {
        models: Vec<String>,
        filter: String,
        selected: usize,
        note: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
    /// List the models for this draft and answer with [`Login::models`].
    Check(u64, Draft),
    Submit(LoginRequest),
}

pub struct Login {
    pub choices: Vec<Choice>,
    pub selected: usize,
    pub stage: Stage,
    pub fields: Vec<Field>,
    pub at: usize,
    pub error: Option<String>,
    /// Where keys are stored, shown under the key field.
    pub auth_path: String,
    /// Opened at a provider's fields directly; esc there closes instead of going back.
    direct: bool,
    builtin: Vec<String>,
    draft: Option<Draft>,
    next_id: u64,
}

pub const CUSTOM_LABEL: &str = "other OpenAI-compatible server";
/// Rows of the model list shown at once.
pub const MODEL_ROWS: usize = 12;

impl Login {
    /// `preselect` picks a provider; with `direct`, the dialog opens at its fields.
    pub fn new(
        choices: Vec<Choice>,
        builtin: Vec<String>,
        preselect: Option<&str>,
        direct: bool,
        auth_path: String,
    ) -> Self {
        let found = preselect.and_then(|p| choices.iter().position(|c| !c.custom && c.name == p));
        let mut login = Self {
            choices,
            selected: found.unwrap_or(0),
            stage: Stage::Pick,
            fields: Vec::new(),
            at: 0,
            error: None,
            auth_path,
            direct: false,
            builtin,
            draft: None,
            next_id: 0,
        };
        if direct && found.is_some() {
            login.direct = true;
            login.choose();
        }
        login
    }

    pub fn choice(&self) -> &Choice {
        &self.choices[self.selected]
    }

    pub fn checking(&self) -> bool {
        matches!(self.stage, Stage::Checking { .. })
    }

    /// Esc closes the dialog rather than stepping back.
    pub fn esc_closes(&self) -> bool {
        self.stage == Stage::Pick || (self.direct && self.stage == Stage::Fill && self.at == 0)
    }

    fn choose(&mut self) {
        let c = &self.choices[self.selected];
        self.fields = if c.custom {
            vec![
                Field::new(FieldKind::Name, "Name for this server", "local", false),
                Field::new(
                    FieldKind::BaseUrl,
                    "Base URL",
                    "http://127.0.0.1:8080/v1",
                    false,
                ),
                Field::new(FieldKind::Key, "API key (enter to skip)", "", true),
            ]
        } else if c.has_key {
            vec![Field::new(
                FieldKind::Key,
                "API key (enter to keep the current one)",
                "",
                true,
            )]
        } else if c.needs_key {
            vec![Field::new(FieldKind::Key, "API key", "", false)]
        } else {
            vec![Field::new(
                FieldKind::Key,
                "API key (enter to skip)",
                "",
                true,
            )]
        };
        self.at = 0;
        self.stage = Stage::Fill;
        self.error = None;
    }

    /// With an optional, empty key field (a key is already stored), skips straight to listing
    /// the models.
    pub fn start_check(&mut self) -> Option<(u64, Draft)> {
        let skippable = self.stage == Stage::Fill
            && self.fields.len() == 1
            && self.fields[0].optional
            && self.fields[0].value.is_empty();
        match skippable.then(|| self.advance()) {
            Some(Outcome::Check(id, draft)) => Some((id, draft)),
            _ => None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Outcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Outcome::Close;
        }
        if key.code == KeyCode::Esc && self.esc_closes() {
            return Outcome::Close;
        }
        match &mut self.stage {
            Stage::Pick => match key.code {
                KeyCode::Up => {
                    self.selected = self
                        .selected
                        .checked_sub(1)
                        .unwrap_or(self.choices.len().saturating_sub(1));
                }
                KeyCode::Down => self.selected = (self.selected + 1) % self.choices.len().max(1),
                KeyCode::Enter if !self.choices.is_empty() => self.choose(),
                _ => {}
            },
            Stage::Fill => match key.code {
                KeyCode::Esc if self.at > 0 => {
                    self.at -= 1;
                    self.error = None;
                }
                KeyCode::Esc => {
                    self.stage = Stage::Pick;
                    self.error = None;
                }
                KeyCode::Enter => return self.advance(),
                KeyCode::Backspace => {
                    self.fields[self.at].value.pop();
                }
                KeyCode::Char('u') if ctrl => self.fields[self.at].value.clear(),
                KeyCode::Char(c) if !ctrl => self.fields[self.at].value.push(c),
                _ => {}
            },
            Stage::Checking { .. } => {
                if key.code == KeyCode::Esc {
                    self.stage = Stage::Fill;
                }
            }
            Stage::Models {
                models,
                filter,
                selected,
                ..
            } => {
                let shown = matching(models, filter).len();
                match key.code {
                    KeyCode::Esc => self.stage = Stage::Fill,
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(shown.saturating_sub(1)),
                    KeyCode::PageUp => *selected = selected.saturating_sub(MODEL_ROWS),
                    KeyCode::PageDown => {
                        *selected = (*selected + MODEL_ROWS).min(shown.saturating_sub(1));
                    }
                    KeyCode::Backspace => {
                        filter.pop();
                        *selected = 0;
                    }
                    KeyCode::Char('u') if ctrl => {
                        filter.clear();
                        *selected = 0;
                    }
                    KeyCode::Char(c) if !ctrl => {
                        filter.push(c);
                        *selected = 0;
                    }
                    KeyCode::Enter => {
                        let list = matching(models, filter);
                        let model = match list.get(*selected) {
                            Some(m) => (*m).clone(),
                            None if !filter.trim().is_empty() => filter.trim().to_owned(),
                            None => return Outcome::Stay,
                        };
                        let Some(draft) = self.draft.clone() else {
                            return Outcome::Stay;
                        };
                        return Outcome::Submit(LoginRequest {
                            provider: draft.provider,
                            key: draft.key,
                            model,
                            base_url: draft.base_url,
                        });
                    }
                    _ => {}
                }
            }
        }
        Outcome::Stay
    }

    pub fn paste(&mut self, text: &str) {
        let clean = text.chars().filter(|c| !c.is_control());
        match &mut self.stage {
            Stage::Fill => self.fields[self.at].value.extend(clean),
            Stage::Models {
                filter, selected, ..
            } => {
                filter.extend(clean);
                *selected = 0;
            }
            _ => {}
        }
    }

    fn advance(&mut self) -> Outcome {
        let field = &mut self.fields[self.at];
        field.value = field.value.trim().to_owned();
        let value = field.value.as_str();
        let problem = match field.kind {
            _ if value.is_empty() && !field.optional => {
                Some(format!("{} is required", field.label))
            }
            FieldKind::Name if !valid_name(value) => {
                Some("use lowercase letters, digits and dashes, starting with a letter".into())
            }
            FieldKind::Name if self.builtin.iter().any(|b| b == value) => {
                Some(format!("{value} is a built-in provider; pick another name"))
            }
            FieldKind::BaseUrl
                if !(value.starts_with("http://") || value.starts_with("https://")) =>
            {
                Some("the URL must start with http:// or https://".into())
            }
            _ => None,
        };
        if let Some(problem) = problem {
            self.error = Some(problem);
            return Outcome::Stay;
        }
        self.error = None;
        if self.at + 1 < self.fields.len() {
            self.at += 1;
            return Outcome::Stay;
        }
        let get = |kind| {
            self.fields
                .iter()
                .find(|f| f.kind == kind)
                .map(|f| f.value.clone())
                .filter(|v| !v.is_empty())
        };
        let c = self.choice();
        let draft = Draft {
            provider: if c.custom {
                get(FieldKind::Name).unwrap_or_default()
            } else {
                c.name.clone()
            },
            key: get(FieldKind::Key),
            base_url: get(FieldKind::BaseUrl),
        };
        self.next_id += 1;
        self.stage = Stage::Checking { id: self.next_id };
        self.draft = Some(draft.clone());
        Outcome::Check(self.next_id, draft)
    }

    /// The host's answer to [`Outcome::Check`]. Answers to an abandoned check are ignored.
    pub fn models(&mut self, id: u64, result: Result<Vec<String>, ModelsError>) {
        if self.stage != (Stage::Checking { id }) {
            return;
        }
        match result {
            Ok(models) => {
                let selected = self
                    .choice()
                    .model
                    .as_ref()
                    .and_then(|m| models.iter().position(|x| x == m))
                    .unwrap_or(0);
                let note = models
                    .is_empty()
                    .then(|| "the server listed no models; type a model name".to_owned());
                self.stage = Stage::Models {
                    models,
                    filter: String::new(),
                    selected,
                    note,
                };
            }
            Err(ModelsError::Rejected(why)) => {
                self.stage = Stage::Fill;
                self.at = self
                    .fields
                    .iter()
                    .position(|f| f.kind == FieldKind::Key)
                    .unwrap_or(0);
                self.error = Some(format!("the key was rejected: {why}"));
            }
            Err(ModelsError::Failed(why)) => {
                self.stage = Stage::Models {
                    models: Vec::new(),
                    filter: String::new(),
                    selected: 0,
                    note: Some(format!("couldn't list models ({why}); type a model name")),
                };
            }
        }
    }
}

/// Models whose id contains the filter, ignoring case.
pub fn matching<'a>(models: &'a [String], filter: &str) -> Vec<&'a String> {
    let needle = filter.trim().to_lowercase();
    models
        .iter()
        .filter(|m| m.to_lowercase().contains(&needle))
        .collect()
}

fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choices() -> Vec<Choice> {
        let c = |name: &str, needs_key, has_key, custom| Choice {
            name: name.into(),
            detail: String::new(),
            status: String::new(),
            needs_key,
            has_key,
            model: None,
            custom,
        };
        vec![
            c("openai", true, false, false),
            c("synthetic", true, true, false),
            c("local", false, false, false),
            c(CUSTOM_LABEL, false, false, true),
        ]
    }

    fn login(preselect: Option<&str>, direct: bool) -> Login {
        Login::new(
            choices(),
            vec!["openai".into(), "synthetic".into()],
            preselect,
            direct,
            "auth.json".into(),
        )
    }

    fn press(l: &mut Login, code: KeyCode) -> Outcome {
        l.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn type_text(l: &mut Login, s: &str) {
        for c in s.chars() {
            press(l, KeyCode::Char(c));
        }
    }

    fn clear(l: &mut Login) {
        l.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    }

    #[test]
    fn key_then_model_list_then_submit() {
        let mut l = login(None, false);
        press(&mut l, KeyCode::Enter);
        assert_eq!(press(&mut l, KeyCode::Enter), Outcome::Stay);
        assert_eq!(l.error.as_deref(), Some("API key is required"));
        l.paste(" sk-abc\n");
        let Outcome::Check(id, draft) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a check")
        };
        assert_eq!(draft.key.as_deref(), Some("sk-abc"));
        assert!(l.checking());

        // A rejected key goes back to the key field with the reason.
        l.models(id, Err(ModelsError::Rejected("invalid api key".into())));
        assert_eq!(l.stage, Stage::Fill);
        assert!(l.error.as_deref().unwrap().contains("invalid api key"));

        let Outcome::Check(id, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a check")
        };
        l.models(id - 1, Ok(vec!["stale".into()]));
        assert!(l.checking(), "an old answer is ignored");
        l.models(
            id,
            Ok(vec!["gpt-new".into(), "gpt-mini".into(), "o-pro".into()]),
        );
        type_text(&mut l, "gpt");
        press(&mut l, KeyCode::Down);
        press(&mut l, KeyCode::Down);
        assert_eq!(
            press(&mut l, KeyCode::Enter),
            Outcome::Submit(LoginRequest {
                provider: "openai".into(),
                key: Some("sk-abc".into()),
                model: "gpt-mini".into(),
                base_url: None,
            })
        );
    }

    #[test]
    fn a_kept_key_and_a_typed_model_name() {
        let mut l = login(Some("synthetic"), true);
        assert_eq!(l.stage, Stage::Fill);
        let Outcome::Check(id, draft) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a check")
        };
        assert_eq!(draft.key, None, "enter keeps the current key");
        l.models(id, Err(ModelsError::Failed("404".into())));
        type_text(&mut l, "hf:zai-org/GLM-5.3");
        assert_eq!(
            press(&mut l, KeyCode::Enter),
            Outcome::Submit(LoginRequest {
                provider: "synthetic".into(),
                key: None,
                model: "hf:zai-org/GLM-5.3".into(),
                base_url: None,
            })
        );
    }

    #[test]
    fn a_new_server_is_validated() {
        let mut l = login(None, false);
        press(&mut l, KeyCode::Up);
        press(&mut l, KeyCode::Enter);
        clear(&mut l);
        type_text(&mut l, "openai");
        press(&mut l, KeyCode::Enter);
        assert!(l.error.as_deref().unwrap().contains("built-in"));
        clear(&mut l);
        type_text(&mut l, "box");
        press(&mut l, KeyCode::Enter);
        clear(&mut l);
        type_text(&mut l, "localhost:8080");
        press(&mut l, KeyCode::Enter);
        assert!(l.error.as_deref().unwrap().contains("http://"));
        clear(&mut l);
        type_text(&mut l, "http://box:8080/v1");
        press(&mut l, KeyCode::Enter);
        let Outcome::Check(id, draft) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a check")
        };
        assert_eq!(
            draft,
            Draft {
                provider: "box".into(),
                key: None,
                base_url: Some("http://box:8080/v1".into()),
            }
        );
        l.models(id, Ok(vec!["qwen".into()]));
        let Outcome::Submit(req) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a submit")
        };
        assert_eq!(req.model, "qwen");
        assert_eq!(req.base_url.as_deref(), Some("http://box:8080/v1"));
    }

    #[test]
    fn escape_steps_back_or_closes() {
        let mut l = login(Some("openai"), true);
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::Close);
        let mut l = login(None, false);
        press(&mut l, KeyCode::Enter);
        press(&mut l, KeyCode::Esc);
        assert_eq!(l.stage, Stage::Pick);
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::Close);
    }
}
