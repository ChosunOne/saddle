//! The full-screen terminal UI. The agent and the session run on a worker thread; this thread
//! reads keys and draws. They talk over two channels, and cancelling goes through the process-wide
//! flag in [`crate::cancel`].

use crate::compact::Event as CompactEvent;
use crate::compact::Settings;
use crate::config::Overrides;
use crate::context;
use crate::plugins::HostFailure;
use crate::provider::Image;
use crate::provider::Price;
use crate::provider::Quota;
use crate::session;
use crate::skills;
use crate::tui::app::Effort;
use crate::tui::theme::Theme;
use crate::tui::themes::ThemePicker;
use base64::Engine as _;
use base64::engine::general_purpose;
use ratatui::crossterm::terminal as crossterm_terminal;
use ratatui::layout::Rect;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::mem;
use std::os::unix::fs::OpenOptionsExt;
use std::panic;
use std::path::Path;
use std::process;
use std::process::Command as ProcessCommand;
use std::sync::Mutex;
mod app;
mod browser;
pub mod catppuccin;
mod editor;
mod files;
mod grant;
pub mod image;
mod login;
pub mod palette;
mod permissions;
mod render;
mod sessions;
mod settings;
pub mod status;
pub mod syntax;
pub mod terminal;
pub mod theme;
mod themes;
mod tree;

use std::fmt::Write as _;
use std::io::{Write as _, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, enable_raw_mode, supports_keyboard_enhancement,
};

pub use app::Header;
use app::{Action, AgentEvent, App, Drawn, NoteKind, Placed};
use login::{CUSTOM_LABEL, Choice, Draft, Listed, Login, LoginRequest, Pick};
use permissions::{Kind, Permissions, Row};

use crate::agent::{Agent, Compacted, End, Ui, call_detail};
use crate::auth::AuthStore;
use crate::cancel;
use crate::compact::Reason;
use crate::config::{self, UserConfig};
use crate::paths::Paths;
use crate::provider::{self, Provider, Reply, Sink, ToolCall};
use crate::session::{Session, Setup};
use crate::store;
use crate::tools::ToolOutput;
use sessions::SessionPicker;
use settings::{Change, SettingsDialog, Values};
use status::Template;

enum Command {
    /// A prompt, and the skill it came from when it was a `/skill:` command (D69).
    Prompt {
        text: String,
        skill: Option<String>,
    },
    Clear,
    /// Applied between turns: the commands queue behind a running turn.
    SetProvider(Box<Provider>),
    /// `None` lifts the limit on model requests per prompt (D37).
    SetMaxSteps(Option<usize>),
    /// A window the endpoint stated after the session opened (D34).
    SetWindow(u64),
    /// A price the endpoint stated after the session opened (D36).
    SetPrice(Price),
    /// Read the policy again after a grant changed, so the session acts on it and the model's next
    /// request carries the new grants (D47).
    ReloadPolicy,
    /// Read the skills and instruction files again and rebuild the prompt (D70).
    Reload,
    SetCompaction(Settings),
    /// Continue a saved conversation, by its id in the session store (D53).
    Resume(String),
    /// Read the prompts on the branch, to offer them in `/tree` (D53).
    OpenTree,
    /// Move the conversation to a point without deleting what came after it (D54).
    Rewind(Option<i64>),
    /// Copy the branch into a new conversation and continue that one (D53).
    Clone,
    /// Name this conversation, or clear the name (D55).
    Name(Option<String>),
    /// Report what this conversation has done (D55).
    SessionInfo,
    /// `/mcp`: report the servers, or start one again (D118).
    Mcp(String),
    /// Remove a saved conversation (D55).
    Delete(String),
    /// Change the reasoning effort (D57).
    Effort(Effort),
    Shell {
        command: String,
        share: bool,
    },
    Compact(Option<String>),
    Quit,
}

/// How long to wait for the agent thread to close the session after the UI exits.
const SHUTDOWN: Duration = Duration::from_secs(3);
/// How often the agent thread wakes with nothing to do, to notice that the session has gone idle
/// (D77). Short enough that a two-minute idle threshold is honoured within a second of it.
const IDLE_TICK: Duration = Duration::from_millis(500);
/// Redraw interval for the spinner while the agent works.
const TICK: Duration = Duration::from_millis(80);

/// How the TUI opens.
pub enum Start {
    Ready,
    /// No provider is chosen: open the provider list.
    ChooseProvider,
    /// The provider lacks a key or a model: open the login dialog at it.
    LogIn(String),
}

/// `resume` continues a saved session as soon as the TUI opens.
pub fn run(
    agent: Agent,
    session: Session,
    header: Header,
    paths: Paths,
    start: Start,
    resume: Option<String>,
) -> Result<()> {
    // What discovery offered, kept here so `/skill:name` can expand without the agent thread: the
    // bodies were read and recorded when the session opened (D69).
    let skills = session.skills.clone();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    let ev_tx_ui = ev_tx.clone();
    let worker = thread::Builder::new()
        .name("saddle-agent".into())
        .spawn(move || work(agent, session, cmd_rx, ev_tx))
        .context("starting the agent thread")?;

    let store = store::Store::open(&paths.session_db(), header.workspace.clone())?;
    let mut app = App::new(header);
    app.skills = skills;
    // What this terminal can draw is decided once, from the setting and the environment (D59).
    app.protocol = image::detect(app.header.images.as_deref(), &|k| env::var(k).ok());
    // The colours, from config.toml (D71). A theme that does not build is reported and the default
    // is drawn: a bad colour must not stop a session from opening, and the person has to be told
    // rather than quietly shown the wrong thing.
    match theme_from_config(&paths, &app.header.theme) {
        Ok(theme) => app.theme = theme,
        Err(e) => app.note(NoteKind::Error, &format!("{e:#}")),
    }
    let control = Controller {
        commands: cmd_tx,
        events: ev_tx_ui,
        paths,
        store,
        signing: Mutex::new(None),
    };
    if let Some(path) = resume {
        control.send(&mut app, Command::Resume(path));
    }
    let mut ready = false;
    let opened = match start {
        Start::Ready => {
            ready = true;
            Ok(())
        }
        Start::ChooseProvider => control.open_login(&mut app, None, false),
        Start::LogIn(name) => control.open_login(&mut app, Some(name), true),
    };
    if let Err(e) = opened {
        app.note(NoteKind::Error, &format!("{e:#}"));
    }
    // Opening a session has not listed models, so the window it starts with may be a guess; ask
    // the endpoint in the background and settle it when the answer arrives (D34).
    if ready && let Err(e) = control.ask_window(&mut app) {
        app.note(NoteKind::Error, &format!("{e:#}"));
    }
    // A provider with a usage endpoint can say what is left before anything is sent (D36).
    if ready && let Err(e) = control.refresh_usage(&mut app) {
        app.note(NoteKind::Error, &format!("{e:#}"));
    }
    let ui_result = {
        let mut screen = Screen::enter(app.header.mouse)?;
        event_loop(&mut screen.terminal, &mut app, &control, &ev_rx)
    };

    if app.busy.is_some() {
        cancel::request();
    }
    let _ = control.commands.send(Command::Quit);
    drop(control);
    // The deadline is checked on every pass: an agent still streaming would otherwise keep
    // this loop fed with events and never reach the timeout.
    let deadline = Instant::now() + SHUTDOWN;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let timed_out = left.is_zero()
            || match ev_rx.recv_timeout(left) {
                Ok(AgentEvent::Closed) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(_) => false,
                Err(RecvTimeoutError::Timeout) => true,
            };
        if timed_out {
            ui_result?;
            return Err(anyhow!(
                "the agent did not stop within {}s; the audit log was left open",
                SHUTDOWN.as_secs()
            ));
        }
    }
    let worker_result = worker
        .join()
        .map_err(|_| anyhow!("the agent thread panicked"))?;
    ui_result?;
    worker_result
}

/// The agent thread: runs one turn per prompt and closes the session at the end.
/// The theme the config selects, or the default (D71). Reads the file again, so `/reload` after a
/// hand edit picks it up without a restart.
/// The theme `[ui] theme` selects, for a caller that only wants to know it builds (D71).
pub fn theme_selected(user: &UserConfig) -> Result<Theme> {
    let name = user
        .ui
        .theme
        .clone()
        .unwrap_or_else(|| "default".to_owned());
    // Ask the terminal about its colours once, here, before the screen is taken over (D73, D75).
    // Not only when the chosen theme needs an answer: `/theme` can choose `system` mid-session, and
    // there is no safe moment to ask after this one.
    terminal::prime();
    theme::build(&name, &user.themes)
}

fn theme_from_config(paths: &Paths, selected: &Option<String>) -> Result<Theme> {
    let user = UserConfig::load(&paths.user_config())?;
    let name = selected
        .clone()
        .or(user.ui.theme.clone())
        .unwrap_or_else(|| "default".to_owned());
    theme::build(&name, &user.themes)
}

