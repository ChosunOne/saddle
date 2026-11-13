//! TUI state and input handling. Nothing here touches the terminal, so it is tested directly.

use std::collections::VecDeque;
use std::path::PathBuf;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use super::editor::Editor;
use super::login::{Draft, Login, LoginRequest, Outcome};
use super::sessions::{PickOutcome, SessionPicker};
use super::settings::{Change, SettingsDialog, SettingsOutcome};
use crate::agent::call_detail;
use crate::agent::{End, Resumed};
use crate::provider::ModelsError;
use crate::provider::Usage;
use crate::provider::{Item, Part};
use crate::term::clean;

/// Sent by the agent thread.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Text(String),
    /// Reasoning from a thinking model, shown dimmed above its answer.
    Reasoning(String),
    Note(String),
    Call {
        name: String,
        detail: String,
    },
    Result {
        ok: bool,
        content: String,
        diff: Option<String>,
        /// Pictures the call returned, such as a screenshot `read` was asked for (D58).
        images: Vec<crate::provider::Image>,
    },
    /// A reply: what it used, and what it cost (D36).
    Reply {
        usage: Usage,
        cost: f64,
    },
    /// What the provider said about the allowance left (D36).
    Quota(crate::provider::Quota),
    /// The skills a re-discovery found, for the `/` menu (D70).
    Skills(Vec<crate::skills::Skill>),
    TurnEnd(End),
    /// The host failed (for example, the audit log could not be written). The session is over.
    Fatal(String),
    /// The agent thread closed the session and exited.
    Closed,
    /// The answer to a model listing started by the login dialog.
    Models(u64, Result<Vec<String>, ModelsError>),
    /// The code to type for a device sign-in: where to go, and what to enter there (D85).
    DeviceCode(u64, Result<(String, String), String>),
    /// That sign-in finished, or failed with a reason to show (D85).
    SignedIn(u64, Result<(), String>),
    /// A listing asked for at session start stated a window for the current model, so the session
    /// can drop the guess it opened with (D34). The answer is in the model cache by now.
    Listed,
    /// A saved session was loaded: its items, what it has already spent, and the tokens its next
    /// request would carry (D35).
    Resumed(Resumed),
    /// The branch as rows, for `/tree` (D54).
    Tree(Vec<crate::store::Point>),
    /// Images attached to the prompt just sent, to show beside it (D58).
    Attached(Vec<crate::provider::Image>),
    Compaction(crate::compact::Event),
}

/// A change to the reasoning effort (D57).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effort {
    /// Move to the next level.
    Cycle,
    /// Set this level, or `None` for the provider's default.
    Set(Option<String>),
}

/// What the controller must do after an input or agent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send(String),
    /// A prompt built from a skill by `/skill:name`, and the skill it came from (D69).
    Skill {
        name: String,
        prompt: String,
    },
    /// Read the skills and instruction files again, and the settings with them (D70).
    Reload,
    /// Show the theme, or switch to the named one and save it (D71).
    Theme(Option<String>),
    Clear,
    Cancel,
    Quit,
    OpenLogin,
    /// Start a device sign-in for this provider, answering with [`AgentEvent::DeviceCode`] and
    /// then [`AgentEvent::SignedIn`] (D85).
    SignIn(u64, String),
    /// List the draft's models, answering with [`AgentEvent::Models`].
    CheckModels(u64, Draft),
    Login(LoginRequest),
    /// Remove the stored key of the named provider, or of the current one.
    Logout(Option<String>),
    /// Show the model, or switch to the named one.
    Model(Option<String>),
    /// Settle the context window again now that the endpoint has answered.
    SettleWindow,
    /// One change from the permissions editor.
    Grant(crate::tui::permissions::Change),
    /// Open the permissions editor (D45).
    OpenPermissions,
    /// Show the conversation tree, to continue from an earlier message (D54).
    OpenTree,
    /// Move the conversation to a point, keeping what came after it in the store (D54).
    Rewind(Option<i64>),
    /// Copy this conversation into a new one and continue that (D53).
    Clone,
    /// Give this conversation a name, or clear it (D55).
    Name(Option<String>),
    /// What this conversation has done, for `/session` (D55).
    SessionInfo,
    /// Remove a saved conversation (D55).
    DeleteSession(String),
    /// Set the reasoning effort, or cycle to the next level (D57).
    Effort(Effort),
    /// Put the selected text on the terminal's clipboard; the event loop owns the terminal.
    /// Put this text on the clipboard, and say what it was (D86).
    Copy(String, &'static str),
    /// Open this link in the person's own browser (D86).
    OpenUrl(String),
    OpenResume,
    /// Continue a saved conversation, by its id in the session store (D53).
    Resume(String),
    OpenSettings,
    Apply(Change),
    /// A `!` command: run it in the sandbox; `share` puts it and its output in the conversation.
    Shell {
        command: String,
        share: bool,
    },
    /// Open the prompt in `$EDITOR`.
    EditPrompt,
    /// Summarize the conversation so far, with optional instructions.
    Compact(Option<String>),
}

/// A point in the transcript: a visual line, and a character index within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub line: usize,
    pub ch: usize,
}

/// A mouse selection (D35). `anchor` is where the drag began and `focus` where it has reached;
/// `dragging` outlives the button so the highlight stays after the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Cell,
    pub focus: Cell,
    pub dragging: bool,
}

impl Selection {
    /// The two ends, first to last, so a drag in either direction selects the same text.
    pub fn ends(&self) -> (Cell, Cell) {
        let (a, b) = (self.anchor, self.focus);
        if (a.line, a.ch) <= (b.line, b.ch) {
            (a, b)
        } else {
            (b, a)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.focus
    }
}

/// The transcript as the last draw left it: where it sat, which line was at the top, and the text
/// of each visual line, so that a click maps to a character and a copy takes exactly what the
/// screen showed (D35).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub area: Rect,
    pub top: usize,
    pub lines: Vec<String>,
}

/// One image in the transcript, next to the text that announces it (D59).
#[derive(Debug, Clone)]
pub struct Placed {
    /// The transcript line the picture starts at.
    pub line: usize,
    /// The id the terminal knows it by, so it can be taken back off the screen (D59).
    pub id: u32,
    /// The picture's full size: cells, and pixels.
    pub box_: crate::tui::image::Fit,
    pub pixels: (u32, u32),
    /// The encoded file, kept so the escape can be built for whatever band is on screen (D59).
    pub png: Vec<u8>,
}

/// What the last frame put on the screen, so the next one can take it back off (D59).
///
/// Kitty images do not scroll with the text: the terminal keeps them at the cell they were drawn
/// on, and ratatui only rewrites characters. Drawing a scrolled picture without erasing the old one
/// leaves copies of it all over the screen, which is what scrolling used to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drawn {
    /// What is on screen: the row each picture was drawn on, its id, and which band of it.
    pub shown: Vec<(usize, u32, u16, u16)>,
    /// Where the transcript area started, since a resize moves every row.
    pub at: Option<(u16, u16)>,
}

/// How long two escapes may be apart and still count as one gesture (D54).
pub const DOUBLE_ESCAPE: std::time::Duration = std::time::Duration::from_millis(500);

/// How long "copied …" stays in the activity line.
pub const COPY_NOTE: std::time::Duration = std::time::Duration::from_secs(3);

/// A list shown above the editor while typing: `/` commands or `@` files.
pub struct Popup {
    pub items: Vec<(String, String)>,
    pub selected: usize,
    pub hint: &'static str,
}

