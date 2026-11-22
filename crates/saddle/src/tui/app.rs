//! TUI state and input handling. Nothing here touches the terminal, so it is tested directly.

use super::files;
use crate::agent::shared_shell;
use crate::agent::shell_ok;
use crate::provider;
use crate::provider::Image;
use crate::provider::Quota;
use crate::provider::{reasoning_text, visible_tool_text};
use crate::skills;
use crate::skills::Skill;
use crate::store::Point;
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

/// Sent by the agent thread.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    Contribution(Contribution),
    Panels(Vec<PanelView>),
    /// isolated plugin spending does not replace the main conversation's context estimate (D156).
    PluginUsage {
        usage: Usage,
        cost: f64,
    },
    Text(String),
    /// Reasoning from a thinking model, shown dimmed above its answer.
    Reasoning(String),
    Note(String),
    Call {
        id: String,
        name: String,
        detail: String,
    },
    Result {
        ok: bool,
        content: String,
        diff: Option<String>,
        /// Pictures the call returned, such as a screenshot `read` was asked for (D58).
        images: Vec<Image>,
    },
    /// A message the user sent while the turn was running entered the conversation (D149).
    Steered(String),
    /// A reply: what it used, and what it cost (D36).
    Reply {
        usage: Usage,
        cost: f64,
    },
    /// What the provider said about the allowance left (D36).
    Quota(Quota),
    /// The skills a re-discovery found, for the `/` menu (D70).
    Skills(Vec<Skill>),
    Tasks(Vec<TaskView>),
    BackgroundReady,
    TurnEnd(End),
    /// The host failed (for example, the audit log could not be written). The session is over.
    Fatal(String),
    /// The agent thread closed the session and exited.
    Closed,
    /// The answer to a model listing started by the login dialog or the picker (D89).
    Models(u64, Result<Listed, ModelsError>),
    /// The code to type for a device sign-in: where to go, and what to enter there (D85).
    DeviceCode(u64, Result<(String, String), String>),
    /// A link could not be opened, with the reason (D86). Sent from the thread that watched the
    /// opener, so the failure a person needs to see is not lost with the click.
    Unopened(String),
    /// That sign-in finished, or failed with a reason to show (D85).
    SignedIn(u64, Result<(), String>),
    /// A listing asked for at session start stated a window for the current model, so the session
    /// can drop the guess it opened with (D34). The answer is in the model cache by now.
    Listed,
    /// A saved session was loaded: its items, what it has already spent, and the tokens its next
    /// request would carry (D35).
    Resumed(Resumed),
    /// The branch as rows, for `/tree` (D54).
    Tree(Vec<Point>),
    /// Images attached to the prompt just sent, to show beside it (D58).
    Attached(Vec<Image>),
    Compaction(CompactEvent),
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
    /// A prompt typed while a turn was running, for the agent to take at the next request (D149).
    Steer(String),
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
    /// List what every provider serves, answering with [`AgentEvent::Models`] (D89).
    ListModels(u64),
    Login(LoginRequest),
    /// Remove the stored key of the named provider, or of the current one.
    Logout(Option<String>),
    /// Show the model, or switch to the named one.
    Model(Option<String>),
    /// Use this provider and model in this conversation, leaving the default alone (D91).
    Use(Pick),
    /// Use it, and write it to config.toml as what a new session starts with (D91).
    Default(Pick),
    /// Come back to the model a resumed conversation was last using (D91).
    Restore(Pick),
    /// Settle the context window again now that the endpoint has answered.
    SettleWindow,
    /// One change from the permissions editor.
    Grant(PermissionsChange),
    /// Open the permissions editor (D45).
    OpenPermissions,
    /// Show the conversation tree, to continue from an earlier message (D54).
    OpenTree,
    /// Move the conversation to a point, keeping what came after it in the store (D54), and
    /// summarize the branch left behind only when the person said to (D167).
    Rewind {
        node: Option<i64>,
        summarize: bool,
    },
    /// Copy this conversation into a new one and continue that (D53).
    Clone,
    /// Give this conversation a name, or clear it (D55).
    Name(Option<String>),
    /// What this conversation has done, for `/session` (D55).
    SessionInfo,
    /// `/mcp`: what the granted servers are doing, or `restart <name>` for one of them (D118).
    Mcp(String),
    Tasks(String),
    /// `/profile`: report the base grants in force, or switch this session to another profile
    /// (D175). Empty is the report.
    Profile(String),
    BackgroundTurn,
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
    /// The formulas drawn in those lines, with the TeX each was typeset from, so a copy takes the
    /// words rather than the blank cells the picture left on screen (D166).
    pub pictures: Vec<Source>,
}

