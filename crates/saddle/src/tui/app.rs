use super::ask::{Answer as AskAnswer, AskDialog};
use super::files;
use crate::agent::shared_shell;
use crate::agent::shell_ok;
use crate::provider;
use crate::provider::Image;
use crate::provider::Quota;
use crate::provider::{reasoning_text, visible_tool_text};
use crate::questions::Asked;
use crate::skills;
use crate::skills::Skill;
use crate::store::Point;
use crate::team::{BusMessage, QueueItem, Render};
use crate::tools::timeout;
use crate::tui::image::Fit;
use crate::tui::image::Protocol;
use crate::tui::keys::Keys;
use crate::tui::math;
use crate::tui::math::Formula;
use crate::tui::notify::Notify;
use crate::tui::permissions::Change as PermissionsChange;
use crate::tui::permissions::Permissions;
use crate::tui::status::Template;
use crate::tui::theme::Theme;
use crate::tui::themes::ThemePicker;
use crate::tui::tree::Tree;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::mem;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use super::editor::Editor;
use super::login::{Draft, Listed, Login, LoginRequest, Outcome, Pick};
use super::render::home_relative;
use super::sessions::{PickOutcome, SessionPicker};
use super::settings::{Change, SettingsDialog, SettingsOutcome};
use crate::agent::call_detail;
use crate::agent::{End, Resumed};
use crate::compact::Event as CompactEvent;
use crate::plugins::services::panel::PanelView;
use crate::provider::ModelsError;
use crate::provider::Usage;
use crate::provider::{Contribution, Item, Part};
use crate::tasks::{State as TaskState, View as TaskView};
use crate::term::clean;
use crate::tui::permissions::Outcome as PermissionOutcome;
use crate::tui::themes::Outcome as ThemeOutcome;
use crate::tui::tree::TreeOutcome;