/// Files the `@` picker offers, at most.
const PICKER_FILES: usize = 20_000;
/// The picker's file list is reread when it is older than this.
const PICKER_REFRESH: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    Assistant(String),
    /// A thinking model's reasoning.
    Thinking(String),
    Tool {
        name: String,
        detail: String,
        /// `None` while running.
        result: Option<(bool, String)>,
        /// The file change, for edit and write.
        diff: Option<String>,
    },
    Note(NoteKind, String),
    /// An image attached to the prompt above it (D58). Drawn where the terminal can, and as this
    /// entry's text everywhere else (D59).
    Image(crate::provider::Image),
    /// A compaction, streamed as it runs, or a summary loaded with a resumed session.
    Compaction(Box<CompactionView>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionView {
    pub header: String,
    pub reasoning: String,
    pub summary: String,
    pub state: CompactState,
    /// Whether it stopped the turn. A background one does not, so it never claims the activity
    /// line, not even when it lands (D78).
    pub blocks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactState {
    Running,
    /// With a line saying what was summarized and kept.
    Done(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Waiting,
    Thinking,
    Compacting,
    Writing,
    Tool(String),
}

pub struct Header {
    pub provider: String,
    pub model: String,
    pub workspace: PathBuf,
    pub audit: PathBuf,
    /// Shown once under the intro, such as files saddle created on first run.
    pub notes: Vec<String>,
    /// The model's context window, for the status line.
    pub context_window: u64,
    /// Whether the model has a price, so `{cost}` can show `$0.00` rather than nothing (D36).
    pub priced: bool,
    /// The status line, parsed from `[ui] status` (D36).
    pub status: crate::tui::status::Template,
    /// Whether the terminal's mouse is reported to saddle. Off by default so a drag selects text
    /// in the terminal itself (D35); `[ui] mouse = true` turns it back on for wheel scrolling.
    pub mouse: bool,
    pub show_reasoning: bool,
    pub expand_tools: bool,
    /// Model requests per prompt, or `None` for no limit (D37).
    pub max_steps: Option<usize>,
    /// `[ui] images`: `kitty`, `iterm2`, `text`, or unset to detect it (D59).
    pub images: Option<String>,
    /// `[ui] theme`: the name of the theme to draw with (D71).
    pub theme: Option<String>,
}

/// Rendered lines for one entry, valid while the width and both toggles match.
pub struct Cached {
    pub width: u16,
    pub expand: bool,
    pub thinking: bool,
    pub lines: Vec<Line<'static>>,
}

/// One line of the `/` menu. Owned, because a skill's command is built from what the session
/// discovered rather than from the static table (D69).
#[derive(Clone)]
pub struct Menu {
    pub name: String,
    pub args: String,
    pub about: String,
}

pub struct SlashCommand {
    pub name: &'static str,
    pub args: &'static str,
    pub about: &'static str,
}

/// The commands the `/` menu offers, in the order it lists them.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/login",
        args: "",
        about: "choose a provider and model, add a key, or add a server",
    },
    SlashCommand {
        name: "/model",
        args: "[name]",
        about: "show the model, or switch to another and save it",
    },
    SlashCommand {
        name: "/settings",
        args: "",
        about: "provider, model, reasoning effort, limits and display",
    },
    SlashCommand {
        name: "/permissions",
        args: "",
        about: "the grants this project has: exclusions, and which scopes are on",
    },
    SlashCommand {
        name: "/resume",
        args: "",
        about: "continue a saved session from this workspace",
    },
    SlashCommand {
        name: "/logout",
        args: "[provider]",
        about: "remove a stored key (the current provider's by default)",
    },
    SlashCommand {
        name: "/tree",
        args: "",
        about: "the prompts on this branch; continue in a new session from one",
    },
    SlashCommand {
        name: "/thinking",
        args: "[level]",
        about: "reasoning effort: default, low, medium or high (shift+tab cycles)",
    },
    SlashCommand {
        name: "/name",
        args: "[name]",
        about: "name this conversation, or show and clear its name",
    },
    SlashCommand {
        name: "/session",
        args: "",
        about: "this conversation's id, messages, tokens and cost",
    },
    SlashCommand {
        name: "/clone",
        args: "",
        about: "copy this conversation into a new one, from here",
    },
    SlashCommand {
        name: "/theme",
        args: "[name]",
        about: "open the list of themes and preview them; a name switches straight to it",
    },
    SlashCommand {
        name: "/reload",
        args: "",
        about: "read the skills, AGENTS.md files and settings again, without restarting",
    },
    SlashCommand {
        name: "/compact",
        args: "[focus]",
        about: "summarize the conversation so far to free context",
    },
    SlashCommand {
        name: "/clear",
        args: "",
        about: "start a new session; the current one stays saved",
    },
    SlashCommand {
        name: "/help",
        args: "",
        about: "list the keys and commands",
    },
    SlashCommand {
        name: "/exit",
        args: "",
        about: "quit",
    },
];

pub struct App {
    pub header: Header,
    /// What discovery offered, so `/skill:name` can put a body into the conversation without going
    /// back to the agent thread (D69). The text was read and recorded at discovery.
    pub skills: Vec<crate::skills::Skill>,
    pub entries: Vec<Entry>,
    /// Parallel to `entries`; `None` means re-render.
    pub cache: Vec<Option<Cached>>,
    pub editor: Editor,
    pub queue: VecDeque<String>,
    pub busy: Option<Activity>,
    pub cancelling: bool,
    /// The session ended after a host error; no more prompts.
    pub stopped: Option<String>,
    pub expand_tools: bool,
    /// Reasoning is shown in full; otherwise each block shrinks to one line.
    pub show_thinking: bool,
    /// Transcript lines scrolled up from the bottom; 0 follows new output.
    pub scroll: usize,
    /// Transcript height and line count at the last draw, for paging and keeping the view still.
    pub view_height: usize,
    /// Where the last draw put the "more lines" badge; a click there jumps to the end.
    pub jump_badge: Option<Rect>,
    /// What the mouse has selected, drawn highlighted until something replaces it (D35).
    pub selection: Option<Selection>,
    /// The transcript geometry of the last draw, which turns a screen point into a character.
    pub seen: Option<Seen>,
    /// When the last copy happened, for the note in the activity line.
    pub copied: Option<std::time::Instant>,
    /// What was copied, for the confirmation (D86).
    pub copied_what: &'static str,
    /// Where the sign-in link was drawn, so a click on it opens the browser (D86).
    pub open_url_at: Option<ratatui::layout::Rect>,
    /// When escape was last pressed on an empty editor, so two of them open the tree (D54).
    last_escape: Option<std::time::Instant>,
    pub total_lines: usize,
    /// Where each image sits in the transcript, and the escape that draws it (D59).
    pub images: Vec<Placed>,
    /// What this terminal can draw, detected once at startup (D59).
    pub protocol: crate::tui::image::Protocol,
    /// The colours to draw with (D71). Every role the renderer paints comes from here.
    pub theme: crate::tui::theme::Theme,
    /// What the last frame drew, and where (D59).
    pub drawn: Drawn,
    pub usage: Usage,
    /// Dollars spent this session, when a price is known (D36).
    pub cost: f64,
    /// The model's context window and price, and the allowance the provider last stated.
    pub quota: Option<crate::provider::Quota>,
    /// Tokens in the context at the last reply.
    pub context: u64,
    pub spinner: usize,
    pub quit: bool,
    /// The /login dialog, drawn over everything else while open.
    pub login: Option<Login>,
    pub resume: Option<SessionPicker>,
    pub settings: Option<SettingsDialog>,
    /// The conversation tree, drawn over everything else while open (D53).
    pub tree: Option<crate::tui::tree::Tree>,
    /// The permissions editor, drawn over everything else while open (D45).
    pub permissions: Option<crate::tui::permissions::Permissions>,
    /// The theme list, drawn over everything else while open, and previewed as it moves (D74).
    pub themes: Option<crate::tui::themes::ThemePicker>,
    /// The theme in force when the list opened — its name and its colours — so escape puts back
    /// exactly what was there (D74).
    theme_before: Option<(String, crate::tui::theme::Theme)>,
    /// The agent's per-prompt limit, shown in /settings.
    /// Model requests per prompt, or `None` for no limit (D37).
    pub max_steps: Option<usize>,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft: String,
    /// The `/` menu's selection, valid while the editor still holds `menu_for`.
    menu_index: usize,
    menu_for: String,
    /// Esc hid the menu for this exact text.
    menu_dismissed: Option<String>,
    /// Workspace files for the `@` picker, and when they were listed.
    files: Option<(Vec<String>, std::time::Instant)>,
}

const KEYS: &str = "\
enter        send (while the agent works, the prompt is queued)
shift+enter  newline (also alt+enter, ctrl+j)
esc          cancel the running turn; queued prompts return to the editor
ctrl+c       cancel, clear the editor, or quit when it is empty
ctrl+d       quit when the editor is empty
up/down      move between lines, or through earlier prompts
ctrl+g       write the prompt in $EDITOR
@path        attach a file to the prompt (type @ to pick one)
!cmd         run a command in the sandbox and share its output; !!cmd keeps it to you
pgup/pgdn    scroll the transcript (also the mouse wheel)
drag         select text in the transcript; it is highlighted and copied on release
end/home     jump to the end or start of the transcript when the editor is empty
             (ctrl+end and ctrl+home always; clicking the ‘more lines’ badge too)
ctrl+o       show or hide full tool output
ctrl+t       show or hide reasoning
/            list the commands; tab completes, enter runs";

pub fn help() -> String {
    let width = COMMANDS
        .iter()
        .map(|c| c.name.len() + c.args.len() + 1)
        .max()
        .unwrap_or(0);
    let mut text = KEYS.to_owned();
    for c in COMMANDS {
        let usage = format!("{} {}", c.name, c.args);
        text.push_str(&format!("\n{usage:<width$}  {}", c.about));
    }
    text
}

impl App {
    /// The image an entry holds, for the renderer that has to find a place to draw it (D59).
    pub fn cached_image(&self, i: usize) -> Option<crate::provider::Image> {
        match self.entries.get(i) {
            Some(Entry::Image(image)) => Some(image.clone()),
            _ => None,
        }
    }

    pub fn new(header: Header) -> Self {
        let (show_reasoning, expand_tools, max_steps) =
            (header.show_reasoning, header.expand_tools, header.max_steps);
        let mut app = Self {
            header,
            skills: Vec::new(),
            entries: Vec::new(),
            cache: Vec::new(),
            editor: Editor::default(),
            queue: VecDeque::new(),
            busy: None,
            cancelling: false,
            stopped: None,
            expand_tools,
            show_thinking: show_reasoning,
            max_steps,
            scroll: 0,
            view_height: 0,
            jump_badge: None,
            selection: None,
            seen: None,
            copied: None,
            copied_what: "the selection",
            open_url_at: None,
            last_escape: None,
            total_lines: 0,
            images: Vec::new(),
            protocol: crate::tui::image::Protocol::Text,
            theme: crate::tui::theme::Theme::default(),
            drawn: Drawn::default(),
            usage: Usage::default(),
            cost: 0.0,
            quota: None,
            context: 0,
            spinner: 0,
            quit: false,
            login: None,
            resume: None,
            settings: None,
            tree: None,
            permissions: None,
            themes: None,
            theme_before: None,
            history: Vec::new(),
            history_pos: None,
            draft: String::new(),
            menu_index: 0,
            menu_for: String::new(),
            menu_dismissed: None,
            files: None,
        };
        let using = if app.header.provider.is_empty() {
            "no provider chosen yet".to_owned()
        } else {
            format!("{} · {}", app.header.provider, app.header.model)
        };
        let intro = format!(
            "saddle · {using}\nworkspace {}\naudit log {}\n/help lists the keys",
            app.header.workspace.display(),
            app.header.audit.display()
        );
        app.push(Entry::Note(NoteKind::Info, intro));
        for note in std::mem::take(&mut app.header.notes) {
            app.push(Entry::Note(NoteKind::Info, note));
        }
        app
    }

    pub fn note(&mut self, kind: NoteKind, text: &str) {
        self.push(Entry::Note(kind, text.to_owned()));
    }

    pub fn open_login(&mut self, login: Login) {
        self.login = Some(login);
    }

    /// A dialog is open and takes the keys.
    pub fn dialog_open(&self) -> bool {
        self.login.is_some()
            || self.resume.is_some()
            || self.settings.is_some()
            || self.themes.is_some()
    }

    fn compaction(&mut self, event: crate::compact::Event) {
        use crate::compact::Event;
        let current = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Compaction(v) if v.state == CompactState::Running));
        match event {
            Event::Started {
                reason,
                tokens,
                window,
                messages,
            } => {
                self.push(Entry::Compaction(Box::new(CompactionView {
                    header: format!(
                        "{messages} messages · {} · ~{} of {} tokens",
                        reason.describe(),
                        short_tokens(tokens),
                        short_tokens(window)
                    ),
                    reasoning: String::new(),
                    summary: String::new(),
                    state: CompactState::Running,
                    blocks: reason.blocks(),
                })));
                // A background compaction does not stop the turn, so it must not claim the
                // activity line: saying "compacting" while the model writes would be a lie about
                // what the terminal is waiting for (D78).
                if self.busy.is_some() && reason.blocks() {
                    self.busy = Some(Activity::Compacting);
                }
            }
            Event::Reasoning(s) => {
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    v.reasoning.push_str(&display(&s));
                    self.cache[i] = None;
                }
            }
            Event::Text(s) => {
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    v.summary.push_str(&display(&s));
                    self.cache[i] = None;
                }
            }
            Event::Done {
                summarized,
                kept,
                before,
                after,
            } => {
                let mut blocks = true;
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    blocks = v.blocks;
                    v.state = CompactState::Done(format!(
                        "{summarized} messages summarized, {kept} kept · ~{} → ~{} tokens",
                        short_tokens(before),
                        short_tokens(after)
                    ));
                    self.cache[i] = None;
                }
                self.context = after;
                if self.busy.is_some() && blocks {
                    self.busy = Some(Activity::Waiting);
                }
            }
            Event::Failed(why) => {
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    v.state = CompactState::Failed(display(&why));
                    self.cache[i] = None;
                }
            }
        }
    }

    /// Replaces the transcript with a resumed session's.
    fn show_resumed(&mut self, items: &[Item]) {
        self.entries.clear();
        self.cache.clear();
        // The old highlight points at lines that no longer exist.
        self.selection = None;
        self.scroll = 0;
        let prompts = items.iter().filter(|i| i.text().is_some()).count();
        let plural = if prompts == 1 { "" } else { "s" };
        self.info(&format!("resumed a session of {prompts} prompt{plural}"));
        for entry in entries_from(items) {
            self.push(entry);
        }
    }

    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.cache.push(None);
    }

    fn info(&mut self, text: &str) {
        self.push(Entry::Note(NoteKind::Info, text.to_owned()));
    }

    fn touch_last(&mut self) {
        if let Some(c) = self.cache.last_mut() {
            *c = None;
        }
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
    }

    /// Commands matching what is typed, while the editor holds only a command name: those that
    /// start with it first, then those that contain it.
    pub fn menu(&self) -> Vec<Menu> {
        let text = self.editor.text();
        if self.dialog_open()
            || !text.starts_with('/')
            || text.contains(char::is_whitespace)
            || self.menu_dismissed.as_deref() == Some(text)
        {
            return Vec::new();
        }
        let typed = text.to_lowercase();
        // The static commands, then one per skill: `/skill:name` is how a skill is forced when the
        // model did not pick it from its description (D69).
        let mut all: Vec<Menu> = COMMANDS
            .iter()
            .map(|c| Menu {
                name: c.name.to_owned(),
                args: c.args.to_owned(),
                about: c.about.to_owned(),
            })
            .collect();
        all.extend(self.skills.iter().map(|s| Menu {
            name: format!("/skill:{}", s.name),
            args: "[request]".to_owned(),
            about: s.description.clone(),
        }));
        let mut found: Vec<Menu> = all
            .iter()
            .filter(|c| c.name.starts_with(&typed))
            .cloned()
            .collect();
        found.extend(
            all.into_iter()
                .filter(|c| !c.name.starts_with(&typed) && c.name.contains(&typed[1..])),
        );
        found
    }

    /// The `@` token the cursor is in: its byte range and the text after `@`.
    fn mention(&self) -> Option<(usize, usize, String)> {
        let text = self.editor.text();
        let end = self.editor.cursor();
        let start = text[..end].rfind(char::is_whitespace).map_or(0, |i| {
            i + text[i..].chars().next().map_or(1, char::len_utf8)
        });
        let token = &text[start..end];
        let query = token.strip_prefix('@')?;
        (self.menu_dismissed.as_deref() != Some(text)).then(|| (start, end, query.to_owned()))
    }

    /// Workspace files matching the `@` token: names starting with it first, then paths that
    /// contain it.
    pub fn mention_matches(&mut self) -> Vec<String> {
        if self.dialog_open() {
            return Vec::new();
        }
        let Some((_, _, query)) = self.mention() else {
            return Vec::new();
        };
        let stale = self
            .files
            .as_ref()
            .is_none_or(|(_, at)| at.elapsed() > PICKER_REFRESH);
        if stale {
            let list = crate::tools::search::list_files(&self.header.workspace, PICKER_FILES);
            self.files = Some((list, std::time::Instant::now()));
        }
        let files = self
            .files
            .as_ref()
            .map(|(f, _)| f.as_slice())
            .unwrap_or_default();
        let q = query.to_lowercase();
        let name = |f: &String| f.rsplit('/').next().unwrap_or(f).to_lowercase();
        let mut found: Vec<String> = files
            .iter()
            .filter(|f| name(f).starts_with(&q))
            .cloned()
            .collect();
        found.extend(
            files
                .iter()
                .filter(|f| !name(f).starts_with(&q) && f.to_lowercase().contains(&q))
                .cloned(),
        );
        found.truncate(50);
        found
    }

    /// The list to show above the editor, if any.
    pub fn popup(&mut self) -> Option<Popup> {
        let menu = self.menu();
        if !menu.is_empty() {
            return Some(Popup {
                selected: self.menu_selected().min(menu.len() - 1),
                items: menu
                    .iter()
                    .map(|c| (format!("{} {}", c.name, c.args), c.about.clone()))
                    .collect(),
                hint: " ↑↓ select · tab complete · enter run · esc hide ",
            });
        }
        let files = self.mention_matches();
        (!files.is_empty()).then(|| Popup {
            selected: self.menu_selected().min(files.len() - 1),
            items: files
                .into_iter()
                .map(|f| (format!("@{f}"), String::new()))
                .collect(),
            hint: " ↑↓ select · tab or enter attach · esc hide ",
        })
    }

    /// Keys the `@` picker takes while it is open; `None` passes the key on.
    fn mention_key(&mut self, key: KeyEvent, newline: bool) -> Option<Option<Action>> {
        let files = self.mention_matches();
        if files.is_empty() {
            return None;
        }
        let selected = self.menu_selected().min(files.len() - 1);
        match key.code {
            KeyCode::Up => self.select((selected + files.len() - 1) % files.len()),
            KeyCode::Down => self.select((selected + 1) % files.len()),
            KeyCode::Tab => self.attach(&files[selected]),
            KeyCode::Enter if !newline => self.attach(&files[selected]),
            KeyCode::Esc => self.menu_dismissed = Some(self.editor.text().to_owned()),
            _ => return None,
        }
        Some(None)
    }

    fn attach(&mut self, path: &str) {
        if let Some((start, end, _)) = self.mention() {
            self.editor.replace(start, end, &format!("@{path} "));
        }
    }

    pub fn menu_selected(&self) -> usize {
        if self.menu_for == self.editor.text() {
            self.menu_index
        } else {
            0
        }
    }

    fn select(&mut self, index: usize) {
        self.menu_index = index;
        self.menu_for = self.editor.text().to_owned();
    }

    /// Keys the `/` menu takes while it is open; `None` passes the key on.
    fn menu_key(&mut self, key: KeyEvent, newline: bool) -> Option<Option<Action>> {
        let menu = self.menu();
        if menu.is_empty() {
            return None;
        }
        let selected = self.menu_selected().min(menu.len() - 1);
        let chosen = menu[selected].clone();
        match key.code {
            KeyCode::Up => self.select((selected + menu.len() - 1) % menu.len()),
            KeyCode::Down => self.select((selected + 1) % menu.len()),
            KeyCode::Tab => self.complete(&chosen),
            KeyCode::Enter if !newline => {
                // Runs the command, unless it takes an argument that has not been typed yet.
                if chosen.args.is_empty() || self.editor.text() == chosen.name {
                    self.editor.set(chosen.name.clone());
                    return Some(self.submit());
                }
                self.complete(&chosen);
            }
            KeyCode::Esc => self.menu_dismissed = Some(self.editor.text().to_owned()),
            _ => return None,
        }
        Some(None)
    }

    fn complete(&mut self, command: &Menu) {
        let text = if command.args.is_empty() {
            command.name.clone()
        } else {
            format!("{} ", command.name)
        };
        self.editor.set(text);
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        // Any other key breaks the pair, so escape then `a` then escape is not a double escape.
        if key.code != KeyCode::Esc {
            self.last_escape = None;
        }
        if let Some(login) = &mut self.login {
            return match login.key(key) {
                Outcome::Stay => None,
                Outcome::Close => {
                    self.login = None;
                    None
                }
                Outcome::Check(id, draft) => Some(Action::CheckModels(id, draft)),
                Outcome::SignIn(id, provider) => Some(Action::SignIn(id, provider)),
                Outcome::OpenLink(url) => Some(Action::OpenUrl(url)),
                Outcome::CopyLink(url) => Some(Action::Copy(url, "the sign-in link")),
                // Cancelling is the dialog saying stop asking; the answer is only ever for it, so
                // there is nothing for the interface to do with it.
                Outcome::CancelSignIn(_) => None,
                Outcome::Submit(req) => {
                    self.login = None;
                    Some(Action::Login(req))
                }
            };
        }
        if self.permissions.is_some() {
            return self.permissions_key(key);
        }
        if self.themes.is_some() {
            return self.themes_key(key);
        }
        if let Some(settings) = &mut self.settings {
            return match settings.key(key) {
                SettingsOutcome::Stay => None,
                SettingsOutcome::Close => {
                    self.settings = None;
                    None
                }
                SettingsOutcome::Apply(change) => {
                    // These hand over to another dialog or to $EDITOR.
                    if matches!(
                        change,
                        Change::OpenLogin
                            | Change::ChooseModel
                            | Change::EditConfig
                            | Change::EditPolicy
                    ) {
                        self.settings = None;
                    }
                    Some(Action::Apply(change))
                }
            };
        }
        if let Some(tree) = &mut self.tree {
            return match tree.key(key) {
                crate::tui::tree::TreeOutcome::Stay => None,
                crate::tui::tree::TreeOutcome::Close => {
                    self.tree = None;
                    None
                }
                crate::tui::tree::TreeOutcome::Select { rewind_to, text } => {
                    self.tree = None;
                    // The prompt goes back into the editor, ready to be changed and sent again.
                    if let Some(text) = text {
                        self.editor.set(text);
                    }
                    Some(Action::Rewind(rewind_to))
                }
                crate::tui::tree::TreeOutcome::Copy(text) => Some(Action::Copy(text, "the branch")),
            };
        }
        if let Some(picker) = &mut self.resume {
            return match picker.key(key) {
                PickOutcome::Stay => None,
                PickOutcome::Close => {
                    self.resume = None;
                    None
                }
                PickOutcome::Resume(id) => {
                    self.resume = None;
                    Some(Action::Resume(id))
                }
                PickOutcome::Rename(id, current) => {
                    // The picker closes and the name is edited in the prompt, which already has
                    // everything an editor needs (D55).
                    self.resume = None;
                    self.editor.set(format!("/name {current}"));
                    self.note(
                        NoteKind::Info,
                        "edit the name and press enter; enter alone clears it",
                    );
                    let _ = id;
                    None
                }
                PickOutcome::Delete(id) => {
                    self.resume = None;
                    Some(Action::DeleteSession(id))
                }
            };
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let Some(action) = self.menu_key(key, shift || alt) {
            return action;
        }
        if let Some(action) = self.mention_key(key, shift || alt) {
            return action;
        }
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if self.busy.is_some() && !self.cancelling {
                    self.cancelling = true;
                    return Some(Action::Cancel);
                }
                if self.busy.is_none() && !self.editor.is_empty() {
                    self.editor.clear();
                    return None;
                }
                return Some(Action::Quit);
            }
            KeyCode::Char('d') if ctrl => {
                if self.editor.is_empty() {
                    return Some(Action::Quit);
                }
                self.editor.delete();
            }
            KeyCode::Esc => {
                if self.busy.is_some() {
                    if !self.cancelling {
                        self.cancelling = true;
                        return Some(Action::Cancel);
                    }
                    return None;
                }
                // Escape twice on an empty editor opens the tree, as it does in pi. With text in
                // the editor escape does nothing, so a half-written prompt is not lost and no
                // stray tap opens a dialog (D54).
                if self.editor.is_empty() {
                    let twice = self
                        .last_escape
                        .is_some_and(|at| at.elapsed() < DOUBLE_ESCAPE);
                    self.last_escape = Some(std::time::Instant::now());
                    if twice {
                        self.last_escape = None;
                        return Some(Action::OpenTree);
                    }
                }
            }
            KeyCode::Enter if shift || alt => self.editor.insert('\n'),
            KeyCode::Char('j') if ctrl => self.editor.insert('\n'),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('o') if ctrl => {
                self.expand_tools = !self.expand_tools;
            }
            KeyCode::Char('t') if ctrl => {
                self.show_thinking = !self.show_thinking;
            }
            // Shift+tab cycles the reasoning level, as it does in pi (D57).
            KeyCode::BackTab => return Some(Action::Effort(Effort::Cycle)),
            KeyCode::Char('g') if ctrl => return Some(Action::EditPrompt),
            KeyCode::PageUp => self.scroll_by(self.page() as isize),
            KeyCode::PageDown => self.scroll_by(-(self.page() as isize)),
            // With an empty editor there is no line to move along, so home and end move through
            // the transcript instead.
            KeyCode::Home if ctrl || self.editor.is_empty() => self.scroll = usize::MAX,
            KeyCode::End if ctrl || self.editor.is_empty() => self.scroll = 0,
            KeyCode::Up => {
                if !self.editor.up() {
                    self.history_back();
                }
            }
            KeyCode::Down => {
                if !self.editor.down() {
                    self.history_forward();
                }
            }
            KeyCode::Left if ctrl || alt => self.editor.word_left(),
            KeyCode::Right if ctrl || alt => self.editor.word_right(),
            KeyCode::Left => self.editor.left(),
            KeyCode::Right => self.editor.right(),
            KeyCode::Char('b') if alt => self.editor.word_left(),
            KeyCode::Char('f') if alt => self.editor.word_right(),
            KeyCode::Home => self.editor.home(),
            KeyCode::End => self.editor.end(),
            KeyCode::Char('a') if ctrl => self.editor.home(),
            KeyCode::Char('e') if ctrl => self.editor.end(),
            KeyCode::Char('u') if ctrl => self.editor.kill_to_line_start(),
            KeyCode::Char('k') if ctrl => self.editor.kill_to_line_end(),
            KeyCode::Char('w') if ctrl => self.editor.delete_word_back(),
            KeyCode::Backspace if alt || ctrl => self.editor.delete_word_back(),
            KeyCode::Backspace => self.editor.backspace(),
            KeyCode::Delete => self.editor.delete(),
            KeyCode::Tab => self.editor.insert_str("    "),
            KeyCode::Char(c) if !ctrl => self.editor.insert(c),
            _ => {}
        }
        None
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(login) = &mut self.login {
            login.paste(text);
        } else if let Some(picker) = &mut self.resume {
            picker.paste(text);
        } else if self.settings.is_none() && self.themes.is_none() {
            self.editor.insert_str(text);
        }
    }

    /// The mouse: the wheel scrolls, and a drag across the transcript selects text (D35). Text is
    /// copied when the button comes up, so a drag that ends anywhere has already done what it
    /// looked like it would.
    /// Opens the theme list on the theme in force, remembering what to put back (D74).
    pub fn open_themes(&mut self, mut picker: crate::tui::themes::ThemePicker) {
        let name = self
            .header
            .theme
            .clone()
            .unwrap_or_else(|| self.theme.name.clone());
        // The app is the one that knows what is drawn, so the `*` the list marks says what was
        // actually in force (D74).
        picker.original = name.clone();
        self.theme_before = Some((name, self.theme.clone()));
        // Stored before the preview, which reads the picker: the other order draws the theme the
        // list opened on only after the first keypress (D74).
        self.themes = Some(picker);
        self.preview_theme();
    }

    /// Draws with the highlighted theme, so the list is read in the theme it offers (D74).
    fn preview_theme(&mut self) {
        if let Some(theme) = self.themes.as_ref().and_then(|p| p.preview()).cloned() {
            if let Some(picker) = &self.themes {
                self.header.theme = Some(picker.current().name.clone());
            }
            self.theme = theme;
        }
    }

    /// The theme list, while it is open: it takes every key (D74).
    ///
    /// Moving the highlight previews, enter keeps and saves, escape puts back the theme that was in
    /// force — and the name in the status line goes back with it.
    pub fn themes_key(&mut self, key: KeyEvent) -> Option<Action> {
        use crate::tui::themes::Outcome;
        let before = self.theme_before.clone();
        let outcome = self.themes.as_mut()?.key(key);
        match outcome {
            Outcome::Stay => {
                self.preview_theme();
                None
            }
            Outcome::Cancel => {
                // Both what is drawn and the name the status line shows come back from what the
                // app had, not from what the list was told: a picker that opened on the wrong
                // entry must not be able to make escape change the theme (D74).
                if let Some((name, theme)) = before {
                    self.theme = theme;
                    self.header.theme = Some(name);
                }
                self.themes = None;
                self.theme_before = None;
                None
            }
            Outcome::Choose(name) => {
                self.preview_theme();
                self.themes = None;
                self.theme_before = None;
                Some(Action::Theme(Some(name)))
            }
        }
    }

    /// The permissions editor, while it is open: it takes every key, and a change it asks for is
    /// carried out by the controller (D45).
    pub fn permissions_key(&mut self, key: KeyEvent) -> Option<Action> {
        use crate::tui::permissions::Outcome;
        let dialog = self.permissions.as_mut()?;
        match dialog.key(key) {
            Outcome::Stay => None,
            Outcome::Close => {
                self.permissions = None;
                None
            }
            Outcome::Apply(change) => Some(Action::Grant(change)),
        }
    }

    pub fn mouse(&mut self, ev: MouseEvent) -> Option<Action> {
        // A dialog owns the screen while it is open, so the transcript beneath is not selectable.
        let overlay = self.login.is_some()
            || self.resume.is_some()
            || self.settings.is_some()
            || self.permissions.is_some()
            || self.themes.is_some();
        match ev.kind {
            // A click on the sign-in link is the one thing a dialog offers the mouse: it is drawn
            // underlined to invite it, and a terminal gives no other way to follow it (D86).
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .open_url_at
                    .is_some_and(|r| r.contains(Position::new(ev.column, ev.row))) =>
            {
                self.login
                    .as_ref()
                    .and_then(|login| login.link())
                    .map(Action::OpenUrl)
            }
            MouseEventKind::ScrollUp => {
                self.scroll_by(3);
                None
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(-3);
                None
            }
            MouseEventKind::Down(MouseButton::Left)
                if self
                    .jump_badge
                    .is_some_and(|r| r.contains(Position::new(ev.column, ev.row))) =>
            {
                self.scroll = 0;
                self.selection = None;
                None
            }
            MouseEventKind::Down(MouseButton::Left) if !overlay => {
                let cell = self.cell_at(&ev)?;
                self.selection = Some(Selection {
                    anchor: cell,
                    focus: cell,
                    dragging: true,
                });
                None
            }
            MouseEventKind::Drag(MouseButton::Left) if !overlay => self.extend(ev),
            MouseEventKind::Up(MouseButton::Left) => {
                let mut sel = self.selection?;
                if !sel.dragging {
                    return None;
                }
                sel.dragging = false;
                self.selection = Some(sel);
                if sel.is_empty() {
                    // A click that selected nothing is not a copy; drop the empty highlight.
                    self.selection = None;
                    return None;
                }
                self.selected_text()
                    .map(|text| Action::Copy(text, "the selection"))
            }
            _ => None,
        }
    }

    /// Extends the drag to the mouse, scrolling when it is pulled past the top or the bottom so
    /// that a selection can be longer than the screen (D35).
    fn extend(&mut self, ev: MouseEvent) -> Option<Action> {
        let mut sel = self.selection?;
        if !sel.dragging {
            return None;
        }
        let (area, top, len) = self.seen.as_ref().map(|s| (s.area, s.top, s.lines.len()))?;
        if ev.row < area.y {
            self.scroll_by(1);
            sel.focus = Cell {
                line: top.saturating_sub(1),
                ch: 0,
            };
        } else if ev.row >= area.bottom() {
            self.scroll_by(-1);
            sel.focus = Cell {
                line: (top + usize::from(area.height)).min(len.saturating_sub(1)),
                ch: usize::MAX,
            };
        } else {
            sel.focus = self.cell_at(&ev).unwrap_or(sel.focus);
        }
        self.selection = Some(sel);
        None
    }

    /// The character under the mouse, in the transcript as it was last drawn.
    fn cell_at(&self, ev: &MouseEvent) -> Option<Cell> {
        let seen = self.seen.as_ref()?;
        let area = seen.area;
        if seen.lines.is_empty() || area.height == 0 {
            return None;
        }
        let row = ev.row.clamp(area.y, area.bottom().saturating_sub(1));
        let line = (seen.top + usize::from(row - area.y)).min(seen.lines.len() - 1);
        let ch = char_at(&seen.lines[line], ev.column.saturating_sub(area.x));
        Some(Cell { line, ch })
    }

    /// The text under the selection, as it was drawn.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let seen = self.seen.as_ref()?;
        if sel.is_empty() || seen.lines.is_empty() {
            return None;
        }
        let (first, last) = sel.ends();
        // A drag pulled below the transcript points past its end; take what is there.
        let last_line = last.line.min(seen.lines.len() - 1);
        let mut out = String::new();
        let rows = seen.lines.get(first.line..=last_line).unwrap_or_default();
        for (offset, text) in rows.iter().enumerate() {
            let line = first.line + offset;
            let chars: Vec<char> = text.chars().collect();
            let from = if line == first.line {
                first.ch.min(chars.len())
            } else {
                0
            };
            let to = if line == last_line {
                last.ch.min(chars.len())
            } else {
                chars.len()
            };
            let part: String = chars[from.min(to)..to].iter().collect();
            // Lines are padded to the frame's width; the padding is not part of the copy.
            out.push_str(part.trim_end());
            if line != last_line {
                out.push('\n');
            }
        }
        let text = out.trim_end().to_owned();
        (!text.is_empty()).then_some(text)
    }

    fn page(&self) -> usize {
        (self.view_height / 2).max(1)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
    }

    fn history_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let pos = match self.history_pos {
            None => {
                self.draft = self.editor.text().to_owned();
                self.history.len() - 1
            }
            Some(p) => p.saturating_sub(1),
        };
        self.history_pos = Some(pos);
        self.editor.set(self.history[pos].clone());
    }

    fn history_forward(&mut self) {
        let Some(pos) = self.history_pos else {
            return;
        };
        if pos + 1 < self.history.len() {
            self.history_pos = Some(pos + 1);
            self.editor.set(self.history[pos + 1].clone());
        } else {
            self.history_pos = None;
            self.editor.set(std::mem::take(&mut self.draft));
        }
    }

    fn submit(&mut self) -> Option<Action> {
        let text = self.editor.text().trim().to_owned();
        if text.is_empty() {
            return None;
        }
        self.editor.clear();
        self.history_pos = None;
        if self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        self.scroll = 0;
        if text.starts_with('/') && !text.contains('\n') {
            return self.command(&text);
        }
        if let Some(reason) = &self.stopped {
            let msg = format!("the session has stopped ({reason}); ctrl+c quits");
            self.push(Entry::Note(NoteKind::Error, msg));
            return None;
        }
        if self.busy.is_some() {
            self.queue.push_back(text);
            return None;
        }
        Some(self.start(text))
    }

    fn command(&mut self, text: &str) -> Option<Action> {
        // `/skill:name request` becomes the skill's instructions followed by the request, which is
        // how a skill is forced when the model did not pick it from its description (D69). It is
        // also the only way to reach a skill marked `disable-model-invocation`, which is what that
        // flag means. The expanded text is what the conversation holds, so the transcript and a
        // resumed session show what the model was actually sent.
        if let Some(rest) = text.strip_prefix("/skill:") {
            let name = rest.split_whitespace().next().unwrap_or_default();
            let args = rest[name.len()..].trim();
            let Some(skill) = self.skills.iter().find(|s| s.name == name) else {
                self.note(
                    NoteKind::Error,
                    &format!(
                        "no skill `{name}`; this session has {}",
                        crate::skills::names(&self.skills)
                    ),
                );
                return None;
            };
            let prompt = crate::skills::invocation(skill, args);
            let name = skill.name.clone();
            self.push(Entry::User(prompt.clone()));
            self.busy = Some(Activity::Waiting);
            return Some(Action::Skill { name, prompt });
        }
        let mut words = text.split_whitespace();
        let command = words.next().unwrap_or_default();
        let arg = words.next().map(str::to_owned);
        match command {
            "/help" => {
                // The skill commands are per session, so they are listed from what was discovered
                // rather than from the static table (D69).
                let mut text = help();
                if !self.skills.is_empty() {
                    text.push_str("\n\nSkills this session offers:");
                    for skill in &self.skills {
                        text.push_str(&format!(
                            "\n/skill:{:<12}  {}",
                            skill.name, skill.description
                        ));
                    }
                }
                self.info(&text);
                None
            }
            "/exit" | "/quit" => Some(Action::Quit),
            "/login" | "/logout" | "/model" | "/resume" | "/settings" | "/tree"
                if self.busy.is_some() =>
            {
                self.info(&format!(
                    "{command} waits for the turn to end; esc cancels it"
                ));
                None
            }
            "/login" => Some(Action::OpenLogin),
            "/compact" if self.busy.is_some() => {
                self.info("/compact waits for the turn to end; esc cancels it");
                None
            }
            "/compact" => {
                let instructions = text["/compact".len()..].trim();
                self.busy = Some(Activity::Compacting);
                self.cancelling = false;
                Some(Action::Compact(
                    Some(instructions.to_owned()).filter(|i| !i.is_empty()),
                ))
            }
            "/resume" => Some(Action::OpenResume),
            "/tree" if self.busy.is_some() => {
                self.info("/tree waits for the turn to end; esc cancels it");
                None
            }
            "/tree" => Some(Action::OpenTree),
            "/thinking" => {
                let level = text["/thinking".len()..].trim();
                if level.is_empty() {
                    // No argument cycles, which is what shift+tab does (D57).
                    return Some(Action::Effort(Effort::Cycle));
                }
                let level = level.to_ascii_lowercase();
                if !crate::provider::EFFORTS.contains(&level.as_str()) {
                    self.note(
                        NoteKind::Error,
                        &format!(
                            "`{level}` is not a reasoning level; the levels are {}",
                            crate::provider::EFFORTS.join(", ")
                        ),
                    );
                    return None;
                }
                Some(Action::Effort(if level == "default" {
                    Effort::Set(None)
                } else {
                    Effort::Set(Some(level))
                }))
            }
            "/name" => {
                let name = text["/name".len()..].trim();
                // Without an argument it reports, and clears: an empty name is no name.
                Some(Action::Name(
                    Some(name.to_owned()).filter(|n| !n.is_empty()),
                ))
            }
            "/session" => Some(Action::SessionInfo),
            "/clone" if self.busy.is_some() => {
                self.info("/clone waits for the turn to end; esc cancels it");
                None
            }
            "/clone" => Some(Action::Clone),
            "/settings" => Some(Action::OpenSettings),
            "/theme" => Some(Action::Theme(arg)),
            "/reload" if self.busy.is_some() => {
                self.info("/reload waits for the turn to end; esc cancels it");
                None
            }
            "/reload" => Some(Action::Reload),
            "/permissions" => Some(Action::OpenPermissions),
            "/logout" => Some(Action::Logout(arg)),
            "/model" => Some(Action::Model(arg)),
            "/clear" if self.busy.is_some() => {
                self.info("/clear waits for the turn to end; esc cancels it");
                None
            }
            "/clear" => {
                self.entries.clear();
                self.cache.clear();
                self.selection = None;
                self.context = 0;
                self.info("new conversation; the session and its audit log continue");
                Some(Action::Clear)
            }
            other => {
                self.info(&format!(
                    "unknown command {other}; /help lists the commands"
                ));
                None
            }
        }
    }

    fn start(&mut self, text: String) -> Action {
        self.cancelling = false;
        // `!cmd` runs a command and shares its output; `!!cmd` keeps the output to the user.
        if let Some(rest) = text.strip_prefix('!') {
            let (command, share) = match rest.strip_prefix('!') {
                Some(command) => (command, false),
                None => (rest, true),
            };
            self.busy = Some(Activity::Tool("bash".into()));
            return Action::Shell {
                command: command.trim().to_owned(),
                share,
            };
        }
        self.push(Entry::User(text.clone()));
        self.busy = Some(Activity::Waiting);
        Action::Send(text)
    }

    /// Applies an event from the agent thread. Text is cleaned of control characters first (D27).
    pub fn agent(&mut self, ev: AgentEvent) -> Option<Action> {
        match ev {
            AgentEvent::Text(s) => {
                let s = display(&s);
                if let Some(Entry::Assistant(text)) = self.entries.last_mut() {
                    text.push_str(&s);
                    self.touch_last();
                } else {
                    self.push(Entry::Assistant(s));
                }
                if self.busy.is_some() {
                    self.busy = Some(Activity::Writing);
                }
            }
            AgentEvent::Reasoning(s) => {
                let s = display(&s);
                if let Some(Entry::Thinking(text)) = self.entries.last_mut() {
                    text.push_str(&s);
                    self.touch_last();
                } else {
                    self.push(Entry::Thinking(s));
                }
                if self.busy.is_some() {
                    self.busy = Some(Activity::Thinking);
                }
            }
            // The list comes back from the agent thread, which owns the session, so the `/` menu
            // and `/skill:` see what discovery just read (D70).
            AgentEvent::Skills(skills) => self.skills = skills,
            AgentEvent::Note(s) => self.push(Entry::Note(NoteKind::Warn, display(&s))),
            AgentEvent::Call { name, detail } => {
                self.busy = Some(Activity::Tool(display(&name)));
                self.push(Entry::Tool {
                    name: display(&name),
                    detail: display(&detail),
                    result: None,
                    diff: None,
                });
            }
            AgentEvent::Result {
                ok,
                content,
                diff: change,
                images,
            } => {
                // A picture a tool returned is shown beside its result, and kept in the transcript
                // the way an attached one is (D58, D59).
                for image in images {
                    self.push(Entry::Image(image));
                }
                let running = self
                    .entries
                    .iter_mut()
                    .rposition(|e| matches!(e, Entry::Tool { result: None, .. }));
                if let Some(i) = running {
                    if let Entry::Tool { result, diff, .. } = &mut self.entries[i] {
                        *result = Some((ok, display(&content)));
                        *diff = change.as_deref().map(display);
                    }
                    self.cache[i] = None;
                }
                if self.busy.is_some() {
                    self.busy = Some(Activity::Waiting);
                }
            }
            AgentEvent::Reply { usage: u, cost } => {
                self.usage.input += u.input;
                self.usage.output += u.output;
                self.usage.cache_read += u.cache_read;
                self.usage.cache_write += u.cache_write;
                self.cost += cost;
                self.context = u.input + u.cache_read + u.cache_write + u.output;
            }
            AgentEvent::Quota(quota) => self.quota = Some(quota),
            AgentEvent::TurnEnd(end) => {
                self.busy = None;
                self.cancelling = false;
                if end == End::Cancelled {
                    self.push(Entry::Note(NoteKind::Warn, "cancelled".into()));
                }
                if end == End::Answered {
                    if let Some(next) = self.queue.pop_front() {
                        return Some(self.start(next));
                    }
                } else if !self.queue.is_empty() {
                    // Nothing queued is sent after a turn that did not finish; it goes back to the
                    // editor instead.
                    let mut parts: Vec<String> = self.queue.drain(..).collect();
                    if !self.editor.is_empty() {
                        parts.push(self.editor.text().to_owned());
                    }
                    self.editor.set(parts.join("\n\n"));
                }
            }
            AgentEvent::Fatal(msg) => {
                self.busy = None;
                self.push(Entry::Note(
                    NoteKind::Error,
                    format!("session stopped: {msg}"),
                ));
                self.stopped = Some(msg);
            }
            AgentEvent::Models(id, result) => {
                if let Some(login) = &mut self.login {
                    login.models(id, result);
                }
            }
            AgentEvent::DeviceCode(id, result) => {
                let code = match &result {
                    Ok((_, code)) => Some(code.clone()),
                    Err(_) => None,
                };
                let mine = self
                    .login
                    .as_mut()
                    .is_some_and(|login| login.is_signing_in(id));
                if let Some(login) = &mut self.login {
                    login.device_code(id, result);
                }
                // The code has to be typed into a page in another window, and nothing in a dialog
                // can be selected, so it goes where the paste will come from (D86).
                if mine {
                    return code.map(|code| Action::Copy(code, "the sign-in code"));
                }
            }
            AgentEvent::SignedIn(id, result) => {
                let next = self
                    .login
                    .as_mut()
                    .and_then(|login| login.signed_in(id, result));
                if let Some((id, draft)) = next {
                    // The token is stored, so the listing proves it and picks the model.
                    return Some(Action::CheckModels(id, draft));
                }
            }
            AgentEvent::Listed => return Some(Action::SettleWindow),
            AgentEvent::Resumed(saved) => {
                // The transcript's items are not the whole story: a resumed session opens on the
                // tokens it has already spent and on what its next request would carry, rather
                // than claiming nothing has been sent or cached (D35).
                self.show_resumed(&saved.items);
                self.usage = saved.usage;
                self.cost = saved.cost;
                self.context = saved.context;
            }
            AgentEvent::Tree(points) => {
                if points.is_empty() {
                    self.note(
                        NoteKind::Info,
                        "nothing to fork from yet: this session has no prompt",
                    );
                } else {
                    self.tree = Some(crate::tui::tree::Tree::new(points));
                }
            }
            AgentEvent::Attached(images) => {
                for image in images {
                    self.push(Entry::Image(image));
                }
            }
            AgentEvent::Compaction(event) => self.compaction(event),
            AgentEvent::Closed => {
                if self.stopped.is_none() {
                    self.stopped = Some("the agent thread exited".into());
                }
            }
        }
        None
    }
}