/// One formula as the transcript drew it: the cells it took, and the TeX that produced it (D166).
///
/// A copy cannot take the cells, because there is nothing in them: the picture is drawn over them by
/// the terminal. It takes the source instead, on the first row of the box, which is where the
/// formula begins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The first row of the picture's box.
    pub line: usize,
    /// The column its first cell is at: zero for a block, the words before it for one inside a line.
    pub column: u16,
    pub rows: u16,
    pub cols: u16,
    /// Whether it is drawn inside a line, which decides the delimiters a copy wraps it in.
    pub inline: bool,
    /// Whether the formula was written in Typst's own notation, which a copy says with the fence
    /// that asks for it instead of TeX's delimiters (D166).
    pub typst: bool,
    pub tex: String,
}

/// One image in the transcript, next to the text that announces it (D59).
#[derive(Debug, Clone)]
pub struct Placed {
    /// The transcript line the picture's box starts at: the row below a marker line for a picture
    /// announced by text, or the formula's own row when it sits inside one (D165).
    pub line: usize,
    /// The column of the picture's first cell: zero for a picture that starts its row, and the
    /// column the words before it leave for a formula inside a line (D165).
    pub column: u16,
    /// The id the terminal knows it by, so it can be taken back off the screen (D59).
    pub id: u32,
    /// The picture's full size: cells, and pixels.
    pub box_: Fit,
    pub pixels: (u32, u32),
    /// The encoded file, base64 as the protocols carry it, kept so a picture the terminal does not
    /// hold can be sent without decoding it again on a frame that only moves it (D59).
    pub data: String,
}

/// What the last frame put on the screen, so the next one can take it back off (D59).
///
/// Kitty images do not scroll with the text: the terminal keeps them at the cell they were drawn
/// on, and ratatui only rewrites characters. Drawing a scrolled picture without erasing the old one
/// leaves copies of it all over the screen, which is what scrolling used to do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drawn {
    /// What is on screen: the row and column each picture was drawn at, its id, and which band of
    /// it. The column is part of it because a formula inside a line moves with the words around it
    /// without the row changing (D165).
    pub shown: Vec<(usize, u16, u32, u16, u16)>,
    /// Ids the terminal holds the data of. It outlives `shown`: a dialog covers a picture without
    /// the picture leaving the transcript, and the data stays in the terminal's store so closing
    /// the dialog puts it back instead of sending it again (D59).
    pub held: Vec<u32>,
    /// The transcript area the frame was drawn in. A resize moves every row, and it also clears
    /// the screen — kitty deletes the images it holds when the screen is cleared — so a change here
    /// is a frame that has to send every picture again (D59).
    pub at: Option<Rect>,
    /// The rectangles a menu or dialog drew over the pictures last frame, so a frame in which they
    /// changed is repainted from scratch: iTerm2 has no delete, and only a full repaint takes a
    /// picture back off the cells it covers (D161).
    pub covered: Vec<Rect>,
    /// Set when the transcript was replaced under the pictures, so the next frame takes every one
    /// of them off rather than trusting what the last one drew: the same id may now name a
    /// different picture.
    pub stale: bool,
}

/// How long two escapes may be apart and still count as one gesture (D54).
pub const DOUBLE_ESCAPE: Duration = Duration::from_millis(500);

/// How long "copied …" stays in the activity line.
pub const COPY_NOTE: Duration = Duration::from_secs(3);

/// A list shown above the editor while typing: `/` commands or `@` files.
pub struct Popup {
    pub items: Vec<(String, String)>,
    pub selected: usize,
    pub hint: &'static str,
}

/// Files the `@` picker offers, at most.
const PICKER_FILES: usize = 20_000;
/// The picker's file list is reread when it is older than this.
const PICKER_REFRESH: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteKind {
    Plugin,
    Info,
    Warn,
    Error,
}