fn work(
    mut agent: Agent,
    mut session: Session,
    commands: Receiver<Command>,
    events: Sender<AgentEvent>,
) -> Result<()> {
    let mut ui = ChannelUi(events.clone());
    let mut fatal = None;
    // How long the agent waits for a command before it looks at the clock itself. A session that
    // stays open is idle most of the time, and idle is when a compaction is cheapest (D77).
    let tick = IDLE_TICK;
    let mut last_activity = Instant::now();
    loop {
        let command = match commands.recv_timeout(tick) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => {
                // A background summary that finished while nobody was looking lands here, within a
                // tick of finishing (D78).
                if let Err(e) = agent.poll_background(&mut session, &mut ui) {
                    let msg = format!("{e:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
                // Nothing has happened for a while. Compacting here costs a request the user did
                // not ask for, so it only happens when the conversation is half full and the
                // session has been left alone (D77).
                if agent.idle_compaction_due(last_activity.elapsed()) {
                    match agent.compact(&mut session, Reason::Idle, None, &mut ui) {
                        Ok(_) => {}
                        Err(e) => {
                            let msg = format!("{e:#}");
                            let _ = events.send(AgentEvent::Fatal(msg.clone()));
                            fatal = Some(msg);
                            break;
                        }
                    }
                    // One attempt per idle stretch: the countdown starts again, so a session that
                    // stays half full does not ask the model the same question every tick.
                    last_activity = Instant::now();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match command {
            Command::Prompt { text, skill } => match skill
                .map(|name| session.record_skill_load(&name))
                .transpose()
            {
                Err(e) => {
                    let msg = format!("{e:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
                Ok(_) => match agent.turn(&mut session, &text, &mut ui) {
                    Ok(end) => {
                        // A background compaction started during the turn may have finished after
                        // the last safe point; collect it before the turn is called done (D78).
                        if let Err(e) = agent.poll_background(&mut session, &mut ui) {
                            let msg = format!("{e:#}");
                            let _ = events.send(AgentEvent::Fatal(msg.clone()));
                            fatal = Some(msg);
                            break;
                        }
                        let _ = events.send(AgentEvent::TurnEnd(end));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let _ = events.send(AgentEvent::Fatal(msg.clone()));
                        fatal = Some(msg);
                        break;
                    }
                },
            },
            Command::Clear => {
                agent.clear();
                session.mcp.restore(&[]);
                agent.refresh_tools(session.declared_tools());
            }
            Command::SetMaxSteps(n) => agent.set_max_steps(n),
            Command::SetWindow(window) => agent.set_window(window),
            Command::SetPrice(price) => agent.set_price(price),
            Command::ReloadPolicy => match session.reload_policy() {
                Ok(()) => {
                    // The prompt is sent with every request, so replacing its grants section is
                    // what tells the model (D47).
                    agent.refresh_extensions(&session);
                    let _ = events.send(AgentEvent::Note(
                        "grants reloaded; the model is told on its next request".into(),
                    ));
                }
                Err(error) if error.is::<HostFailure>() => {
                    let message = format!("{error:#}");
                    let _ = events.send(AgentEvent::Fatal(message.clone()));
                    fatal = Some(message);
                    break;
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!(
                        "the policy was not reloaded, so this session keeps its grants: {e:#}"
                    )));
                }
            },
            // `/reload`: everything the prompt or the interface was built from, read again (D70).
            // Skills and instruction files reach the model on its next request; a newly added
            // instruction file is also handed to the conversation, so the transcript shows it
            // arriving rather than the next reply simply obeying a rule that appeared from nowhere.
            Command::Reload => {
                match session.reload_policy() {
                    Ok(()) => agent.refresh_extensions(&session),
                    Err(error) if error.is::<HostFailure>() => {
                        let message = format!("{error:#}");
                        let _ = events.send(AgentEvent::Fatal(message.clone()));
                        fatal = Some(message);
                        break;
                    }
                    Err(error) => {
                        let _ = events.send(AgentEvent::Note(format!(
                            "could not reload policy/plugins: {error:#}"
                        )));
                    }
                }
                let cfg = session.setup.context.clone();
                let paths = session.setup.paths.clone();
                let notes = match context::load(&mut session, &cfg, &paths) {
                    Ok((files, refused)) => {
                        let added = agent.rebuild_prompt(&session, files.clone());
                        agent.note_context(&added);
                        let mut notes = refused;
                        if !added.is_empty() {
                            let labels: Vec<&str> =
                                added.iter().map(|f| f.label.as_str()).collect();
                            notes.push(format!("instructions reloaded: {}", labels.join(", ")));
                        }
                        notes
                    }
                    Err(e) => vec![format!("the instruction files were not reloaded: {e:#}")],
                };
                match session.reload_skills() {
                    Ok(()) => {
                        let names = skills::names(&session.skills);
                        let _ = events.send(AgentEvent::Skills(session.skills.clone()));
                        let mut said = vec![format!("skills reloaded: {names}")];
                        said.extend(session.skill_notes.clone());
                        said.extend(notes);
                        for note in said {
                            let _ = events.send(AgentEvent::Note(note));
                        }
                        // Said plainly, because it is the one thing a reload cannot change: text
                        // already in the conversation stays there.
                        let _ = events.send(AgentEvent::Note(
                            "a skill or instruction file already read into this conversation is \
                             unchanged"
                                .into(),
                        ));
                    }
                    Err(e) => {
                        for note in notes {
                            let _ = events.send(AgentEvent::Note(note));
                        }
                        let _ = events.send(AgentEvent::Note(format!(
                            "the skills were not reloaded: {e:#}"
                        )));
                    }
                }
            }
            Command::SetCompaction(settings) => agent.set_compaction(settings),
            Command::Shell { command, share } => {
                match agent.shell(&mut session, &command, share, &mut ui) {
                    Ok(end) => {
                        let _ = events.send(AgentEvent::TurnEnd(end));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let _ = events.send(AgentEvent::Fatal(msg.clone()));
                        fatal = Some(msg);
                        break;
                    }
                }
            }
            Command::Compact(focus) => {
                let result = agent.compact(&mut session, Reason::Manual, focus.as_deref(), &mut ui);
                match result {
                    Ok(done) => {
                        let end = match done {
                            Compacted::Done | Compacted::Nothing => End::Answered,
                            Compacted::Cancelled => End::Cancelled,
                            Compacted::Failed => End::Failed,
                        };
                        let _ = events.send(AgentEvent::TurnEnd(end));
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let _ = events.send(AgentEvent::Fatal(msg.clone()));
                        fatal = Some(msg);
                        break;
                    }
                }
            }
            Command::Resume(id) => match agent.resume(&id) {
                Ok(saved) => {
                    session.mcp.restore(&saved.declared);
                    agent.refresh_tools(session.declared_tools());
                    let _ = events.send(AgentEvent::Resumed(saved));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!(
                        "could not resume session {id}: {e:#}"
                    )));
                }
            },
            Command::OpenTree => match agent.rows() {
                Ok(rows) => {
                    let _ = events.send(AgentEvent::Tree(rows));
                }
                Err(e) => {
                    let _ =
                        events.send(AgentEvent::Note(format!("could not read the tree: {e:#}")));
                }
            },
            Command::Effort(change) => {
                // A cycle starts from what is in force, so shift+tab walks the same list
                // `/thinking` accepts (D57).
                let next = match change {
                    Effort::Set(value) => value,
                    Effort::Cycle => provider::next_effort(agent.effort().as_deref()),
                };
                let applied = agent.set_effort(next);
                let text = match applied.as_deref() {
                    Some(level) => format!("reasoning effort: {level}"),
                    None => "reasoning effort: the provider's default".to_owned(),
                };
                let _ = events.send(AgentEvent::Note(text));
            }
            Command::Name(name) => match agent.name(name.as_deref()) {
                Ok(()) => {
                    let text = match name {
                        Some(n) => format!("this conversation is now named `{n}`"),
                        None => {
                            "the name is cleared; the first prompt is the title again".to_owned()
                        }
                    };
                    let _ = events.send(AgentEvent::Note(text));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("could not name it: {e:#}")));
                }
            },
            Command::SessionInfo => match agent.session_info(&mut session) {
                Ok(text) => {
                    let _ = events.send(AgentEvent::Note(text));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("{e:#}")));
                }
            },
            Command::Mcp(arg) => {
                // The registry lives on the session, and starting a server needs it, so it is
                // taken out for the call and put back (D108, D118).
                let mut registry = mem::take(&mut session.mcp);
                let outcome = match arg.split_once(' ') {
                    Some(("restart", name)) => registry.restart(name.trim(), &mut session),
                    Some(("disable", name)) => registry.suspend(name.trim()),
                    Some(("enable", name)) => registry.resume(name.trim()),
                    Some((other, _)) => Err(anyhow::anyhow!(
                        "`/mcp {other}` is not a command: /mcp, /mcp restart <server>, \
                         /mcp disable <server>, /mcp enable <server>"
                    )),
                    None if !arg.is_empty() => Err(anyhow::anyhow!(
                        "`/mcp {arg}` needs a server: /mcp lists them"
                    )),
                    None => {
                        let mut lines = registry.report();
                        lines.push(
                            "\n/mcp restart <server> · /mcp disable <server> · /mcp enable <server>"
                                .to_owned(),
                        );
                        Ok(lines.join("\n"))
                    }
                };
                session.mcp = registry;
                let _ = events.send(AgentEvent::Note(match outcome {
                    Ok(text) => text,
                    Err(e) => format!("{e:#}"),
                }));
            }
            Command::Delete(id) => match agent.delete_session(&id) {
                Ok(()) => {
                    let _ = events.send(AgentEvent::Note("deleted that conversation".into()));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("could not delete: {e:#}")));
                }
            },
            Command::Clone => match agent.fork(None) {
                Ok(saved) => {
                    let _ = events.send(AgentEvent::Resumed(saved));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("could not clone: {e:#}")));
                }
            },
            Command::Rewind(node) => match agent.rewind(node) {
                Ok(left) => {
                    // The branch being left is worth a note on the one entered, so what it found
                    // is not wasted. A failure here does not undo the move (D54).
                    let mut resumed = left.resumed;
                    session.mcp.restore(&resumed.declared);
                    agent.refresh_tools(session.declared_tools());
                    if !left.abandoned.is_empty()
                        && agent
                            .summarize_left_behind(&mut session, &mut ui, &left.abandoned)
                            .unwrap_or(false)
                    {
                        resumed = agent.state()?;
                        session.mcp.restore(&resumed.declared);
                        agent.refresh_tools(session.declared_tools());
                    }
                    let _ = events.send(AgentEvent::Resumed(resumed));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("could not go back: {e:#}")));
                }
            },
            Command::SetProvider(provider) => {
                if let Err(e) = agent.set_provider(&mut session, *provider, &mut ui) {
                    let msg = format!("{e:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
            }
            Command::Quit => break,
        }
        // Idle is measured from when the work finished, not from when it was asked for: a turn
        // that took ten minutes has been busy for ten minutes, and the session is not "left alone"
        // the instant it ends (D77).
        last_activity = Instant::now();
    }
    let closed = session.close();
    let _ = events.send(AgentEvent::Closed);
    closed?;
    match fatal {
        Some(msg) => Err(anyhow!(msg)),
        None => Ok(()),
    }
}

struct ChannelUi(Sender<AgentEvent>);

impl Sink for ChannelUi {
    fn text(&mut self, s: &str) {
        let _ = self.0.send(AgentEvent::Text(s.to_owned()));
    }

    fn note(&mut self, s: &str) {
        let _ = self.0.send(AgentEvent::Note(s.to_owned()));
    }

    fn reasoning(&mut self, s: &str) {
        let _ = self.0.send(AgentEvent::Reasoning(s.to_owned()));
    }

    fn quota(&mut self, quota: &Quota) {
        let _ = self.0.send(AgentEvent::Quota(quota.clone()));
    }

    fn cancelled(&self) -> bool {
        cancel::requested()
    }
}

impl Ui for ChannelUi {
    fn attached(&mut self, images: &[Image]) {
        let _ = self.0.send(AgentEvent::Attached(images.to_vec()));
    }

    fn call(&mut self, call: &ToolCall) {
        let _ = self.0.send(AgentEvent::Call {
            name: call.name.clone(),
            detail: call_detail(call),
        });
    }

    fn result(&mut self, out: &ToolOutput) {
        let _ = self.0.send(AgentEvent::Result {
            ok: out.ok,
            content: out.visible_content().to_owned(),
            diff: out.diff.clone(),
            images: out.images.clone(),
        });
    }

    fn reply(&mut self, reply: &Reply, cost: f64) {
        let _ = self.0.send(AgentEvent::Reply {
            usage: reply.usage,
            cost,
        });
    }

    fn compaction(&mut self, event: CompactEvent) {
        let _ = self.0.send(AgentEvent::Compaction(event));
    }
}

/// Puts each attached picture on the screen, at the row its marker line was drawn on (D59).
///
/// The escapes are written after the frame, because the terminal position is a side effect the
/// frame cannot express: the picture occupies cells the layout reserved as blanks, and the cursor
/// is put back where the frame left it so the next draw starts from the same place.
/// What one frame has to do to the images on screen: the ids to take off, and the escapes to put
/// back on (D59). Pure, so the scrolling behaviour can be tested without a terminal.
///
/// A picture is treated like text: the rows that are scrolled off are simply not drawn. The old rule
/// was that the whole picture had to fit, which meant two large screenshots could never both be
/// visible, so one of them silently disappeared.
fn plan_images(
    protocol: image::Protocol,
    drawn: &Drawn,
    images: &[Placed],
    area: Rect,
    top: usize,
) -> (Vec<u32>, Vec<(usize, String)>, Drawn) {
    let height = area.height as usize;
    // The band each picture is drawn with is part of "what is on screen": scrolling changes which
    // part of a picture is visible even when its row has not moved (D59).
    let mut wanted: Vec<(usize, u32, u16, u16)> = Vec::new();
    let mut draw: Vec<(usize, String)> = Vec::new();
    for placed in images {
        // The picture occupies the lines after its marker; the screen shows [top, top + height).
        // What is left of that intersection is what gets drawn (D59).
        let Some((from, rows)) = image::band(placed.line, placed.box_.rows, top, height) else {
            continue;
        };
        let window = image::Window {
            full: placed.box_,
            from,
            rows,
        };
        let Some(escape) = image::draw(protocol, &placed.png, placed.pixels, &window, placed.id)
        else {
            // This terminal cannot draw pictures; the marker line is all there is.
            continue;
        };
        // The screen row of the picture's first visible line: the marker, plus the rows scrolled
        // off its top, plus the one line between the marker and the picture.
        let row = placed.line + 1 + from as usize - top;
        draw.push((row, escape));
        wanted.push((row, placed.id, window.from, window.rows));
    }
    // Only where the area starts matters: a width change moves nothing now that no cell box is
    // sent for a picture at its own size, and anything else that changed shows up in `wanted`.
    let at = (area.x, area.y);
    let after = Drawn {
        shown: wanted.clone(),
        at: Some(at),
    };
    if drawn.at == Some(at) && drawn.shown == wanted {
        // Nothing moved and nothing new arrived: re-sending would only flicker.
        return (Vec::new(), Vec::new(), after);
    }
    let remove = drawn.shown.iter().map(|(_, id, _, _)| *id).collect();
    (remove, draw, after)
}

