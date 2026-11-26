use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::provider::ModelsError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub detail: String,
    pub status: String,
    pub needs_key: bool,
    pub has_key: bool,
    pub model: Option<String>,
    pub custom: bool,
    pub oauth: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub url: String,
    pub code: Option<String>,
    pub error: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub provider: String,
    pub key: Option<String>,
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
pub struct Pick {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub picks: Vec<Pick>,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Login,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Pick,
    Fill,
    Checking {
        id: u64,
    },
    SigningIn {
        id: u64,
    },
    Models {
        models: Vec<Pick>,
        filter: String,
        selected: usize,
        note: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
    Check(u64, Draft),
    ListAll(u64),
    Use(Pick),
    Default(Pick),
    Submit(LoginRequest),
    SignIn(u64, String),
    CancelSignIn(u64),
    OpenLink(String),
    CopyLink(String),
}

pub struct Login {
    pub choices: Vec<Choice>,
    pub selected: usize,
    pub stage: Stage,
    pub fields: Vec<Field>,
    pub at: usize,
    pub error: Option<String>,
    pub device: Device,
    pending: Option<Outcome>,
    purpose: Purpose,
    current: Option<String>,
    current_model: Option<String>,
    pub auth_path: String,
    direct: bool,
    builtin: Vec<String>,
    draft: Option<Draft>,
    next_id: u64,
}

pub const CUSTOM_LABEL: &str = "other OpenAI-compatible server";
pub const MODEL_ROWS: usize = 12;

impl Login {
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
            device: Device::default(),
            pending: None,
            purpose: Purpose::Login,
            current: None,
            current_model: None,
            auth_path,
            direct: false,
            builtin,
            draft: None,
            next_id: 0,
        };
        if direct && found.is_some() {
            login.direct = true;
            let outcome = login.choose();
            if outcome != Outcome::Stay {
                login.pending = Some(outcome);
            }
        }
        login
    }

    pub fn for_all_models(
        choices: Vec<Choice>,
        current: &str,
        current_model: &str,
        auth_path: String,
    ) -> Self {
        let mut login = Self::new(choices, Vec::new(), Some(current), false, auth_path);
        login.purpose = Purpose::Model;
        login.current = Some(current.to_owned());
        login.current_model = Some(current_model.to_owned()).filter(|m| !m.is_empty());
        login.next_id += 1;
        let id = login.next_id;
        login.stage = Stage::Checking { id };
        login.pending = Some(Outcome::ListAll(id));
        login
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    pub fn choice(&self) -> &Choice {
        &self.choices[self.selected]
    }

    pub fn in_force(&self) -> Option<(&str, &str)> {
        Some((
            self.current.as_deref()?,
            self.current_model.as_deref().unwrap_or_default(),
        ))
    }

    pub fn checking(&self) -> bool {
        matches!(self.stage, Stage::Checking { .. })
    }

    pub fn esc_closes(&self) -> bool {
        self.purpose == Purpose::Model
            || self.stage == Stage::Pick
            || (self.direct && self.stage == Stage::Fill && self.at == 0)
    }

    fn choose(&mut self) -> Outcome {
        if self.purpose == Purpose::Model {
            self.next_id += 1;
            let id = self.next_id;
            self.stage = Stage::Checking { id };
            self.error = None;
            return Outcome::ListAll(id);
        }
        if self.choices[self.selected].oauth {
            if self.choices[self.selected].has_key {
                return self.ask_models();
            }
            return match self.begin_sign_in() {
                Some((id, provider)) => Outcome::SignIn(id, provider),
                None => Outcome::Stay,
            };
        }
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
        Outcome::Stay
    }

    fn back(&mut self) {
        if self.fields.is_empty() {
            self.stage = Stage::Pick;
            self.device = Device::default();
        } else {
            self.stage = Stage::Fill;
        }
        self.error = None;
    }

    pub fn link(&self) -> Option<String> {
        let url = self.device.url.trim();
        (!url.is_empty()).then(|| url.to_owned())
    }

    pub fn is_signing_in(&self, id: u64) -> bool {
        self.stage == (Stage::SigningIn { id })
    }

    fn begin_sign_in(&mut self) -> Option<(u64, String)> {
        let c = self.choices.get(self.selected)?;
        if !c.oauth {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        let provider = c.name.clone();
        self.device = Device::default();
        self.stage = Stage::SigningIn { id };
        self.error = None;
        Some((id, provider))
    }

    fn ask_models(&mut self) -> Outcome {
        let draft = Draft {
            provider: self.choice().name.clone(),
            key: None,
            base_url: None,
        };
        self.next_id += 1;
        let id = self.next_id;
        self.stage = Stage::Checking { id };
        self.draft = Some(draft.clone());
        self.error = None;
        Outcome::Check(id, draft)
    }

    pub fn take_pending(&mut self) -> Option<Outcome> {
        self.pending.take()
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
        let current = self.current.clone().or_else(|| {
            self.draft
                .as_ref()
                .map(|draft| draft.provider.clone())
                .filter(|p| !p.is_empty())
        });
        match &mut self.stage {
            Stage::Pick => match key.code {
                KeyCode::Up => {
                    self.selected = self
                        .selected
                        .checked_sub(1)
                        .unwrap_or(self.choices.len().saturating_sub(1));
                }
                KeyCode::Down => self.selected = (self.selected + 1) % self.choices.len().max(1),
                KeyCode::Enter if !self.choices.is_empty() => return self.choose(),
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
                    if self.purpose == Purpose::Model {
                        return Outcome::Close;
                    }
                    self.back();
                }
            }
            Stage::SigningIn { id } => {
                let url = self.device.url.trim().to_owned();
                if !url.is_empty() {
                    match key.code {
                        KeyCode::Char('o') | KeyCode::Char('O') => {
                            return Outcome::OpenLink(url);
                        }
                        KeyCode::Char('c') | KeyCode::Char('C') => {
                            return Outcome::CopyLink(url);
                        }
                        _ => {}
                    }
                }
                if key.code == KeyCode::Esc {
                    let id = *id;
                    self.stage = Stage::Pick;
                    self.device = Device::default();
                    self.error = None;
                    return Outcome::CancelSignIn(id);
                }
            }
            Stage::Models {
                models,
                filter,
                selected,
                ..
            } => {
                let shown = matching(models, filter).len();
                let chosen = matches!(key.code, KeyCode::Enter | KeyCode::Char('s'))
                    .then(|| pick_of(models, filter, *selected, current.as_deref()))
                    .flatten();
                match key.code {
                    KeyCode::Esc if self.purpose == Purpose::Model => return Outcome::Close,
                    KeyCode::Esc => self.back(),
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
                    KeyCode::Char('s') if ctrl && self.purpose == Purpose::Model => {
                        if let Some(pick) = chosen {
                            return Outcome::Default(pick);
                        }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        filter.push(c);
                        *selected = 0;
                    }
                    KeyCode::Enter => {
                        let Some(pick) = chosen else {
                            return Outcome::Stay;
                        };
                        if self.purpose == Purpose::Model {
                            return Outcome::Use(pick);
                        }
                        let draft = self
                            .draft
                            .clone()
                            .filter(|draft| draft.provider == pick.provider);
                        return Outcome::Submit(LoginRequest {
                            provider: pick.provider,
                            key: draft.as_ref().and_then(|draft| draft.key.clone()),
                            model: pick.model,
                            base_url: draft.as_ref().and_then(|draft| draft.base_url.clone()),
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

    pub fn device_code(&mut self, id: u64, result: Result<(String, String), String>) {
        if self.stage != (Stage::SigningIn { id }) {
            return;
        }
        match result {
            Ok((url, code)) => {
                self.device.url = url;
                self.device.code = Some(code);
                self.device.error = None;
            }
            Err(why) => self.device.error = Some(why),
        }
    }

    pub fn signed_in(&mut self, id: u64, result: Result<(), String>) -> Option<(u64, Draft)> {
        if self.stage != (Stage::SigningIn { id }) {
            return None;
        }
        if let Err(why) = result {
            self.device.code = None;
            self.device.error = Some(why);
            return None;
        }
        self.device = Device::default();
        let draft = Draft {
            provider: self.choice().name.clone(),
            key: None,
            base_url: None,
        };
        self.next_id += 1;
        let check = self.next_id;
        self.stage = Stage::Checking { id: check };
        self.draft = Some(draft.clone());
        Some((check, draft))
    }

    pub fn for_model_unavailable(provider: &str, why: String, auth_path: String) -> Self {
        let mut login = Self::unavailable(provider, why, auth_path);
        login.purpose = Purpose::Model;
        login
    }

    fn unavailable(provider: &str, why: String, auth_path: String) -> Self {
        let choice = Choice {
            name: provider.to_owned(),
            detail: String::new(),
            status: String::new(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: false,
        };
        let mut login = Self::new(vec![choice], Vec::new(), None, false, auth_path);
        login.note_listing_failed(why);
        login
    }

    pub fn note_listing_failed(&mut self, why: String) {
        self.stage = Stage::Models {
            models: Vec::new(),
            filter: String::new(),
            selected: 0,
            note: Some(why),
        };
        self.error = None;
    }

    pub fn models(&mut self, id: u64, result: Result<Listed, ModelsError>) {
        if self.stage != (Stage::Checking { id }) {
            return;
        }
        match result {
            Ok(Listed {
                picks: models,
                failed,
            }) => {
                let selected = self
                    .current
                    .as_deref()
                    .zip(self.current_model.as_deref())
                    .and_then(|(provider, model)| {
                        models
                            .iter()
                            .position(|p| p.provider == provider && p.model == model)
                    })
                    .unwrap_or(0);
                let note = if models.is_empty() {
                    Some(if failed.is_empty() {
                        "nothing is served right now; type a model name".to_owned()
                    } else {
                        format!("{}; type a model name instead", failed.join("; "))
                    })
                } else if !failed.is_empty() {
                    Some(failed.join("; "))
                } else {
                    None
                };
                self.stage = Stage::Models {
                    models,
                    filter: String::new(),
                    selected,
                    note,
                };
            }
            Err(ModelsError::Rejected(why)) => {
                let why = format!("the key was rejected: {why}");
                if self.fields.is_empty() {
                    self.note_listing_failed(format!("{why}; type a model name"));
                    return;
                }
                self.stage = Stage::Fill;
                self.at = self
                    .fields
                    .iter()
                    .position(|f| f.kind == FieldKind::Key)
                    .unwrap_or(0);
                self.error = Some(why);
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

pub fn answer(picks: Vec<Pick>, failed: Vec<String>) -> Result<Listed, ModelsError> {
    if picks.is_empty() && !failed.is_empty() {
        return Err(ModelsError::Failed(failed.join("; ")));
    }
    Ok(Listed { picks, failed })
}

pub fn matching<'a>(models: &'a [Pick], filter: &str) -> Vec<&'a Pick> {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return models.iter().collect();
    }
    if let Some((provider, model)) = needle.split_once('/') {
        return models
            .iter()
            .filter(|p| {
                p.provider.to_lowercase().contains(provider)
                    && p.model.to_lowercase().contains(model)
            })
            .collect();
    }
    models
        .iter()
        .filter(|p| {
            p.model.to_lowercase().contains(&needle) || p.provider.to_lowercase().contains(&needle)
        })
        .collect()
}

fn pick_of(models: &[Pick], filter: &str, selected: usize, current: Option<&str>) -> Option<Pick> {
    if let Some(pick) = matching(models, filter).get(selected) {
        return Some((**pick).clone());
    }
    let model = filter.trim();
    if model.is_empty() {
        return None;
    }
    Some(Pick {
        provider: current?.to_owned(),
        model: model.to_owned(),
    })
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
            oauth: false,
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

    fn with_sign_in() -> Login {
        let mut list = choices();
        list.insert(
            1,
            Choice {
                name: "chatgpt".into(),
                detail: "with a ChatGPT plan".into(),
                status: "not signed in".into(),
                needs_key: false,
                has_key: false,
                model: None,
                custom: false,
                oauth: true,
            },
        );
        Login::new(list, vec![], None, false, "auth.json".into())
    }

    fn listed(provider: &str, models: &[&str]) -> Listed {
        Listed {
            picks: models
                .iter()
                .map(|m| Pick {
                    provider: provider.to_owned(),
                    model: (*m).to_owned(),
                })
                .collect(),
            failed: Vec::new(),
        }
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

        l.models(id, Err(ModelsError::Rejected("invalid api key".into())));
        assert_eq!(l.stage, Stage::Fill);
        assert!(l.error.as_deref().unwrap().contains("invalid api key"));

        let Outcome::Check(id, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a check")
        };
        l.models(id - 1, Ok(listed("openai", &["stale"])));
        assert!(l.checking(), "an old answer is ignored");
        l.models(id, Ok(listed("openai", &["gpt-new", "gpt-mini", "o-pro"])));
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
        l.models(id, Ok(listed("box", &["qwen"])));
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
    #[test]
    fn an_oauth_provider_signs_in_instead_of_asking_for_a_key() {
        let mut l = with_sign_in();
        press(&mut l, KeyCode::Down);
        assert_eq!(l.choice().name, "chatgpt");
        assert_eq!(l.stage, Stage::Pick);
        let outcome = press(&mut l, KeyCode::Enter);
        let Outcome::SignIn(id, provider) = outcome else {
            panic!("expected a sign-in, got {outcome:?}");
        };
        assert_eq!(provider, "chatgpt");
        assert_eq!(l.stage, Stage::SigningIn { id });
        assert!(l.fields.is_empty(), "an OAuth provider has no key field");
        assert_eq!(l.take_pending(), None);

        l.device_code(
            id,
            Ok(("https://auth.test/device".into(), "AB12-CD34".into())),
        );
        assert_eq!(l.device.url, "https://auth.test/device");
        assert_eq!(l.device.code.as_deref(), Some("AB12-CD34"));
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::CancelSignIn(id));
        assert_eq!(l.stage, Stage::Pick);
        assert_eq!(l.device, Device::default());
        let Outcome::SignIn(again, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a second sign-in");
        };
        assert_ne!(again, id, "an abandoned attempt's id must not be reused");
    }

    #[test]
    fn a_fan_out_that_answered_something_lists_it_and_says_what_is_missing() {
        let pick = |provider: &str, model: &str| Pick {
            provider: provider.to_owned(),
            model: model.to_owned(),
        };
        let some = answer(
            vec![pick("openai", "gpt-5.4"), pick("synthetic", "glm")],
            vec!["anthropic: no route to host".to_owned()],
        )
        .expect("the ones that answered are usable");
        assert_eq!(some.picks.len(), 2);
        assert_eq!(some.failed, ["anthropic: no route to host"]);

        let ModelsError::Failed(why) = answer(
            Vec::new(),
            vec!["openai: 401".to_owned(), "synthetic: timeout".to_owned()],
        )
        .unwrap_err() else {
            panic!("a total failure is a failure");
        };
        assert!(why.contains("openai: 401"), "{why}");
        assert!(why.contains("synthetic: timeout"), "{why}");

        let empty = answer(Vec::new(), Vec::new()).expect("not an error");
        assert!(empty.picks.is_empty() && empty.failed.is_empty());
    }

    #[test]
    fn the_filter_matches_a_provider_name_and_a_paired_name() {
        let picks = vec![
            Pick {
                provider: "openai".into(),
                model: "gpt-5.4".into(),
            },
            Pick {
                provider: "synthetic".into(),
                model: "gpt-5.4-turbo".into(),
            },
            Pick {
                provider: "synthetic".into(),
                model: "glm".into(),
            },
        ];
        let ids = |f: &str| {
            matching(&picks, f)
                .into_iter()
                .map(|p| p.model.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(""), ["gpt-5.4", "gpt-5.4-turbo", "glm"], "everything");
        assert_eq!(ids("openai"), ["gpt-5.4"], "by provider");
        assert_eq!(ids("SYNTH"), ["gpt-5.4-turbo", "glm"], "case folded");
        assert_eq!(ids("5.4"), ["gpt-5.4", "gpt-5.4-turbo"], "by model");
        assert_eq!(
            ids("openai/5.4"),
            ["gpt-5.4"],
            "a provider and a model together"
        );
        assert_eq!(ids("synthetic/gpt"), ["gpt-5.4-turbo"]);
    }

    #[test]
    fn the_model_picker_goes_straight_to_the_list_and_filters_as_you_type() {
        let mut list = choices();
        list[0].model = Some("synthetic-model".into());
        let mut l = Login::for_all_models(list, "openai", "synthetic-model", "auth.json".into());
        assert_eq!(l.purpose(), Purpose::Model);
        let pending = l.take_pending().expect("the listing to ask for");
        let Outcome::ListAll(id) = pending else {
            panic!("expected a listing of every provider, got {pending:?}");
        };
        assert_eq!(l.stage, Stage::Checking { id });
        assert!(l.fields.is_empty(), "nothing to type but the filter");

        l.models(id, Ok(listed("openai", &["gpt-5.4", "gpt-5.5", "o4-mini"])));
        type_text(&mut l, "5.5");
        assert_eq!(
            press(&mut l, KeyCode::Enter),
            Outcome::Use(Pick {
                provider: "openai".into(),
                model: "gpt-5.5".into(),
            })
        );
    }

    #[test]
    fn ctrl_s_chooses_the_same_entry_as_enter_and_says_it_is_the_default() {
        let mut l = Login::for_all_models(choices(), "openai", "m", "auth.json".into());
        let Some(Outcome::ListAll(id)) = l.take_pending() else {
            panic!("a listing to ask for");
        };
        l.models(id, Ok(listed("openai", &["a", "b"])));
        press(&mut l, KeyCode::Down);
        let chosen = Pick {
            provider: "openai".into(),
            model: "b".into(),
        };
        assert_eq!(press(&mut l, KeyCode::Enter), Outcome::Use(chosen.clone()));
        let save = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert_eq!(l.key(save), Outcome::Default(chosen));
        let typed = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(l.key(typed), Outcome::Stay);
        let Stage::Models { filter, .. } = &l.stage else {
            panic!("still a picker");
        };
        assert_eq!(filter, "s");
    }

    #[test]
    fn a_rejected_listing_leaves_the_picker_a_picker() {
        let mut l = Login::for_all_models(choices(), "openai", "m", "auth.json".into());
        let Some(Outcome::ListAll(id)) = l.take_pending() else {
            panic!("a listing to ask for");
        };
        l.models(id, Err(ModelsError::Rejected("401 nope".into())));
        let Stage::Models { note, models, .. } = &l.stage else {
            panic!("still a picker, got {:?}", l.stage);
        };
        assert!(models.is_empty());
        let note = note.as_deref().unwrap_or_default();
        assert!(note.contains("rejected"), "{note}");
        assert!(note.contains("type a model name"), "{note}");
        type_text(&mut l, "a-typed-model");
        assert_eq!(
            press(&mut l, KeyCode::Enter),
            Outcome::Use(Pick {
                provider: "openai".into(),
                model: "a-typed-model".into(),
            })
        );
    }

    #[test]
    fn a_picker_that_could_not_ask_what_is_served_says_so_on_the_list() {
        let mut l = Login::for_model_unavailable(
            "openai",
            "config.toml: expected an equals".into(),
            "auth.json".into(),
        );
        assert_eq!(l.purpose(), Purpose::Model);
        assert_eq!(l.choice().name, "openai");
        let Stage::Models { note, .. } = &l.stage else {
            panic!("a picker, got {:?}", l.stage);
        };
        assert!(
            note.as_deref()
                .unwrap_or_default()
                .contains("expected an equals")
        );
        assert_eq!(l.take_pending(), None, "nothing to ask for");
    }

    #[test]
    fn the_model_picker_closes_rather_than_backing_into_a_key_field() {
        let mut l = Login::for_all_models(choices(), "openai", "m", "auth.json".into());
        let Some(Outcome::ListAll(id)) = l.take_pending() else {
            panic!("a listing to ask for");
        };
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::Close);
        l.models(id, Ok(listed("openai", &["m"])));
        assert!(matches!(l.stage, Stage::Models { .. }));
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::Close);
        let mut l = Login::for_all_models(choices(), "openai", "m", "auth.json".into());
        let Some(Outcome::ListAll(id)) = l.take_pending() else {
            panic!("a listing to ask for");
        };
        l.models(id, Ok(Listed::default()));
        type_text(&mut l, "a-model-the-server-does-not-list");
        assert_eq!(
            press(&mut l, KeyCode::Enter),
            Outcome::Use(Pick {
                provider: "openai".into(),
                model: "a-model-the-server-does-not-list".into(),
            })
        );
    }

    #[test]
    fn o_opens_the_link_and_c_copies_it() {
        let mut l = with_sign_in();
        press(&mut l, KeyCode::Down);
        let Outcome::SignIn(id, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a sign-in");
        };
        assert_eq!(press(&mut l, KeyCode::Char('o')), Outcome::Stay);
        assert_eq!(press(&mut l, KeyCode::Char('c')), Outcome::Stay);
        l.device_code(id, Ok(("https://auth.test/device".into(), "AB12".into())));
        assert_eq!(
            press(&mut l, KeyCode::Char('o')),
            Outcome::OpenLink("https://auth.test/device".into())
        );
        assert_eq!(
            press(&mut l, KeyCode::Char('c')),
            Outcome::CopyLink("https://auth.test/device".into())
        );
        l.device_code(id, Ok((" https://auth.test/device ".into(), "AB12".into())));
        assert_eq!(
            l.link().as_deref(),
            Some("https://auth.test/device"),
            "whitespace is not part of a link"
        );
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::CancelSignIn(id));
    }

    #[test]
    fn an_approved_sign_in_goes_on_to_the_model_list() {
        let mut l = with_sign_in();
        press(&mut l, KeyCode::Down);
        let Outcome::SignIn(id, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a sign-in");
        };
        let (check, draft) = l.signed_in(id, Ok(())).expect("the model list follows");
        assert_eq!(draft.provider, "chatgpt");
        assert_eq!(draft.key, None);
        assert_eq!(l.stage, Stage::Checking { id: check });
        assert_eq!(l.signed_in(id, Ok(())), None);
        l.models(check, Ok(listed("chatgpt", &["gpt-5-codex", "gpt-5"])));
        assert!(matches!(l.stage, Stage::Models { selected: 0, .. }));
        let Outcome::Submit(req) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a submission");
        };
        assert_eq!(req.provider, "chatgpt");
        assert_eq!(req.model, "gpt-5-codex");
        assert_eq!(req.key, None);
    }

    #[test]
    fn a_failed_sign_in_stays_on_the_screen_that_failed() {
        let mut l = with_sign_in();
        press(&mut l, KeyCode::Down);
        let Outcome::SignIn(id, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a sign-in");
        };
        l.device_code(id, Ok(("https://auth.test/device".into(), "AB12".into())));
        assert_eq!(l.signed_in(id, Err("the code expired".into())), None);
        assert_eq!(l.stage, Stage::SigningIn { id });
        assert_eq!(l.device.code, None);
        assert_eq!(l.device.error.as_deref(), Some("the code expired"));
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::CancelSignIn(id));
        assert_eq!(l.stage, Stage::Pick);
    }

    #[test]
    fn a_dialog_opened_at_an_oauth_provider_owes_the_host_a_sign_in() {
        let mut list = choices();
        list.push(Choice {
            name: "chatgpt".into(),
            detail: String::new(),
            status: String::new(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: true,
        });
        let mut l = Login::new(list, vec![], Some("chatgpt"), true, "auth.json".into());
        assert!(matches!(l.stage, Stage::SigningIn { .. }));
        let pending = l.take_pending().expect("the sign-in the host must start");
        let Outcome::SignIn(id, provider) = pending else {
            panic!("expected a sign-in, got {pending:?}");
        };
        assert_eq!(provider, "chatgpt");
        assert_eq!(l.stage, Stage::SigningIn { id });
        assert_eq!(l.take_pending(), None, "started twice");

        let mut l = login(Some("openai"), true);
        assert_eq!(l.take_pending(), None);
        assert_eq!(l.stage, Stage::Fill);

        let mut list = choices();
        list.push(Choice {
            name: "chatgpt".into(),
            detail: String::new(),
            status: "signed in".into(),
            needs_key: false,
            has_key: true,
            model: Some("gpt-5-codex".into()),
            custom: false,
            oauth: true,
        });
        let mut l = Login::new(list, vec![], Some("chatgpt"), true, "auth.json".into());
        let pending = l.take_pending().expect("the model list to ask for");
        let Outcome::Check(id, draft) = pending else {
            panic!("expected a listing, got {pending:?}");
        };
        assert_eq!(draft.provider, "chatgpt");
        assert_eq!(draft.key, None, "a signed-in provider has no key");
        assert_eq!(l.stage, Stage::Checking { id });
    }
}