#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Contribution(Contribution),
    Panels(Vec<PanelView>),
    PluginUsage {
        usage: Usage,
        cost: f64,
    },
    Text(String),
    Reasoning(String),
    Note(String),
    Call {
        id: String,
        name: String,
        detail: String,
        timeout: Option<u64>,
    },
    Result {
        ok: bool,
        content: String,
        diff: Option<String>,
        images: Vec<Image>,
    },
    Steered(String),
    Reply {
        usage: Usage,
        cost: f64,
    },
    Quota(Quota),
    Skills(Vec<Skill>),
    Tasks(Vec<TaskView>),
    Workers(Vec<Render>),
    Bus(Vec<BusMessage>),
    Queue(Vec<QueueItem>),
    BackgroundReady,
    TurnEnd(End),
    Fatal(String),
    Closed,
    Models(u64, Result<Listed, ModelsError>),
    Ask(Arc<Asked>),
    DeviceCode(u64, Result<(String, String), String>),
    Unopened(String),
    SignedIn(u64, Result<(), String>),
    Listed,
    Resumed(Resumed),
    Tree(Vec<Point>),
    Attached(Vec<Image>),
    Compaction(CompactEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effort {
    Cycle,
    Set(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Send(String),
    Steer(String),
    Skill { name: String, prompt: String },
    Reload,
    Theme(Option<String>),
    Clear,
    Cancel,
    Quit,
    OpenLogin,
    SignIn(u64, String),
    CheckModels(u64, Draft),
    ListModels(u64),
    Login(LoginRequest),
    Logout(Option<String>),
    Model(Option<String>),
    Use(Pick),
    Default(Pick),
    Restore(Pick),
    SettleWindow,
    Grant(PermissionsChange),
    OpenPermissions,
    OpenTree,
    Rewind { node: Option<i64>, summarize: bool },
    Clone,
    Name(Option<String>),
    SessionInfo,
    Mcp(String),
    Tasks(String),
    Workers,
    OpenWorker(String),
    WorkerInput(String),
    WorkerBack,
    WorkerInterrupt(String),
    WorkerStop(String),
    Profile(String),
    BackgroundTurn,
    DeleteSession(String),
    Effort(Effort),
    Copy(String, &'static str),
    OpenUrl(String),
    OpenResume,
    Resume(String),
    OpenSettings,
    Apply(Change),
    Shell { command: String, share: bool },
    EditPrompt,
    Compact(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub line: usize,
    pub ch: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: Cell,
    pub focus: Cell,
    pub dragging: bool,
}

impl Selection {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub area: Rect,
    pub top: usize,
    pub lines: Vec<String>,
    pub pictures: Vec<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub line: usize,
    pub column: u16,
    pub rows: u16,
    pub cols: u16,
    pub inline: bool,
    pub typst: bool,
    pub tex: String,
}

#[derive(Debug, Clone)]
pub struct Placed {
    pub line: usize,
    pub column: u16,
    pub id: u32,
    pub box_: Fit,
    pub pixels: (u32, u32),
    pub data: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drawn {
    pub shown: Vec<(usize, u16, u32, u16, u16)>,
    pub held: Vec<u32>,
    pub at: Option<Rect>,
    pub covered: Vec<Rect>,
    pub stale: bool,
}

pub const DOUBLE_ESCAPE: Duration = Duration::from_millis(500);

pub const COPY_NOTE: Duration = Duration::from_secs(3);

pub struct Popup {
    pub items: Vec<(String, String)>,
    pub selected: usize,
    pub hint: &'static str,
}

const PICKER_FILES: usize = 20_000;
const PICKER_REFRESH: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteKind {
    Plugin,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    Running(Instant),
    Took(Duration),
}

impl Spent {
    pub fn elapsed(self, now: Instant) -> Duration {
        match self {
            Self::Running(started) => now.saturating_duration_since(started),
            Self::Took(took) => took,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool {
        id: String,
        tasks: Vec<TaskView>,
        name: String,
        detail: String,
        result: Option<(bool, String)>,
        diff: Option<String>,
        timeout: Option<u64>,
        spent: Option<Spent>,
    },
    Note(NoteKind, String),
    Image(Image),
    Compaction(Box<CompactionView>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionView {
    pub header: String,
    pub reasoning: String,
    pub summary: String,
    pub state: CompactState,
    pub blocks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactState {
    Running,
    Done(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub left: usize,
    pub rewind_to: Option<i64>,
    pub text: Option<String>,
}

impl Confirm {
    pub fn asked(&self) -> (String, String) {
        let (what, it) = if self.left == 1 {
            ("1 message would be left behind".to_owned(), "it")
        } else {
            (
                format!("{} messages would be left behind", self.left),
                "they",
            )
        };
        (
            what,
            format!("summarize what {it} established onto the branch you are entering?"),
        )
    }
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
    pub notes: Vec<String>,
    pub context_window: u64,
    pub priced: bool,
    pub status: Template,
    pub mouse: bool,
    pub show_reasoning: bool,
    pub expand_tools: bool,
    pub max_steps: Option<usize>,
    pub images: Option<String>,
    pub theme: Option<String>,
    pub notify: Notify,
    pub math_scale: f32,
    pub keys: Keys,
}

pub struct Cached {
    pub width: u16,
    pub expand: bool,
    pub thinking: bool,
    pub lines: Vec<Line<'static>>,
    pub pictures: Vec<Formula>,
}

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

pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/login",
        args: "",
        about: "choose a provider and model, add a key, or add a server",
    },
    SlashCommand {
        name: "/model",
        args: "[name]",
        about: "choose a model from any provider, or switch to the one named; this session only",
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
        name: "/profile",
        args: "[name]",
        about: "the set of grants in force: list config.toml's profiles, or switch to one",
    },
    SlashCommand {
        name: "/new",
        args: "",
        about: "start a new session in this workspace; the last one stays saved",
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
        name: "/tasks",
        args: "[cancel <id>]",
        about: "list background shell tasks or cancel one",
    },
    SlashCommand {
        name: "/workers",
        args: "",
        about: "the live workers of this session: flip through them, steer, interrupt or stop one (ctrl+b)",
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
        about: "the same as /new: a new session, with the last one saved",
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
    pub skills: Vec<Skill>,
    pub entries: Vec<Entry>,
    pub plugin_panels: Vec<PanelView>,
    pub cache: Vec<Option<Cached>>,
    pub editor: Editor,
    pub queue: VecDeque<String>,
    pub steering: VecDeque<String>,
    pub busy: Option<Activity>,
    pub running_tasks: usize,
    pub cancelling: bool,
    pub stopped: Option<String>,
    pub expand_tools: bool,
    pub show_thinking: bool,
    pub scroll: usize,
    pub view_height: usize,
    pub jump_badge: Option<Rect>,
    pub selection: Option<Selection>,
    pub seen: Option<Seen>,
    pub copied: Option<Instant>,
    pub copied_what: &'static str,
    pub open_url_at: Option<Rect>,
    pub dialog_seen: Option<Seen>,
    last_escape: Option<Instant>,
    pub total_lines: usize,
    pub images: Vec<Placed>,
    pub covered: Vec<Rect>,
    pub protocol: Protocol,
    pub theme: Theme,
    pub drawn: Drawn,
    pub repaint: bool,
    pub usage: Usage,
    pub cost: f64,
    pub quota: Option<Quota>,
    pub context: u64,
    pub spinner: usize,
    pub now: Instant,
    pub quit: bool,
    pub login: Option<Login>,
    pub resume: Option<SessionPicker>,
    pub settings: Option<SettingsDialog>,
    pub tree: Option<Tree>,
    pub team: Vec<Render>,
    pub bus: Vec<BusMessage>,
    pub jobs: Vec<QueueItem>,
    pub board_scroll: usize,
    pub board_height: usize,
    pub overview: Option<usize>,
    pub worker: Option<String>,
    pub worker_scroll: usize,
    pub worker_total: usize,
    pub worker_height: usize,
    pub confirm: Option<Confirm>,
    pub permissions: Option<Permissions>,
    pub themes: Option<ThemePicker>,
    pub ask: Option<AskDialog>,
    theme_before: Option<(String, Theme)>,
    pub max_steps: Option<usize>,
    history: Vec<String>,
    history_pos: Option<usize>,
    draft: String,
    menu_index: usize,
    menu_for: String,
    menu_dismissed: Option<String>,
    files: Option<(Vec<String>, Instant)>,
}

const KEYS: &str = "\
enter        send; while the agent works, enter steers it at the next request
alt+enter    newline; while the agent works, queue the prompt for after the turn
             (shift+enter and ctrl+j insert a newline too)
esc          cancel the running turn; queued prompts return to the editor
ctrl+c       cancel, clear the editor, or quit when it is empty
ctrl+d       quit when the editor is empty
up/down      move between lines, or through earlier prompts
ctrl+g       write the prompt in $EDITOR
@path        attach a file to the prompt (type @ to pick one)
!cmd         run a command in the sandbox and share its output; !!cmd keeps it to you
pgup/pgdn    scroll the transcript (also the mouse wheel)
drag         select text — in the transcript, or in the dialog that is open; it is
             highlighted and copied on release
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
    pub fn cached_image(&self, i: usize) -> Option<Image> {
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
            plugin_panels: Vec::new(),
            cache: Vec::new(),
            editor: Editor::default(),
            queue: VecDeque::new(),
            steering: VecDeque::new(),
            busy: None,
            running_tasks: 0,
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
            dialog_seen: None,
            last_escape: None,
            total_lines: 0,
            images: Vec::new(),
            covered: Vec::new(),
            protocol: Protocol::Text,
            theme: Theme::default(),
            drawn: Drawn::default(),
            repaint: false,
            usage: Usage::default(),
            cost: 0.0,
            quota: None,
            context: 0,
            spinner: 0,
            now: Instant::now(),
            quit: false,
            login: None,
            resume: None,
            settings: None,
            tree: None,
            team: Vec::new(),
            bus: Vec::new(),
            jobs: Vec::new(),
            board_scroll: 0,
            board_height: 0,
            overview: None,
            worker: None,
            worker_scroll: 0,
            worker_total: 0,
            worker_height: 0,
            confirm: None,
            permissions: None,
            themes: None,
            ask: None,
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
        for note in mem::take(&mut app.header.notes) {
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

    pub fn dialog_open(&self) -> bool {
        self.login.is_some()
            || self.resume.is_some()
            || self.settings.is_some()
            || self.themes.is_some()
            || self.ask.is_some()
    }

    fn compaction(&mut self, event: CompactEvent) {
        let current = self
            .entries
            .iter()
            .rposition(|e| matches!(e, Entry::Compaction(v) if v.state == CompactState::Running));
        match event {
            CompactEvent::Started {
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
                if self.busy.is_some() && reason.blocks() {
                    self.busy = Some(Activity::Compacting);
                }
            }
            CompactEvent::Reasoning(s) => {
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    v.reasoning.push_str(&display(&s));
                    self.cache[i] = None;
                }
            }
            CompactEvent::Text(s) => {
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    v.summary.push_str(&display(&s));
                    self.cache[i] = None;
                }
            }
            CompactEvent::Done {
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
            CompactEvent::Failed(why) => {
                if let Some(i) = current
                    && let Entry::Compaction(v) = &mut self.entries[i]
                {
                    v.state = CompactState::Failed(display(&why));
                    self.cache[i] = None;
                }
            }
        }
    }

    fn show_resumed(&mut self, items: &[Item]) {
        self.entries.clear();
        self.cache.clear();
        self.drawn.stale = true;
        self.selection = None;
        self.scroll = 0;
        let prompts = items.iter().filter(|i| i.prompt().is_some()).count();
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

    pub fn background_running(&self) -> bool {
        self.running_tasks != 0
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        self.now = Instant::now();
        if self.ask.as_ref().is_some_and(|dialog| !dialog.asked.open()) {
            self.ask = None;
        }
    }

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
            let list = files::list_files(&self.header.workspace, PICKER_FILES);
            self.files = Some((list, Instant::now()));
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
                if chosen.args.is_empty() || self.editor.text() == chosen.name {
                    self.editor.set(chosen.name.clone());
                    return Some(self.submit(false));
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
        if let Some(dialog) = &mut self.ask {
            return match dialog.key(key) {
                AskAnswer::Stay => None,
                AskAnswer::Pass => self.transcript_key(key),
                AskAnswer::Given(outcome) => {
                    if let Some(dialog) = self.ask.take() {
                        dialog.asked.answer(outcome);
                    }
                    None
                }
                AskAnswer::Cancel => {
                    self.ask = None;
                    Some(Action::Cancel)
                }
            };
        }
        if key.code != KeyCode::Esc {
            self.last_escape = None;
        }
        if self.confirm.is_some() {
            return self.answer(key);
        }
        if let Some(selected) = self.overview {
            let sessions = self.team.len() + 1;
            let rows = sessions + 2;
            let selected = selected.min(rows - 1);
            let next = (selected + 1) % rows;
            let previous = (selected + rows - 1) % rows;
            return match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.overview = None;
                    self.selection = None;
                    None
                }
                KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.overview = None;
                    self.selection = None;
                    None
                }
                KeyCode::Tab | KeyCode::Down | KeyCode::Right | KeyCode::Char('j') => {
                    self.overview = Some(next);
                    self.worker_scroll = 0;
                    self.board_scroll = 0;
                    self.selection = None;
                    None
                }
                KeyCode::BackTab | KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
                    self.overview = Some(previous);
                    self.worker_scroll = 0;
                    self.board_scroll = 0;
                    self.selection = None;
                    None
                }
                KeyCode::Char(digit @ '0'..='9') => {
                    let index = digit as usize - '0' as usize;
                    self.overview = Some(index.min(rows - 1));
                    self.worker_scroll = 0;
                    self.board_scroll = 0;
                    self.selection = None;
                    None
                }
                KeyCode::PageUp => {
                    self.overview_scroll(selected, sessions, true);
                    None
                }
                KeyCode::PageDown => {
                    self.overview_scroll(selected, sessions, false);
                    None
                }
                KeyCode::Home => {
                    self.overview_jump(selected, sessions, false);
                    None
                }
                KeyCode::End => {
                    self.overview_jump(selected, sessions, true);
                    None
                }
                KeyCode::Enter if selected > 0 && selected < sessions => {
                    let name = self.team[selected - 1].name.clone();
                    self.overview = None;
                    self.worker = Some(name.clone());
                    self.worker_scroll = 0;
                    self.selection = None;
                    Some(Action::OpenWorker(name))
                }
                KeyCode::Enter if selected == 0 => {
                    self.overview = None;
                    self.selection = None;
                    None
                }
                KeyCode::Char('i') if selected > 0 && selected < sessions => Some(
                    Action::WorkerInterrupt(self.team[selected - 1].name.clone()),
                ),
                KeyCode::Char('x') if selected > 0 && selected < sessions => {
                    Some(Action::WorkerStop(self.team[selected - 1].name.clone()))
                }
                _ => None,
            };
        }
        if self.worker.is_some() {
            let page = self.worker_height.saturating_sub(1).max(1);
            let handled = match key.code {
                KeyCode::Up => {
                    self.worker_scroll = (self.worker_scroll + 1).min(self.worker_total);
                    true
                }
                KeyCode::Down => {
                    self.worker_scroll = self.worker_scroll.saturating_sub(1);
                    true
                }
                KeyCode::PageUp => {
                    self.worker_scroll = (self.worker_scroll + page).min(self.worker_total);
                    true
                }
                KeyCode::PageDown => {
                    self.worker_scroll = self.worker_scroll.saturating_sub(page);
                    true
                }
                KeyCode::Home => {
                    self.worker_scroll = self.worker_total;
                    true
                }
                KeyCode::End => {
                    self.worker_scroll = 0;
                    true
                }
                _ => false,
            };
            if handled {
                return None;
            }
        }
        if self.worker.is_some()
            && (key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('b')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            let name = self.worker.take().unwrap_or_default();
            self.worker_scroll = 0;
            self.selection = None;
            self.overview = Some(
                self.team
                    .iter()
                    .position(|worker| worker.name == name)
                    .map_or(0, |index| index + 1),
            );
            return Some(Action::WorkerBack);
        }
        if key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.open_overview();
            return Some(Action::Workers);
        }
        if let Some(login) = &mut self.login {
            return match login.key(key) {
                Outcome::Stay => None,
                Outcome::Close => {
                    self.login = None;
                    None
                }
                Outcome::Check(id, draft) => Some(Action::CheckModels(id, draft)),
                Outcome::ListAll(id) => Some(Action::ListModels(id)),
                Outcome::SignIn(id, provider) => Some(Action::SignIn(id, provider)),
                Outcome::OpenLink(url) => Some(Action::OpenUrl(url)),
                Outcome::CopyLink(url) => Some(Action::Copy(url, "the sign-in link")),
                Outcome::CancelSignIn(_) => None,
                Outcome::Submit(req) => {
                    self.login = None;
                    Some(Action::Login(req))
                }
                Outcome::Use(pick) => {
                    self.login = None;
                    Some(Action::Use(pick))
                }
                Outcome::Default(pick) => {
                    self.login = None;
                    Some(Action::Default(pick))
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
                TreeOutcome::Stay => None,
                TreeOutcome::Close => {
                    self.tree = None;
                    None
                }
                TreeOutcome::Select {
                    rewind_to,
                    text,
                    left,
                } => {
                    self.tree = None;
                    if left > 0 {
                        self.confirm = Some(Confirm {
                            left,
                            rewind_to,
                            text,
                        });
                        return None;
                    }
                    self.move_to(rewind_to, text, false)
                }
                TreeOutcome::Copy(text) => Some(Action::Copy(text, "the branch")),
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
                if self.editor.is_empty() {
                    let twice = self
                        .last_escape
                        .is_some_and(|at| at.elapsed() < DOUBLE_ESCAPE);
                    self.last_escape = Some(Instant::now());
                    if twice {
                        self.last_escape = None;
                        return Some(Action::OpenTree);
                    }
                }
            }
            KeyCode::Enter if alt && self.busy.is_some() => return self.submit(true),
            KeyCode::Enter if shift || alt => self.editor.insert('\n'),
            KeyCode::Char('j') if ctrl => self.editor.insert('\n'),
            KeyCode::Enter => return self.submit(false),
            KeyCode::Char('o') if ctrl => {
                self.expand_tools = !self.expand_tools;
            }
            KeyCode::Char('t') if ctrl => {
                self.show_thinking = !self.show_thinking;
            }
            KeyCode::BackTab => return Some(Action::Effort(Effort::Cycle)),
            KeyCode::Char('g') if ctrl => return Some(Action::EditPrompt),
            KeyCode::PageUp => self.scroll_by(self.page() as isize),
            KeyCode::PageDown => self.scroll_by(-(self.page() as isize)),
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
        if let Some(dialog) = &mut self.ask {
            dialog.paste(text);
            return;
        }
        if let Some(login) = &mut self.login {
            login.paste(text);
        } else if let Some(picker) = &mut self.resume {
            picker.paste(text);
        } else if self.settings.is_none() && self.themes.is_none() {
            self.editor.insert_str(text);
        }
    }

    pub fn open_themes(&mut self, mut picker: ThemePicker) {
        let name = self
            .header
            .theme
            .clone()
            .unwrap_or_else(|| self.theme.name.clone());
        picker.original = name.clone();
        self.theme_before = Some((name, self.theme.clone()));
        self.themes = Some(picker);
        self.preview_theme();
    }

    fn preview_theme(&mut self) {
        if let Some(theme) = self.themes.as_ref().and_then(|p| p.preview()).cloned() {
            if let Some(picker) = &self.themes {
                self.header.theme = Some(picker.current().name.clone());
            }
            self.theme = theme;
        }
    }

    pub fn themes_key(&mut self, key: KeyEvent) -> Option<Action> {
        let before = self.theme_before.clone();
        let outcome = self.themes.as_mut()?.key(key);
        match outcome {
            ThemeOutcome::Stay => {
                self.preview_theme();
                None
            }
            ThemeOutcome::Cancel => {
                if let Some((name, theme)) = before {
                    self.theme = theme;
                    self.header.theme = Some(name);
                }
                self.themes = None;
                self.theme_before = None;
                None
            }
            ThemeOutcome::Choose(name) => {
                self.preview_theme();
                self.themes = None;
                self.theme_before = None;
                Some(Action::Theme(Some(name)))
            }
        }
    }

    pub fn permissions_key(&mut self, key: KeyEvent) -> Option<Action> {
        let dialog = self.permissions.as_mut()?;
        match dialog.key(key) {
            PermissionOutcome::Stay => None,
            PermissionOutcome::Close => {
                self.permissions = None;
                None
            }
            PermissionOutcome::Apply(change) => Some(Action::Grant(change)),
        }
    }

    pub fn mouse(&mut self, ev: MouseEvent) -> Option<Action> {
        let overlay = self.login.is_some()
            || self.resume.is_some()
            || self.settings.is_some()
            || self.permissions.is_some()
            || self.themes.is_some();
        match ev.kind {
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
            MouseEventKind::Down(MouseButton::Left) if !overlay || self.dialog_seen.is_some() => {
                let cell = self.cell_at(&ev)?;
                self.selection = Some(Selection {
                    anchor: cell,
                    focus: cell,
                    dragging: true,
                });
                None
            }
            MouseEventKind::Drag(MouseButton::Left) if !overlay || self.dialog_seen.is_some() => {
                self.extend(ev)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let on_link = self
                    .open_url_at
                    .is_some_and(|r| r.contains(Position::new(ev.column, ev.row)));
                let mut sel = self.selection?;
                if !sel.dragging {
                    return None;
                }
                sel.dragging = false;
                self.selection = Some(sel);
                if sel.is_empty() {
                    self.selection = None;
                    return on_link.then(|| {
                        self.login
                            .as_ref()
                            .and_then(|login| login.link())
                            .map(Action::OpenUrl)
                    })?;
                }
                self.selected_text()
                    .map(|text| Action::Copy(text, "the selection"))
            }
            _ => None,
        }
    }

    fn extend(&mut self, ev: MouseEvent) -> Option<Action> {
        let mut sel = self.selection?;
        if !sel.dragging {
            return None;
        }
        let seen = self.surface()?;
        let (area, top, len) = (seen.area, seen.top, seen.lines.len());
        if self.dialog_seen.is_some() {
            let row = ev.row.clamp(area.y, area.bottom().saturating_sub(1));
            let line = (top + usize::from(row - area.y)).min(len.saturating_sub(1));
            let ch = char_at(&seen.lines[line], ev.column.saturating_sub(area.x));
            sel.focus = Cell { line, ch };
            self.selection = Some(sel);
            return None;
        }
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

    fn surface(&self) -> Option<&Seen> {
        self.dialog_seen.as_ref().or(self.seen.as_ref())
    }

    fn cell_at(&self, ev: &MouseEvent) -> Option<Cell> {
        let seen = self.surface()?;
        let area = seen.area;
        if seen.lines.is_empty() || area.height == 0 {
            return None;
        }
        let row = ev.row.clamp(area.y, area.bottom().saturating_sub(1));
        let line = (seen.top + usize::from(row - area.y)).min(seen.lines.len() - 1);
        let ch = char_at(&seen.lines[line], ev.column.saturating_sub(area.x));
        Some(Cell { line, ch })
    }

    fn answer(&mut self, key: KeyEvent) -> Option<Action> {
        let confirm = self.confirm.take()?;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.move_to(confirm.rewind_to, confirm.text, true)
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.move_to(confirm.rewind_to, confirm.text, false)
            }
            KeyCode::Esc => None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => None,
            _ => {
                self.confirm = Some(confirm);
                None
            }
        }
    }

    fn move_to(
        &mut self,
        node: Option<i64>,
        text: Option<String>,
        summarize: bool,
    ) -> Option<Action> {
        if let Some(text) = text {
            self.editor.set(text);
        }
        Some(Action::Rewind { node, summarize })
    }

    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let seen = self.surface()?;
        if sel.is_empty() || seen.lines.is_empty() {
            return None;
        }
        let (first, last) = sel.ends();
        let last_line = last.line.min(seen.lines.len() - 1);
        let mut parts: Vec<String> = Vec::new();
        if let Some(picture) = seen.pictures.iter().find(|p| below(p, first.line)) {
            parts.push(delimited(picture));
            parts.push(String::new());
        }
        let rows = seen.lines.get(first.line..=last_line).unwrap_or_default();
        for (offset, text) in rows.iter().enumerate() {
            let line = first.line + offset;
            let mut chars: Vec<char> = text.chars().collect();
            let width = chars.len().max(cells_end(line, &seen.pictures));
            chars.resize(width, ' ');
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
            let (part, block) = selected(&chars, from.min(to), to, line, &seen.pictures);
            let part = part.trim_end().to_owned();
            if part.is_empty() && continued(line, &seen.pictures) {
                continue;
            }
            parts.push(part);
            if block {
                parts.push(String::new());
            }
        }
        let text = parts.join("\n");
        let text = text.trim_end().to_owned();
        (!text.is_empty()).then_some(text)
    }

    fn page(&self) -> usize {
        (self.view_height / 2).max(1)
    }

    fn transcript_key(&mut self, key: KeyEvent) -> Option<Action> {
        let page = self.page() as isize;
        if self.worker.is_some() {
            let page = self.worker_height.saturating_sub(1).max(1);
            match key.code {
                KeyCode::PageUp => {
                    self.worker_scroll = (self.worker_scroll + page).min(self.worker_total)
                }
                KeyCode::PageDown => self.worker_scroll = self.worker_scroll.saturating_sub(page),
                KeyCode::Home => self.worker_scroll = self.worker_total,
                KeyCode::End => self.worker_scroll = 0,
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::PageUp => self.scroll_by(page),
            KeyCode::PageDown => self.scroll_by(-page),
            KeyCode::Home => self.scroll = usize::MAX,
            KeyCode::End => self.scroll = 0,
            _ => {}
        }
        None
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
            self.editor.set(mem::take(&mut self.draft));
        }
    }

    fn open_overview(&mut self) {
        let index = self
            .team
            .iter()
            .position(|worker| worker.state.starts_with("running"))
            .or_else(|| self.team.iter().position(|worker| !worker.finished));
        self.overview = Some(index.map_or(0, |index| index + 1));
        self.worker_scroll = 0;
        self.board_scroll = 0;
        self.selection = None;
    }

    fn overview_scroll(&mut self, selected: usize, sessions: usize, back: bool) {
        if selected >= sessions {
            let page = self.board_height.saturating_sub(1).max(1);
            self.board_scroll = if back {
                self.board_scroll.saturating_add(page)
            } else {
                self.board_scroll.saturating_sub(page)
            };
        } else if selected == 0 {
            let page = self.view_height.saturating_sub(1).max(1);
            self.scroll = if back {
                (self.scroll + page).min(self.total_lines)
            } else {
                self.scroll.saturating_sub(page)
            };
        } else {
            let page = self.worker_height.saturating_sub(1).max(1);
            self.worker_scroll = if back {
                (self.worker_scroll + page).min(self.worker_total)
            } else {
                self.worker_scroll.saturating_sub(page)
            };
        }
    }

    fn overview_jump(&mut self, selected: usize, sessions: usize, newest: bool) {
        if selected >= sessions {
            self.board_scroll = if newest { 0 } else { usize::MAX };
        } else if selected == 0 {
            self.scroll = if newest { 0 } else { self.total_lines };
        } else {
            self.worker_scroll = if newest { 0 } else { self.worker_total };
        }
    }

    fn submit(&mut self, queued: bool) -> Option<Action> {
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
        if self.worker.is_some() && !text.starts_with('/') {
            return Some(Action::WorkerInput(text));
        }
        if text.starts_with('/') && !text.contains('\n') {
            return self.command(&text);
        }
        if let Some(reason) = &self.stopped {
            let msg = format!("the session has stopped ({reason}); ctrl+c quits");
            self.push(Entry::Note(NoteKind::Error, msg));
            return None;
        }
        if self.busy.is_some() {
            if queued {
                self.queue.push_back(text);
                return None;
            }
            self.steering.push_back(text.clone());
            return Some(Action::Steer(text));
        }
        Some(self.start(text))
    }

    fn command(&mut self, text: &str) -> Option<Action> {
        if let Some(rest) = text.strip_prefix("/skill:") {
            let name = rest.split_whitespace().next().unwrap_or_default();
            let args = rest[name.len()..].trim();
            let Some(skill) = self.skills.iter().find(|s| s.name == name) else {
                self.note(
                    NoteKind::Error,
                    &format!(
                        "no skill `{name}`; this session has {}",
                        skills::names(&self.skills)
                    ),
                );
                return None;
            };
            let prompt = skills::invocation(skill, args);
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
                    return Some(Action::Effort(Effort::Cycle));
                }
                let level = level.to_ascii_lowercase();
                if !provider::EFFORTS.contains(&level.as_str()) {
                    self.note(
                        NoteKind::Error,
                        &format!(
                            "`{level}` is not a reasoning level; the levels are {}",
                            provider::EFFORTS.join(", ")
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
                Some(Action::Name(
                    Some(name.to_owned()).filter(|n| !n.is_empty()),
                ))
            }
            "/session" => Some(Action::SessionInfo),
            "/mcp" => Some(Action::Mcp(text["/mcp".len()..].trim().to_owned())),
            "/tasks" => Some(Action::Tasks(text["/tasks".len()..].trim().to_owned())),
            "/workers" => {
                self.open_overview();
                Some(Action::Workers)
            }
            "/profile" => Some(Action::Profile(text["/profile".len()..].trim().to_owned())),
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
            "/new" | "/clear" if self.busy.is_some() => {
                self.info(&format!(
                    "{command} waits for the turn to end; esc cancels it"
                ));
                None
            }
            "/new" | "/clear" => {
                self.entries.clear();
                self.cache.clear();
                self.drawn.stale = true;
                self.selection = None;
                self.usage = Usage::default();
                self.cost = 0.0;
                self.context = 0;
                self.info(&format!(
                    "new session in {}; the last one stays saved",
                    home_relative(&self.header.workspace)
                ));
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
            AgentEvent::Panels(panels) => self.plugin_panels = panels,
            AgentEvent::Contribution(value) => {
                self.push(Entry::Note(NoteKind::Plugin, contribution_text(&value)))
            }
            AgentEvent::Skills(skills) => self.skills = skills,
            AgentEvent::Ask(asked) => {
                self.ask = Some(AskDialog::new(asked));
                return None;
            }
            AgentEvent::Note(s) => self.push(Entry::Note(NoteKind::Warn, display(&s))),
            AgentEvent::Call {
                id,
                name,
                detail,
                timeout,
            } => {
                self.busy = Some(Activity::Tool(display(&name)));
                let name = display(&name);
                let spent = (name == "bash").then(|| Spent::Running(Instant::now()));
                self.push(Entry::Tool {
                    id,
                    tasks: Vec::new(),
                    name,
                    detail: display(&detail),
                    timeout,
                    result: None,
                    diff: None,
                    spent,
                });
            }
            AgentEvent::Result {
                ok,
                content,
                diff: change,
                images,
            } => {
                for image in images {
                    self.push(Entry::Image(image));
                }
                let running = self
                    .entries
                    .iter_mut()
                    .rposition(|e| matches!(e, Entry::Tool { result: None, .. }));
                if let Some(i) = running {
                    let now = Instant::now();
                    if let Entry::Tool {
                        result,
                        diff,
                        spent,
                        ..
                    } = &mut self.entries[i]
                    {
                        *result = Some((ok, display(&content)));
                        *diff = change.as_deref().map(display);
                        *spent = Some(Spent::Took(
                            spent.map_or(Duration::ZERO, |spent| spent.elapsed(now)),
                        ));
                    }
                    self.cache[i] = None;
                }
                if self.busy.is_some() {
                    self.busy = Some(Activity::Waiting);
                }
            }
            AgentEvent::Steered(text) => {
                if let Some(at) = self.steering.iter().position(|waiting| *waiting == text) {
                    self.steering.remove(at);
                }
                self.scroll = 0;
                self.push(Entry::User(display(&text)));
            }
            AgentEvent::PluginUsage { usage: u, cost } => {
                self.usage.input += u.input;
                self.usage.output += u.output;
                self.usage.cache_read += u.cache_read;
                self.usage.cache_write += u.cache_write;
                self.cost += cost;
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
            AgentEvent::BackgroundReady => {
                if self.busy.is_none() {
                    self.cancelling = false;
                    self.busy = Some(Activity::Waiting);
                    return Some(Action::BackgroundTurn);
                }
            }
            AgentEvent::Bus(bus) => self.bus = bus,
            AgentEvent::Queue(jobs) => self.jobs = jobs,
            AgentEvent::Workers(views) => {
                self.team = views;
                if let Some(selected) = self.overview {
                    self.overview = Some(selected.min(self.team.len() + 2));
                }
                if let Some(name) = &self.worker
                    && !self.team.iter().any(|worker| &worker.name == name)
                {
                    self.note(NoteKind::Info, &format!("worker {name} has finished"));
                }
            }
            AgentEvent::Tasks(views) => {
                self.running_tasks = views
                    .iter()
                    .filter(|view| view.state == TaskState::Running)
                    .count();
                for (index, entry) in self.entries.iter_mut().enumerate() {
                    if let Entry::Tool { id, tasks, .. } = entry {
                        let mut next: Vec<_> = views
                            .iter()
                            .filter(|view| view.call.as_ref() == Some(id))
                            .cloned()
                            .collect();
                        for previous in tasks.iter() {
                            if !next.iter().any(|view| view.id == previous.id) {
                                let mut previous = previous.clone();
                                if previous.state == TaskState::Running {
                                    previous.state = TaskState::Unavailable;
                                }
                                next.push(previous);
                            }
                        }
                        if *tasks != next {
                            *tasks = next;
                            self.cache[index] = None;
                        }
                    }
                }
            }
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
            AgentEvent::Unopened(why) => match &mut self.login {
                Some(login) => login.device.error = Some(why),
                None => self.push(Entry::Note(NoteKind::Warn, display(&why))),
            },
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
                    return Some(Action::CheckModels(id, draft));
                }
            }
            AgentEvent::Listed => return Some(Action::SettleWindow),
            AgentEvent::Resumed(saved) => {
                self.show_resumed(&saved.items);
                self.usage = saved.usage;
                self.cost = saved.cost;
                self.context = saved.context;
                if let Some(saved) = saved.model {
                    let pick = Pick {
                        provider: saved.provider,
                        model: saved.model,
                    };
                    if pick.provider != self.header.provider || pick.model != self.header.model {
                        return Some(Action::Restore(pick));
                    }
                }
            }
            AgentEvent::Tree(points) => {
                if points.is_empty() {
                    self.note(
                        NoteKind::Info,
                        "nothing to fork from yet: this session has no prompt",
                    );
                } else {
                    let mut tree = Tree::new(points);
                    tree.keys = self.header.keys.clone();
                    self.tree = Some(tree);
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

pub fn entries_from(items: &[Item]) -> Vec<Entry> {
    let mut results = HashMap::new();
    for item in items {
        if let Item::ToolResults(rs) = item {
            for r in rs {
                results.insert(
                    r.id.as_str(),
                    (
                        !r.is_error,
                        visible_tool_text(&r.content, r.provenance.as_deref()),
                        r.diff.as_deref(),
                    ),
                );
            }
        }
    }
    let mut out = Vec::new();
    for item in items {
        match item {
            Item::Contribution(value) => {
                out.push(Entry::Note(NoteKind::Plugin, contribution_text(value)))
            }
            Item::User(text) => match shared_shell(text) {
                Some((command, output)) => out.push(Entry::Tool {
                    id: String::new(),
                    tasks: Vec::new(),
                    name: "bash".into(),
                    detail: format!("$ {command}"),
                    result: Some((shell_ok(output), display(output))),
                    diff: None,
                    timeout: None,
                    spent: None,
                }),
                None => out.push(Entry::User(display(text))),
            },
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
                                id: c.id.clone(),
                                tasks: Vec::new(),
                                name: display(&c.name),
                                detail: display(&call_detail(c)),
                                result: found.map(|(ok, content, _)| (*ok, display(content))),
                                diff: found.and_then(|(_, _, d)| d.map(display)),
                                timeout: timeout::requested(c),
                                spent: None,
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
                        Part::Native { .. } => {
                            if let Some(reasoning) =
                                reasoning_text(part).filter(|r| !r.trim().is_empty())
                            {
                                out.push(Entry::Thinking(display(&reasoning)));
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

fn contribution_text(value: &Contribution) -> String {
    format!(
        "[{} contribution]\n{}",
        value.provenance.principal,
        display(visible_tool_text(&value.content, Some(&value.provenance)))
    )
}

pub fn short_tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{}k", n / 1_000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

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

fn selected(
    chars: &[char],
    from: usize,
    to: usize,
    line: usize,
    pictures: &[Source],
) -> (String, bool) {
    let mut on_row: Vec<&Source> = pictures.iter().filter(|p| p.line == line).collect();
    on_row.sort_by_key(|p| p.column);
    let mut out = String::new();
    let mut cursor = from;
    let mut block = false;
    for picture in on_row {
        let start = usize::from(picture.column);
        let end = start.saturating_add(usize::from(picture.cols));
        let (lo, hi) = (start.max(from), end.min(to));
        if lo >= hi || hi <= cursor {
            continue;
        }
        out.extend(&chars[cursor.min(lo)..lo]);
        out.push_str(&delimited(picture));
        block = block || !picture.inline;
        cursor = hi;
    }
    out.extend(&chars[cursor.min(to)..to]);
    (out, block)
}

fn delimited(picture: &Source) -> String {
    if picture.typst {
        format!(
            "```{}\n{}\n```",
            math::TYPST,
            picture.tex.trim_end_matches(['\n', '\r'])
        )
    } else if picture.inline {
        format!("\\({}\\)", picture.tex)
    } else {
        format!("\\[{}\\]", picture.tex)
    }
}

fn below(picture: &Source, line: usize) -> bool {
    !picture.inline
        && picture.line < line
        && line < picture.line.saturating_add(usize::from(picture.rows))
}

fn cells_end(line: usize, pictures: &[Source]) -> usize {
    pictures
        .iter()
        .filter(|p| p.line == line)
        .map(|p| usize::from(p.column).saturating_add(usize::from(p.cols)))
        .max()
        .unwrap_or(0)
}

fn continued(line: usize, pictures: &[Source]) -> bool {
    pictures.iter().any(|p| below(p, line))
}

fn display(s: &str) -> String {
    clean(s).replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Usage;
    use crate::provider::{Api, ToolCall, ToolResult};
    use crate::skills;
    use crate::skills::Skill;
    use crate::store::Model;
    use crate::store::Point;
    use crate::store::PointKind;
    use crate::tui::login::{Choice, Login, Outcome as LoginOutcome};
    use crate::tui::status::Template;
    use crate::tui::theme::ThemeTables;
    use crate::tui::themes::ThemePicker;

    use crate::tasks::{State as TaskState, View as TaskView};

    use serde_json::json;

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
            status: Template::default(),
            images: None,
            theme: None,
            notify: Notify::Off,
            math_scale: 1.0,
            keys: Keys::default(),
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn a_link_that_could_not_be_opened_says_so_where_the_person_is_looking() {
        let mut app = app();
        let choice = Choice {
            name: "openai-chatgpt".into(),
            detail: String::new(),
            status: "not signed in".into(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: true,
        };
        let mut login = Login::new(
            vec![choice],
            vec![],
            Some("openai-chatgpt"),
            true,
            "auth.json".into(),
        );
        let Some(LoginOutcome::SignIn(id, _)) = login.take_pending() else {
            panic!("a sign-in to start");
        };
        login.device_code(id, Ok(("https://auth.test/device".into(), "AB12".into())));
        app.open_login(login);
        let why = "this process has no display (DISPLAY and WAYLAND_DISPLAY are unset)";
        assert_eq!(app.agent(AgentEvent::Unopened(why.into())), None);
        let login = app.login.as_ref().unwrap();
        assert_eq!(login.device.error.as_deref(), Some(why));
        assert_eq!(login.device.code.as_deref(), Some("AB12"));
        assert_eq!(login.link().as_deref(), Some("https://auth.test/device"));

        app.login = None;
        let before = app.entries.len();
        app.agent(AgentEvent::Unopened(why.into()));
        assert!(app.entries.len() > before);
    }

    #[test]
    fn the_sign_in_code_goes_to_the_clipboard_when_it_arrives() {
        let mut app = app();
        let choice = Choice {
            name: "openai-chatgpt".into(),
            detail: String::new(),
            status: "not signed in".into(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: false,
            oauth: true,
        };
        let mut login = Login::new(
            vec![choice],
            vec![],
            Some("openai-chatgpt"),
            true,
            "auth.json".into(),
        );
        let Some(LoginOutcome::SignIn(id, _)) = login.take_pending() else {
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
        assert_eq!(
            app.login.as_ref().unwrap().link().as_deref(),
            Some("https://auth.test/device")
        );
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
        let mut a = app();
        assert_eq!(a.agent(AgentEvent::Listed), Some(Action::SettleWindow));
    }

    #[test]
    fn isolated_plugin_usage_adds_spending_without_replacing_main_context() {
        let mut app = app();
        app.context = 1234;
        app.agent(AgentEvent::PluginUsage {
            usage: Usage {
                input: 10,
                output: 5,
                cache_read: 2,
                cache_write: 1,
            },
            cost: 0.25,
        });
        assert_eq!(app.context, 1234);
        assert_eq!(app.usage.input, 10);
        assert_eq!(app.usage.output, 5);
        assert_eq!(app.cost, 0.25);
    }

    #[test]
    fn a_resumed_session_shows_what_it_has_already_spent() {
        let mut a = app();
        assert_eq!((a.usage.input, a.context), (0, 0));
        a.agent(AgentEvent::Resumed(Resumed {
            items: vec![Item::User("earlier".into())],
            declared: Vec::new(),
            usage: Usage {
                input: 1_500,
                output: 200,
                cache_read: 900,
                cache_write: 0,
            },
            cost: 0.42,
            context: 2_600,
            model: Some(Model {
                provider: "mock".into(),
                model: "m".into(),
            }),
        }));
        assert_eq!(
            a.agent(AgentEvent::Listed),
            Some(Action::SettleWindow),
            "a conversation on the model in force needs no switch"
        );
        assert_eq!(a.usage.input, 1_500);
        assert_eq!(a.usage.output, 200);
        assert_eq!(a.usage.cache_read, 900);
        assert_eq!(a.context, 2_600);

        a.agent(AgentEvent::Reply {
            usage: Usage {
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
    fn a_prompt_sent_during_a_turn_can_steer_it_or_wait_for_it() {
        let mut a = app();
        type_text(&mut a, "first");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Send("first".into()))
        );
        assert_eq!(a.busy, Some(Activity::Waiting));

        type_text(&mut a, "steer this");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Steer("steer this".into()))
        );
        assert_eq!(a.steering, ["steer this"]);
        assert!(a.queue.is_empty());

        type_text(&mut a, "and then this");
        assert_eq!(
            a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            None
        );
        assert_eq!(a.queue, ["and then this"]);

        a.agent(AgentEvent::Text("working".into()));
        a.agent(AgentEvent::Steered("steer this".into()));
        assert!(a.steering.is_empty());
        assert_eq!(a.entries.last(), Some(&Entry::User("steer this".into())));
        assert_eq!(a.queue, ["and then this"]);
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
        assert_eq!(
            a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            None
        );
        assert_eq!(a.queue, ["second"]);

        a.agent(AgentEvent::Text("he\x1b[2Jllo".into()));
        a.agent(AgentEvent::Text(" there".into()));
        assert_eq!(
            a.entries.last(),
            Some(&Entry::Assistant("he[2Jllo there".into()))
        );
        assert_eq!(a.busy, Some(Activity::Writing));
        a.agent(AgentEvent::Call {
            id: "test-call".into(),
            name: "bash".into(),
            detail: "ls".into(),
            timeout: None,
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
        a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut a, "draft");
        assert_eq!(a.key(key(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(a.key(key(KeyCode::Esc)), None, "one cancel at a time");
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
    fn a_new_session_starts_the_transcript_and_the_figures_again() {
        let mut a = app();
        a.push(Entry::User("earlier".into()));
        a.cost = 1.50;
        a.usage.input = 900;
        a.context = 12_000;
        type_text(&mut a, "/new");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Clear));
        assert_eq!(a.entries.len(), 1, "{:?}", a.entries);
        assert_eq!(a.cost, 0.0);
        assert_eq!(a.usage, Usage::default());
        assert_eq!(a.context, 0);
        assert!(
            matches!(&a.entries[0], Entry::Note(NoteKind::Info, t)
                if t.starts_with("new session in ")
                    && t.ends_with("; the last one stays saved")),
            "{:?}",
            a.entries[0]
        );

        a.cost = 2.0;
        type_text(&mut a, "/clear");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Clear));
        assert_eq!(a.cost, 0.0);
        assert!(
            COMMANDS
                .iter()
                .any(|c| c.name == "/new" && c.about.starts_with("start a new session"))
        );
        assert!(
            COMMANDS
                .iter()
                .any(|c| c.name == "/clear" && c.about.starts_with("the same as /new"))
        );
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
        type_text(&mut a, "/thinking high");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Effort(Effort::Set(Some("high".into()))))
        );
        type_text(&mut a, "/thinking DEFAULT");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Effort(Effort::Set(None)))
        );
        type_text(&mut a, "/thinking");
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Effort(Effort::Cycle))
        );
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
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert_eq!(a.key(key(KeyCode::Esc)), Some(Action::OpenTree));
        assert_eq!(a.key(key(KeyCode::Esc)), None);
    }

    #[test]
    fn escape_twice_needs_an_empty_editor_and_no_gap() {
        let mut a = app();
        type_text(&mut a, "half a prompt");
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert_eq!(a.editor.text(), "half a prompt");

        let mut a = app();
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        type_text(&mut a, "x");
        a.editor.clear();
        assert_eq!(a.key(key(KeyCode::Esc)), None, "the pair was broken");

        let mut a = app();
        a.busy = Some(Activity::Waiting);
        assert_eq!(a.key(key(KeyCode::Esc)), Some(Action::Cancel));
        assert_eq!(a.key(key(KeyCode::Esc)), None);
    }

    #[test]
    fn a_skill_is_in_the_menu_and_its_command_loads_the_body() {
        let mut a = app();
        a.skills = vec![Skill {
            name: "manim".into(),
            description: "Animate a proof.".into(),
            dir: PathBuf::from("/ws/.agents/skills/manim"),
            label: ".agents/skills/manim/SKILL.md".into(),
            text: "---\nname: manim\n---\nRender with Scene.".into(),
            hidden: false,
        }];

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
        let mut a = app();
        let tables: ThemeTables =
            toml::from_str("[mine]\ntext = \"#ffffff\"\naccent = \"#ff0000\"\n").unwrap();
        let was = a.theme.name.clone();
        let picker = ThemePicker::collect(&tables, "dark");
        a.open_themes(picker);
        assert_eq!(
            a.theme.name, "dark",
            "the theme it opens on is the one drawn"
        );
        assert_eq!(a.header.theme.as_deref(), Some("dark"));
        let start = "dark".to_owned();

        assert_eq!(a.key(key(KeyCode::Down)), None);
        let previewed = a.themes.as_ref().unwrap().current().name.clone();
        assert_ne!(previewed, start);
        assert_eq!(a.theme.name, previewed, "the preview is the drawing");
        assert_eq!(a.header.theme.as_deref(), Some(previewed.as_str()));

        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert!(a.themes.is_none(), "the list closed");
        assert_eq!(a.theme.name, was, "the old theme is back");
        assert_eq!(a.header.theme.as_deref(), Some(was.as_str()));

        let picker = ThemePicker::collect(&tables, &was);
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
        let mut a = app();
        let tables = ThemeTables::new();
        let name = a.theme.name.clone();
        a.open_themes(ThemePicker::collect(&tables, &name));
        assert!(a.dialog_open());
        assert!(a.menu().is_empty(), "no / menu over a dialog");
        for c in ['x', '/', 'q'] {
            assert_eq!(a.key(key(KeyCode::Char(c))), None);
        }
        assert!(a.editor.text().is_empty(), "nothing reached the editor");
    }

    #[test]
    fn reload_asks_the_agent_thread_to_read_again_and_takes_the_new_list() {
        let mut a = app();
        type_text(&mut a, "/reload");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Reload));
        a.agent(AgentEvent::Skills(vec![Skill {
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
        let mut a = app();
        a.skills = vec![Skill {
            name: "forced".into(),
            description: "Only when asked for.".into(),
            dir: PathBuf::from("/ws/.agents/skills/forced"),
            label: ".agents/skills/forced/SKILL.md".into(),
            text: "---\nname: forced\n---\nbody".into(),
            hidden: true,
        }];
        assert_eq!(
            skills::listing(&a.skills),
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
        a.skills = vec![Skill {
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
    fn task_slash_commands_route_to_human_controls_instead_of_the_model() {
        let mut a = app();
        type_text(&mut a, "/tasks cancel task-123");
        assert!(
            matches!(a.key(key(KeyCode::Enter)),Some(Action::Tasks(argument)) if argument=="cancel task-123")
        );
    }

    #[test]
    fn the_slash_menu_filters_completes_and_runs() {
        let names = |a: &App| a.menu().iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        let mut a = app();
        type_text(&mut a, "/");
        assert_eq!(names(&a).len(), COMMANDS.len());
        type_text(&mut a, "lo");
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

        type_text(&mut a, "/mo");
        assert_eq!(a.key(key(KeyCode::Enter)), None);
        assert_eq!(a.editor.text(), "/model ");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Model(None)));
        type_text(&mut a, "/model");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Model(None)));

        type_text(&mut a, "/x");
        assert_eq!(names(&a), ["/exit"]);
        a.key(key(KeyCode::Esc));
        assert!(a.menu().is_empty());
        a.key(key(KeyCode::Backspace));
        assert_eq!(names(&a).len(), COMMANDS.len());
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
    fn resumed_responses_reasoning_uses_summary_parts_without_exposing_opaque_data() {
        let items = [Item::Assistant(vec![
            Part::Native {
                api: Api::OpenaiResponses,
                block: json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"***Testing the library***"},{"type":"summary_text","text":"***Reviewing results***"}],"encrypted_content":"never display"}),
            },
            Part::Native {
                api: Api::OpenaiResponses,
                block: json!({"type":"reasoning","summary":[],"encrypted_content":"never display"}),
            },
            Part::Text("Done.".into()),
        ])];
        assert_eq!(
            entries_from(&items),
            [
                Entry::Thinking("***Testing the library***\n\n***Reviewing results***".into()),
                Entry::Assistant("Done.".into())
            ]
        );
    }

    #[test]
    fn a_running_call_counts_up_and_keeps_its_total_once_it_answers() {
        let mut a = app();
        a.agent(AgentEvent::Call {
            id: "c1".into(),
            name: "bash".into(),
            detail: "sleep 1".into(),
            timeout: None,
        });
        let Some(Entry::Tool {
            spent: Some(Spent::Running(started)),
            ..
        }) = a.entries.last()
        else {
            panic!("a call in flight is timed");
        };
        let started = *started;
        a.now = started + Duration::from_secs(3);
        assert_eq!(
            Spent::Running(started).elapsed(a.now),
            Duration::from_secs(3)
        );
        a.agent(AgentEvent::Result {
            ok: true,
            content: "exit code 0".into(),
            diff: None,
            images: Vec::new(),
        });
        let Some(Entry::Tool {
            spent: Some(Spent::Took(took)),
            ..
        }) = a.entries.last()
        else {
            panic!("an answered call keeps the time it took");
        };
        assert!(*took < Duration::from_secs(1), "{}", took.as_secs_f64());
        let frozen = *took;
        a.now += Duration::from_secs(60);
        assert_eq!(Spent::Took(frozen).elapsed(a.now), frozen);
    }

    #[test]
    fn a_resumed_shared_shell_command_is_the_block_the_run_drew() {
        let items = [
            Item::User(
                "I ran a command in the workspace:\n$ printf hi\nexit code 0\n--- stdout ---\nhi"
                    .into(),
            ),
            Item::User(
                "I ran a command in the workspace:\n$ false\n<plugin-output-abc plugin=\"standard-bash\">\nexit code 1\n</plugin-output-abc>"
                    .into(),
            ),
            Item::User("I ran a command in the workspace: yesterday".into()),
        ];
        assert_eq!(
            entries_from(&items),
            [
                Entry::Tool {
                    id: String::new(),
                    tasks: Vec::new(),
                    name: "bash".into(),
                    detail: "$ printf hi".into(),
                    timeout: None,
                    result: Some((true, "exit code 0\n--- stdout ---\nhi".into())),
                    diff: None,
                    spent: None,
                },
                Entry::Tool {
                    id: String::new(),
                    tasks: Vec::new(),
                    name: "bash".into(),
                    detail: "$ false".into(),
                    timeout: None,
                    result: Some((false, "exit code 1".into())),
                    diff: None,
                    spent: None,
                },
                Entry::User("I ran a command in the workspace: yesterday".into()),
            ]
        );
    }

    #[test]
    fn a_resumed_session_is_redrawn_from_its_items() {
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
                provenance: None,
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
            declared: Vec::new(),
            usage: Usage::default(),
            cost: 0.0,
            context: 0,
            model: None,
        }));
        assert_eq!(
            a.entries[1..],
            [
                Entry::User("fix it".into()),
                Entry::Thinking("look first".into()),
                Entry::Assistant("Reading.".into()),
                Entry::Tool {
                    id: "c1".into(),
                    tasks: Vec::new(),
                    name: "read".into(),
                    detail: "src/x.rs".into(),
                    timeout: None,
                    result: Some((true, "fn x() {}".into())),
                    diff: None,
                    spent: None,
                },
                Entry::Assistant("Done.".into()),
            ]
        );
        assert!(matches!(&a.entries[0], Entry::Note(_, t) if t == "resumed a session of 1 prompt"));
    }

    fn drawn_with_formulas() -> App {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 40, 8),
            top: 0,
            lines: vec![
                "by parts".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                "so it holds".into(),
                format!("if {} then", " ".repeat(6)),
            ],
            pictures: vec![
                Source {
                    line: 2,
                    column: 0,
                    rows: 3,
                    cols: 10,
                    inline: false,
                    typst: false,
                    tex: r"\int u\,dv=uv-\int v\,du".into(),
                },
                Source {
                    line: 6,
                    column: 3,
                    rows: 1,
                    cols: 6,
                    inline: true,
                    typst: false,
                    tex: "x^2".into(),
                },
            ],
        });
        a
    }

    #[test]
    fn a_copy_of_a_formula_is_the_tex_it_was_drawn_from() {
        let mut a = drawn_with_formulas();
        a.selection = Some(Selection {
            anchor: Cell { line: 0, ch: 0 },
            focus: Cell { line: 7, ch: 40 },
            dragging: false,
        });
        assert_eq!(
            a.selected_text().as_deref(),
            Some(
                "by parts\n\n\\[\\int u\\,dv=uv-\\int v\\,du\\]\n\nso it holds\nif \\(x^2\\) then"
            )
        );
    }

    #[test]
    fn a_copy_of_a_formula_takes_it_whole() {
        let mut a = drawn_with_formulas();
        a.selection = Some(Selection {
            anchor: Cell { line: 6, ch: 5 },
            focus: Cell { line: 6, ch: 7 },
            dragging: false,
        });
        assert_eq!(a.selected_text().as_deref(), Some("\\(x^2\\)"));
    }

    #[test]
    fn a_copy_that_begins_inside_a_formula_still_takes_it() {
        let mut a = drawn_with_formulas();
        a.selection = Some(Selection {
            anchor: Cell { line: 4, ch: 0 },
            focus: Cell { line: 5, ch: 11 },
            dragging: false,
        });
        assert_eq!(
            a.selected_text().as_deref(),
            Some("\\[\\int u\\,dv=uv-\\int v\\,du\\]\n\nso it holds")
        );
    }

    #[test]
    fn a_copy_of_text_with_no_formula_in_it_is_unchanged() {
        let mut a = drawn_with_formulas();
        a.selection = Some(Selection {
            anchor: Cell { line: 0, ch: 0 },
            focus: Cell { line: 0, ch: 8 },
            dragging: false,
        });
        assert_eq!(a.selected_text().as_deref(), Some("by parts"));
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

    fn branching_app() -> App {
        let point = |node: i64, kind: PointKind, preview: &str| Point {
            node,
            parent: (node > 1).then(|| node - 1),
            kind,
            preview: preview.into(),
            text: if kind == PointKind::User {
                format!("{preview} (full)")
            } else {
                String::new()
            },
            here: false,
            active: true,
        };
        let mut points = vec![
            point(1, PointKind::User, "first task"),
            point(2, PointKind::Assistant, "done"),
            point(3, PointKind::Tool, "2 tool results"),
            point(4, PointKind::User, "second task"),
            point(5, PointKind::Assistant, "also done"),
        ];
        points[4].here = true;
        let mut a = app();
        a.agent(AgentEvent::Tree(points));
        a
    }

    #[test]
    fn leaving_a_branch_asks_before_summarizing_it() {
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        assert_eq!(a.key(key(KeyCode::Enter)), None, "it asks before it moves");
        let confirm = a.confirm.as_ref().expect("the question is up");
        assert_eq!(confirm.left, 1, "the reply that follows the tool result");
        assert!(a.tree.is_none(), "the tree has closed");
        assert_eq!(confirm.asked().0, "1 message would be left behind");
        assert_eq!(
            confirm.asked().1,
            "summarize what it established onto the branch you are entering?"
        );
        let several = Confirm {
            left: 4,
            ..confirm.clone()
        };
        assert_eq!(several.asked().0, "4 messages would be left behind");
        assert!(
            several
                .asked()
                .1
                .starts_with("summarize what they established")
        );
        assert_eq!(a.key(key(KeyCode::Char('q'))), None);
        assert!(a.confirm.is_some());
        assert_eq!(
            a.key(key(KeyCode::Char('y'))),
            Some(Action::Rewind {
                node: Some(3),
                summarize: true,
            })
        );
        assert!(a.confirm.is_none());
        assert_eq!(a.editor.text(), "second task (full)");
    }

    #[test]
    fn a_branch_can_be_left_without_a_summary() {
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        a.key(key(KeyCode::Enter));
        assert_eq!(
            a.key(key(KeyCode::Char('n'))),
            Some(Action::Rewind {
                node: Some(3),
                summarize: false,
            })
        );
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert!(a.confirm.is_none());
        assert!(a.editor.is_empty(), "nothing was put back in the editor");
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.key(ctrl('c')), None);
        assert!(a.confirm.is_none());
        assert_eq!(a.key(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn a_move_that_leaves_nothing_behind_does_not_ask() {
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 4;
        assert_eq!(
            a.key(key(KeyCode::Enter)),
            Some(Action::Rewind {
                node: Some(5),
                summarize: false,
            })
        );
        assert!(a.confirm.is_none());
    }

    #[test]
    fn a_drag_selects_the_text_under_it_and_the_release_copies_it() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 20, 5),
            top: 0,
            lines: vec!["› hello world".into(), "  second line".into()],
            pictures: Vec::new(),
        });
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
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
            pictures: Vec::new(),
        });
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(ev(MouseEventKind::Down(MouseButton::Left), 8, 0));
        a.mouse(ev(MouseEventKind::Drag(MouseButton::Left), 4, 0));
        assert_eq!(a.selected_text().as_deref(), Some("two"));
    }

    #[test]
    fn a_click_that_selects_nothing_copies_nothing() {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 20, 5),
            top: 0,
            lines: vec!["some text".into()],
            pictures: Vec::new(),
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
            pictures: Vec::new(),
        });
        let ev = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        a.mouse(ev(MouseEventKind::Down(MouseButton::Left), 0, 6));
        a.mouse(ev(MouseEventKind::Drag(MouseButton::Left), 0, 2));
        assert_eq!(a.scroll, 1);
        assert_eq!(a.selection.unwrap().focus, Cell { line: 9, ch: 0 });
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
            pictures: Vec::new(),
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
    #[test]
    fn background_updates_keep_launches_pending_without_stealing_other_results() {
        let mut a = app();
        a.agent(AgentEvent::Call {
            id: "launch".into(),
            name: "bash".into(),
            detail: "sleep 9".into(),
            timeout: None,
        });
        a.agent(AgentEvent::Result {
            ok: true,
            content: "untrusted result text".into(),
            diff: None,
            images: Vec::new(),
        });
        let view = |state| TaskView {
            id: "task-host-id".into(),
            call: Some("launch".into()),
            state,
            started: Instant::now(),
            stopped: Some(Instant::now()),
        };
        a.agent(AgentEvent::Tasks(vec![view(TaskState::Running)]));
        a.agent(AgentEvent::TurnEnd(End::Answered));
        assert!(a.background_running());
        assert!(a.busy.is_none());
        a.agent(AgentEvent::Call {
            id: "other".into(),
            name: "read".into(),
            detail: "file".into(),
            timeout: None,
        });
        a.agent(AgentEvent::Result {
            ok: true,
            content: "other output".into(),
            diff: None,
            images: Vec::new(),
        });
        a.agent(AgentEvent::Tasks(vec![view(TaskState::Failed)]));
        assert!(!a.background_running());
        assert!(a.entries.iter().any(|entry| matches!(entry, Entry::Tool {id,result:Some((true, text)),tasks,..} if id == "other" && text == "other output" && tasks.is_empty())));
        a.agent(AgentEvent::Tasks(vec![view(TaskState::Running)]));
        a.agent(AgentEvent::Tasks(Vec::new()));
        assert!(
            !a.background_running(),
            "registry replacement cannot leave a permanent spinner"
        );
    }

    #[test]
    fn background_wakeup_starts_only_when_the_editor_is_idle() {
        let mut a = app();
        assert_eq!(
            a.agent(AgentEvent::BackgroundReady),
            Some(Action::BackgroundTurn)
        );
        assert_eq!(a.busy, Some(Activity::Waiting));
        assert_eq!(a.agent(AgentEvent::BackgroundReady), None);
        a.agent(AgentEvent::TurnEnd(End::Answered));
        a.start("human prompt".into());
        assert_eq!(a.agent(AgentEvent::BackgroundReady), None);
        assert_eq!(a.entries.last(), Some(&Entry::User("human prompt".into())));
    }
}