fn draw_images(app: &mut App) {
    if app.protocol == image::Protocol::Text {
        return;
    }
    let Some(seen) = &app.seen else {
        return;
    };
    let area = seen.area;
    let (remove, draw, after) = plan_images(app.protocol, &app.drawn, &app.images, area, seen.top);
    app.drawn = after;
    if remove.is_empty() && draw.is_empty() {
        return;
    }
    let mut out = String::new();
    // Take the last frame's pictures off the screen before drawing this frame's. Kitty keeps an
    // image at the cell it was drawn on, and ratatui only rewrites characters, so a scroll would
    // otherwise leave the old copy behind and add another (D59).
    for id in remove {
        let _ = write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\");
    }
    for (row, escape) in draw {
        // `row` is the picture's first visible line, counted from the top of the area; terminal
        // rows are one-based from the top of the screen.
        let _ = write!(
            out,
            "\x1b[{};{}H{}",
            area.y as usize + row + 1,
            area.x as usize + 1,
            escape
        );
    }
    // Leave the cursor where the frame put it, so the editor's caret does not jump.
    let _ = write!(out, "\x1b[?25h");
    let mut stdout = io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// How often the event loop checks that the terminal is still in raw mode.
const MODE_CHECK: u64 = 500;

/// Puts raw mode back if it was lost, so the interface cannot end up echoing every keystroke
/// instead of receiving it (D75).
fn keep_raw_mode() {
    if matches!(crossterm_terminal::is_raw_mode_enabled(), Ok(false)) {
        let _ = crossterm_terminal::enable_raw_mode();
    }
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    control: &Controller,
    events: &Receiver<AgentEvent>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    let mut last_mode_check = Instant::now();
    loop {
        // The interface is unusable without raw mode: keys are echoed instead of delivered, and the
        // screen fills with what is typed. Whatever took it away — a child process, a terminal
        // query that should never have run (D75) — it is put back rather than left to the person to
        // recover by closing the terminal.
        if last_mode_check.elapsed() >= Duration::from_millis(MODE_CHECK) {
            last_mode_check = Instant::now();
            keep_raw_mode();
        }
        if dirty {
            terminal.draw(|f| render::draw(f, app))?;
            draw_images(app);
            dirty = false;
        }
        // A short poll keeps streamed text flowing without a second wake-up mechanism.
        if event::poll(Duration::from_millis(30))? {
            loop {
                let action = match event::read()? {
                    Event::Key(key) => app.key(key),
                    Event::Paste(text) => {
                        app.paste(&text);
                        None
                    }
                    Event::Mouse(m) => app.mouse(m),
                    _ => None,
                };
                match action {
                    Some(Action::Copy(text, what)) => match copy_to_clipboard(&text) {
                        Ok(()) => {
                            app.copied = Some(Instant::now());
                            app.copied_what = what;
                        }
                        Err(e) => app.note(NoteKind::Error, &format!("could not copy: {e}")),
                    },
                    Some(Action::EditPrompt) => edit_prompt(terminal, app, &control.paths)?,
                    Some(Action::Apply(Change::EditConfig)) => {
                        let path = control.paths.user_config();
                        edit_file(terminal, &path, app.header.mouse)?;
                        control.after_config_edit(app);
                    }
                    Some(Action::Apply(Change::EditPolicy)) => {
                        // One base file: the grants are its `[agent]` table (D63).
                        let path = control.paths.user_config();
                        edit_file(terminal, &path, app.header.mouse)?;
                        control.after_policy_edit(app);
                    }
                    Some(action) => control.perform(app, action),
                    None => {}
                }
                dirty = true;
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        while let Ok(ev) = events.try_recv() {
            // A turn has just spent tokens, so what is left is worth asking about again (D36).
            let turn_ended = matches!(ev, AgentEvent::TurnEnd(_));
            if let Some(action) = app.agent(ev) {
                control.perform(app, action);
            }
            if turn_ended && let Err(e) = control.refresh_usage(app) {
                app.note(NoteKind::Error, &format!("{e:#}"));
            }
            dirty = true;
        }
        let working = app.busy.is_some() || app.login.as_ref().is_some_and(|l| l.checking());
        // The "copied" note is timed, so its redraws stop on their own once it has expired.
        let noting = app.copied.is_some_and(|at| at.elapsed() < app::COPY_NOTE);
        if (working || noting) && last_tick.elapsed() >= TICK {
            app.tick();
            last_tick = Instant::now();
            dirty = true;
        }
        if app.quit {
            return Ok(());
        }
    }
}

/// Carries out actions on the UI thread: talks to the agent thread, and reads and writes the
/// user's config and key store for /login, /logout and /model.
struct Controller {
    commands: Sender<Command>,
    /// For answers produced off the UI thread, such as a model list.
    events: Sender<AgentEvent>,
    paths: Paths,
    /// This workspace's saved sessions, for /resume.
    store: store::Store,
    /// The sign-in the login dialog is waiting on, so a cancel or a second attempt stops the poll
    /// that is no longer wanted (D85).
    signing: Mutex<Option<Arc<AtomicBool>>>,
}

impl Controller {
    fn perform(&self, app: &mut App, action: Action) {
        let result = match action {
            Action::Send(text) => {
                // Reset here, before the turn starts, so an Esc pressed right after Enter counts.
                cancel::reset();
                self.send(app, Command::Prompt { text, skill: None });
                Ok(())
            }
            Action::Theme(name) => {
                self.theme(app, name);
                Ok(())
            }
            Action::Reload => {
                // The settings side first, keeping the session-only choices a person made:
                // `/thinking` and ctrl+t/ctrl+o are not in config.toml (D70).
                self.reload_settings(app);
                self.reload_theme(app);
                self.send(app, Command::Reload);
                Ok(())
            }
            Action::Skill { name, prompt } => {
                cancel::reset();
                self.send(
                    app,
                    Command::Prompt {
                        text: prompt,
                        skill: Some(name),
                    },
                );
                Ok(())
            }
            Action::Clear => {
                self.send(app, Command::Clear);
                Ok(())
            }
            Action::Cancel => {
                cancel::request();
                Ok(())
            }
            Action::Quit => {
                app.quit = true;
                Ok(())
            }
            Action::OpenLogin => {
                let current = Some(app.header.provider.clone()).filter(|p| !p.is_empty());
                self.open_login(app, current, false)
            }
            Action::CheckModels(id, draft) => self.check_models(id, draft),
            Action::ListModels(id) => self.list_all_models(app, id),
            Action::SignIn(id, provider) => {
                self.sign_in(app, id, &provider);
                Ok(())
            }
            Action::Login(req) => self.login(app, &req),
            Action::Logout(name) => self.logout(app, name),
            Action::Model(name) => self.model(app, name),
            Action::Use(pick) => self.use_model(app, pick),
            Action::Default(pick) => self.save_default(app, pick),
            Action::Restore(pick) => {
                self.restore_model(app, &pick);
                Ok(())
            }
            Action::SettleWindow => self.settle_window(app),
            // The dialog asks for one change and stays open, so a person can do several.
            Action::Grant(change) => {
                self.grant(app, change);
                Ok(())
            }
            Action::OpenResume => self.open_resume(app),
            Action::Resume(id) => {
                self.send(app, Command::Resume(id));
                Ok(())
            }
            Action::OpenSettings => self.open_settings(app),
            Action::OpenPermissions => self.open_permissions(app),
            Action::OpenTree => {
                self.send(app, Command::OpenTree);
                Ok(())
            }
            Action::Rewind(node) => {
                self.send(app, Command::Rewind(node));
                Ok(())
            }
            Action::Clone => {
                self.send(app, Command::Clone);
                Ok(())
            }
            Action::Effort(level) => {
                self.send(app, Command::Effort(level));
                Ok(())
            }
            Action::Name(name) => {
                self.send(app, Command::Name(name));
                Ok(())
            }
            Action::SessionInfo => {
                self.send(app, Command::SessionInfo);
                Ok(())
            }
            Action::Mcp(arg) => {
                self.send(app, Command::Mcp(arg));
                Ok(())
            }
            Action::DeleteSession(id) => {
                // The agent thread may hold it, so deleting goes through the same channel as
                // everything else that touches the store (D55).
                self.send(app, Command::Delete(id));
                Ok(())
            }
            Action::Apply(change) => self.apply(app, change),
            Action::Shell { command, share } => {
                cancel::reset();
                self.send(app, Command::Shell { command, share });
                Ok(())
            }
            Action::Compact(focus) => {
                cancel::reset();
                self.send(app, Command::Compact(focus));
                Ok(())
            }
            // The event loop handles it, since it needs the terminal.
            Action::OpenUrl(url) => {
                // The opener answers twice: at once if nothing could be started, and from a thread
                // if the program it started then failed, which is the silent case a person cannot
                // see (D86).
                let events = self.events.clone();
                let say: browser::Report = Arc::new(move |why: String| {
                    let _ = events.send(AgentEvent::Unopened(why));
                });
                browser::open_or_say(&url, &say);
                Ok(())
            }
            Action::EditPrompt | Action::Copy(..) => Ok(()),
        };
        if let Err(e) = result {
            app.note(NoteKind::Error, &format!("{e:#}"));
        }
    }

    fn send(&self, app: &mut App, command: Command) {
        if self.commands.send(command).is_err() {
            app.agent(AgentEvent::Fatal("the agent thread has exited".into()));
        }
    }

    /// The providers `/login` offers, with what each one's credential looks like from here.
    ///
    /// Built once, because two dialogs read it: `/login`, and the model picker, which needs the
    /// provider's configured model and nothing else (D88).
    fn login_choices(&self) -> Result<(Vec<Choice>, Vec<String>)> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let builtin = provider::builtin_names();
        let mut choices: Vec<Choice> = provider::catalog(&user.providers)
            .into_iter()
            .map(|(name, entry)| {
                let env = entry
                    .api_key_env
                    .clone()
                    .filter(|var| env::var(var).is_ok_and(|k| !k.is_empty()));
                let stored = auth.key(&name).is_some();
                // An OAuth provider's credential is the token a sign-in stored, and it is not a
                // key: showing "not logged in" beside a key page would send a person looking for
                // something that provider does not issue (D85).
                let oauth = entry.auth == Some(provider::Auth::Oauth);
                let signed_in = oauth && auth.oauth(&name).is_some();
                let status = match (&env, stored, oauth, &entry.api_key_env) {
                    (_, _, true, _) if signed_in => "signed in".into(),
                    (_, _, true, _) => "not signed in".into(),
                    (Some(var), _, _, _) => format!("key from ${var}"),
                    (None, true, _, _) => "logged in".into(),
                    (None, false, _, None) => "no key needed".into(),
                    (None, false, _, Some(_)) => String::new(),
                };
                let detail = match (&entry.model, provider::key_page(&name), oauth) {
                    (Some(model), _, _) => model.clone(),
                    (None, _, true) => "with a ChatGPT plan".into(),
                    (None, Some(page), false) => format!("keys at {page}"),
                    (None, None, false) => entry.base_url.clone().unwrap_or_default(),
                };
                Choice {
                    needs_key: entry.api_key_env.is_some() && !oauth,
                    has_key: if oauth {
                        signed_in
                    } else {
                        env.is_some() || stored
                    },
                    model: entry.model.clone(),
                    name,
                    detail,
                    status,
                    custom: false,
                    oauth,
                }
            })
            .collect();
        choices.push(Choice {
            name: CUSTOM_LABEL.into(),
            detail: String::new(),
            status: String::new(),
            needs_key: false,
            has_key: false,
            model: None,
            custom: true,
            oauth: false,
        });
        Ok((choices, builtin))
    }

    fn open_login(&self, app: &mut App, preselect: Option<String>, direct: bool) -> Result<()> {
        let (choices, builtin) = self.login_choices()?;
        let auth_path = render::home_relative(&self.paths.auth_store());
        let mut login = Login::new(choices, builtin, preselect.as_deref(), direct, auth_path);
        // Opened straight at a provider, the dialog owes it something at once: a sign-in to start, or
        // the model list to ask for, since neither is a key to type (D85).
        let pending = login.take_pending();
        app.open_login(login);
        match pending {
            Some(login::Outcome::SignIn(id, provider)) => self.sign_in(app, id, &provider),
            Some(login::Outcome::Check(id, draft)) => self.check_models(id, draft)?,
            _ => {}
        }
        Ok(())
    }

    /// Runs a device sign-in on its own thread: the code goes to the dialog, then the poll
    /// continues until it is approved and the tokens are stored (D85).
    ///
    /// The poll belongs on a thread because it is a loop of requests with seconds between them; on
    /// the interface thread the screen would freeze for as long as the person takes to fetch a
    /// phone. The flag is what makes esc stop it, rather than leaving a poller talking to the server
    /// for a sign-in nobody is watching.
    fn sign_in(&self, app: &mut App, id: u64, provider: &str) {
        // Anything the config is missing is said in the dialog rather than as a note, because the
        // dialog is where the person is looking and where esc is (D85).
        if let Err(e) = self.spawn_sign_in(id, provider)
            && let Some(login) = &mut app.login
        {
            login.device_code(id, Err(format!("{e:#}")));
        }
    }

    fn spawn_sign_in(&self, id: u64, provider: &str) -> Result<()> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let config = provider::endpoint(provider, &user.providers)?;
        let flow = config.oauth.clone().with_context(|| {
            format!("{provider} does not sign in with OAuth; /login expects a key")
        })?;
        let settings = flow.settings()?;
        let events = self.events.clone();
        let store = self.paths.auth_store();
        let provider = provider.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        if let Ok(mut current) = self.signing.lock() {
            // A second sign-in replaces the first: the dialog has one on screen at a time.
            if let Some(previous) = current.replace(stop.clone()) {
                previous.store(true, Ordering::Relaxed);
            }
        }
        thread::Builder::new()
            .name("saddle-sign-in".into())
            .spawn(move || {
                let mut show = |device: &provider::oauth::DeviceCode, url: &str| {
                    let _ = events.send(AgentEvent::DeviceCode(
                        id,
                        Ok((url.to_owned(), device.user_code.clone())),
                    ));
                };
                let mut pause = |seconds: u64| {
                    // In steps, so a cancel is noticed within a second of the flag going up.
                    for _ in 0..seconds {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        thread::sleep(Duration::from_secs(1));
                    }
                };
                let outcome =
                    provider::oauth::device_login_step(&settings, &mut show, &mut pause, &stop)
                        .and_then(|tokens| {
                            let mut auth = AuthStore::load(&store)?;
                            auth.set_oauth(&provider, tokens);
                            auth.save(&store)?;
                            Ok(())
                        });
                let _ = events.send(AgentEvent::SignedIn(
                    id,
                    outcome.map_err(|e| format!("{e:#}")),
                ));
            })
            .context("starting the sign-in")?;
        Ok(())
    }

    /// Opens the model list for the provider in force, searchable, with the current one selected
    /// (D88).
    ///
    /// `/model <name>` is still there for someone who knows the name; this is for everyone else,
    /// and for the case the name is not knowable — a subscription backend renames models, and a
    /// gateway decides what it serves.
    fn open_models(&self, app: &mut App) -> Result<()> {
        let provider = app.header.provider.clone();
        if provider.is_empty() {
            // No provider yet, so the question is really which one, and `/login` asks it.
            return self.open_login(app, None, false);
        }
        let auth_path = render::home_relative(&self.paths.auth_store());
        // Whatever goes wrong is said inside the picker, never as a note: a note is a transcript
        // entry, a dialog covers the transcript, and the person read the silence as nothing having
        // happened at all (D88).
        let prepared = self.login_choices().and_then(|(choices, _)| {
            if choices.iter().any(|c| !c.custom && c.name == provider) {
                Ok(choices)
            } else {
                bail!("{provider} is not a configured provider; /login chooses one")
            }
        });
        let mut login = match prepared {
            Ok(choices) => Login::for_all_models(choices, &provider, &app.header.model, auth_path),
            Err(e) => Login::for_model_unavailable(&provider, format!("{e:#}"), auth_path),
        };
        let pending = login.take_pending();
        app.open_login(login);
        if let Some(login::Outcome::ListAll(id)) = pending
            && let Err(e) = self.list_all_models(app, id)
            && let Some(login) = &mut app.login
        {
            login.note_listing_failed(format!("{e:#}"));
        }
        Ok(())
    }

    /// Lists the draft's models on a separate thread, so the dialog stays live meanwhile.
    fn check_models(&self, id: u64, draft: Draft) -> Result<()> {
        let config = match &draft.base_url {
            Some(url) => provider::ProviderConfig {
                api: provider::Api::Openai,
                base_url: url.trim_end_matches('/').to_owned(),
                model: String::new(),
                api_key_env: None,
                max_tokens: None,
                context_window: None,
                effort: None,
                price: None,
                usage_url: None,
                search_url: None,
                headers: Default::default(),
                key_header: None,
                cache_retention: provider::CacheRetention::Short,
                cache_affinity: false,
                auth: provider::Auth::ApiKey,
                oauth: None,
            },
            None => {
                let user = UserConfig::load(&self.paths.user_config())?;
                provider::endpoint(&draft.provider, &user.providers)?
            }
        };
        let key = match draft.key {
            Some(key) => Some(key),
            None => {
                let auth = AuthStore::load(&self.paths.auth_store())?;
                provider::available_key(&config, auth.key(&draft.provider))
            }
        };
        let store = self.paths.auth_store();
        let events = self.events.clone();
        let cache = self.paths.reported_windows();
        let base_url = config.base_url.clone();
        // Built before the thread so the headers are the ones a turn would send: a key just typed,
        // or the token the sign-in stored a moment ago (D85).
        let p = Provider::new(
            &draft.provider,
            config,
            key.as_deref(),
            &provider::Reported::default(),
        )
        .with_stored_oauth(&store);
        thread::Builder::new()
            .name("saddle-models".into())
            .spawn(move || {
                let listed = p
                    .refresh_tokens()
                    .map_err(|e| provider::ModelsError::Failed(format!("{e:#}")))
                    .and_then(|()| provider::list_models(&p));
                // A listing is where the server states its real limits, so keep them for the
                // sessions that never list anything (D34). Failing to cache costs nothing but the
                // default window next time.
                if let Ok(models) = &listed {
                    let mut reported = provider::Reported::stored(&cache);
                    reported.record(&base_url, models);
                    let _ = reported.save(&cache);
                }
                let name = draft.provider.clone();
                let answer = listed.map(|models| Listed {
                    picks: models
                        .into_iter()
                        .map(|m| Pick {
                            provider: name.clone(),
                            model: m.id,
                        })
                        .collect(),
                    failed: Vec::new(),
                });
                let _ = events.send(AgentEvent::Models(id, answer));
            })
            .context("starting the model lookup")?;
        Ok(())
    }

    /// Lists what every configured provider serves, one thread each, and answers once (D89).
    ///
    /// Concurrently because the list is the union: a provider that is down spends its whole timeout,
    /// and doing that one after another would leave the dialog waiting for the sum of every failure.
    /// A provider that cannot be asked is reported beside the list rather than left out silently —
    /// "missing" and "serves nothing" look the same otherwise.
    fn list_all_models(&self, app: &mut App, id: u64) -> Result<()> {
        let (choices, _) = self.login_choices()?;
        // A provider with no credential cannot be listed; it is not a failure, it is one the person
        // has not set up, and `/login` is where that happens.
        let ready: Vec<Choice> = choices
            .into_iter()
            .filter(|c| !c.custom && (c.has_key || !c.needs_key))
            .collect();
        if ready.is_empty() {
            if let Some(login) = &mut app.login {
                login.models(
                    id,
                    Ok(Listed {
                        picks: Vec::new(),
                        failed: vec!["no provider has a credential yet; /login first".to_owned()],
                    }),
                );
            }
            return Ok(());
        }
        let mut built = Vec::new();
        let mut failed = Vec::new();
        for choice in &ready {
            match self.provider_for_listing(&choice.name) {
                Ok(p) => built.push(p),
                Err(e) => failed.push(format!("{}: {e:#}", choice.name)),
            }
        }
        let events = self.events.clone();
        let cache = self.paths.reported_windows();
        thread::Builder::new()
            .name("saddle-models-all".into())
            .spawn(move || {
                type Answer = (String, String, Result<Vec<provider::ModelInfo>, String>);

                let mut picks: Vec<Pick> = Vec::new();
                let mut failed = failed;
                // One thread per provider, all of them joined: the answer is one list, so it can
                // only be assembled once every provider has answered or given up. Each thread
                // returns its models *and* its own name and base URL, so nothing has to be matched
                // up afterwards.
                let answers: Vec<Answer> = thread::scope(|scope| {
                    let handles: Vec<_> = built
                        .iter()
                        .map(|p| {
                            scope.spawn(move || {
                                let listened = p
                                    .refresh_tokens()
                                    .map_err(|e| format!("{}: {e:#}", p.name))
                                    .and_then(|()| {
                                        provider::list_models(p)
                                            .map_err(|e| format!("{}: {}", p.name, e.reason()))
                                    });
                                (p.name.clone(), p.config.base_url.clone(), listened)
                            })
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| {
                            h.join().unwrap_or_else(|_| {
                                (
                                    String::new(),
                                    String::new(),
                                    Err("the listing thread panicked".into()),
                                )
                            })
                        })
                        .collect()
                });
                // Every window the servers state is kept, not just the one chosen (D34). Recorded
                // here rather than in the threads: one file, one writer.
                let mut reported = provider::Reported::stored(&cache);
                let mut recorded = false;
                for (name, base_url, answer) in answers {
                    match answer {
                        Ok(models) => {
                            recorded |= !models.is_empty();
                            reported.record(&base_url, &models);
                            picks.extend(models.into_iter().map(|m| Pick {
                                provider: name.clone(),
                                model: m.id,
                            }));
                        }
                        Err(why) => failed.push(why),
                    }
                }
                if recorded {
                    let _ = reported.save(&cache);
                }
                let _ = events.send(AgentEvent::Models(id, login::answer(picks, failed)));
            })
            .context("starting the model lookup")?;
        Ok(())
    }

    /// A provider for listing only: no model is needed, and the stored credential is used as a turn
    /// would (D85).
    fn provider_for_listing(&self, name: &str) -> Result<Provider> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let reported = provider::Reported::stored(&self.paths.reported_windows());
        let config = provider::endpoint(name, &user.providers)?;
        Ok(Provider::new(name, config, auth.key(name), &reported)
            .with_stored_oauth(&self.paths.auth_store()))
    }

    fn open_resume(&self, app: &mut App) -> Result<()> {
        let sessions = self.store.list()?;
        if sessions.is_empty() {
            app.note(NoteKind::Info, "no saved sessions in this workspace yet");
        } else {
            app.resume = Some(SessionPicker::new(sessions));
        }
        Ok(())
    }

    fn settings_values(&self, app: &App) -> Result<Values> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let provider = &app.header.provider;
        let (effort, max_tokens, context_window) = if provider.is_empty() {
            (None, None, None)
        } else {
            let c = provider::endpoint(provider, &user.providers)?;
            (c.effort, c.max_tokens, c.context_window)
        };
        Ok(Values {
            provider: provider.clone(),
            model: app.header.model.clone(),
            effort,
            max_tokens,
            context_window,
            window: app.header.context_window,
            status: app.header.status.to_string(),
            max_steps: app.max_steps,
            auto_compact: user.compaction.auto.unwrap_or(true),
            idle_compact: user.compaction.idle.unwrap_or(true),
            background_compact: user.compaction.background.unwrap_or(true),
            show_reasoning: app.show_thinking,
            expand_tools: app.expand_tools,
            config_path: render::home_relative(&self.paths.user_config()),
            policy_path: render::home_relative(&self.paths.user_config()),
        })
    }

    /// Writes one change into this workspace's own policy file (D45). The global policy is never
    /// touched: a change made while working in a project is a change to that project.
    fn grant(&self, app: &mut App, change: permissions::Change) {
        if let Err(e) = self.apply_grant(app, &change) {
            // Reported in the dialog, which is where the person is looking.
            if let Some(dialog) = &mut app.permissions {
                dialog.error = Some(format!("{e:#}"));
            }
            return;
        }
        if let Some(dialog) = &mut app.permissions {
            dialog.error = None;
        }
        // Rows are rebuilt from the file, so what is on screen is what the file now says.
        match self.permission_rows(app) {
            Ok(rows) => {
                if let Some(dialog) = &mut app.permissions {
                    let selected = dialog.selected.min(rows.len().saturating_sub(1));
                    dialog.rows = rows;
                    dialog.selected = selected;
                }
            }
            Err(e) => app.note(NoteKind::Error, &format!("{e:#}")),
        }
        // The change takes effect now, and the agent thread tells the model on its next request
        // (D47).
        self.send(app, Command::ReloadPolicy);
    }

    fn apply_grant(&self, app: &mut App, change: &permissions::Change) -> Result<()> {
        let setup = Setup::load(
            &app.header.workspace,
            &env::temp_dir(),
            &Overrides::default(),
        )?;
        // The roots the session would resolve, so the write is validated as a session would.
        let roots = session::roots_for_validation()?;
        if let permissions::Change::Add(what) = change {
            let added = grant::add(&setup.workspace_policy_path, None, what, &roots)?;
            app.note(
                NoteKind::Info,
                &format!(
                    "added {added} to {}; it applies from the next session",
                    setup.workspace_policy_path.display()
                ),
            );
            return Ok(());
        }
        grant::edit(&setup.workspace_policy_path, None, change, &roots)
    }

    fn open_settings(&self, app: &mut App) -> Result<()> {
        app.settings = Some(SettingsDialog::new(self.settings_values(app)?));
        Ok(())
    }

    /// Saves one change from /settings and applies it to the running session.
    fn apply(&self, app: &mut App, change: Change) -> Result<()> {
        let provider = app.header.provider.clone();
        let path = self.paths.user_config();
        let set_or_remove =
            |table: &mut toml_edit::Table, key: &str, value: Option<toml_edit::Item>| match value {
                Some(v) => {
                    table[key] = v;
                }
                None => {
                    table.remove(key);
                }
            };
        match change {
            Change::OpenLogin => {
                let current = Some(provider).filter(|p| !p.is_empty());
                return self.open_login(app, current, false);
            }
            Change::ChooseModel if provider.is_empty() => {
                return self.open_login(app, None, false);
            }
            Change::ChooseModel => return self.open_models(app),
            Change::Effort(effort) => {
                config::edit(&path, |doc| {
                    let t = config::provider_table(doc, &provider)?;
                    set_or_remove(t, "effort", effort.as_deref().map(toml_edit::value));
                    Ok(())
                })?;
                let p = self.build(&provider)?;
                self.switch(app, p);
            }
            Change::MaxTokens(n) => {
                config::edit(&path, |doc| {
                    let t = config::provider_table(doc, &provider)?;
                    set_or_remove(t, "max-tokens", n.map(|n| toml_edit::value(i64::from(n))));
                    Ok(())
                })?;
                let p = self.build(&provider)?;
                self.switch(app, p);
            }
            Change::ContextWindow(n) => {
                config::edit(&path, |doc| {
                    let t = config::provider_table(doc, &provider)?;
                    set_or_remove(
                        t,
                        "context-window",
                        n.map(|n| toml_edit::value(i64::from(n))),
                    );
                    Ok(())
                })?;
                let p = self.build(&provider)?;
                self.switch(app, p);
            }
            Change::StatusLine(template) => {
                // Checked before it is saved, so a typo is an error now rather than a line with
                // `{costt}` in it later (D36).
                let parsed = match &template {
                    None => Template::default(),
                    Some(text) => Template::parse(text).map_err(|e| anyhow!("{e}"))?,
                };
                config::edit(&path, |doc| {
                    set_or_remove(
                        config::table(doc, "ui")?,
                        "status",
                        template.as_deref().map(toml_edit::value),
                    );
                    Ok(())
                })?;
                app.header.status = parsed;
            }
            Change::AutoCompact(on) => self.set_compaction(app, "auto", on)?,
            Change::IdleCompact(on) => self.set_compaction(app, "idle", on)?,
            Change::BackgroundCompact(on) => self.set_compaction(app, "background", on)?,
            Change::MaxSteps(n) => {
                config::edit(&path, |doc| {
                    set_or_remove(
                        doc.as_table_mut(),
                        "max-steps",
                        n.map(|n| toml_edit::value(n as i64)),
                    );
                    Ok(())
                })?;
                app.max_steps = n.filter(|n| *n > 0);
                self.send(app, Command::SetMaxSteps(app.max_steps));
            }
            Change::ShowReasoning(on) => {
                config::edit(&path, |doc| {
                    config::table(doc, "ui")?["show-reasoning"] = toml_edit::value(on);
                    Ok(())
                })?;
                app.show_thinking = on;
            }
            Change::ExpandTools(on) => {
                config::edit(&path, |doc| {
                    config::table(doc, "ui")?["expand-tools"] = toml_edit::value(on);
                    Ok(())
                })?;
                app.expand_tools = on;
            }
            // Handled by the event loop, which owns the terminal.
            // The event loop handles these: one needs the terminal, the other opens a dialog.
            Change::EditConfig | Change::EditPolicy => return Ok(()),
            Change::OpenPermissions => {
                return self.open_permissions(app);
            }
        }
        let values = self.settings_values(app)?;
        if let Some(dialog) = &mut app.settings {
            dialog.values = values;
        }
        Ok(())
    }

    /// `/theme`: open the list, or switch to the theme named and write it to config.toml (D71, D74).
    ///
    /// Writing the file is what `/model` does: a choice made once should not have to be made again.
    /// The switch is applied here as well, so it takes effect without a reload.
    fn theme(&self, app: &mut App, name: Option<String>) {
        let Some(name) = name else {
            // A theme is chosen by looking at it, so the list is a picker that previews rather than
            // a printed set of names to choose from blind (D74).
            let user = match UserConfig::load(&self.paths.user_config()) {
                Ok(user) => user,
                Err(e) => return app.note(NoteKind::Error, &format!("{e:#}")),
            };
            let current = app
                .header
                .theme
                .clone()
                .or(user.ui.theme.clone())
                .unwrap_or_else(|| "default".to_owned());
            app.open_themes(ThemePicker::collect(&user.themes, &current));
            return;
        };
        let user = match UserConfig::load(&self.paths.user_config()) {
            Ok(user) => user,
            Err(e) => return app.note(NoteKind::Error, &format!("{e:#}")),
        };
        // Built before it is written: a name that does not build must not be saved, or the next
        // session would open on an error the person cannot see the cause of.
        let theme = match theme::build(&name, &user.themes) {
            Ok(theme) => theme,
            Err(e) => return app.note(NoteKind::Error, &format!("{e:#}")),
        };
        if let Err(e) = config::edit(&self.paths.user_config(), |doc| {
            if doc.get("ui").is_none() {
                doc.insert("ui", toml_edit::Item::Table(toml_edit::Table::new()));
            }
            doc["ui"]["theme"] = toml_edit::value(name.as_str());
            Ok(())
        }) {
            return app.note(NoteKind::Error, &format!("{e:#}"));
        }
        app.header.theme = Some(name.clone());
        app.theme = theme;
        app.note(
            NoteKind::Info,
            &format!("theme: {name}; saved in config.toml"),
        );
    }

    /// Writes one `[compaction]` switch and applies the result to the running session (D77, D78).
    fn set_compaction(&self, app: &mut App, key: &str, on: bool) -> Result<()> {
        let path = self.paths.user_config();
        config::edit(&path, |doc| {
            config::table(doc, "compaction")?[key] = toml_edit::value(on);
            Ok(())
        })?;
        let user = UserConfig::load(&path)?;
        let settings = Settings::from_config(&user.compaction);
        self.send(app, Command::SetCompaction(settings));
        Ok(())
    }

    /// The theme named by the config, or the default (D71).
    ///
    /// Built from the answer the terminal gave at startup. It is not asked again: the interface owns
    /// stdin and raw mode by now, and reading a reply from there broke a session once (D75).
    pub fn reload_theme(&self, app: &mut App) {
        let selected = app.header.theme.clone();
        match theme_from_config(&self.paths, &selected) {
            Ok(theme) => {
                let name = theme.name.clone();
                app.theme = theme;
                app.note(NoteKind::Info, &format!("theme: {name}"));
            }
            Err(e) => app.note(NoteKind::Error, &format!("{e:#}")),
        }
    }

    /// `/reload`: the settings half, read from config.toml (D70).
    ///
    /// Not [`after_config_edit`], because that is for a dialog that just wrote the file: it resets
    /// the session-only state a person set on purpose. `/thinking` and ctrl+t/ctrl+o change the
    /// agent and are not written to config, so reloading over them would silently drop a choice
    /// made minutes earlier in the same session. This keeps them; only what the file states is
    /// taken again.
    fn reload_settings(&self, app: &mut App) {
        let user = match UserConfig::load(&self.paths.user_config()) {
            Ok(user) => user,
            Err(e) => {
                let msg = format!("config.toml has an error and was not applied: {e:#}");
                return app.note(NoteKind::Error, &msg);
            }
        };
        match user.ui.status.as_deref().map(Template::parse) {
            None => app.header.status = Template::default(),
            Some(Ok(template)) => app.header.status = template,
            Some(Err(e)) => app.note(NoteKind::Error, &format!("[ui] status: {e}")),
        }
        // The theme is rebuilt from the answer the terminal gave at startup; it is not asked again
        // here, because the interface owns the terminal by now (D75).
        self.reload_theme(app);
        app.max_steps = user.max_steps.filter(|n| *n > 0);
        self.send(app, Command::SetMaxSteps(app.max_steps));
        let compaction = Settings::from_config(&user.compaction);
        self.send(app, Command::SetCompaction(compaction));
        // The provider and the model are this conversation's own (D91), so a file that now names
        // another provider is what the next session starts with, not a command to move this one.
        // Said out loud, because the file changed and nothing else will mention it.
        if let Some(name) = user.provider.clone().filter(|n| *n != app.header.provider) {
            app.note(
                NoteKind::Info,
                &format!(
                    "config.toml now names {name}; this session stays on {} with its model, and \
                     /model switches it",
                    app.header.provider
                ),
            );
        }
        app.note(NoteKind::Info, "settings reloaded from config.toml");
    }

    /// Picks up what can change mid-session after config.toml was edited by hand.
    fn after_config_edit(&self, app: &mut App) {
        let user = match UserConfig::load(&self.paths.user_config()) {
            Ok(user) => user,
            Err(e) => {
                let msg = format!("config.toml has an error and was not applied: {e:#}");
                return app.note(NoteKind::Error, &msg);
            }
        };
        match user.ui.status.as_deref().map(Template::parse) {
            None => app.header.status = Template::default(),
            Some(Ok(template)) => app.header.status = template,
            Some(Err(e)) => app.note(NoteKind::Error, &format!("[ui] status: {e}")),
        }
        app.show_thinking = user.ui.show_reasoning.unwrap_or(true);
        app.expand_tools = user.ui.expand_tools.unwrap_or(false);
        app.max_steps = user.max_steps.filter(|n| *n > 0);
        self.send(app, Command::SetMaxSteps(app.max_steps));
        let compaction = Settings::from_config(&user.compaction);
        self.send(app, Command::SetCompaction(compaction));
        // What the file says about the provider in force — effort, limits, the address — is what
        // was just edited, so it is re-read; its model is the default for new sessions, so the
        // model this conversation is working in is handed back in rather than taken from the file
        // (D91). The file's `provider =` is the next session's default for the same reason.
        let name = app.header.provider.clone();
        if !name.is_empty() {
            let model = Some(app.header.model.clone()).filter(|m| !m.is_empty());
            match self.build_model(&name, model.as_deref()) {
                Ok(p) => self.switch(app, p),
                Err(e) => return app.note(NoteKind::Error, &format!("{e:#}")),
            }
        }
        app.note(
            NoteKind::Info,
            "config.toml reloaded; changes to roots and [sandbox] apply from the next session",
        );
    }

    fn after_policy_edit(&self, app: &mut App) {
        let path = self.paths.user_config();
        let checked = fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|text| Ok(text.parse::<toml::Table>()?));
        match checked {
            Ok(_) => app.note(
                NoteKind::Info,
                "the grants apply from the next session; this one keeps the grants it started \
                 with",
            ),
            Err(e) => app.note(
                NoteKind::Error,
                &format!("{} is not valid TOML: {e:#}", path.display()),
            ),
        }
    }

    /// Asks the provider's own endpoint what this model takes, off the UI thread, so the window a
    /// session opens with is the server's word on it and not a default (D34).
    fn ask_window(&self, app: &mut App) -> Result<()> {
        let name = app.header.provider.clone();
        if name.is_empty() {
            return Ok(());
        }
        let p = self.build(&name)?;
        let cache = self.paths.reported_windows();
        let events = self.events.clone();
        thread::Builder::new()
            .name("saddle-window".into())
            .spawn(move || {
                if p.refresh_tokens().is_ok() && provider::ask_endpoint(&p, &cache).is_some() {
                    let _ = events.send(AgentEvent::Listed);
                }
            })
            .context("starting the window lookup")?;
        Ok(())
    }

    /// The grants this session is running with, as editable rows (D45). What is not editable is
    /// still listed: seeing that `exec` is on, and that it is not changed from a menu, is the point
    /// of showing it.
    fn permission_rows(&self, app: &App) -> Result<Vec<Row>> {
        let setup = Setup::load(
            &app.header.workspace,
            &env::temp_dir(),
            &Overrides::default(),
        )?;
        let policy = setup.parse_policy()?;
        let Some(grants) = policy.grants(&saddle_cap::Principal::Agent) else {
            return Ok(Vec::new());
        };
        let mut rows = Vec::new();
        for (kind, list) in [
            (Kind::FsRead, &grants.fs_read),
            (Kind::FsWrite, &grants.fs_write),
        ] {
            for g in list {
                rows.push(Row {
                    rule: g.rule.to_string(),
                    kind,
                    root: g.root.as_str().to_owned(),
                    exclude: g.exclude.join(", "),
                    on: true,
                    editable: true,
                    locked: None,
                });
            }
        }
        if let Some(e) = &grants.exec {
            rows.push(Row {
                rule: e.rule.to_string(),
                kind: Kind::Exec,
                root: String::new(),
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some(
                    "exec is granted in the policy file: a menu keypress is too easy a way to hand \
                     over a capability. Edit the file for this one."
                        .into(),
                ),
            });
        }
        for n in &grants.net {
            rows.push(Row {
                rule: n.rule.to_string(),
                kind: Kind::Net,
                root: n.host.to_string(),
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some(
                    "a host is granted in the policy file, so it is written down where it can be \
                     read. Edit the file for this one."
                        .into(),
                ),
            });
        }
        if let Some(b) = &grants.model_invoke {
            rows.push(Row {
                rule: b.rule.to_string(),
                kind: Kind::ModelInvoke,
                root: format!("{} tokens per session", b.tokens_per_session),
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some("a token budget is granted in the policy file".into()),
            });
        }
        for (scope, g) in &grants.plain {
            rows.push(Row {
                rule: g.rule.to_string(),
                kind: Kind::Plain,
                root: scope.to_string(),
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some("granted in the policy file".into()),
            });
        }
        Ok(rows)
    }

    fn open_permissions(&self, app: &mut App) -> Result<()> {
        let rows = self.permission_rows(app)?;
        let setup = Setup::load(
            &app.header.workspace,
            &env::temp_dir(),
            &Overrides::default(),
        )?;
        app.permissions = Some(Permissions::new(
            rows,
            render::home_relative(&setup.policy_path),
            render::home_relative(&self.paths.user_config()),
        ));
        Ok(())
    }

    /// Asks the provider's usage endpoint, off the UI thread, and shows the answer as `{quota}`.
    /// A provider without one costs nothing here: there is no request to make (D36).
    fn refresh_usage(&self, app: &mut App) -> Result<()> {
        let name = app.header.provider.clone();
        if name.is_empty() {
            return Ok(());
        }
        let user = UserConfig::load(&self.paths.user_config())?;
        let config = provider::endpoint(&name, &user.providers)?;
        let Some(url) = config.usage_url.clone() else {
            return Ok(());
        };
        let p = self.build_model(&name, Some(&app.header.model))?;
        let events = self.events.clone();
        thread::Builder::new()
            .name("saddle-usage".into())
            .spawn(move || {
                if let Ok(page) = p.usage(&url) {
                    let (quota, _spent) = provider::quota_from_page(&page);
                    if !quota.is_empty() {
                        let _ = events.send(AgentEvent::Quota(quota));
                    }
                }
            })
            .context("starting the usage lookup")?;
        Ok(())
    }

    /// The endpoint has answered, so the window is settled again the way it is settled at build
    /// time: `context-window` in config.toml still wins, else what the endpoint just stated. The
    /// header and the loop move together, so they cannot disagree (D34).
    fn settle_window(&self, app: &mut App) -> Result<()> {
        let name = app.header.provider.clone();
        if name.is_empty() {
            return Ok(());
        }
        let user = UserConfig::load(&self.paths.user_config())?;
        let config = provider::endpoint(&name, &user.providers)?;
        let reported = provider::Reported::stored(&self.paths.reported_windows());
        let window = provider::context_window(&config, &reported);
        if window != app.header.context_window {
            app.header.context_window = window;
            self.send(app, Command::SetWindow(window));
        }
        // The same listing that states the window may state the price (D36).
        if config.price.is_none()
            && let Some(price) = reported.price(&config.base_url, &config.model)
        {
            app.header.priced = true;
            self.send(app, Command::SetPrice(price));
        }
        Ok(())
    }

    /// The provider as the config, the key store and the model cache now describe it.
    fn build(&self, name: &str) -> Result<Provider> {
        self.build_model(name, None)
    }

    /// A provider as config.toml has it, except for the model: a model chosen for one conversation
    /// is not in the file, so it is handed in rather than read back (D91).
    fn build_model(&self, name: &str, model: Option<&str>) -> Result<Provider> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let reported = provider::Reported::stored(&self.paths.reported_windows());
        let config = provider::resolve(name, &user.providers, model)?;
        Ok(Provider::new(name, config, auth.key(name), &reported)
            .with_stored_oauth(&self.paths.auth_store()))
    }

    fn switch(&self, app: &mut App, provider: Provider) {
        app.header.provider = provider.name.clone();
        app.header.model = provider.config.model.clone();
        app.header.context_window = provider.window();
        app.header.priced = provider.price().is_some();
        self.send(app, Command::SetProvider(Box::new(provider)));
    }

    fn login(&self, app: &mut App, req: &LoginRequest) -> Result<()> {
        if let Some(key) = &req.key {
            let path = self.paths.auth_store();
            let mut auth = AuthStore::load(&path)?;
            auth.set(&req.provider, key.clone());
            auth.save(&path)?;
        }
        // What the file gets is only what makes this provider exist: a server added here has no
        // entry until its address is written. The provider and model in force are this
        // conversation's, and only ctrl+s in the model list writes a default (D91). Writing a
        // provider's `model` here would be writing that default by another name, since a new
        // session reads it.
        if let Some(url) = &req.base_url {
            config::edit(&self.paths.user_config(), |doc| {
                let table = config::provider_table(doc, &req.provider)?;
                table["api"] = toml_edit::value("openai");
                table["base-url"] = toml_edit::value(url.as_str());
                Ok(())
            })?;
        }
        // The model is the one just chosen, not the file's: there need not be one there (D91).
        let provider = self.build_model(&req.provider, Some(&req.model))?;
        if let Some(why) = provider.missing_key() {
            bail!(why);
        }
        let mut msg = format!(
            "using {} with {} in this conversation; ctrl+s in the model list makes it the default \
             for new sessions",
            provider.name, provider.config.model
        );
        if let Some(var) = provider
            .config
            .api_key_env
            .as_ref()
            .filter(|var| req.key.is_some() && env::var(var).is_ok_and(|k| !k.is_empty()))
        {
            msg.push_str(&format!(
                "; ${var} is set and is used instead of the key you entered"
            ));
        }
        self.switch(app, provider);
        app.note(NoteKind::Info, &msg);
        Ok(())
    }

    fn logout(&self, app: &mut App, name: Option<String>) -> Result<()> {
        let name = name.unwrap_or_else(|| app.header.provider.clone());
        let path = self.paths.auth_store();
        let mut auth = AuthStore::load(&path)?;
        if !auth.remove(&name) {
            app.note(NoteKind::Info, &format!("no stored key for {name}"));
            return Ok(());
        }
        auth.save(&path)?;
        let mut msg = format!("removed the stored key for {name}");
        if name == app.header.provider {
            let provider = self.build(&name)?;
            if provider.missing_key().is_some() {
                msg.push_str("; /login to add one");
            }
            self.switch(app, provider);
        }
        app.note(NoteKind::Info, &msg);
        Ok(())
    }

    fn model(&self, app: &mut App, name: Option<String>) -> Result<()> {
        let Some(model) = name else {
            // No name: the list, filtered by typing, rather than a name to remember (D88).
            return self.open_models(app);
        };
        // No provider in force yet, so there is nothing to name a model against: the list asks
        // which provider first, which is what `/model` with no provider has always done.
        if app.header.provider.is_empty() {
            return self.open_models(app);
        }
        // A name is a choice about this conversation, like picking one from the list: it is not
        // written to config.toml, so it cannot move the next session too (D91).
        self.use_model(
            app,
            Pick {
                provider: app.header.provider.clone(),
                model,
            },
        )
    }

    /// Switches this conversation to a provider and model (D91).
    fn use_model(&self, app: &mut App, pick: Pick) -> Result<()> {
        let (provider, model) = self.switch_to(app, &pick)?;
        app.note(
            NoteKind::Info,
            &format!(
                "now using {model} on {provider} in this conversation · \
                 ctrl+s in the model list makes it the default"
            ),
        );
        Ok(())
    }

    /// Switches to a provider and model and writes both to config.toml, which is what a new session
    /// starts with (D91).
    fn save_default(&self, app: &mut App, pick: Pick) -> Result<()> {
        config::edit(&self.paths.user_config(), |doc| {
            doc["provider"] = toml_edit::value(pick.provider.as_str());
            config::provider_table(doc, &pick.provider)?["model"] =
                toml_edit::value(pick.model.as_str());
            Ok(())
        })?;
        let (provider, model) = self.switch_to(app, &pick)?;
        app.note(
            NoteKind::Info,
            &format!("now using {model} on {provider}; saved as the default"),
        );
        Ok(())
    }

    /// Puts a resumed conversation back on the model it was working in (D91).
    ///
    /// Quiet when it works: the resume note has already said which conversation came back. Loud
    /// when it does not, because a conversation quietly continuing on a model it was not using is
    /// exactly the surprise this is here to prevent.
    fn restore_model(&self, app: &mut App, pick: &Pick) {
        let Err(e) = self.switch_to(app, pick) else {
            return;
        };
        app.note(
            NoteKind::Error,
            &format!(
                "this conversation was last using {} on {}, which is not available now: {e:#}; \
                 staying on {} on {}",
                pick.model, pick.provider, app.header.model, app.header.provider
            ),
        );
    }

    /// Builds the provider for a choice and switches to it, answering with what was chosen.
    fn switch_to(&self, app: &mut App, pick: &Pick) -> Result<(String, String)> {
        let provider = self.build_model(&pick.provider, Some(&pick.model))?;
        let model = provider.config.model.clone();
        self.switch(app, provider);
        Ok((pick.provider.clone(), model))
    }
}

