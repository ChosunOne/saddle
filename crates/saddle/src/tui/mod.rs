//! The full-screen terminal UI. The agent and the session run on a worker thread; this thread
//! reads keys and draws. They talk over two channels, and cancelling goes through the process-wide
//! flag in [`crate::cancel`].

mod app;
mod editor;
mod grant;
pub mod image;
mod login;
mod permissions;
mod render;
mod sessions;
mod settings;
pub mod status;
mod tree;

use std::fmt::Write as _;
use std::io::{Write as _, stdout};
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
use login::{CUSTOM_LABEL, Choice, Draft, Login, LoginRequest};
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
    SetPrice(crate::provider::Price),
    /// Read the policy again after a grant changed, so the session acts on it and the model's next
    /// request carries the new grants (D47).
    ReloadPolicy,
    /// Read the skills and instruction files again and rebuild the prompt (D70).
    Reload,
    SetCompaction(crate::compact::Settings),
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
    /// Remove a saved conversation (D55).
    Delete(String),
    /// Change the reasoning effort (D57).
    Effort(crate::tui::app::Effort),
    Shell {
        command: String,
        share: bool,
    },
    Compact(Option<String>),
    Quit,
}

/// How long to wait for the agent thread to close the session after the UI exits.
const SHUTDOWN: Duration = Duration::from_secs(3);
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
    app.protocol = image::detect(app.header.images.as_deref(), &|k| std::env::var(k).ok());
    let control = Controller {
        commands: cmd_tx,
        events: ev_tx_ui,
        paths,
        store,
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
fn work(
    mut agent: Agent,
    mut session: Session,
    commands: Receiver<Command>,
    events: Sender<AgentEvent>,
) -> Result<()> {
    let mut ui = ChannelUi(events.clone());
    let mut fatal = None;
    for command in commands {
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
            Command::Clear => agent.clear(),
            Command::SetMaxSteps(n) => agent.set_max_steps(n),
            Command::SetWindow(window) => agent.set_window(window),
            Command::SetPrice(price) => agent.set_price(price),
            Command::ReloadPolicy => match session.reload_policy() {
                Ok(()) => {
                    // The prompt is sent with every request, so replacing its grants section is
                    // what tells the model (D47).
                    agent.set_grants(crate::agent::grants_section(&session));
                    let _ = events.send(AgentEvent::Note(
                        "grants reloaded; the model is told on its next request".into(),
                    ));
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
                let cfg = session.setup.context.clone();
                let paths = session.setup.paths.clone();
                let notes = match crate::context::load(&mut session, &cfg, &paths) {
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
                        let names = crate::skills::names(&session.skills);
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
                    crate::tui::app::Effort::Set(value) => value,
                    crate::tui::app::Effort::Cycle => {
                        crate::provider::next_effort(agent.effort().as_deref())
                    }
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
                    if !left.abandoned.is_empty()
                        && agent
                            .summarize_left_behind(&mut session, &mut ui, &left.abandoned)
                            .unwrap_or(false)
                    {
                        resumed = agent.state()?;
                    }
                    let _ = events.send(AgentEvent::Resumed(resumed));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("could not go back: {e:#}")));
                }
            },
            Command::SetProvider(provider) => {
                if let Err(e) = agent.set_provider(&mut session, *provider) {
                    let msg = format!("{e:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
            }
            Command::Quit => break,
        }
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

    fn quota(&mut self, quota: &crate::provider::Quota) {
        let _ = self.0.send(AgentEvent::Quota(quota.clone()));
    }

    fn cancelled(&self) -> bool {
        cancel::requested()
    }
}

impl Ui for ChannelUi {
    fn attached(&mut self, images: &[crate::provider::Image]) {
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
            content: out.content.clone(),
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

    fn compaction(&mut self, event: crate::compact::Event) {
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
    area: ratatui::layout::Rect,
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
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    control: &Controller,
    events: &Receiver<AgentEvent>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    loop {
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
                    Some(Action::Copy(text)) => match copy_to_clipboard(&text) {
                        Ok(()) => app.copied = Some(Instant::now()),
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
            Action::Reload => {
                // The settings side first, keeping the session-only choices a person made:
                // `/thinking` and ctrl+t/ctrl+o are not in config.toml (D70).
                self.reload_settings(app);
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
            Action::Login(req) => self.login(app, &req),
            Action::Logout(name) => self.logout(app, name),
            Action::Model(name) => self.model(app, name),
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
            Action::EditPrompt | Action::Copy(_) => Ok(()),
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

    fn open_login(&self, app: &mut App, preselect: Option<String>, direct: bool) -> Result<()> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let builtin = provider::builtin_names();
        let mut choices: Vec<Choice> = provider::catalog(&user.providers)
            .into_iter()
            .map(|(name, entry)| {
                let env = entry
                    .api_key_env
                    .clone()
                    .filter(|var| std::env::var(var).is_ok_and(|k| !k.is_empty()));
                let stored = auth.key(&name).is_some();
                let status = match (&env, stored, &entry.api_key_env) {
                    (Some(var), _, _) => format!("key from ${var}"),
                    (None, true, _) => "logged in".into(),
                    (None, false, None) => "no key needed".into(),
                    (None, false, Some(_)) => String::new(),
                };
                let detail = match (&entry.model, provider::key_page(&name)) {
                    (Some(model), _) => model.clone(),
                    (None, Some(page)) => format!("keys at {page}"),
                    (None, None) => entry.base_url.clone().unwrap_or_default(),
                };
                Choice {
                    needs_key: entry.api_key_env.is_some(),
                    has_key: env.is_some() || stored,
                    model: entry.model.clone(),
                    name,
                    detail,
                    status,
                    custom: false,
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
        });
        let auth_path = render::home_relative(&self.paths.auth_store());
        app.open_login(Login::new(
            choices,
            builtin,
            preselect.as_deref(),
            direct,
            auth_path,
        ));
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
        let events = self.events.clone();
        let cache = self.paths.reported_windows();
        thread::Builder::new()
            .name("saddle-models".into())
            .spawn(move || {
                let listed = provider::list_models(&config, key.as_deref());
                // A listing is where the server states its real limits, so keep them for the
                // sessions that never list anything (D34). Failing to cache costs nothing but the
                // default window next time.
                if let Ok(models) = &listed {
                    let mut reported = provider::Reported::stored(&cache);
                    reported.record(&config.base_url, models);
                    let _ = reported.save(&cache);
                }
                let ids = listed.map(|models| models.into_iter().map(|m| m.id).collect());
                let _ = events.send(AgentEvent::Models(id, ids));
            })
            .context("starting the model lookup")?;
        Ok(())
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
            &std::env::temp_dir(),
            &crate::config::Overrides::default(),
        )?;
        // The roots the session would resolve, so the write is validated as a session would.
        let roots = crate::session::roots_for_validation()?;
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
            Change::ChooseModel => {
                self.open_login(app, Some(provider), true)?;
                // The current key is kept, so go straight to the model list.
                if let Some((id, draft)) = app.login.as_mut().and_then(|l| l.start_check()) {
                    self.check_models(id, draft)?;
                }
                return Ok(());
            }
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
            Change::AutoCompact(on) => {
                config::edit(&path, |doc| {
                    config::table(doc, "compaction")?["auto"] = toml_edit::value(on);
                    Ok(())
                })?;
                let user = UserConfig::load(&path)?;
                let settings = crate::compact::Settings::from_config(&user.compaction);
                self.send(app, Command::SetCompaction(settings));
            }
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
        app.max_steps = user.max_steps.filter(|n| *n > 0);
        self.send(app, Command::SetMaxSteps(app.max_steps));
        let compaction = crate::compact::Settings::from_config(&user.compaction);
        self.send(app, Command::SetCompaction(compaction));
        // The provider is rebuilt only when the file names a different one; otherwise the session
        // keeps the provider it has, with the effort and the window it has settled on.
        let named = user.provider.clone();
        if let Some(name) = named
            && name != app.header.provider
        {
            match self.build(&name) {
                Ok(p) => self.switch(app, p),
                Err(e) => app.note(NoteKind::Error, &format!("{e:#}")),
            }
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
        let compaction = crate::compact::Settings::from_config(&user.compaction);
        self.send(app, Command::SetCompaction(compaction));
        let name = user
            .provider
            .clone()
            .or_else(|| Some(app.header.provider.clone()).filter(|p| !p.is_empty()));
        if let Some(name) = name {
            match self.build(&name) {
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
        let checked = std::fs::read_to_string(&path)
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
        let user = UserConfig::load(&self.paths.user_config())?;
        let config = provider::endpoint(&name, &user.providers)?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let key = provider::available_key(&config, auth.key(&name));
        let cache = self.paths.reported_windows();
        let events = self.events.clone();
        thread::Builder::new()
            .name("saddle-window".into())
            .spawn(move || {
                if provider::ask_endpoint(&config, key.as_deref(), &cache).is_some() {
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
            &std::env::temp_dir(),
            &crate::config::Overrides::default(),
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
            &std::env::temp_dir(),
            &crate::config::Overrides::default(),
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
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let key = provider::available_key(&config, auth.key(&name));
        let events = self.events.clone();
        thread::Builder::new()
            .name("saddle-usage".into())
            .spawn(move || {
                if let Ok(page) = provider::fetch_usage(&config, key.as_deref(), &url) {
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
        let user = UserConfig::load(&self.paths.user_config())?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let reported = provider::Reported::stored(&self.paths.reported_windows());
        let config = provider::resolve(name, &user.providers, None)?;
        Ok(Provider::new(name, config, auth.key(name), &reported))
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
        config::edit(&self.paths.user_config(), |doc| {
            doc["provider"] = toml_edit::value(req.provider.as_str());
            if let Some(url) = &req.base_url {
                let table = config::provider_table(doc, &req.provider)?;
                table["api"] = toml_edit::value("openai");
                table["base-url"] = toml_edit::value(url.as_str());
            }
            config::provider_table(doc, &req.provider)?["model"] =
                toml_edit::value(req.model.as_str());
            Ok(())
        })?;
        let provider = self.build(&req.provider)?;
        if let Some(why) = provider.missing_key() {
            bail!(why);
        }
        let mut msg = format!(
            "using {} with {}; saved as the default provider",
            provider.name, provider.config.model
        );
        if let Some(var) = provider
            .config
            .api_key_env
            .as_ref()
            .filter(|var| req.key.is_some() && std::env::var(var).is_ok_and(|k| !k.is_empty()))
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
        let provider_name = app.header.provider.clone();
        let Some(model) = name else {
            let msg = format!(
                "model: {} on {provider_name}. /model <name> switches to another and saves it as \
                 {provider_name}'s default.",
                app.header.model
            );
            app.note(NoteKind::Info, &msg);
            return Ok(());
        };
        config::edit(&self.paths.user_config(), |doc| {
            config::provider_table(doc, &provider_name)?["model"] =
                toml_edit::value(model.as_str());
            Ok(())
        })?;
        let provider = self.build(&provider_name)?;
        self.switch(app, provider);
        app.note(
            NoteKind::Info,
            &format!("now using {model} on {provider_name}; saved as its default"),
        );
        Ok(())
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
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(&paths.state)?;
    let path = paths
        .state
        .join(format!("prompt-{}.md", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(app.editor.text().as_bytes())?;
    drop(file);
    let edited = edit_file(terminal, &path, app.header.mouse);
    let text = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    edited?;
    if let Ok(text) = text {
        app.editor.set(text.trim_end_matches('\n').to_owned());
    }
    Ok(())
}

/// Hands the terminal to `$VISUAL` or `$EDITOR` (or vi) for `path`, then takes it back.
fn edit_file(terminal: &mut DefaultTerminal, path: &std::path::Path, mouse: bool) -> Result<()> {
    leave_modes();
    ratatui::restore();
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|e| !e.trim().is_empty())
        .unwrap_or_else(|| "vi".into());
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or("vi").to_owned();
    let status = std::process::Command::new(&program)
        .args(words)
        .arg(path)
        .status();
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
    use std::io::Write as _;
    let tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let mut stdout = stdout();
    stdout.write_all(clipboard_sequence(text, tmux).as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// The bytes that put `text` on the terminal's clipboard: OSC 52 with the text base64-encoded.
/// tmux forwards an escape sequence only when it is wrapped in one of its own, with every ESC
/// inside doubled, so that form is built here rather than in the writer.
fn clipboard_sequence(text: &str, tmux: bool) -> String {
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::STANDARD.encode(text);
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
        enter_modes(mouse)?;
        // ratatui's hook restores raw mode and the screen; this one also turns off the modes above.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
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
    use super::*;

    fn placed(line: usize, id: u32) -> Placed {
        Placed {
            line,
            id,
            box_: crate::tui::image::Fit {
                cols: 10,
                rows: 4,
                scaled: false,
            },
            pixels: (100, 80),
            png: vec![7u8; 8],
        }
    }

    fn area() -> ratatui::layout::Rect {
        ratatui::layout::Rect::new(0, 0, 80, 24)
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
        let wider = ratatui::layout::Rect::new(0, 0, 120, 24);
        let (remove, draw, _) =
            plan_images(image::Protocol::Kitty, &after, &[placed(10, 7)], wider, 0);
        assert!(remove.is_empty() && draw.is_empty());
        // A shorter transcript: only the first two rows of the picture are on screen, so it is
        // drawn again with a smaller band.
        let shorter = ratatui::layout::Rect::new(0, 0, 80, 12);
        let (remove, draw, after) =
            plan_images(image::Protocol::Kitty, &after, &[placed(10, 7)], shorter, 0);
        assert_eq!(remove, [7]);
        assert_eq!(draw.len(), 1);
        assert_eq!(after.shown, [(11, 7, 0, 1)], "one row of four");
        // Moved down the screen: every row shifts, so it is drawn again.
        let moved = ratatui::layout::Rect::new(0, 2, 80, 24);
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