/// How long a tool call has been going (D172): the instant it started, or, once it has answered,
/// the duration it took. Frozen on the answer, so a finished block keeps its total instead of
/// counting up for as long as it is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spent {
    Running(Instant),
    Took(Duration),
}

impl Spent {
    /// How long it has run, or did run.
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
    /// A thinking model's reasoning.
    Thinking(String),
    Tool {
        id: String,
        tasks: Vec<TaskView>,
        name: String,
        detail: String,
        /// `None` while running.
        result: Option<(bool, String)>,
        /// The file change, for edit and write.
        diff: Option<String>,
        /// How long the call has been running (D172). Only a `bash` call is timed, and a resumed
        /// one shows none: the store keeps what was said, not how long it took.
        spent: Option<Spent>,
    },
    Note(NoteKind, String),
    /// An image attached to the prompt above it (D58). Drawn where the terminal can, and as this
    /// entry's text everywhere else (D59).
    Image(Image),
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

/// The question `/tree` asks before a move that would leave work behind (D167).
///
/// Summarizing the branch being left is a model request, so it is asked for rather than spent: the
/// move is already the person's decision, and this is the one part of it that costs something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    /// How many messages the move would leave behind that produced something.
    pub left: usize,
    /// The node the conversation moves to, and the prompt to put back in the editor.
    pub rewind_to: Option<i64>,
    pub text: Option<String>,
}

impl Confirm {
    /// The question, as the two lines it is drawn as: what would be left behind, and what the
    /// answer does. Two lines rather than one because a dialog that cuts its own question with an
    /// ellipsis is not asking anything.
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
    /// Shown once under the intro, such as files saddle created on first run.
    pub notes: Vec<String>,
    /// The model's context window, for the status line.
    pub context_window: u64,
    /// Whether the model has a price, so `{cost}` can show `$0.00` rather than nothing (D36).
    pub priced: bool,
    /// The status line, parsed from `[ui] status` (D36).
    pub status: Template,
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
    /// `[ui] notify`: how a turn that ends while the terminal is not focused is announced (D164).
    pub notify: Notify,
    /// `[ui] math-scale`: how much larger or smaller than the fonts and the cell say a formula is
    /// drawn, 1.0 being what they say (D165).
    pub math_scale: f32,
    /// `[keys]`: the moves that have a key only because a person chose one (D169).
    pub keys: Keys,
}

/// Rendered lines for one entry, valid while the width and both toggles match.
pub struct Cached {
    pub width: u16,
    pub expand: bool,
    pub thinking: bool,
    pub lines: Vec<Line<'static>>,
    /// The formulas this entry reserved rows for, in the order their markers appear (D165).
    pub pictures: Vec<Formula>,
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
    /// What discovery offered, so `/skill:name` can put a body into the conversation without going
    /// back to the agent thread (D69). The text was read and recorded at discovery.
    pub skills: Vec<Skill>,
    pub entries: Vec<Entry>,
    pub plugin_panels: Vec<PanelView>,
    /// Parallel to `entries`; `None` means re-render.
    pub cache: Vec<Option<Cached>>,
    pub editor: Editor,
    pub queue: VecDeque<String>,
    /// Messages handed to the running turn and not yet delivered, oldest first (D149).
    pub steering: VecDeque<String>,
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
    pub copied: Option<Instant>,
    /// What was copied, for the confirmation (D86).
    pub copied_what: &'static str,
    /// Where the sign-in link was drawn, so a click on it opens the browser (D86).
    pub open_url_at: Option<Rect>,
    /// The text of the dialog that is open, as it was drawn, so its text can be selected and copied
    /// exactly as the transcript's can (D87).
    pub dialog_seen: Option<Seen>,
    /// When escape was last pressed on an empty editor, so two of them open the tree (D54).
    last_escape: Option<Instant>,
    pub total_lines: usize,
    /// Where each image sits in the transcript, and the escape that draws it (D59).
    pub images: Vec<Placed>,
    /// What this frame drew over the pictures: a menu above the editor, or a dialog that owns the
    /// screen. Nothing is drawn where it would sit on top of them (D161).
    pub covered: Vec<Rect>,
    /// What this terminal can draw, detected once at startup (D59).
    pub protocol: Protocol,
    /// The colours to draw with (D71). Every role the renderer paints comes from here.
    pub theme: Theme,
    /// What the last frame drew, and where (D59).
    pub drawn: Drawn,
    /// A frame where what covers the pictures changed, so the next one repaints from scratch: a
    /// terminal with no delete takes a picture off only by writing over it (D161).
    pub repaint: bool,
    pub usage: Usage,
    /// Dollars spent this session, when a price is known (D36).
    pub cost: f64,
    /// The model's context window and price, and the allowance the provider last stated.
    pub quota: Option<Quota>,
    /// Tokens in the context at the last reply.
    pub context: u64,
    pub spinner: usize,
    /// The frame's one clock, advanced by the tick that runs while something is working: a running
    /// call's elapsed time is measured against this, so a redraw and a test agree on "now" (D172).
    pub now: Instant,
    pub quit: bool,
    /// The /login dialog, drawn over everything else while open.
    pub login: Option<Login>,
    pub resume: Option<SessionPicker>,
    pub settings: Option<SettingsDialog>,
    /// The conversation tree, drawn over everything else while open (D53).
    pub tree: Option<Tree>,
    /// The question a move in `/tree` asks when it would leave work behind (D167).
    pub confirm: Option<Confirm>,
    /// The permissions editor, drawn over everything else while open (D45).
    pub permissions: Option<Permissions>,
    /// The theme list, drawn over everything else while open, and previewed as it moves (D74).
    pub themes: Option<ThemePicker>,
    /// The theme in force when the list opened — its name and its colours — so escape puts back
    /// exactly what was there (D74).
    theme_before: Option<(String, Theme)>,
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
    /// The image an entry holds, for the renderer that has to find a place to draw it (D59).
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
            confirm: None,
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