/// Transcript entries for a saved conversation: prompts, reasoning, replies, and each tool call
/// with its result.
pub fn entries_from(items: &[Item]) -> Vec<Entry> {
    let mut results = std::collections::HashMap::new();
    for item in items {
        if let Item::ToolResults(rs) = item {
            for r in rs {
                results.insert(
                    r.id.as_str(),
                    (!r.is_error, r.content.as_str(), r.diff.as_deref()),
                );
            }
        }
    }
    let mut out = Vec::new();
    for item in items {
        match item {
            Item::User(text) => out.push(Entry::User(display(text))),
            // A resumed conversation shows the prompt and what was attached to it (D58).
            Item::Attached { text, images } => {
                out.push(Entry::User(display(text)));
                for image in images {
                    out.push(Entry::Note(
                        NoteKind::Info,
                        format!(
                            "[image] {} {}×{}",
                            display(&image.name),
                            image.width,
                            image.height
                        ),
                    ));
                }
            }
            Item::Assistant(parts) => {
                for part in parts {
                    match part {
                        Part::Text(t) if !t.trim().is_empty() => {
                            out.push(Entry::Assistant(display(t)));
                        }
                        Part::Call(c) => {
                            let found = results.get(c.id.as_str());
                            out.push(Entry::Tool {
                                name: display(&c.name),
                                detail: display(&call_detail(c)),
                                result: found.map(|(ok, content, _)| (*ok, display(content))),
                                diff: found.and_then(|(_, _, d)| d.map(display)),
                            });
                        }
                        Part::Image(image) => {
                            out.push(Entry::Note(
                                NoteKind::Info,
                                format!(
                                    "[image] {} {}×{}",
                                    display(&image.name),
                                    image.width,
                                    image.height
                                ),
                            ));
                        }
                        Part::Native { block, .. } => {
                            let reasoning = block["reasoning_content"].as_str().or_else(|| {
                                (block["type"] == "thinking")
                                    .then(|| block["thinking"].as_str())
                                    .flatten()
                            });
                            if let Some(r) = reasoning.filter(|r| !r.trim().is_empty()) {
                                out.push(Entry::Thinking(display(r)));
                            }
                        }
                        Part::Text(_) => {}
                    }
                }
            }
            Item::ToolResults(_) => {}
            Item::Summary(text) => out.push(Entry::Compaction(Box::new(CompactionView {
                header: "the earlier conversation, as summarized".into(),
                reasoning: String::new(),
                summary: display(text),
                state: CompactState::Done(String::new()),
                blocks: false,
            }))),
        }
    }
    out
}

