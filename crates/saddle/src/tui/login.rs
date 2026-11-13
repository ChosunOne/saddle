//! The /login dialog: pick a provider, enter its key or sign in through the browser, let the host
//! check the credential by listing the provider's models, then pick a model.
//!
//! A provider that authenticates with OAuth has no key to type, so picking it starts a device
//! sign-in instead: the code and the address go on screen, the host polls in the background, and the
//! model list follows the approval (D85). The dialog never does the network itself, so the
//! interface stays live while a poll is in flight.

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
    /// This provider signs in with OAuth, so it has no key to type (D85).
    pub oauth: bool,
}

/// A device sign-in as the dialog shows it: where to go, the code to type, and what went wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub url: String,
    /// `None` until the host has asked the server for one.
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

/// One model of one provider, as the picker lists them (D89).
///
/// The id alone is not enough once the list spans providers: two of them may serve a model with the
/// same name, and choosing an entry has to say which provider it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub provider: String,
    pub model: String,
}

/// What a listing answered, and which providers it could not ask (D89).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub picks: Vec<Pick>,
    /// One line per provider that failed, so a partial list says what is missing from it.
    pub failed: Vec<String>,
}

/// Why the dialog is open: to sign in, or to choose the model of a provider already set up (D88).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Login,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Pick,
    Fill,
    /// Waiting for the model list; `id` matches the answer to this request.
    Checking {
        id: u64,
    },
    /// Waiting for a device sign-in to be approved; `id` matches this attempt (D85).
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
    /// List the models for this draft and answer with [`Login::models`].
    Check(u64, Draft),
    /// List what every provider serves and answer with [`Login::models`] (D89).
    ListAll(u64),
    /// Use this provider and model in this conversation only (D91).
    Use(Pick),
    /// Use it, and make it what a new session starts with: both the provider and the model (D91).
    Default(Pick),
    /// The login flow, where the credential and the model entered are the new default (D85).
    Submit(LoginRequest),
    /// Start a device sign-in for this provider (D85).
    SignIn(u64, String),
    /// That sign-in was abandoned, so the host should stop polling for it (D85).
    CancelSignIn(u64),
    /// Open this link in the person's browser (D86).
    OpenLink(String),
    /// Put this link on the clipboard (D86).
    CopyLink(String),
}