/// Raw mode, the alternate screen, mouse wheel, bracketed paste and, where the terminal supports
/// it, unambiguous keys (so shift+enter differs from enter). Dropping it restores the terminal.
struct Screen {
    terminal: DefaultTerminal,
}

/// Ctrl+G: the prompt goes to a private file, `$EDITOR` edits it, and the result comes back into
/// the editor.
fn edit_prompt(terminal: &mut DefaultTerminal, app: &mut App, paths: &Paths) -> Result<()> {
    fs::create_dir_all(&paths.state)?;
    let path = paths.state.join(format!("prompt-{}.md", process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(app.editor.text().as_bytes())?;
    drop(file);
    let edited = edit_file(terminal, &path, app.header.mouse);
    let text = fs::read_to_string(&path);
    let _ = fs::remove_file(&path);
    edited?;
    if let Ok(text) = text {
        app.editor.set(text.trim_end_matches('\n').to_owned());
    }
    Ok(())
}

/// Hands the terminal to `$VISUAL` or `$EDITOR` (or vi) for `path`, then takes it back.
fn edit_file(terminal: &mut DefaultTerminal, path: &Path, mouse: bool) -> Result<()> {
    leave_modes();
    ratatui::restore();
    let editor = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .ok()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi").to_owned();
    let status = ProcessCommand::new(&program).args(words).arg(path).status();
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen)?;
    enter_modes(mouse)?;
    terminal.clear()?;
    status.with_context(|| format!("running {program}"))?;
    Ok(())
}

/// Enables the modes the UI runs in. Mouse reporting is on unless `[ui] mouse = false` (D35): while
/// it is on, the terminal sends drags to saddle rather than selecting them itself, and saddle does
/// its own selection, highlight and copy.
/// Copies `text` to the terminal's clipboard with OSC 52.
///
/// Once saddle owns the mouse a drag no longer reaches the terminal's own selection, so this is how
/// selected text leaves the UI, and it needs no X or Wayland connection. A terminal that refuses the
/// sequence leaves the highlight as the only feedback; shift+drag still reaches the terminal in most
/// of them (D35).
fn copy_to_clipboard(text: &str) -> Result<()> {
    let tmux = env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let mut stdout = stdout();
    stdout.write_all(clipboard_sequence(text, tmux).as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// The bytes that put `text` on the terminal's clipboard: OSC 52 with the text base64-encoded.
/// tmux forwards an escape sequence only when it is wrapped in one of its own, with every ESC
/// inside doubled, so that form is built here rather than in the writer.
fn clipboard_sequence(text: &str, tmux: bool) -> String {
    let payload = general_purpose::STANDARD.encode(text);
    let sequence = format!("\x1b]52;c;{payload}\x07");
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence
    }
}

fn enter_modes(mouse: bool) -> Result<()> {
    let mut out = stdout();
    execute!(out, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    if matches!(supports_keyboard_enhancement(), Ok(true)) {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    Ok(())
}

fn leave_modes() {
    let mut out = stdout();
    let _ = execute!(out, PopKeyboardEnhancementFlags);
    let _ = execute!(out, DisableBracketedPaste, DisableMouseCapture);
}

impl Screen {
    fn enter(mouse: bool) -> Result<Self> {
        let terminal = ratatui::try_init().context("starting the terminal UI")?;
        // A reply to the startup's colour query that arrived after its deadline is a keystroke as
        // far as this loop is concerned, and would be typed into the prompt (D75).
        terminal::discard_pending();
        enter_modes(mouse)?;
        // ratatui's hook restores raw mode and the screen; this one also turns off the modes above.
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            leave_modes();
            previous(info);
        }));
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        leave_modes();
        ratatui::restore();
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::Resumed;
    use crate::provider::Usage;
    use crate::tui::Header;
    use crate::tui::image::Fit;
    use crate::tui::status::Template;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;
    use std::fs;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::mpsc;

    use super::*;
    use crate::auth::AuthStore;
    use crate::tui::app::Entry;

    /// A controller over a throwaway config, so a slash command can be followed all the way to
    /// what it does (D74).
    fn controller(dir: &Path, config: &str) -> Controller {
        let paths = Paths {
            state: dir.join("state"),
            config: dir.join("config"),
        };
        fs::create_dir_all(paths.user_config().parent().unwrap()).unwrap();
        fs::write(paths.user_config(), config).unwrap();
        let (commands, _) = mpsc::channel();
        let (events, _) = mpsc::channel();
        Controller {
            commands,
            events,
            store: store::Store::open(&paths.sessions_index(), dir.to_path_buf()).unwrap(),
            paths,
            signing: Mutex::new(None),
        }
    }

    fn an_app() -> App {
        App::new(Header {
            provider: "mock".into(),
            model: "mock-model".into(),
            workspace: ".".into(),
            audit: "a.jsonl".into(),
            notes: Vec::new(),
            show_reasoning: true,
            expand_tools: false,
            max_steps: None,
            context_window: 128_000,
            priced: false,
            mouse: false,
            status: Template::default(),
            images: None,
            theme: None,
        })
    }

    #[test]
    fn a_sign_in_runs_off_the_interface_thread_and_answers_on_the_events_channel() {
        // D85: the poll is a loop of requests with seconds between them, so it cannot run on the
        // thread that draws the screen. This checks the seam — the dialogs's request starts a
        // thread, and what that thread finds arrives as an event the interface reads — with the
        // endpoints pointed at a port nothing is listening on, so no code is ever issued.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            state: dir.path().join("state"),
            config: dir.path().join("config"),
        };
        fs::create_dir_all(paths.user_config().parent().unwrap()).unwrap();
        fs::write(
            paths.user_config(),
            "[providers.mine]\napi = \"openai\"\n\
             base-url = \"http://127.0.0.1:9/v1\"\nmodel = \"m\"\n\
             [providers.mine.oauth]\nmethod = \"device\"\nclient-id = \"cid\"\n\
             authorize-url = \"http://127.0.0.1:9/authorize\"\n\
             token-url = \"http://127.0.0.1:9/token\"\n\
             device-redirect-uri = \"http://127.0.0.1:9/callback\"\n\
             device-code-url = \"http://127.0.0.1:9/usercode\"\n\
             device-token-url = \"http://127.0.0.1:9/device-token\"\n",
        )
        .unwrap();
        let (commands, _) = mpsc::channel();
        let (events, answers) = mpsc::channel();
        let c = Controller {
            commands,
            events,
            store: store::Store::open(&paths.sessions_index(), dir.path().to_path_buf()).unwrap(),
            paths,
            signing: Mutex::new(None),
        };
        let mut app = an_app();
        // An unreachable endpoint is a failure the dialog has to be told about, not a hang: nothing
        // is left waiting for a code that will never come.
        c.perform(&mut app, Action::SignIn(7, "mine".into()));
        let answer = answers
            .recv_timeout(Duration::from_secs(20))
            .expect("the sign-in answered on the events channel");
        match answer {
            AgentEvent::SignedIn(id, Err(why)) => {
                assert_eq!(id, 7, "the answer is for the attempt that asked");
                assert!(!why.is_empty());
            }
            other => panic!("expected a failed sign-in, got {other:?}"),
        }
        // Nothing was stored, so the provider is still not signed in.
        assert!(
            !c.paths.auth_store().exists()
                || AuthStore::load(&c.paths.auth_store())
                    .unwrap()
                    .oauth("mine")
                    .is_none()
        );
    }

    #[test]
    fn a_sign_in_that_cannot_start_says_so_in_the_dialog() {
        // D85: a provider the config cannot sign in to must not leave the dialog waiting for a code
        // that is never coming. The reason belongs on the screen the person is looking at, with esc
        // right there to leave it.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(dir.path(), "provider = \"openai\"\n");
        let mut app = an_app();
        let choice = |name: &str, oauth: bool| Choice {
            name: name.into(),
            detail: String::new(),
            status: String::new(),
            needs_key: !oauth,
            has_key: false,
            model: None,
            custom: false,
            oauth,
        };
        // A name the config does not define, which is what a typo in a provider table looks like.
        let mut login = Login::new(
            vec![choice("kimi", true)],
            vec![],
            Some("kimi"),
            true,
            "auth.json".into(),
        );
        let Some(login::Outcome::SignIn(id, _)) = login.take_pending() else {
            panic!("a sign-in to start");
        };
        app.open_login(login);
        c.perform(&mut app, Action::SignIn(id, "kimi".into()));
        let login = app.login.as_ref().unwrap();
        let why = login.device.error.as_deref().unwrap_or_default();
        assert!(why.contains("kimi"), "{why}");
        assert!(login.device.code.is_none(), "no code was issued");
        // Still on the sign-in screen, so a key press cannot index an empty field list.
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    #[test]
    fn slash_model_with_no_name_opens_the_list_for_the_provider_in_force() {
        // D88: `/model` on its own is the picker. This walks the whole way — the command, the
        // dialog, the listing it asks for — because the point is what happens when it is typed.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[providers.synthetic]\nmodel = \"hf:zai-org/GLM-5.3\"\n",
        );
        // A stored key, so choosing a model is the only thing that could fail.
        let mut auth = AuthStore::load(&c.paths.auth_store()).unwrap();
        auth.set("synthetic", "sk-test".into());
        auth.save(&c.paths.auth_store()).unwrap();
        let mut app = an_app();
        // The dialog is opened for the provider in force, and it owes the host a listing.
        app.header.provider = "synthetic".into();
        c.perform(&mut app, Action::Model(None));
        let login = app.login.as_ref().expect("the picker is open");
        assert_eq!(login.purpose(), login::Purpose::Model);
        assert_eq!(login.choice().name, "synthetic");
        assert!(
            matches!(login.stage, login::Stage::Checking { .. }),
            "the list is being asked for"
        );
        // Choosing writes the model and switches to it, exactly as `/model <name>` does.
        let id = match login.stage {
            login::Stage::Checking { id } => id,
            _ => unreachable!(),
        };
        app.login.as_mut().unwrap().models(
            id,
            Ok(Listed {
                picks: vec![
                    Pick {
                        provider: "synthetic".into(),
                        model: "picked-a".into(),
                    },
                    Pick {
                        provider: "synthetic".into(),
                        model: "picked-b".into(),
                    },
                ],
                failed: Vec::new(),
            }),
        );
        let action = {
            let login = app.login.as_mut().unwrap();
            login.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            login.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        };
        // D91: choosing a model is a choice about this conversation, so it does not write the file
        // a new session starts from.
        let login::Outcome::Use(pick) = action else {
            panic!("expected a choice for this conversation, got {action:?}");
        };
        assert_eq!(pick.provider, "synthetic");
        assert_eq!(pick.model, "picked-b");
        c.perform(&mut app, Action::Use(pick));
        assert_eq!(app.header.model, "picked-b");
        let written = fs::read_to_string(c.paths.user_config()).unwrap();
        assert!(
            written.contains("hf:zai-org/GLM-5.3"),
            "the default is still the one it was: {written}"
        );
        assert!(!written.contains("picked-b"), "{written}");
    }

    #[test]
    fn reloading_the_settings_does_not_move_a_session_to_the_files_provider() {
        // D91: `/reload` reads the settings again, and the default in the file is what the next
        // session starts with. This one keeps the provider and model it is working in — a reload
        // in the middle of a task must not quietly start spending on another account.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[providers.synthetic]\nmodel = \"the-default\"\n",
        );
        let mut app = an_app();
        app.header.provider = "openai-chatgpt".into();
        app.header.model = "session-model".into();
        c.reload_settings(&mut app);
        assert_eq!(app.header.provider, "openai-chatgpt");
        assert_eq!(app.header.model, "session-model");
        assert!(
            app.entries
                .iter()
                .any(|e| matches!(e, Entry::Note(NoteKind::Info, t)
                if t.contains("config.toml now names synthetic") && t.contains("stays on"))),
            "the file changed, so it is said: {:?}",
            app.entries
        );
    }

    #[test]
    fn logging_in_does_not_change_the_default_provider_or_model() {
        // D91: `/login` was the last thing still writing the default. A key is a credential, not a
        // choice of what a new session starts with, and the model just chosen there is a choice
        // about this conversation like any other.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[providers.synthetic]\nmodel = \"the-default\"\n",
        );
        let mut app = an_app();
        app.header.provider = "synthetic".into();
        app.header.model = "the-default".into();
        c.perform(
            &mut app,
            Action::Login(LoginRequest {
                provider: "synthetic".into(),
                key: Some("sk-entered".into()),
                model: "chosen-at-login".into(),
                base_url: None,
            }),
        );
        // The session moved, and says how to make that the default.
        assert_eq!(app.header.model, "chosen-at-login");
        assert!(
            app.entries
                .iter()
                .any(|e| matches!(e, Entry::Note(NoteKind::Info, t)
                if t.contains("chosen-at-login") && t.contains("ctrl+s"))),
            "{:?}",
            app.entries
        );
        // The key was stored, and the file was not touched at all.
        let auth = AuthStore::load(&c.paths.auth_store()).unwrap();
        assert_eq!(auth.key("synthetic"), Some("sk-entered"));
        let written = fs::read_to_string(c.paths.user_config()).unwrap();
        assert!(
            written.contains("the-default") && !written.contains("chosen-at-login"),
            "the default is what it was: {written}"
        );
    }

    #[test]
    fn logging_in_to_a_new_server_still_writes_the_server_itself() {
        // D91 stops `/login` writing a *default*, not a provider: a server added there has no entry
        // until its address is in the file, and without one there is nothing to switch to.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(dir.path(), "");
        let mut app = an_app();
        c.perform(
            &mut app,
            Action::Login(LoginRequest {
                provider: "mine".into(),
                key: None,
                model: "qwen".into(),
                base_url: Some("http://127.0.0.1:8080/v1".into()),
            }),
        );
        let written = fs::read_to_string(c.paths.user_config()).unwrap();
        assert!(written.contains("[providers.mine]"), "{written}");
        assert!(written.contains("http://127.0.0.1:8080/v1"), "{written}");
        assert!(
            !written.contains("provider = \"mine\""),
            "the provider in force is not a default: {written}"
        );
        assert_eq!(app.header.provider, "mine");
        assert_eq!(app.header.model, "qwen");
    }

    #[test]
    fn ctrl_s_writes_the_choice_where_a_new_session_reads_it() {
        // The other half of D91: the default is not gone, it just cannot be changed by accident.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[providers.synthetic]\nmodel = \"hf:zai-org/GLM-5.3\"\n",
        );
        let mut app = an_app();
        app.header.provider = "synthetic".into();
        c.perform(
            &mut app,
            Action::Default(Pick {
                provider: "synthetic".into(),
                model: "kept".into(),
            }),
        );
        assert_eq!(app.header.model, "kept");
        let written = fs::read_to_string(c.paths.user_config()).unwrap();
        assert!(written.contains("kept"), "{written}");
    }

    #[test]
    fn a_default_changed_elsewhere_does_not_move_a_conversation_that_already_chose() {
        // D91: the resume is where the two used to meet. A conversation saved on one model comes
        // back on it, even though the file now names another one.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[providers.synthetic]\nmodel = \"a-new-default\"\n",
        );
        let mut app = an_app();
        app.header.provider = "synthetic".into();
        app.header.model = "a-new-default".into();
        let event = AgentEvent::Resumed(Resumed {
            items: Vec::new(),
            declared: Vec::new(),
            usage: Usage::default(),
            cost: 0.0,
            context: 0,
            model: Some(store::Model {
                provider: "synthetic".into(),
                model: "what-it-was-using".into(),
            }),
        });
        let action = app.agent(event).expect("a switch back to its own model");
        c.perform(&mut app, action);
        assert_eq!(app.header.model, "what-it-was-using");
        assert_eq!(app.header.provider, "synthetic");
        let written = fs::read_to_string(c.paths.user_config()).unwrap();
        assert!(
            written.contains("a-new-default"),
            "the default is what another session set it to: {written}"
        );
    }

    #[test]
    fn typing_slash_model_and_pressing_enter_opens_the_picker() {
        fn press(app: &mut App, code: KeyCode) -> Option<Action> {
            app.key(KeyEvent::new(code, KeyModifiers::NONE))
        }

        // The keyboard path, which is the only one that matters and the one I did not test: typing
        // the command with the `/` menu open, then enter. Testing `Action::Model(None)` directly
        // proved the dialog works and nothing about whether `/model` reaches it (D88).
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[providers.synthetic]\nmodel = \"hf:zai-org/GLM-5.3\"\n",
        );
        let mut auth = AuthStore::load(&c.paths.auth_store()).unwrap();
        auth.set("synthetic", "sk-test".into());
        auth.save(&c.paths.auth_store()).unwrap();
        let mut app = an_app();
        app.header.provider = "synthetic".into();
        for ch in "/model".chars() {
            press(&mut app, KeyCode::Char(ch));
        }
        // The menu is open over the command being typed, which is where enter used to go instead.
        assert!(app.popup().is_some(), "the / menu is showing");
        let action = press(&mut app, KeyCode::Enter);
        assert_eq!(action, Some(Action::Model(None)), "enter runs /model");
        assert_eq!(app.editor.text(), "", "and the editor is clear");
        c.perform(&mut app, action.unwrap());
        let login = app.login.as_ref().expect("the picker is open");
        assert_eq!(login.purpose(), login::Purpose::Model);
        assert_eq!(login.choice().name, "synthetic");
    }

    #[test]
    fn slash_model_opens_a_picker_even_when_it_cannot_list_anything() {
        // D88: whatever is wrong — an unreadable config, a provider not in the catalog — the command
        // opens the picker and says why. Silence was the reported bug: a note behind a transcript
        // that a dialog covers reads as nothing happening.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(dir.path(), "provider = \"synthetic\"\n");
        // A config that cannot be parsed, so preparing the picker fails.
        fs::write(c.paths.user_config(), "provider = ").unwrap();
        let mut app = an_app();
        app.header.provider = "synthetic".into();
        let action = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(action, None);
        for ch in "/model".chars() {
            app.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        let action = app
            .key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .expect("enter runs /model");
        c.perform(&mut app, action);
        let login = app.login.as_ref().expect("the picker opened anyway");
        assert_eq!(login.purpose(), login::Purpose::Model);
        let login::Stage::Models { note, .. } = &login.stage else {
            panic!("a picker, got {:?}", login.stage);
        };
        let note = note.as_deref().unwrap_or_default();
        assert!(
            note.contains("config.toml") || note.contains("TOML") || note.contains("expected"),
            "the reason is on the list, not in a note behind it: {note}"
        );
    }

    #[test]
    fn slash_model_with_a_name_still_switches_straight_to_it() {
        // The picker is for not knowing the name; someone who knows it should not have to wait for
        // a listing to come back (D88).
        let dir = tempfile::tempdir().unwrap();
        let c = controller(dir.path(), "provider = \"synthetic\"\n");
        let mut app = an_app();
        app.header.provider = "synthetic".into();
        c.perform(&mut app, Action::Model(Some("named".into())));
        assert!(app.login.is_none(), "no dialog for a name");
        assert_eq!(app.header.model, "named");
    }

    #[test]
    fn slash_theme_opens_the_list_on_the_theme_in_force() {
        // `/theme` with no name is the list, not a printed set of names to choose from blind (D74).
        let dir = tempfile::tempdir().unwrap();
        let c = controller(
            dir.path(),
            "provider = \"synthetic\"\n[ui]\ntheme = \"mono\"\n[themes.mine]\naccent = \"#ff0000\"\n",
        );
        let mut app = an_app();
        c.perform(&mut app, Action::Theme(None));
        let picker = app.themes.as_ref().expect("the list is open");
        assert_eq!(
            picker.current().name,
            "mono",
            "opened on the theme in force"
        );
        assert!(
            picker.entries.iter().any(|e| e.name == "mine"),
            "the theme defined in the config is offered"
        );
        assert_eq!(app.theme.name, "mono", "and it is what is being drawn");
    }

    #[test]
    fn a_slash_theme_that_cannot_be_read_says_so_instead_of_opening_message() {
        // The config is a person's file; a broken one is a note, not a crash and not a silent no-op.
        let dir = tempfile::tempdir().unwrap();
        let c = controller(dir.path(), "this is not toml");
        let mut app = an_app();
        c.perform(&mut app, Action::Theme(None));
        assert!(
            app.themes.is_none(),
            "no list from a config that did not parse"
        );
    }

    fn placed(line: usize, id: u32) -> Placed {
        Placed {
            line,
            id,
            box_: Fit {
                cols: 10,
                rows: 4,
                scaled: false,
            },
            pixels: (100, 80),
            png: vec![7u8; 8],
        }
    }

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn a_scroll_takes_the_old_picture_off_before_drawing_the_new_one() {
        // The bug this fixes: kitty keeps an image where it was drawn while ratatui rewrites only
        // characters, so a scroll left the old copy behind and drew a second one (D59).
        let first = Drawn::default();
        let images = [placed(10, 7)];
        let (remove, draw, after) = plan_images(image::Protocol::Kitty, &first, &images, area(), 0);
        assert!(remove.is_empty(), "nothing was on screen yet");
        assert_eq!(draw.len(), 1);
        assert_eq!(draw[0].0, 11, "on the row after its marker");

        // Scroll by two lines: the same picture is now at row 8, and the old one is taken off.
        let (remove, draw, after) = plan_images(image::Protocol::Kitty, &after, &images, area(), 2);
        assert_eq!(remove, [7], "the id that was on screen");
        assert_eq!(draw.len(), 1);
        assert_eq!(draw[0].0, 9);
        assert_eq!(after.shown, [(9, 7, 0, 4)]);
    }

    #[test]
    fn a_frame_where_nothing_moved_sends_nothing() {
        // Otherwise every keystroke would erase and redraw the picture, which flickers.
        let (_, _, after) = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
        );
        let (remove, draw, _) =
            plan_images(image::Protocol::Kitty, &after, &[placed(10, 7)], area(), 0);
        assert!(remove.is_empty() && draw.is_empty());
    }

    #[test]
    fn a_picture_below_the_fold_is_drawn_in_part_not_dropped() {
        // This is what made two images look like one: the second picture sat below the visible
        // rows, and the old rule skipped it entirely. Now the part that is on screen is drawn, the
        // way text would be (D59).
        let images = [placed(20, 7)];
        let (_, draw, after) = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
        );
        assert_eq!(draw.len(), 1, "the visible part is drawn: {draw:?}");
        // The picture occupies 21..25, and the area is 24 tall, so three of its four rows show.
        assert_eq!(after.shown, [(21, 7, 0, 3)]);
        // Scrolled so the picture starts above the window, the top band comes off and the rest is
        // drawn from the second row down.
        let (_, _, after) = plan_images(image::Protocol::Kitty, &after, &images, area(), 2);
        assert_eq!(after.shown, [(19, 7, 0, 4)], "still whole, just higher");
        let (_, _, after) = plan_images(image::Protocol::Kitty, &after, &images, area(), 22);
        assert_eq!(after.shown, [(0, 7, 1, 3)], "the last three rows only");
    }

    #[test]
    fn a_picture_whose_marker_scrolled_off_is_drawn_from_where_it_is_visible() {
        // The bug: the layout only placed a picture whose *marker line* was on screen, so scrolling
        // the marker off the top dropped the picture even though most of it was still visible. The
        // drawing pass was never the problem; it never saw the picture (D59).
        let images = [placed(8, 7)];
        // The marker is two lines above the top of a 24-row area, so the picture, which occupies
        // 9..13, has its first row off screen and three on it.
        let (remove, draw, after) = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            10,
        );
        assert!(remove.is_empty(), "nothing was on screen before");
        assert_eq!(draw.len(), 1, "the visible part is still drawn: {draw:?}");
        assert_eq!(
            after.shown,
            [(0, 7, 1, 3)],
            "cropped, at the top of the area"
        );
    }

    #[test]
    fn a_picture_scrolled_out_of_view_is_removed() {
        let (_, _, after) = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
        );
        // Scrolled far past it: the picture is not in the list any more, and its id is cleared.
        let (remove, draw, after2) = plan_images(image::Protocol::Kitty, &after, &[], area(), 40);
        assert_eq!(remove, [7]);
        assert!(draw.is_empty());
        assert!(after2.shown.is_empty());
    }

    #[test]
    fn a_resize_redraws_when_it_moves_the_area_or_changes_what_fits() {
        let (_, _, after) = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
        );
        // Wider only: the picture sits exactly where it did, so nothing is sent.
        let wider = Rect::new(0, 0, 120, 24);
        let (remove, draw, _) =
            plan_images(image::Protocol::Kitty, &after, &[placed(10, 7)], wider, 0);
        assert!(remove.is_empty() && draw.is_empty());
        // A shorter transcript: only the first two rows of the picture are on screen, so it is
        // drawn again with a smaller band.
        let shorter = Rect::new(0, 0, 80, 12);
        let (remove, draw, after) =
            plan_images(image::Protocol::Kitty, &after, &[placed(10, 7)], shorter, 0);
        assert_eq!(remove, [7]);
        assert_eq!(draw.len(), 1);
        assert_eq!(after.shown, [(11, 7, 0, 1)], "one row of four");
        // Moved down the screen: every row shifts, so it is drawn again.
        let moved = Rect::new(0, 2, 80, 24);
        let (remove, draw, _) =
            plan_images(image::Protocol::Kitty, &after, &[placed(10, 7)], moved, 0);
        assert_eq!(remove, [7]);
        assert_eq!(draw.len(), 1);
    }

    #[test]
    fn two_pictures_each_keep_their_own_id() {
        let images = [placed(2, 1), placed(10, 2)];
        let (_, draw, after) = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
        );
        assert_eq!(draw.len(), 2);
        assert_eq!(after.shown, [(3, 1, 0, 4), (11, 2, 0, 4)]);
        let (remove, _, _) = plan_images(image::Protocol::Kitty, &after, &[], area(), 0);
        assert_eq!(remove, [1, 2], "both are taken off, not just the first");
    }

    #[test]
    fn the_clipboard_sequence_is_osc_52_with_base64() {
        // "hi" is aGk= in base64; BEL ends the sequence.
        assert_eq!(clipboard_sequence("hi", false), "\x1b]52;c;aGk=\x07");
        // Under tmux the same sequence is wrapped and every ESC inside it doubled.
        let wrapped = clipboard_sequence("hi", true);
        assert_eq!(wrapped, "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        // Nothing selected is not a copy.
        assert_eq!(clipboard_sequence("", false), "\x1b]52;c;\x07");
    }
}