    /// A dialog is open and takes the keys.
    pub fn dialog_open(&self) -> bool {
        self.login.is_some()
            || self.resume.is_some()
            || self.settings.is_some()
            || self.themes.is_some()
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
                // A background compaction does not stop the turn, so it must not claim the
                // activity line: saying "compacting" while the model writes would be a lie about
                // what the terminal is waiting for (D78).
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

    /// Replaces the transcript with a resumed session's.
    fn show_resumed(&mut self, items: &[Item]) {
        self.entries.clear();
        self.cache.clear();
        // The pictures on the screen belong to the transcript being replaced, and an id may now
        // name a different one (D59).
        self.drawn.stale = true;
        // The old highlight points at lines that no longer exist.
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
        self.entries.iter().any(|entry| matches!(entry, Entry::Tool { tasks, .. } if tasks.iter().any(|task| task.state == TaskState::Running)))
    }

    pub fn tick(&mut self) {
        self.spinner = self.spinner.wrapping_add(1);
        self.now = Instant::now();
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
        // Any other key breaks the pair, so escape then `a` then escape is not a double escape.
        if key.code != KeyCode::Esc {
            self.last_escape = None;
        }
        // The question a move in `/tree` asks comes first: it is one keystroke, and it was asked
        // for the move that is already half made (D167).
        if self.confirm.is_some() {
            return self.answer(key);
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
                // Cancelling is the dialog saying stop asking; the answer is only ever for it, so
                // there is nothing for the interface to do with it.
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
                    // A move that leaves replies and tool output behind used to summarize them
                    // without being asked, which is a model request spent on a decision nobody
                    // made. It asks now, and only when there is something to summarize (D167).
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
                    self.last_escape = Some(Instant::now());
                    if twice {
                        self.last_escape = None;
                        return Some(Action::OpenTree);
                    }
                }
            }
            // While a turn is running the two ways to send differ: enter steers it, alt+enter
            // waits for it to finish (D149). ctrl+j still inserts a newline then.
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
    ///
    /// A dialog's text is selected the same way while it is open, because a dialog owns the screen
    /// and nothing in it would otherwise be copyable (D87). The one other pointer target is the
    /// sign-in link, which opens rather than selects (D86).
    /// Opens the theme list on the theme in force, remembering what to put back (D74).
    pub fn open_themes(&mut self, mut picker: ThemePicker) {
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
        let before = self.theme_before.clone();
        let outcome = self.themes.as_mut()?.key(key);
        match outcome {
            ThemeOutcome::Stay => {
                self.preview_theme();
                None
            }
            ThemeOutcome::Cancel => {
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
            ThemeOutcome::Choose(name) => {
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
        // A dialog owns the screen while it is open, so the transcript beneath is not selectable.
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
            // A drag over a dialog selects the dialog's text, which is the same thing it does over
            // the transcript; nothing else in an overlay is pointer-driven (D87).
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
                // A click that never moved is a click, not a selection: on the sign-in link that
                // means follow it (D86). Deciding here rather than on the way down is what lets a
                // drag that *starts* on the link select it like any other text, which it could not
                // do while the press was swallowed by the link (D87).
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
                    // A click that selected nothing is not a copy; drop the empty highlight.
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

    /// Extends the drag to the mouse, scrolling when it is pulled past the top or the bottom so
    /// that a selection can be longer than the screen (D35).
    fn extend(&mut self, ev: MouseEvent) -> Option<Action> {
        let mut sel = self.selection?;
        if !sel.dragging {
            return None;
        }
        let seen = self.surface()?;
        let (area, top, len) = (seen.area, seen.top, seen.lines.len());
        // A dialog does not scroll: a drag past its edge selects up to the edge and stops there,
        // rather than scrolling the transcript underneath it (D87).
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

    /// The character under the mouse, in the transcript as it was last drawn.
    /// The surface a selection is over: the open dialog, else the transcript (D87).
    ///
    /// A dialog is drawn over the transcript and owns the screen while it is open, so a drag there
    /// means the dialog's text. Its own `top` is zero: a dialog is not scrolled.
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

    /// Answers the question a move in `/tree` asks: `y` summarizes the branch left behind, `n`
    /// leaves it as it is, and escape stays where the conversation is (D167).
    fn answer(&mut self, key: KeyEvent) -> Option<Action> {
        let confirm = self.confirm.take()?;
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.move_to(confirm.rewind_to, confirm.text, true)
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.move_to(confirm.rewind_to, confirm.text, false)
            }
            // Escape puts the question away and the conversation stays where it was: the move has
            // not happened yet, so there is nothing to undo. ctrl+c is the other way to put a
            // dialog away, as it is for every other one; a second press is the quit it always was.
            KeyCode::Esc => None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => None,
            _ => {
                // Anything else is not an answer, so the question stays up.
                self.confirm = Some(confirm);
                None
            }
        }
    }

    /// Moves the conversation to a point in the tree, putting a prompt back in the editor when the
    /// point was one (D54).
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

    /// The text under the selection, as it was drawn.
    ///
    /// A formula is a picture the terminal drew, and the cells under it are blank: what a copy takes
    /// there is the TeX the picture was typeset from, wrapped in the delimiters that make it a
    /// formula again wherever it is pasted (D166). A picture is copied whole — a drag across part of
    /// one takes all of it — and the rows a block was drawn in below the first add no blank lines,
    /// since there is nothing on them.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let seen = self.surface()?;
        if sel.is_empty() || seen.lines.is_empty() {
            return None;
        }
        let (first, last) = sel.ends();
        // A drag pulled below the transcript points past its end; take what is there.
        let last_line = last.line.min(seen.lines.len() - 1);
        let mut parts: Vec<String> = Vec::new();
        // A drag that begins part-way down a block formula still copies it: the rows it began in
        // hold nothing but the picture, which is copied where the formula starts, and the rows of
        // the box are dropped below (D166).
        if let Some(picture) = seen.pictures.iter().find(|p| below(p, first.line)) {
            parts.push(delimited(picture));
            parts.push(String::new());
        }
        let rows = seen.lines.get(first.line..=last_line).unwrap_or_default();
        for (offset, text) in rows.iter().enumerate() {
            let line = first.line + offset;
            // The rows a block formula is drawn in are blank, and the picture covers cells the row
            // does not have: those cells are part of the row as far as a selection goes, which is
            // how a drag over a formula reaches it at all (D166). The padding they add is trimmed
            // with the frame's, below.
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
            // Lines are padded to the frame's width; the padding is not part of the copy.
            let (part, block) = selected(&chars, from.min(to), to, line, &seen.pictures);
            let part = part.trim_end().to_owned();
            // Below its first row a block formula's box holds the picture and nothing else, and the
            // picture is copied where it begins (D166).
            if part.is_empty() && continued(line, &seen.pictures) {
                continue;
            }
            parts.push(part);
            // A display formula is a paragraph of its own in what it is pasted into, and the rows
            // that separated it from what follows are the ones the box took (D166).
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

    /// `queued` chooses what happens when a turn is already running: the prompt waits for the turn
    /// to finish, or enters the conversation at the next request (D149).
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
                // Without an argument it reports, and clears: an empty name is no name.
                Some(Action::Name(
                    Some(name.to_owned()).filter(|n| !n.is_empty()),
                ))
            }
            "/session" => Some(Action::SessionInfo),
            "/mcp" => Some(Action::Mcp(text["/mcp".len()..].trim().to_owned())),
            "/tasks" => Some(Action::Tasks(text["/tasks".len()..].trim().to_owned())),
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
                // the transcript, what it has spent and how large it had grown all belong to the
                // conversation being left, so each starts again with the next one; what it did is
                // still in the store and under this run's audit log (D90).
                self.entries.clear();
                self.cache.clear();
                // Any picture on the screen belongs to the conversation being left (D59).
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
            AgentEvent::Panels(panels) => self.plugin_panels = panels,
            AgentEvent::Contribution(value) => {
                self.push(Entry::Note(NoteKind::Plugin, contribution_text(&value)))
            }
            AgentEvent::Skills(skills) => self.skills = skills,
            AgentEvent::Note(s) => self.push(Entry::Note(NoteKind::Warn, display(&s))),
            AgentEvent::Call { id, name, detail } => {
                self.busy = Some(Activity::Tool(display(&name)));
                let name = display(&name);
                // A command is the call whose duration a person is waiting on, and the one that can
                // start background work; a file read that took 2 ms is not worth a number on every
                // block (D172).
                let spent = (name == "bash").then(|| Spent::Running(Instant::now()));
                self.push(Entry::Tool {
                    id,
                    tasks: Vec::new(),
                    name,
                    detail: display(&detail),
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
                        // The block keeps the total it took, rather than a timer that goes on
                        // counting under a finished call (D172).
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
                // It is in the conversation now, so it leaves the pending line and joins the
                // transcript in the order the model saw it (D149).
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
                // A queued human prompt will carry the completion at its safe request point.
            }
            AgentEvent::Tasks(views) => {
                for (index, entry) in self.entries.iter_mut().enumerate() {
                    if let Entry::Tool { id, tasks, .. } = entry {
                        let mut next: Vec<_> = views
                            .iter()
                            .filter(|view| view.call.as_ref() == Some(id))
                            .cloned()
                            .collect();
                        // Finished snapshots stay historical after eviction; forgotten live work
                        // must not spin forever after reload or registry replacement.
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
            AgentEvent::Unopened(why) => {
                // Into the dialog when one is open, because a dialog covers the transcript and a note
                // pushed there is a message nobody can read (D86).
                match &mut self.login {
                    Some(login) => login.device.error = Some(why),
                    None => self.push(Entry::Note(NoteKind::Warn, display(&why))),
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
                // A conversation comes back on the model it was working in, so a default chosen
                // since — here or in another session — does not move it (D91). Asks only when it
                // is a different one, so resuming the usual case stays quiet.
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
                    // Only the moves with no default key are configurable, and the tree is where
                    // the one of them lives (D169).
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

/// Transcript entries for a saved conversation: prompts, reasoning, replies, and each tool call
/// with its result.
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
            Item::User(text) => {
                // A `!` command that was shared is a prompt carrying the run's output, and it
                // comes back as the block the run showed rather than as prose (D170).
                match shared_shell(text) {
                    Some((command, output)) => out.push(Entry::Tool {
                        id: String::new(),
                        tasks: Vec::new(),
                        name: "bash".into(),
                        detail: format!("$ {command}"),
                        result: Some((shell_ok(output), display(output))),
                        diff: None,
                        spent: None,
                    }),
                    None => out.push(Entry::User(display(text))),
                }
            }
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
                                id: c.id.clone(),
                                tasks: Vec::new(),
                                name: display(&c.name),
                                detail: display(&call_detail(c)),
                                result: found.map(|(ok, content, _)| (*ok, display(content))),
                                diff: found.and_then(|(_, _, d)| d.map(display)),
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

/// One row of a copy: the selected characters, with every formula the selection touches replaced by
/// its TeX (D166).
///
/// A picture is copied whole or not at all — half of a matrix is not a matrix — and a run of cells
/// the selection misses entirely stays what it is on screen, which is blank. The flag says a block
/// formula was one of them, since that is a paragraph of its own where it is pasted.
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

/// The invocation a copy of a formula carries: `\(…\)` for one inside a line, `\[…\]` for a block
/// (D166).
///
/// TeX's own spelling rather than `$…$`, because what the copy is for is a paper: `amsmath`
/// discourages `$$`, which is a plain TeX construct, and these are what a document gets. They are
/// also what this front end reads back, so pasting a copy into saddle draws it again (D165).
///
/// A formula written in Typst's own notation is copied with the fence that asks for it, and for the
/// same reason: its body is not LaTeX, and in TeX's delimiters it would be read as letters — so the
/// one spelling that means the same thing where it is pasted is the one it was written in (D165).
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

/// Whether `line` is a row a block formula was drawn in, below the row its source is copied from
/// (D166).
fn below(picture: &Source, line: usize) -> bool {
    !picture.inline
        && picture.line < line
        && line < picture.line.saturating_add(usize::from(picture.rows))
}

/// The column the last cell of a formula on `line` reaches, or zero when none is drawn there (D166).
fn cells_end(line: usize, pictures: &[Source]) -> usize {
    pictures
        .iter()
        .filter(|p| p.line == line)
        .map(|p| usize::from(p.column).saturating_add(usize::from(p.cols)))
        .max()
        .unwrap_or(0)
}

/// Whether `line` is such a row for any of the formulas drawn (D166).
fn continued(line: usize, pictures: &[Source]) -> bool {
    pictures.iter().any(|p| below(p, line))
}

/// Text from the model or a tool, made safe and stable for the screen.
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
        // D86: a note goes into the transcript, and a dialog covers it, so the reason a click did
        // nothing was unreachable exactly when it was needed.
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
        // The address and code stay: what failed is the browser, not the sign-in.
        assert_eq!(login.device.code.as_deref(), Some("AB12"));
        assert_eq!(login.link().as_deref(), Some("https://auth.test/device"));

        // With no dialog open it is an ordinary note, which is visible in the transcript.
        app.login = None;
        let before = app.entries.len();
        app.agent(AgentEvent::Unopened(why.into()));
        assert!(app.entries.len() > before);
    }

    #[test]
    fn the_sign_in_code_goes_to_the_clipboard_when_it_arrives() {
        // D86: the code has to be typed into a page in another window, and nothing drawn in a dialog
        // can be selected, so it is put where the paste comes from. An answer for an attempt that is
        // no longer on screen is not copied: it belongs to a sign-in nobody is looking at.
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
        // Nothing has been sent this run, but the transcript says otherwise, and the estimate of
        // what the next request would carry comes with it (D35).
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
            // The same model this session is already on, so resuming asks for no switch (D91).
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

        // Replies after the resume add to those totals rather than replacing them.
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
        // D149: enter while the agent works hands the message to the running turn, which takes it
        // at its next request; alt+enter keeps it for after the turn.
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

        // The agent says it delivered one: it leaves the pending line and joins the transcript,
        // between the tool output and the reply that follows it.
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
    fn a_new_session_starts_the_transcript_and_the_figures_again() {
        // D90: `/new` is pi's name for what `/clear` did, and a session that has just started has
        // spent nothing and carries nothing, whatever the one before it did.
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

        // `/clear` is the same action under the other name, and says so in the menu.
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
        a.skills = vec![Skill {
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
        let tables: ThemeTables =
            toml::from_str("[mine]\ntext = \"#ffffff\"\naccent = \"#ff0000\"\n").unwrap();
        let was = a.theme.name.clone();
        // Opened on a theme that is not the app's, so "opens on the one in force" is a real claim,
        // and not the last entry, so there is somewhere to move to.
        let picker = ThemePicker::collect(&tables, "dark");
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
        // A picker that let a letter reach the editor would send a prompt instead of choosing.
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
        // D70: the prompt carries the instruction files and the advertised skills, so a change
        // during the session only reaches the model if the prompt is rebuilt. The TUI's part is the
        // ask, and taking the list back.
        let mut a = app();
        type_text(&mut a, "/reload");
        assert_eq!(a.key(key(KeyCode::Enter)), Some(Action::Reload));
        // And the list it comes back with replaces what the menu offers.
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
        });
        let Some(Entry::Tool {
            spent: Some(Spent::Running(started)),
            ..
        }) = a.entries.last()
        else {
            panic!("a call in flight is timed");
        };
        // The frame's clock is what a running block is measured against, so a tick moves it.
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
        // Whatever the clock does afterwards, the block no longer counts up.
        let frozen = *took;
        a.now += Duration::from_secs(60);
        assert_eq!(Spent::Took(frozen).elapsed(a.now), frozen);
    }

    #[test]
    fn a_resumed_shared_shell_command_is_the_block_the_run_drew() {
        // What a `!` command leaves in the conversation is a prompt carrying the output (D170), and
        // the transcript shows it as the block the run showed rather than as prose with a `$` line.
        let items = [
            Item::User(
                "I ran a command in the workspace:\n$ printf hi\nexit code 0\n--- stdout ---\nhi"
                    .into(),
            ),
            // One saved before D170 kept the host's frame in its output, which the block drops.
            Item::User(
                "I ran a command in the workspace:\n$ false\n<plugin-output-abc plugin=\"standard-bash\">\nexit code 1\n</plugin-output-abc>"
                    .into(),
            ),
            // The sentence alone is not the shape: a person may type it, and stays a prompt.
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
                    result: Some((true, "exit code 0\n--- stdout ---\nhi".into())),
                    diff: None,
                    spent: None,
                },
                Entry::Tool {
                    id: String::new(),
                    tasks: Vec::new(),
                    name: "bash".into(),
                    detail: "$ false".into(),
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
                    result: Some((true, "fn x() {}".into())),
                    diff: None,
                    spent: None,
                },
                Entry::Assistant("Done.".into()),
            ]
        );
        assert!(matches!(&a.entries[0], Entry::Note(_, t) if t == "resumed a session of 1 prompt"));
    }

    /// A transcript with one block formula drawn over three rows below its marker row, and one
    /// inside a line (D166).
    fn drawn_with_formulas() -> App {
        let mut a = app();
        a.seen = Some(Seen {
            area: Rect::new(0, 0, 40, 8),
            top: 0,
            lines: vec![
                "by parts".into(),
                // The marker row, which a block formula leaves blank.
                String::new(),
                // The first row of its box, and the two the box goes on for.
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
        // D166: the cells a picture is drawn in are blank on screen, so a copy that took them would
        // paste a hole. It takes the source instead, on the row the picture starts at, in the
        // delimiters a document reads.
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
        // Half a formula is not a formula: a drag across part of one takes all of it, and the cells
        // it took are not left as spaces beside it.
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
        // A drag that starts part-way down a block formula begins on a row that holds nothing but
        // the picture, which is what the copy puts at its start (D166).
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
        // Everything else is copied as it was drawn, pictures or not.
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

    /// A conversation of one prompt, a reply, tool results and a second prompt with its reply.
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
        // D167: the summary is a model request, so the move asks rather than spending one. The
        // question is answered with one key, and escape stays where the conversation is.
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        assert_eq!(a.key(key(KeyCode::Enter)), None, "it asks before it moves");
        let confirm = a.confirm.as_ref().expect("the question is up");
        assert_eq!(confirm.left, 1, "the reply that follows the tool result");
        assert!(a.tree.is_none(), "the tree has closed");
        // One message is `it`, several are `they`: the question is a sentence, not a template.
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
        // A key that is not an answer leaves the question up.
        assert_eq!(a.key(key(KeyCode::Char('q'))), None);
        assert!(a.confirm.is_some());
        // `y` summarizes the branch it leaves, and the prompt goes back to the editor.
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
        // Escape is the other way to decline: the question goes and nothing moves.
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.key(key(KeyCode::Esc)), None);
        assert!(a.confirm.is_none());
        assert!(a.editor.is_empty(), "nothing was put back in the editor");
        // ctrl+c puts the question away too, and the second one is the quit it always was.
        let mut a = branching_app();
        a.tree.as_mut().unwrap().selected = 3;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.key(ctrl('c')), None);
        assert!(a.confirm.is_none());
        assert_eq!(a.key(ctrl('c')), Some(Action::Quit));
    }

    #[test]
    fn a_move_that_leaves_nothing_behind_does_not_ask() {
        // Choosing the last reply on the branch continues from it: there is nothing to summarize,
        // so there is nothing to ask.
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