/// 950, 12k, 1.2M.
pub fn short_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// The character index at display column `col` of `text`, which is how a mouse position becomes a
/// position in a line (D35). A wide character counts for the cell it occupies.
fn char_at(text: &str, col: u16) -> usize {
    let mut width: u16 = 0;
    for (i, c) in text.chars().enumerate() {
        if width >= col {
            return i;
        }
        width = width.saturating_add(c.width().unwrap_or(0) as u16);
    }
    text.chars().count()
}

/// Text from the model or a tool, made safe and stable for the screen.
fn display(s: &str) -> String {
    clean(s).replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(Header {
            provider: "mock".into(),
            model: "m".into(),
            workspace: "/ws".into(),
            audit: "/audit.jsonl".into(),
            notes: vec!["created /c/config.toml".into()],
            show_reasoning: true,
            expand_tools: false,
            max_steps: None,
            context_window: 128_000,
            priced: false,
            mouse: false,
            status: crate::tui::status::Template::default(),
            images: None,
            theme: None,
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn the_sign_in_code_goes_to_the_clipboard_when_it_arrives() {
        // D86: the code has to be typed into a page in another window, and nothing drawn in a dialog
        // can be selected, so it is put where the paste comes from. An answer for an attempt that is
        // no longer on screen is not copied: it belongs to a sign-in nobody is looking at.
        let mut app = app();
        let choice = crate::tui::login::Choice {
            name: "openai-chatgpt".into(),
            detail: String::new(),
            status: "not signed in".into(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: true,
        };
        let mut login = crate::tui::login::Login::new(
            vec![choice],
            vec![],
            Some("openai-chatgpt"),
            true,
            "auth.json".into(),
        );
        let Some(crate::tui::login::Outcome::SignIn(id, _)) = login.take_pending() else {
            panic!("a sign-in to start");
        };
        app.open_login(login);
        assert_eq!(
            app.agent(AgentEvent::DeviceCode(
                id,
                Ok(("https://auth.test/device".into(), "AB12-CD34".into()))
            )),
            Some(Action::Copy("AB12-CD34".into(), "the sign-in code"))
        );
        // The address is on screen, so a click can open it.
        assert_eq!(
            app.login.as_ref().unwrap().link().as_deref(),
            Some("https://auth.test/device")
        );
        // A stale attempt's code is dropped rather than copied over the current one.
        assert_eq!(
            app.agent(AgentEvent::DeviceCode(
                id + 99,
                Ok(("https://auth.test/other".into(), "ZZ99".into()))
            )),
            None
        );
    }

    #[test]
    fn a_listing_answer_asks_the_controller_to_settle_the_window() {
        // The answer is already in the model cache, so the event carries only the news that one
        // arrived; the controller re-reads the sources and moves the header and the loop (D34).
        let mut a = app();
        assert_eq!(a.agent(AgentEvent::Listed), Some(Action::SettleWindow));
    }

    #[test]
    fn a_resumed_session_shows_what_it_has_already_spent() {
        // Nothing has been sent this run, but the transcript says otherwise, and the estimate of
        // what the next request would carry comes with it (D35).
        let mut a = app();
        assert_eq!((a.usage.input, a.context), (0, 0));
        a.agent(AgentEvent::Resumed(Resumed {
            items: vec![Item::User("earlier".into())],
            usage: crate::provider::Usage {
                input: 1_500,
                output: 200,
                cache_read: 900,
                cache_write: 0,
            },
            cost: 0.42,
            context: 2_600,
        }));
        assert_eq!(a.usage.input, 1_500);
        assert_eq!(a.usage.output, 200);
        assert_eq!(a.usage.cache_read, 900);
        assert_eq!(a.context, 2_600);

        // Replies after the resume add to those totals rather than replacing them.
        a.agent(AgentEvent::Reply {
            usage: crate::provider::Usage {
                input: 10,
                output: 5,
                cache_read: 1,
                cache_write: 0,
            },
            cost: 0.01,
        });
        assert_eq!((a.usage.input, a.usage.output), (1_510, 205));
        assert!((a.cost - 0.43).abs() < 1e-9, "cost adds up too");
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn prompts_send_queue_and_flush() {
        let mut a = app();
        type_text(&mut a, "first");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Send("first".into()))
        );
        assert_eq!(a.busy, Some(Activity::Waiting));
        type_text(&mut a, "second");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert_eq!(a.queue, ["second"]);

        a.agent(AgentEvent::Text("he\x1b[2Jllo".into()));
        a.agent(AgentEvent::Text(" there".into()));
        assert_eq!(
            a.entries.last(),
            Some(&Entry::Assistant("he[2Jllo there".into()))
        );
        assert_eq!(a.busy, Some(Activity::Writing));
        a.agent(AgentEvent::Call {
            name: "bash".into(),
            detail: "ls".into(),
        });
        assert_eq!(a.busy, Some(Activity::Tool("bash".into())));
        a.agent(AgentEvent::Result {
            images: Vec::new(),
            ok: true,
            content: "exit code 0".into(),
            diff: None,
        });
        assert!(matches!(
            a.entries.last(),
            Some(Entry::Tool {
                result: Some((true, _)),
                ..
            })
        ));
        assert_eq!(
            a.agent(AgentEvent::TurnEnd(End::Answered)),
            Some(Action::Send("second".into()))
        );
        assert!(a.queue.is_empty());
    }

    #[test]
    fn cancel_returns_the_queue_to_the_editor() {
        let mut a = app();
        type_text(&mut a, "go");
        a.key(key(KeyCode::Enter));
        type_text(&mut a, "later");
        a.key(key(KeyCode::Enter));
        type_text(&mut a, "draft");
        assert_eq!(a.key(key(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(a.key(key(KeyCode::Esc)), None, "one cancel at a time");
        // A second ctrl+c while cancelling quits.
        assert_eq!(a.key(ctrl('c')), Some(Action::Quit));
        assert_eq!(a.agent(AgentEvent::TurnEnd(End::Cancelled)), None);
        assert_eq!(a.editor.text(), "later\n\ndraft");
        assert_eq!(a.busy, None);
        assert!(
            a.entries
                .contains(&Entry::Note(NoteKind::Warn, "cancelled".into()))
        );
    }

    #[test]
    fn ctrl_c_clears_then_quits_and_commands_work() {
        let mut a = app();
        type_text(&mut a, "abc");
        assert_eq!(a.key(ctrl('c')), None);
        assert!(a.editor.is_empty());
        assert_eq!(a.key(ctrl('c')), Some(Action::Quit));

        assert!(a.entries.contains(&Entry::Note(
            NoteKind::Info,
            "created /c/config.toml".into()
        )));
        type_text(&mut a, "/model gpt-x");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Model(Some("gpt-x".into())))
        );
        type_text(&mut a, "/login");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::OpenLogin));
        type_text(&mut a, "/clear");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Clear));
        assert_eq!(a.entries.len(), 1);
        type_text(&mut a, "/nope");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert!(
            matches!(a.entries.last(), Some(Entry::Note(_, t)) if t.contains("unknown command /nope"))
        );
        type_text(&mut a, "/exit");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Quit));
    }

    #[test]
    fn newline_keys_and_history() {
        let mut a = app();
        type_text(&mut a, "a");
        a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        a.key(ctrl('j'));
        type_text(&mut a, "b");
        assert_eq!(a.editor.text(), "a\n\n\nb");
        a.key(key(KeyCode::Enter));
        a.agent(AgentEvent::TurnEnd(End::Answered));
        type_text(&mut a, "two");
        a.key(key(KeyCode::Enter));
        a.agent(AgentEvent::TurnEnd(End::Answered));

        type_text(&mut a, "unsent");
        a.key(key(KeyCode::Up));
        assert_eq!(a.editor.text(), "two");
        a.key(key(KeyCode::Up));
        assert_eq!(a.editor.text(), "a\n\n\nb");
        // Inside a multi-line prompt, up moves between its lines first.
        a.key(key(KeyCode::Up));
        assert_eq!(a.editor.text(), "a\n\n\nb");
        a.key(key(KeyCode::Down));
        a.key(key(KeyCode::Down));
        assert_eq!(a.editor.text(), "two");
        a.key(key(KeyCode::Down));
        assert_eq!(a.editor.text(), "unsent");
    }

    #[test]
    fn thinking_sets_a_level_cycles_it_and_refuses_a_word_that_is_not_one() {
        let mut a = app();
        // A level by name.
        type_text(&mut a, "/thinking high");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Effort(Effort::Set(Some("high".into()))))
        );
        // The default is no level at all, so nothing is sent to a provider that has no such
        // setting (D57).
        type_text(&mut a, "/thinking DEFAULT");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Effort(Effort::Set(None)))
        );
        // No argument cycles, which is what shift+tab does.
        type_text(&mut a, "/thinking");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Effort(Effort::Cycle))
        );
        // A word that is not a level is reported and nothing is sent.
        a.editor.clear();
        type_text(&mut a, "/thinking very-hard");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert!(
            matches!(a.entries.last(), Some(Entry::Note(NoteKind::Error, t))
                if t.contains("not a reasoning level")),
            "{:?}",
            a.entries.last()
        );
    }

    #[test]
    fn shift_tab_cycles_the_reasoning_level() {
        let mut a = app();
        assert_eq!(
            a.key(key(KeyCode::BackTab)),
            Some(Action::Effort(Effort::Cycle))
        );
    }

    #[test]
    fn escape_twice_on_an_empty_editor_opens_the_tree() {
        let mut a = app();
        // Nothing running, editor empty: the first escape does nothing, the second opens it.
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert_eq!(a.key(key(KeyCode::Esc)), Some(Action::OpenTree));
        // And that pair is spent: a third escape starts over rather than opening it again.
        assert_eq!(a.key(key(KeyCode::Esc)), None);
    }

    #[test]
    fn escape_twice_needs_an_empty_editor_and_no_gap() {
        // Text in the editor: escape is not a gesture, so a half-written prompt is never lost.
        let mut a = app();
        type_text(&mut a, "half a prompt");
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert_eq!(a.editor.text(), "half a prompt");

        // Any other key breaks the pair.
        let mut a = app();
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        type_text(&mut a, "x");
        a.editor.clear();
        assert_eq!(a.key(key(KeyCode::Esc)), None, "the pair was broken");

        // While a turn runs, escape is cancel and never the tree.
        let mut a = app();
        a.busy = Some(Activity::Waiting);
        assert_eq!(a.key(key(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(a.key(key(KeyCode::Esc)), None);
    }

    #[test]
    fn a_skill_is_in_the_menu_and_its_command_loads_the_body() {
        // D69: `/skill:name` puts the instructions into the conversation and the rest of the line
        // after them as the request, which is how pi forces a skill the model did not pick.
        let mut a = app();
        a.skills = vec![crate::skills::Skill {
            name: "manim".into(),
            description: "Animate a proof.".into(),
            dir: PathBuf::from("/ws/.agents/skills/manim"),
            label: ".agents/skills/manim/SKILL.md".into(),
            text: "---\nname: manim\n---\nRender with Scene.".into(),
            hidden: false,
        }];

        // It is offered by the menu, with the description as its about text.
        type_text(&mut a, "/skill");
        let menu = a.menu();
        assert_eq!(
            menu.len(),
            1,
            "{:?}",
            menu.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
        assert_eq!(menu[0].name, "/skill:manim");
        assert!(menu[0].about.contains("Animate a proof."));

        // Running it sends the body and the request, and the transcript holds what was sent.
        a.editor.set("/skill:manim animate it".into());
        let action = a.key(key(KeyCode::Enter)).expect("an action");
        let Action::Skill { name, prompt } = action else {
            panic!("expected a skill action");
        };
        assert_eq!(name, "manim");
        assert!(prompt.contains("Render with Scene."), "{prompt}");
        assert!(prompt.trim_end().ends_with("animate it"), "{prompt}");
        assert!(
            matches!(a.entries.last(), Some(Entry::User(t)) if t.contains("Render with Scene.")),
            "the conversation holds the expanded prompt"
        );
        assert!(a.busy.is_some(), "the turn starts");
    }

    #[test]
    fn theme_is_a_command_with_an_optional_name() {
        // D71: no argument reports, a name switches. The switch itself needs a config to write, so
        // what the app decides is which action the command is.
        let mut a = app();
        type_text(&mut a, "/theme");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Theme(None)));
        type_text(&mut a, "/theme light");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Theme(Some("light".into())))
        );
    }

    #[test]
    fn the_theme_list_previews_as_it_moves_and_escape_puts_the_old_theme_back() {
        // D74: a theme is chosen by looking at it. Moving the highlight draws with the new one;
        // escape restores both the drawing and the name in the status line; enter keeps it.
        let mut a = app();
        let tables: crate::tui::theme::ThemeTables =
            toml::from_str("[mine]\ntext = \"#ffffff\"\naccent = \"#ff0000\"\n").unwrap();
        let was = a.theme.name.clone();
        // Opened on a theme that is not the app's, so "opens on the one in force" is a real claim,
        // and not the last entry, so there is somewhere to move to.
        let picker = crate::tui::themes::ThemePicker::collect(&tables, "dark");
        a.open_themes(picker);
        assert_eq!(
            a.theme.name, "dark",
            "the theme it opens on is the one drawn"
        );
        assert_eq!(a.header.theme.as_deref(), Some("dark"));
        let start = "dark".to_owned();

        // Down moves to the next entry and the screen is drawn in it.
        assert_eq!(a.key(key(KeyCode::Down)), None);
        let previewed = a.themes.as_ref().unwrap().current().name.clone();
        assert_ne!(previewed, start);
        assert_eq!(a.theme.name, previewed, "the preview is the drawing");
        assert_eq!(a.header.theme.as_deref(), Some(previewed.as_str()));

        // Escape: the theme that was in force comes back, by drawing and by name.
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert!(a.themes.is_none(), "the list closed");
        assert_eq!(a.theme.name, was, "the old theme is back");
        assert_eq!(a.header.theme.as_deref(), Some(was.as_str()));

        // Enter keeps the highlight: the action carries the name to save.
        let picker = crate::tui::themes::ThemePicker::collect(&tables, &was);
        a.open_themes(picker);
        a.key(key(KeyCode::Down));
        let chosen = a.themes.as_ref().unwrap().current().name.clone();
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Theme(Some(chosen.clone())))
        );
        assert!(a.themes.is_none());
        assert_eq!(a.theme.name, chosen, "kept after the list closes");
    }

    #[test]
    fn the_theme_list_takes_every_key_while_it_is_open() {
        // A picker that let a letter reach the editor would send a prompt instead of choosing.
        let mut a = app();
        let tables = crate::tui::theme::ThemeTables::new();
        let name = a.theme.name.clone();
        a.open_themes(crate::tui::themes::ThemePicker::collect(&tables, &name));
        assert!(a.dialog_open());
        assert!(a.menu().is_empty(), "no / menu over a dialog");
        for c in ['x', '/', 'q'] {
            assert_eq!(a.key(key(KeyCode::Char(c))), None);
        }
        assert!(a.editor.text().is_empty(), "nothing reached the editor");
    }

    #[test]
    fn reload_asks_the_agent_thread_to_read_again_and_takes_the_new_list() {
        // D70: the prompt carries the instruction files and the advertised skills, so a change
        // during the session only reaches the model if the prompt is rebuilt. The TUI's part is the
        // ask, and taking the list back.
        let mut a = app();
        type_text(&mut a, "/reload");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Reload));
        // And the list it comes back with replaces what the menu offers.
        a.agent(AgentEvent::Skills(vec![crate::skills::Skill {
            name: "fresh".into(),
            description: "Just written.".into(),
            dir: PathBuf::from("/ws/.agents/skills/fresh"),
            label: ".agents/skills/fresh/SKILL.md".into(),
            text: "body".into(),
            hidden: false,
        }]));
        type_text(&mut a, "/skill:fresh");
        assert!(
            matches!(a.key(key(KeyCode::Enter)), Some(Action::Skill { name, .. }) if name == "fresh")
        );
    }

    #[test]
    fn a_reload_waits_for_the_turn_to_end() {
        // The agent thread owns the session; reading it mid-turn would interleave with a request.
        let mut a = app();
        a.busy = Some(Activity::Waiting);
        type_text(&mut a, "/reload");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert!(
            matches!(a.entries.last(), Some(Entry::Note(_, t)) if t.contains("waits for the turn to end")),
            "{:?}",
            a.entries.last()
        );
    }

    #[test]
    fn a_hidden_skill_is_reachable_only_by_its_command() {
        // `disable-model-invocation` keeps a skill out of the prompt; the command is the point of
        // it, so it must still appear in the menu and still load.
        let mut a = app();
        a.skills = vec![crate::skills::Skill {
            name: "forced".into(),
            description: "Only when asked for.".into(),
            dir: PathBuf::from("/ws/.agents/skills/forced"),
            label: ".agents/skills/forced/SKILL.md".into(),
            text: "---\nname: forced\n---\nbody".into(),
            hidden: true,
        }];
        assert_eq!(
            crate::skills::listing(&a.skills),
            "",
            "not advertised in the prompt"
        );
        type_text(&mut a, "/skill:forced");
        assert!(
            matches!(a.key(key(KeyCode::Enter)), Some(Action::Skill { name, .. }) if name == "forced")
        );
    }

    #[test]
    fn an_unknown_skill_name_says_which_exist() {
        let mut a = app();
        a.skills = vec![crate::skills::Skill {
            name: "manim".into(),
            description: "d".into(),
            dir: PathBuf::from("/ws/.agents/skills/manim"),
            label: ".agents/skills/manim/SKILL.md".into(),
            text: "body".into(),
            hidden: false,
        }];
        type_text(&mut a, "/skill:nope");
        assert_eq!(a.key(key(KeyCode::Enter)), None, "nothing is sent");
        assert!(
            matches!(a.entries.last(), Some(Entry::Note(_, t)) if t.contains("no skill `nope`") && t.contains("manim")),
            "{:?}",
            a.entries.last()
        );
    }

    #[test]
    fn the_slash_menu_filters_completes_and_runs() {
        let names = |a: &App| a.menu().iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        let mut a = app();
        type_text(&mut a, "/");
        assert_eq!(names(&a).len(), COMMANDS.len());
        type_text(&mut a, "lo");
        // `/clone` and `/reload` match too: the menu filters by subsequence, not by prefix.
        assert_eq!(names(&a), ["/login", "/logout", "/clone", "/reload"]);
        a.key(key(KeyCode::Down));
        assert_eq!(a.menu_selected(), 1);
        a.key(key(KeyCode::Tab));
        assert_eq!(a.editor.text(), "/logout ");
        assert!(a.menu().is_empty(), "the menu closes once arguments start");

        a.editor.clear();
        type_text(&mut a, "/he");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert!(
            matches!(a.entries.last(), Some(Entry::Note(_, t)) if t.contains("/logout [provider]"))
        );

        // A command with an argument completes first; typed in full, enter runs it.
        type_text(&mut a, "/mo");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert_eq!(a.editor.text(), "/model ");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Model(None)));
        type_text(&mut a, "/model");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Model(None)));

        // Substring matches follow prefix matches; esc hides the menu until the text changes.
        type_text(&mut a, "/x");
        assert_eq!(names(&a), ["/exit"]);
        a.key(key(KeyCode::Esc));
        assert!(a.menu().is_empty());
        a.key(key(KeyCode::Backspace));
        assert_eq!(names(&a).len(), COMMANDS.len());
        // With the menu open, up and down move the selection rather than the history.
        a.key(key(KeyCode::Up));
        assert_eq!(a.menu_selected(), COMMANDS.len() - 1);
    }

    #[test]
    fn reasoning_streams_into_its_own_entry() {
        let mut a = app();
        type_text(&mut a, "go");
        a.key(key(KeyCode::Enter));
        a.agent(AgentEvent::Reasoning("let me ".into()));
        a.agent(AgentEvent::Reasoning("think".into()));
        assert_eq!(a.busy, Some(Activity::Thinking));
        a.agent(AgentEvent::Text("answer".into()));
        let n = a.entries.len();
        assert_eq!(a.entries[n - 2], Entry::Thinking("let me think".into()));
        assert_eq!(a.entries[n - 1], Entry::Assistant("answer".into()));
        a.key(ctrl('t'));
        assert!(!a.show_thinking);
    }

    #[test]
    fn a_resumed_session_is_redrawn_from_its_items() {
        use crate::provider::{Api, ToolCall, ToolResult};
        let items = vec![
            Item::User("fix it".into()),
            Item::Assistant(vec![
                Part::Native {
                    api: Api::Openai,
                    block: serde_json::json!({"reasoning_content": "look first"}),
                },
                Part::Text("Reading.".into()),
                Part::Call(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: Ok(serde_json::json!({"path": "src/x.rs"})),
                }),
            ]),
            Item::ToolResults(vec![ToolResult {
                id: "c1".into(),
                images: Vec::new(),
                content: "fn x() {}".into(),
                is_error: false,
                diff: None,
            }]),
            Item::Assistant(vec![Part::Text("Done.".into())]),
        ];
        let mut a = app();
        a.agent(AgentEvent::Resumed(Resumed {
            items,
            usage: crate::provider::Usage::default(),
            cost: 0.0,
            context: 0,
        }));
        assert_eq!(
            a.entries[1..],
            [
                Entry::User("fix it".into()),
                Entry::Thinking("look first".into()),
                Entry::Assistant("Reading.".into()),
                Entry::Tool {
                    name: "read".into(),
                    detail: "src/x.rs".into(),
                    result: Some((true, "fn x() {}".into())),
                    diff: None,
                },
                Entry::Assistant("Done.".into()),
            ]
        );
        assert!(matches!(&a.entries[0], Entry::Note(_, t) if t == "resumed a session of 1 prompt"));
    }

    #[test]
    fn dialogs_take_the_keys_and_commands_open_them() {
        let mut a = app();
        type_text(&mut a, "/settings");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::OpenSettings));
        type_text(&mut a, "/resume");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::OpenResume));
        a.resume = Some(SessionPicker::new(Vec::new()));
        type_text(&mut a, "abc");
        assert!(a.editor.is_empty(), "typing goes to the picker's filter");
        assert_eq!(a.resume.as_ref().unwrap().filter, "abc");
        a.key(key(KeyCode::Esc));
        assert!(a.resume.is_none());
    }

    #[test]
    fn a_drag_selects_the_text_under_it_and_the_release_copies_it() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 20, 5),
            top: 0,
            lines: vec!["› hello world".into(), "  second line".into()],
        });
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        // Press at the `h`, drag to just after `second`.
        assert_eq!(
            a.mouse(ev(MouseEventKind::Down(MouseButton::Left), 2, 0)),
            None
        );
        assert_eq!(
            a.mouse(ev(MouseEventKind::Drag(MouseButton::Left), 8, 1)),
            None
        );
        assert_eq!(
            a.selected_text().as_deref(),
            Some("hello world\n  second"),
            "both ends and the line between them"
        );
        // The button coming up is what copies, and the highlight stays to show what was taken.
        assert_eq!(
            a.mouse(ev(MouseEventKind::Up(MouseButton::Left), 8, 1)),
            Some(Action::Copy(
                "hello world\n  second".into(),
                "the selection"
            ))
        );
        assert!(a.selection.is_some_and(|s| !s.dragging));
    }

    #[test]
    fn a_drag_backwards_selects_the_same_text() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 20, 5),
            top: 0,
            lines: vec!["one two three".into()],
        });
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(ev(MouseEventKind::Down(MouseButton::Left), 8, 0));
        a.mouse(ev(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        // Columns 4..8 are `two `; the padding at the end is not part of the copy.
        assert_eq!(a.selected_text().as_deref(), Some("two"));
    }

    #[test]
    fn a_click_that_selects_nothing_copies_nothing() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 20, 5),
            top: 0,
            lines: vec!["some text".into()],
        });
        let ev = |kind| MouseEvent {
            kind,
            column: 3,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(ev(MouseEventKind::Down(MouseButton::Left)));
        assert_eq!(a.mouse(ev(MouseEventKind::Up(MouseButton::Left))), None);
        assert!(
            a.selection.is_none(),
            "an empty highlight is not left behind"
        );
        assert_eq!(a.selected_text(), None);
    }

    #[test]
    fn dragging_past_an_edge_scrolls_and_keeps_selecting() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 4, 20, 5),
            top: 10,
            lines: (0..20).map(|i| format!("line {i}")).collect(),
        });
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(ev(MouseEventKind::Down(MouseButton::Left), 0, 6));
        // Pulled above the transcript: it scrolls up a line at a time and the selection follows.
        a.mouse(ev(MouseEventKind::Drag(MouseButton::Left), 0, 2));
        assert_eq!(a.scroll, 1);
        assert_eq!(a.selection.unwrap().focus, Cell { line: 9, ch: 0 });
        // Pulled below it: the other way, to the end of the last visible line.
        a.mouse(ev(MouseEventKind::Drag(MouseButton::Left), 0, 12));
        assert_eq!(a.scroll, 0);
        assert_eq!(
            a.selection.unwrap().focus,
            Cell {
                line: 15,
                ch: usize::MAX
            }
        );
    }

    #[test]
    fn a_click_maps_through_the_drawn_text_including_wide_characters() {
        assert_eq!(char_at("› hello", 0), 0);
        assert_eq!(char_at("› hello", 2), 2);
        assert_eq!(char_at("› hello", 99), 7, "past the end is the end");
        // A wide character takes two cells, so its cell and the one after it are the same index.
        assert_eq!(char_at("日本x", 0), 0);
        assert_eq!(char_at("日本x", 2), 1);
        assert_eq!(char_at("日本x", 3), 2);
    }

    #[test]
    fn a_dialog_takes_the_mouse_so_the_transcript_is_not_selectable() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 20, 5),
            top: 0,
            lines: vec!["some text".into()],
        });
        a.resume = Some(SessionPicker::new(Vec::new()));
        let ev = |kind| MouseEvent {
            kind,
            column: 3,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(ev(MouseEventKind::Down(MouseButton::Left)));
        assert!(a.selection.is_none());
    }

    #[test]
    fn jumping_to_the_end_of_the_transcript() {
        let mut a = app();
        a.scroll = 40;
        a.key(key(KeyCode::End));
        assert_eq!(a.scroll, 0, "end jumps when the editor is empty");
        a.key(key(KeyCode::Home));
        assert_eq!(
            a.scroll,
            usize::MAX,
            "home goes to the top; the next draw clamps it"
        );

        a.scroll = 40;
        type_text(&mut a, "draft");
        a.key(key(KeyCode::Home));
        assert_eq!(a.scroll, 40, "with text, home moves the cursor instead");
        a.key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert_eq!(a.scroll, 0);

        a.scroll = 40;
        a.jump_badge = Some(Rect::new(50, 20, 30, 1));
        let click = |column, row| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(click(10, 20));
        assert_eq!(a.scroll, 40, "a click elsewhere does nothing");
        a.mouse(click(60, 20));
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn a_fatal_error_stops_new_prompts() {
        let mut a = app();
        type_text(&mut a, "x");
        a.key(key(KeyCode::Enter));
        a.agent(AgentEvent::Fatal("writing the audit log".into()));
        type_text(&mut a, "y");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert!(
            matches!(a.entries.last(), Some(Entry::Note(NoteKind::Error, t)) if t.contains("ctrl+c quits"))
        );
    }
}