pub struct Login {
    pub choices: Vec<Choice>,
    pub selected: usize,
    pub stage: Stage,
    pub fields: Vec<Field>,
    pub at: usize,
    pub error: Option<String>,
    /// The sign-in on screen while `Stage::SigningIn` waits.
    pub device: Device,
    /// What the host owes this dialog, when it was opened straight at a provider: a sign-in to
    /// start, or the model list to ask for (D85).
    pending: Option<Outcome>,
    /// Signing in, or only picking a model (D88).
    purpose: Purpose,
    /// The provider in force when the picker opened, so a model typed rather than chosen goes to
    /// the provider the session is already using (D89).
    current: Option<String>,
    /// That provider's model, which the title names and the highlight starts on (D89).
    current_model: Option<String>,
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
            // A constructor cannot start anything, so what the choice asks for is left for the
            // host, which takes it straight after opening the dialog.
            let outcome = login.choose();
            if outcome != Outcome::Stay {
                login.pending = Some(outcome);
            }
        }
        login
    }

    /// The picker over every provider at once (D89).
    ///
    /// `/model` has to answer "what can I switch to", and on a machine with more than one provider
    /// that is not one list: the subscription serves models the API key does not, and each names
    /// them differently. So the list is the union, every entry carrying its provider, and choosing
    /// one switches both. `current` is the provider in force, which is what a typed name means and
    /// whose model is preselected.
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

    /// The provider and model in force, which a typed name is relative to and the title names
    /// (D89).
    pub fn in_force(&self) -> Option<(&str, &str)> {
        Some((
            self.current.as_deref()?,
            self.current_model.as_deref().unwrap_or_default(),
        ))
    }

    pub fn checking(&self) -> bool {
        matches!(self.stage, Stage::Checking { .. })
    }

    /// Esc closes the dialog rather than stepping back.
    pub fn esc_closes(&self) -> bool {
        self.purpose == Purpose::Model
            || self.stage == Stage::Pick
            || (self.direct && self.stage == Stage::Fill && self.at == 0)
    }

    fn choose(&mut self) -> Outcome {
        // Picking a model for a provider that is already set up: there is nothing to type, so the
        // listing is the only question (D88).
        if self.purpose == Purpose::Model {
            self.next_id += 1;
            let id = self.next_id;
            self.stage = Stage::Checking { id };
            self.error = None;
            return Outcome::ListAll(id);
        }
        if self.choices[self.selected].oauth {
            // Already signed in: there is nothing to type and nothing to wait for, so the model
            // list is the only question left (D85).
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

    /// Where esc goes from a stage the credential was needed for: the fields, or the list.
    ///
    /// An OAuth provider has no fields, so going back to them would leave the next keystroke
    /// indexing an empty list.
    fn back(&mut self) {
        if self.fields.is_empty() {
            self.stage = Stage::Pick;
            self.device = Device::default();
        } else {
            self.stage = Stage::Fill;
        }
        self.error = None;
    }

    /// The address to type the code at, once the server has named one.
    pub fn link(&self) -> Option<String> {
        let url = self.device.url.trim();
        (!url.is_empty()).then(|| url.to_owned())
    }

    /// Whether this attempt is the one on screen, so a stale answer is not acted on (D85).
    pub fn is_signing_in(&self, id: u64) -> bool {
        self.stage == (Stage::SigningIn { id })
    }

    /// Moves to the sign-in stage and answers with what the host must start (D85).
    ///
    /// Marking it asked is what keeps a second call from polling a second time: the host asks once
    /// when the dialog opens at a provider directly, and the key handler answers once when a person
    /// picks one from the list.
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

    /// Moves to the model list for this provider's already-stored credential.
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

    /// What the host owes the dialog, when it was opened straight at a provider: taken once, so a
    /// sign-in is not started twice and a listing is not asked for twice.
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
        // Read before the stage is borrowed mutably. A typed name belongs to the provider in force
        // in the picker, or — in the login flow, where the list is one provider's — to the provider
        // the draft is for (D89).
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
                    // A picker has nothing behind it to step back to (D88).
                    if self.purpose == Purpose::Model {
                        return Outcome::Close;
                    }
                    self.back();
                }
            }
            Stage::SigningIn { id } => {
                // The two things a person needs from this screen: the link opened, and the link
                // copied. The code is already on the clipboard by now (D86).
                // Read from `device` rather than through `link()`, because `stage` is borrowed
                // mutably by the match this arm is in.
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
                // Back to the list, which is where a failed or abandoned sign-in is retried from.
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
                // Worked out before the match, so both keys agree on what is chosen: ctrl+s is
                // enter plus "and remember it", never a different entry (D91).
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
                    // The one key that changes what a later session starts with: everything else
                    // here is a choice about this conversation (D91). Only in the picker: the
                    // login flow's list carries the credential that was just entered, and
                    // submitting it is the whole reason it opened.
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
                        // `/model` chooses for the conversation. It used to write the choice into
                        // config.toml, which silently moved every later session too (D91).
                        if self.purpose == Purpose::Model {
                            return Outcome::Use(pick);
                        }
                        // The key and address entered for a login belong to the provider they were
                        // entered for, and only to that one: the picker's own list has neither, and
                        // taking one from a draft for another provider would save a credential to
                        // the wrong place (D89).
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

    /// The host's answer for a started sign-in: where to go, and the code to type there (D85).
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

    /// The host's answer when the sign-in ended: an approved one goes on to the model list.
    ///
    /// There is no key to check afterwards, so the only question left is which model to use, and the
    /// listing is asked for with the token that was just stored.
    pub fn signed_in(&mut self, id: u64, result: Result<(), String>) -> Option<(u64, Draft)> {
        if self.stage != (Stage::SigningIn { id }) {
            return None;
        }
        if let Err(why) = result {
            // The failure is shown where the code was, so what failed and what to do about it are
            // on one screen; esc goes back to the list to try again (D85).
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

    /// The same screen, opened without a listing, saying why it could not be asked (D88).
    ///
    /// A picker must always appear: the alternative is a note in the transcript, which a dialog
    /// covers, so the command looks like it did nothing at all.
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

    /// Say on the list that it could not be fetched, keeping the screen a picker (D88).
    pub fn note_listing_failed(&mut self, why: String) {
        self.stage = Stage::Models {
            models: Vec::new(),
            filter: String::new(),
            selected: 0,
            note: Some(why),
        };
        self.error = None;
    }

    /// The host's answer to [`Outcome::Check`]. Answers to an abandoned check are ignored.
    pub fn models(&mut self, id: u64, result: Result<Listed, ModelsError>) {
        if self.stage != (Stage::Checking { id }) {
            return;
        }
        match result {
            Ok(Listed {
                picks: models,
                failed,
            }) => {
                // The model in force is where the highlight starts, matched on the provider too: the
                // same id can be served by two of them (D89).
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
                    // A partial list says what is missing from it, or a provider that is down looks
                    // like one that serves nothing (D89).
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
                // A picker has no key field to send anyone back to, so the reason goes on the list
                // and a name can still be typed (D88).
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

/// What a fan-out answered, as the dialog takes it (D89).
///
/// Nothing at all, with failures to explain it, is a failure: a picker that says "no models" without
/// saying that three providers are unreachable is a picker that looks broken. Some models with some
/// failures is a list with a note, because the ones that answered are usable.
pub fn answer(picks: Vec<Pick>, failed: Vec<String>) -> Result<Listed, ModelsError> {
    if picks.is_empty() && !failed.is_empty() {
        return Err(ModelsError::Failed(failed.join("; ")));
    }
    Ok(Listed { picks, failed })
}

/// The entries the filter keeps, ignoring case (D89).
///
/// A model is matched on its own id and on its provider's name, so typing `openai` narrows the union
/// to one provider — which is how a list that spans providers is read. `provider/model` matches the
/// pair, for the case where a name alone is ambiguous between two of them.
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

/// What enter would choose in the model list: the highlighted entry, or the name typed when
/// nothing lists it (D89).
///
/// A name nothing serves goes to the provider in force, which is the one a person is switching when
/// they type a name at all; with no provider in force there is nothing to send it to.
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

    /// The same list with an OAuth provider in it, which is the one that has no key (D85).
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

    /// A listing answer for one provider, which is what the login flow gets.
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

        // A rejected key goes back to the key field with the reason.
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
        // D85: there is no key to type, so picking it must start the sign-in rather than show a
        // field that expects one.
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
        // Picking it from the list answers the host directly: the constructor has nothing pending.
        assert_eq!(l.take_pending(), None);

        // The code arriving is what the screen shows, and esc is the way out of it.
        l.device_code(
            id,
            Ok(("https://auth.test/device".into(), "AB12-CD34".into())),
        );
        assert_eq!(l.device.url, "https://auth.test/device");
        assert_eq!(l.device.code.as_deref(), Some("AB12-CD34"));
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::CancelSignIn(id));
        assert_eq!(l.stage, Stage::Pick);
        assert_eq!(l.device, Device::default());
        // Back on the list it can be picked again, which is a new attempt.
        let Outcome::SignIn(again, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a second sign-in");
        };
        assert_ne!(again, id, "an abandoned attempt's id must not be reused");
    }

    #[test]
    fn a_fan_out_that_answered_something_lists_it_and_says_what_is_missing() {
        // D89: the picker spans providers, so one of them being down must not look like one that
        // serves nothing, and a total failure must not look like an empty list.
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

        // Nothing at all, and a reason, is a failure that names the reasons.
        let ModelsError::Failed(why) = answer(
            Vec::new(),
            vec!["openai: 401".to_owned(), "synthetic: timeout".to_owned()],
        )
        .unwrap_err() else {
            panic!("a total failure is a failure");
        };
        assert!(why.contains("openai: 401"), "{why}");
        assert!(why.contains("synthetic: timeout"), "{why}");

        // Nothing to list and nothing wrong is an empty list, which the dialog words itself.
        let empty = answer(Vec::new(), Vec::new()).expect("not an error");
        assert!(empty.picks.is_empty() && empty.failed.is_empty());
    }

    #[test]
    fn the_filter_matches_a_provider_name_and_a_paired_name() {
        // D89: a union list is narrowed by provider as often as by model, and `provider/model` is
        // how an id two providers share is disambiguated.
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
        // D88: `/model` with no name must not need the name. The list is the dialog, typing is the
        // filter, and choosing submits without a key.
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
        // Typing narrows the list; enter takes what is highlighted, for this conversation only
        // (D91).
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
        // D91: `/model` used to write the choice into config.toml, so changing the model here moved
        // every later session too. Both keys are now the same entry, and only ctrl+s is a default.
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
        // A plain `s` is still a letter in the filter, not a key.
        let typed = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE);
        assert_eq!(l.key(typed), Outcome::Stay);
        let Stage::Models { filter, .. } = &l.stage else {
            panic!("still a picker");
        };
        assert_eq!(filter, "s");
    }

    #[test]
    fn a_rejected_listing_leaves_the_picker_a_picker() {
        // D88: a rejected listing sent a picker into the field stage, which it has no fields for —
        // an empty box with nothing to type in. The reason belongs on the list, where a name can
        // still be typed instead.
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
        // And a name typed there still works, which is the way out.
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
        // D88: the command must always open something. A note in the transcript is invisible while
        // a dialog is what the person is waiting for, which reads as the command doing nothing.
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
        // It was opened for a model, so there is nothing behind it: esc closes from the wait and
        // from the list, and never lands on a field asking for a key that already exists (D88).
        let mut l = Login::for_all_models(choices(), "openai", "m", "auth.json".into());
        let Some(Outcome::ListAll(id)) = l.take_pending() else {
            panic!("a listing to ask for");
        };
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::Close);
        l.models(id, Ok(listed("openai", &["m"])));
        assert!(matches!(l.stage, Stage::Models { .. }));
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::Close);
        // A name typed that the server does not list is still usable, which is the way out when a
        // listing is empty.
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
        // D86: the address is what the person needs, and a dialog's text is not selectable, so both
        // ways of getting it out are keys rather than a mouse drag.
        let mut l = with_sign_in();
        press(&mut l, KeyCode::Down);
        let Outcome::SignIn(id, _) = press(&mut l, KeyCode::Enter) else {
            panic!("expected a sign-in");
        };
        // Before there is an address there is nothing to open or copy, and esc still cancels.
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
        // The address pasted in with a trailing space is still the address.
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
        // No key: the token the sign-in stored is what the listing is sent with.
        assert_eq!(draft.key, None);
        assert_eq!(l.stage, Stage::Checking { id: check });
        // The answer is matched by id, so a stale one does nothing.
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
        // Still signing in, with the reason where the code was, so esc is one key away.
        assert_eq!(l.stage, Stage::SigningIn { id });
        assert_eq!(l.device.code, None);
        assert_eq!(l.device.error.as_deref(), Some("the code expired"));
        assert_eq!(press(&mut l, KeyCode::Esc), Outcome::CancelSignIn(id));
        assert_eq!(l.stage, Stage::Pick);
    }

    #[test]
    fn a_dialog_opened_at_an_oauth_provider_owes_the_host_a_sign_in() {
        // The host asks after opening, because a constructor has nothing to answer to. It must be
        // told exactly once, or a provider chosen from a menu would poll twice.
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

        // A provider with a key is still a key field, and asks for nothing.
        let mut l = login(Some("openai"), true);
        assert_eq!(l.take_pending(), None);
        assert_eq!(l.stage, Stage::Fill);

        // One already signed in has no key to type and nothing to wait for, so the model list is
        // the only question left and that is what the host is asked for (D85).
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
