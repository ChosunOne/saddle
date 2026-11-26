use crate::compact::Event as CompactEvent;
use crate::compact::Settings;
use crate::config::Overrides;
use crate::context;
use crate::plugins::HostFailure;
use crate::plugins::services::ModelUsage;
use crate::plugins::services::panel::PanelView;
use crate::provider::Contribution;
use crate::provider::Image;
use crate::provider::Price;
use crate::provider::Quota;
use crate::questions::{Asked, Outcome as QuestionOutcome, Questioner};
use crate::session;
use crate::skills;
use crate::tasks;
use crate::team::{Render, Steering};
use crate::tui::app::Effort;
use crate::tui::theme::Theme;
use crate::tui::themes::ThemePicker;
use base64::Engine as _;
use base64::engine::general_purpose;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::terminal as crossterm_terminal;
use ratatui::layout::Rect;
use saddle_cap::Scope;
use std::cell::RefCell;
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
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::OnceLock;
mod app;
mod ask;
mod browser;
pub mod catppuccin;
mod editor;
mod files;
mod grant;
pub mod image;
pub mod keys;
mod login;
mod math;
pub mod notify;
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
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, enable_raw_mode, supports_keyboard_enhancement,
};

pub use app::Header;
use app::{Action, AgentEvent, App, Drawn, NoteKind, Placed};
use login::{CUSTOM_LABEL, Choice, Draft, Listed, Login, LoginRequest, Pick};
use notify::Focus;
use permissions::{Kind, Permissions, Row};

use crate::agent::{Agent, Compacted, End, Steer, Ui, call_detail};
use crate::auth::AuthStore;
use crate::cancel;
use crate::compact::Reason;
use crate::config::{self, UserConfig};
use crate::paths::Paths;
use crate::provider::{self, Provider, Reply, Sink, ToolCall};
use crate::session::{Session, Setup};
use crate::store;
use crate::tools::ToolOutput;
use crate::tools::timeout;
use sessions::SessionPicker;
use settings::{Change, SettingsDialog, Values};
use status::Template;

enum Command {
    Prompt { text: String, skill: Option<String> },
    BackgroundTurn,
    Clear,
    SetProvider(Box<Provider>),
    SetMaxSteps(Option<usize>),
    SetWindow(u64),
    SetPrice(Price),
    ReloadPolicy,
    Reload,
    SetCompaction(Settings),
    Resume(String),
    OpenTree,
    Rewind { node: Option<i64>, summarize: bool },
    Clone,
    Name(Option<String>),
    SessionInfo,
    Mcp(String),
    Tasks(String),
    Workers,
    OpenWorker(String),
    WorkerBack,
    Profile(String),
    Delete(String),
    Effort(Effort),
    Shell { command: String, share: bool },
    Compact(Option<String>),
    Quit,
}

const SHUTDOWN: Duration = Duration::from_secs(3);
const IDLE_TICK: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(80);

pub enum Start {
    Ready,
    ChooseProvider,
    LogIn(String),
}

pub fn run(
    agent: Agent,
    session: Session,
    header: Header,
    paths: Paths,
    start: Start,
    resume: Option<String>,
) -> Result<()> {
    let skills = session.skills.clone();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    let ev_tx_ui = ev_tx.clone();
    let steering = agent.steering();
    let team = session.team.steering();
    let worker = thread::Builder::new()
        .name("saddle-agent".into())
        .spawn(move || work(agent, session, cmd_rx, ev_tx))
        .context("starting the agent thread")?;

    let store = store::Store::open(&paths.session_db(), header.workspace.clone())?;
    let mut app = App::new(header);
    app.skills = skills;
    app.protocol = image::detect(app.header.images.as_deref(), &|k| env::var(k).ok());
    if app.protocol != image::Protocol::Text {
        image::set_cell_px(terminal::cell_px());
    }
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
        steering,
        team,
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
    if ready && let Err(e) = control.ask_window(&mut app) {
        app.note(NoteKind::Error, &format!("{e:#}"));
    }
    if ready && let Err(e) = control.refresh_usage(&mut app) {
        app.note(NoteKind::Error, &format!("{e:#}"));
    }
    let ui_result = {
        let mut screen = Screen::enter(app.header.mouse)?;
        event_loop(&mut screen, &mut app, &control, &ev_rx)
    };

    if app.busy.is_some() {
        cancel::request();
    }
    let _ = control.commands.send(Command::Quit);
    drop(control);
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

pub fn theme_selected(user: &UserConfig) -> Result<Theme> {
    let name = user
        .ui
        .theme
        .clone()
        .unwrap_or_else(|| "default".to_owned());
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
    session.questions = Some(Box::new(ChannelQuestions {
        events: events.clone(),
    }));
    let mut fatal = None;
    let mut shown_tasks = Vec::new();
    let mut completion_requested = false;
    let tick = IDLE_TICK;
    let mut last_activity = Instant::now();
    loop {
        if let Err(error) = tasks::notices(&mut session, false).map(|notes| {
            for note in notes {
                let _ = events.send(AgentEvent::Note(note));
            }
        }) {
            let msg = format!("{error:#}");
            let _ = events.send(AgentEvent::Fatal(msg.clone()));
            fatal = Some(msg);
            break;
        }
        for note in session.team.notices(false) {
            let _ = events.send(AgentEvent::Note(note));
        }
        let views = session.tasks.views();
        if views != shown_tasks {
            ui.tasks(&views);
            shown_tasks = views;
        }
        ui.workers(&session.team.views());
        if !(session.tasks.pending() || session.team.pending()) {
            completion_requested = false;
        }
        let command = match commands.recv_timeout(tick) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => {
                if let Err(e) = agent.poll_background(&mut session, &mut ui) {
                    let msg = format!("{e:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
                if (session.tasks.pending() || session.team.pending()) && !completion_requested {
                    let _ = events.send(AgentEvent::BackgroundReady);
                    completion_requested = true;
                    last_activity = Instant::now();
                    continue;
                }
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
                    last_activity = Instant::now();
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if matches!(&command, Command::Prompt { .. }) {
            completion_requested = false;
        }
        match command {
            Command::BackgroundTurn => match agent.background_turn(&mut session, &mut ui) {
                Ok(end) => {
                    if end.is_some() {
                        completion_requested = false;
                    }
                    let _ = events.send(AgentEvent::TurnEnd(end.unwrap_or(End::Answered)));
                }
                Err(error) => {
                    let msg = format!("{error:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
            },
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
            Command::Profile(name) if name.is_empty() => {
                let _ = events.send(AgentEvent::Note(session::profile_report(&session.setup)));
            }
            Command::Profile(name) => {
                let chosen = (name != config::NONE_PROFILE).then_some(name.as_str());
                match session.switch_profile(chosen) {
                    Ok(()) => {
                        agent.refresh_extensions(&session, &mut ChannelUi(events.clone()));
                        let _ = events.send(AgentEvent::Note(format!(
                            "profile: {}; the model is told on its next request",
                            session.setup.profile.as_deref().unwrap_or("none")
                        )));
                    }
                    Err(error) if error.is::<HostFailure>() => {
                        let message = format!("{error:#}");
                        let _ = events.send(AgentEvent::Fatal(message.clone()));
                        fatal = Some(message);
                        break;
                    }
                    Err(error) => {
                        let _ = events.send(AgentEvent::Note(format!(
                            "the profile was not switched, so this session keeps its grants: {error:#}"
                        )));
                    }
                }
            }
            Command::SetMaxSteps(n) => agent.set_max_steps(n),
            Command::SetWindow(window) => agent.set_window(window),
            Command::SetPrice(price) => agent.set_price(price),
            Command::ReloadPolicy => match session.reload_policy() {
                Ok(()) => {
                    agent.refresh_extensions(&session, &mut ChannelUi(events.clone()));
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
            Command::Reload => {
                match session.reload_policy() {
                    Ok(()) => agent.refresh_extensions(&session, &mut ChannelUi(events.clone())),
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
            Command::Tasks(arg) => match tasks::human(&mut session, &arg) {
                Ok(note) => {
                    let _ = events.send(AgentEvent::Note(note));
                }
                Err(error) if error.is::<HostFailure>() => {
                    let msg = format!("{error:#}");
                    let _ = events.send(AgentEvent::Fatal(msg.clone()));
                    fatal = Some(msg);
                    break;
                }
                Err(error) => {
                    let _ = events.send(AgentEvent::Note(format!("{error:#}")));
                }
            },
            Command::Workers => {
                let _ = events.send(AgentEvent::Workers(session.team.views()));
            }
            Command::OpenWorker(name) => match agent.open_worker(&mut session, &name, &mut ui) {
                Ok(()) => {
                    let _ = events.send(AgentEvent::Workers(session.team.views()));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("{e:#}")));
                    let _ = events.send(AgentEvent::Workers(session.team.views()));
                }
            },
            Command::WorkerBack => match agent.close_worker(&mut session, &mut ui) {
                Ok(()) => {
                    let _ = events.send(AgentEvent::Workers(session.team.views()));
                }
                Err(e) => {
                    let _ = events.send(AgentEvent::Note(format!("{e:#}")));
                }
            },
            Command::Mcp(arg) => {
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
            Command::Rewind { node, summarize } => match agent.rewind(node) {
                Ok(left) => {
                    let mut resumed = left.resumed;
                    session.mcp.restore(&resumed.declared);
                    agent.refresh_tools(session.declared_tools());
                    if summarize
                        && !left.abandoned.is_empty()
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
    fn workers(&mut self, workers: &[Render]) {
        let _ = self.0.send(AgentEvent::Workers(workers.to_vec()));
    }
    fn tasks(&mut self, tasks: &[tasks::View]) {
        let _ = self.0.send(AgentEvent::Tasks(tasks.to_vec()));
    }
    fn contribution(&mut self, value: &Contribution) {
        let _ = self.0.send(AgentEvent::Contribution(value.clone()));
    }
    fn panels(&mut self, panels: &[PanelView]) {
        let _ = self.0.send(AgentEvent::Panels(panels.to_vec()));
    }
    fn plugin_usage(&mut self, used: &ModelUsage) {
        let _ = self.0.send(AgentEvent::PluginUsage {
            usage: used.usage,
            cost: used.cost.unwrap_or(0.0),
        });
    }

    fn steered(&mut self, text: &str) {
        let _ = self.0.send(AgentEvent::Steered(text.to_owned()));
    }

    fn attached(&mut self, images: &[Image]) {
        let _ = self.0.send(AgentEvent::Attached(images.to_vec()));
    }

    fn call(&mut self, call: &ToolCall) {
        let _ = self.0.send(AgentEvent::Call {
            id: call.id.clone(),
            name: call.name.clone(),
            detail: call_detail(call),
            timeout: timeout::requested(call),
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

struct ChannelQuestions {
    events: Sender<AgentEvent>,
}

impl Questioner for ChannelQuestions {
    fn ask(&mut self, asked: Arc<Asked>, _until: Option<Instant>) {
        if self.events.send(AgentEvent::Ask(asked.clone())).is_err() {
            asked.answer(QuestionOutcome::Unavailable("the interface closed".into()));
        }
    }
}

#[derive(Debug, Default)]
struct Plan {
    free: Vec<u32>,
    hide: Vec<u32>,
    transmit: Vec<(u32, String)>,
    place: Vec<(usize, u16, String)>,
    drawn: Drawn,
}

fn plan_images(
    protocol: image::Protocol,
    drawn: &Drawn,
    images: &[Placed],
    area: Rect,
    top: usize,
    covered: &[Rect],
) -> Plan {
    let height = area.height as usize;
    let mut visible: Vec<(usize, &Placed, u16, u16)> = Vec::new();
    for placed in images {
        let Some((from, rows)) = image::band(placed.line, placed.box_.rows, top, height) else {
            continue;
        };
        let row = placed.line + from as usize - top;
        visible.push((row, placed, from, rows));
    }
    let clear_rows = |row: usize, rows: u16| -> Option<u16> {
        let top = area.y + row as u16;
        let mut cut = rows;
        for rect in covered {
            if !Rect::new(area.x, top, area.width, rows).intersects(*rect) {
                continue;
            }
            if rect.y <= top {
                return None;
            }
            cut = cut.min(rect.y - top);
        }
        (cut > 0).then_some(cut)
    };
    let wanted: Vec<(usize, &Placed, u16, u16)> = visible
        .iter()
        .filter_map(|(row, placed, from, rows)| {
            clear_rows(*row, *rows).map(|cut| (*row, *placed, *from, cut))
        })
        .collect();
    let resized = drawn
        .at
        .is_some_and(|at| (at.width, at.height) != (area.width, area.height));
    let stale = drawn.stale || resized;
    let moved = stale || drawn.at != Some(area);
    let kitty = protocol == image::Protocol::Kitty;
    let mut plan = Plan::default();
    let mut held = if stale {
        Vec::new()
    } else {
        drawn.held.clone()
    };
    if kitty && stale {
        plan.free.extend(drawn.held.iter().copied());
    }
    held.retain(|id| {
        let keep = visible.iter().any(|(_, placed, _, _)| placed.id == *id);
        if kitty && !keep {
            plan.free.push(*id);
        }
        keep
    });
    if kitty && !stale {
        for (_, _, id, _, _) in &drawn.shown {
            let hidden = visible.iter().any(|(_, placed, _, _)| placed.id == *id)
                && !wanted.iter().any(|(_, placed, _, _)| placed.id == *id);
            if hidden {
                plan.hide.push(*id);
            }
        }
    }
    for (row, placed, from, rows) in &wanted {
        let id = placed.id;
        if !held.contains(&id) {
            if let Some(escape) = image::transmit(protocol, &placed.data, id) {
                plan.transmit.push((id, escape));
            }
            held.push(id);
        }
        let old = drawn
            .shown
            .iter()
            .find(|(_, _, drawn_id, _, _)| *drawn_id == id);
        let unchanged = !moved
            && old.is_some_and(|(drawn_row, drawn_col, _, drawn_from, drawn_rows)| {
                *drawn_row == *row
                    && *drawn_col == placed.column
                    && *drawn_from == *from
                    && *drawn_rows == *rows
            });
        if !unchanged {
            let window = image::Window {
                full: placed.box_,
                from: *from,
                rows: *rows,
            };
            if let Some(escape) = image::draw(protocol, &placed.data, placed.pixels, &window, id) {
                plan.place.push((*row, placed.column, escape));
            }
        }
    }
    plan.drawn = Drawn {
        shown: wanted
            .iter()
            .map(|(row, placed, from, rows)| (*row, placed.column, placed.id, *from, *rows))
            .collect(),
        held,
        at: Some(area),
        covered: covered.to_vec(),
        stale: false,
    };
    plan
}

fn image_escapes(plan: &Plan, area: Rect) -> String {
    let mut out = String::from("\x1b7");
    for id in &plan.free {
        let _ = write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\");
    }
    for id in &plan.hide {
        let _ = write!(out, "\x1b_Ga=d,d=i,i={id},q=2\x1b\\");
    }
    for (_, escape) in &plan.transmit {
        out.push_str(escape);
    }
    for (row, column, escape) in &plan.place {
        let _ = write!(
            out,
            "\x1b[{};{}H{}",
            area.y as usize + row + 1,
            area.x as usize + *column as usize + 1,
            escape
        );
    }
    if out.len() > 4 {
        out.push_str("\x1b8");
    } else {
        out.clear();
    }
    out
}

fn image_log() -> Option<&'static Mutex<io::BufWriter<fs::File>>> {
    static LOG: OnceLock<Option<Mutex<io::BufWriter<fs::File>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let path = env::var_os("SADDLE_IMAGE_LOG")?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()?;
        Some(Mutex::new(io::BufWriter::new(file)))
    })
    .as_ref()
}

fn trace_line(plan: &Plan, area: Rect, top: usize) -> String {
    let mut line = format!(
        "frame x={} y={} {}x{} top={top} covered={}",
        area.x,
        area.y,
        area.width,
        area.height,
        plan.drawn.covered.len()
    );
    for id in &plan.free {
        let _ = write!(line, " free {id}");
    }
    for id in &plan.hide {
        let _ = write!(line, " hide {id}");
    }
    for (id, _) in &plan.transmit {
        let _ = write!(line, " transmit {id}");
    }
    for (row, column, escape) in &plan.place {
        let _ = write!(
            line,
            " place{row}.{column} {}",
            escape.replace('\x1b', "ESC").replace('\\', "ST")
        );
    }
    for (row, column, id, from, rows) in &plan.drawn.shown {
        let _ = write!(line, " shown {id}@{row}.{column}+{from}/{rows}");
    }
    line.push('\n');
    line
}

fn trace_frame(plan: &Plan, area: Rect, top: usize) {
    let Some(log) = image_log() else {
        return;
    };
    let line = trace_line(plan, area, top);
    if let Ok(mut out) = log.lock() {
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

fn flush_frame(frame: &FrameBuffer, app: &mut App) {
    let mut out = frame.take();
    if app.protocol != image::Protocol::Text
        && let Some(seen) = &app.seen
    {
        let (area, top) = (seen.area, seen.top);
        let plan = plan_images(
            app.protocol,
            &app.drawn,
            &app.images,
            area,
            top,
            &app.covered,
        );
        out.extend_from_slice(image_escapes(&plan, area).as_bytes());
        trace_frame(&plan, area, top);
        app.repaint |= app.drawn.covered != plan.drawn.covered;
        app.drawn = plan.drawn;
    }
    let mut stdout = io::stdout();
    let _ = stdout.write_all(&out);
    let _ = stdout.flush();
}

const MODE_CHECK: u64 = 500;

fn keep_raw_mode() {
    if matches!(crossterm_terminal::is_raw_mode_enabled(), Ok(false)) {
        let _ = crossterm_terminal::enable_raw_mode();
    }
}

fn event_loop(
    screen: &mut Screen,
    app: &mut App,
    control: &Controller,
    events: &Receiver<AgentEvent>,
) -> Result<()> {
    let mut dirty = true;
    let mut last_tick = Instant::now();
    let mut last_mode_check = Instant::now();
    let mut focus = Focus::Unknown;
    loop {
        if last_mode_check.elapsed() >= Duration::from_millis(MODE_CHECK) {
            last_mode_check = Instant::now();
            keep_raw_mode();
        }
        if app.repaint {
            app.repaint = false;
            if app.protocol == image::Protocol::Iterm2 {
                screen.terminal.clear()?;
                dirty = true;
            }
        }
        if dirty {
            screen.terminal.draw(|f| render::draw(f, app))?;
            flush_frame(&screen.frame, app);
            dirty = false;
        }
        if event::poll(Duration::from_millis(30))? {
            loop {
                let action = match event::read()? {
                    Event::Key(key) => app.key(key),
                    Event::Paste(text) => {
                        app.paste(&text);
                        None
                    }
                    Event::Mouse(m) => app.mouse(m),
                    Event::FocusGained => {
                        focus = Focus::In;
                        None
                    }
                    Event::FocusLost => {
                        focus = Focus::Out;
                        None
                    }
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
                    Some(Action::EditPrompt) => {
                        edit_prompt(&mut screen.terminal, app, &control.paths)?
                    }
                    Some(Action::Apply(Change::EditConfig)) => {
                        let path = control.paths.user_config();
                        edit_file(&mut screen.terminal, &path, app)?;
                        control.after_config_edit(app);
                    }
                    Some(Action::Apply(Change::EditPolicy)) => {
                        let path = control.paths.user_config();
                        edit_file(&mut screen.terminal, &path, app)?;
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
            let turn_ended = matches!(ev, AgentEvent::TurnEnd(_));
            let outcome = match &ev {
                AgentEvent::TurnEnd(end) => notify::ended(*end),
                AgentEvent::Fatal(_) => Some("the session stopped"),
                _ => None,
            };
            if let Some(action) = app.agent(ev) {
                control.perform(app, action);
            }
            if turn_ended && let Err(e) = control.refresh_usage(app) {
                app.note(NoteKind::Error, &format!("{e:#}"));
            }
            if let Some(outcome) = outcome
                && app.busy.is_none()
            {
                announce(app, focus, outcome);
            }
            dirty = true;
        }
        if app.overview.is_some() || app.worker.is_some() {
            let views = control.team.views();
            if views != app.team {
                if let Some(action) = app.agent(AgentEvent::Workers(views)) {
                    control.perform(app, action);
                }
                dirty = true;
            }
            if app.overview.is_some() {
                let bus = control.team.bus();
                if bus != app.bus {
                    app.agent(AgentEvent::Bus(bus));
                    dirty = true;
                }
                let jobs = control.team.queue();
                if jobs != app.jobs {
                    app.agent(AgentEvent::Queue(jobs));
                    dirty = true;
                }
            }
        }
        let watching = app.overview.is_some() || app.worker.is_some();
        let working = app.busy.is_some()
            || app.background_running()
            || (watching
                && app
                    .team
                    .iter()
                    .any(|worker| worker.state.starts_with("running")))
            || app.login.as_ref().is_some_and(|l| l.checking());
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

struct Controller {
    commands: Sender<Command>,
    events: Sender<AgentEvent>,
    paths: Paths,
    store: store::Store,
    signing: Mutex<Option<Arc<AtomicBool>>>,
    steering: Steer,
    team: Steering,
}

impl Controller {
    fn perform(&self, app: &mut App, action: Action) {
        let result = match action {
            Action::Send(text) => {
                cancel::reset();
                self.send(app, Command::Prompt { text, skill: None });
                Ok(())
            }
            Action::Steer(text) => {
                self.steering.push(text);
                Ok(())
            }
            Action::Theme(name) => {
                self.theme(app, name);
                Ok(())
            }
            Action::Reload => {
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
            Action::Rewind { node, summarize } => {
                self.send(app, Command::Rewind { node, summarize });
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
            Action::BackgroundTurn => {
                cancel::reset();
                self.send(app, Command::BackgroundTurn);
                Ok(())
            }
            Action::Tasks(arg) => {
                self.send(app, Command::Tasks(arg));
                Ok(())
            }
            Action::Profile(arg) => {
                self.send(app, Command::Profile(arg));
                Ok(())
            }
            Action::Mcp(arg) => {
                self.send(app, Command::Mcp(arg));
                Ok(())
            }
            Action::Workers => {
                self.send(app, Command::Workers);
                Ok(())
            }
            Action::OpenWorker(name) => {
                self.send(app, Command::OpenWorker(name));
                Ok(())
            }
            Action::WorkerBack => {
                self.send(app, Command::WorkerBack);
                Ok(())
            }
            Action::WorkerInput(text) => {
                if let Some(name) = &app.worker {
                    self.team.say(name, text);
                }
                Ok(())
            }
            Action::WorkerInterrupt(name) => {
                self.team.interrupt(&name);
                Ok(())
            }
            Action::WorkerStop(name) => {
                self.team.stop(&name);
                Ok(())
            }
            Action::DeleteSession(id) => {
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
            Action::OpenUrl(url) => {
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
        let pending = login.take_pending();
        app.open_login(login);
        match pending {
            Some(login::Outcome::SignIn(id, provider)) => self.sign_in(app, id, &provider),
            Some(login::Outcome::Check(id, draft)) => self.check_models(id, draft)?,
            _ => {}
        }
        Ok(())
    }

    fn sign_in(&self, app: &mut App, id: u64, provider: &str) {
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

    fn open_models(&self, app: &mut App) -> Result<()> {
        let provider = app.header.provider.clone();
        if provider.is_empty() {
            return self.open_login(app, None, false);
        }
        let auth_path = render::home_relative(&self.paths.auth_store());
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

    fn list_all_models(&self, app: &mut App, id: u64) -> Result<()> {
        let (choices, _) = self.login_choices()?;
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

    fn provider_for_listing(&self, name: &str) -> Result<Provider> {
        let user = UserConfig::load(&self.paths.user_config())?;
        let auth = AuthStore::load(&self.paths.auth_store())?;
        let reported = provider::Reported::stored(&self.paths.reported_windows());
        let config = provider::endpoint(name, &user.providers)?;
        Ok(Provider::new(name, config, auth.key(name), &reported)
            .with_stored_oauth(&self.paths.auth_store()))
    }

    fn open_resume(&self, app: &mut App) -> Result<()> {
        let mut listed = self.store.list()?;
        self.store
            .adopt_workers(&self.paths.audit_dir(), &mut listed)?;
        let sessions: Vec<_> = listed
            .into_iter()
            .filter(|session| !session.worker)
            .collect();
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

    fn grant(&self, app: &mut App, change: permissions::Change) {
        let setup = Setup::load(
            &app.header.workspace,
            &env::temp_dir(),
            &Overrides::default(),
        );
        let setup = match setup {
            Ok(setup) => setup,
            Err(e) => return Self::grant_failed(app, &change, e),
        };
        if let Err(e) = self.apply_grant(&setup, app, &change) {
            return Self::grant_failed(app, &change, e);
        }
        if let Some(dialog) = &mut app.permissions {
            dialog.error = None;
        }
        match self.permissions_snapshot(&setup) {
            Ok((rows, text)) => {
                if let Some(dialog) = &mut app.permissions {
                    dialog.reload(rows, text);
                }
            }
            Err(e) => app.note(NoteKind::Error, &format!("{e:#}")),
        }
        self.send(app, Command::ReloadPolicy);
    }

    fn grant_failed(app: &mut App, change: &permissions::Change, e: anyhow::Error) {
        if let Some(dialog) = &mut app.permissions {
            match change {
                permissions::Change::Add(what) => dialog.retry(what.clone()),
                permissions::Change::Exclude { patterns, .. } => {
                    dialog.editing = Some(patterns.join(", "));
                }
                permissions::Change::Toggle { .. } => {}
            }
            dialog.error = Some(format!("{e:#}"));
        }
    }

    fn apply_grant(
        &self,
        setup: &Setup,
        app: &mut App,
        change: &permissions::Change,
    ) -> Result<()> {
        let roots = session::roots_for_validation()?;
        if let permissions::Change::Add(what) = change {
            let added = grant::add(&setup.workspace_policy_path, what, &roots)?;
            app.note(NoteKind::Info, &format!("added {}", added.summary));
            return Ok(());
        }
        grant::edit(&setup.workspace_policy_path, change, &roots)
    }

    fn open_settings(&self, app: &mut App) -> Result<()> {
        app.settings = Some(SettingsDialog::new(self.settings_values(app)?));
        Ok(())
    }

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

    fn theme(&self, app: &mut App, name: Option<String>) {
        let Some(name) = name else {
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
        self.reload_theme(app);
        app.header.notify = user.ui.notify.unwrap_or_default();
        app.header.keys = user.keys.clone();
        app.max_steps = user.max_steps.filter(|n| *n > 0);
        self.send(app, Command::SetMaxSteps(app.max_steps));
        let compaction = Settings::from_config(&user.compaction);
        self.send(app, Command::SetCompaction(compaction));
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

    fn permission_rows(&self, setup: &Setup) -> Result<Vec<Row>> {
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
            let port = n.port.unwrap_or(n.scheme.default_port());
            rows.push(Row {
                rule: n.rule.to_string(),
                kind: Kind::Net,
                root: format!("{}://{}:{port}", n.scheme.as_str(), n.host),
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some(
                    "press `a` and type `net <host>` or a url to add another host; a host that is \
                     already granted is removed in the policy file"
                        .into(),
                ),
            });
        }
        if let Some(b) = &grants.model_invoke {
            rows.push(Row {
                rule: b.rule.to_string(),
                kind: Kind::ModelInvoke,
                root: match b.tokens_per_session {
                    Some(tokens) => format!("{tokens} tokens per session"),
                    None => "no token budget".into(),
                },
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some("a token budget is granted in the policy file".into()),
            });
        }
        for (scope, g) in &grants.plain {
            let root = if *scope == Scope::TranscriptRead {
                format!("{scope} · {}", g.sessions)
            } else {
                scope.to_string()
            };
            rows.push(Row {
                rule: g.rule.to_string(),
                kind: Kind::Plain,
                root,
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some("granted in the policy file".into()),
            });
        }
        Ok(rows)
    }

    fn permissions_snapshot(&self, setup: &Setup) -> Result<(Vec<Row>, String)> {
        let text = fs::read_to_string(&setup.workspace_policy_path)
            .with_context(|| format!("reading {}", setup.workspace_policy_path.display()))?;
        Ok((self.permission_rows(setup)?, text))
    }

    fn open_permissions(&self, app: &mut App) -> Result<()> {
        let setup = Setup::load(
            &app.header.workspace,
            &env::temp_dir(),
            &Overrides::default(),
        )?;
        let (rows, text) = self.permissions_snapshot(&setup)?;
        app.permissions = Some(Permissions::new(
            rows,
            permissions::Source {
                policy_path: render::home_relative(&setup.workspace_policy_path),
                global_path: render::home_relative(&self.paths.user_config()),
                text,
                base: setup.global_text.clone(),
            },
            session::roots_for_validation()?,
        ));
        Ok(())
    }

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
        if config.price.is_none()
            && let Some(price) = reported.price(&config.base_url, &config.model)
        {
            app.header.priced = true;
            self.send(app, Command::SetPrice(price));
        }
        Ok(())
    }

    fn build(&self, name: &str) -> Result<Provider> {
        self.build_model(name, None)
    }

    fn build_model(&self, name: &str, model: Option<&str>) -> Result<Provider> {
        session::provider_for(&self.paths, name, model)
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
        if let Some(url) = &req.base_url {
            config::edit(&self.paths.user_config(), |doc| {
                let table = config::provider_table(doc, &req.provider)?;
                table["api"] = toml_edit::value("openai");
                table["base-url"] = toml_edit::value(url.as_str());
                Ok(())
            })?;
        }
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
            return self.open_models(app);
        };
        if app.header.provider.is_empty() {
            return self.open_models(app);
        }
        self.use_model(
            app,
            Pick {
                provider: app.header.provider.clone(),
                model,
            },
        )
    }

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

    fn switch_to(&self, app: &mut App, pick: &Pick) -> Result<(String, String)> {
        let provider = self.build_model(&pick.provider, Some(&pick.model))?;
        let model = provider.config.model.clone();
        self.switch(app, provider);
        Ok((pick.provider.clone(), model))
    }
}

#[derive(Clone, Default)]
struct FrameBuffer(Rc<RefCell<Vec<u8>>>);

impl FrameBuffer {
    fn take(&self) -> Vec<u8> {
        mem::take(&mut *self.0.borrow_mut())
    }
}

impl io::Write for FrameBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

type ScreenTerminal = Terminal<CrosstermBackend<FrameBuffer>>;

struct Screen {
    terminal: ScreenTerminal,
    frame: FrameBuffer,
}

fn edit_prompt(terminal: &mut ScreenTerminal, app: &mut App, paths: &Paths) -> Result<()> {
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
    let edited = edit_file(terminal, &path, app);
    let text = fs::read_to_string(&path);
    let _ = fs::remove_file(&path);
    edited?;
    if let Ok(text) = text {
        app.editor.set(text.trim_end_matches('\n').to_owned());
    }
    Ok(())
}

fn edit_file(terminal: &mut ScreenTerminal, path: &Path, app: &mut App) -> Result<()> {
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
    enter_modes(app.header.mouse)?;
    terminal.clear()?;
    app.drawn.stale = true;
    status.with_context(|| format!("running {program}"))?;
    Ok(())
}

fn announce(app: &App, focus: Focus, outcome: &str) {
    let tmux = env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let title = match app.header.workspace.file_name() {
        Some(name) => format!("saddle {}", name.to_string_lossy()),
        None => "saddle".to_owned(),
    };
    if let Some(bytes) = notify::sequence(app.header.notify, focus, &title, outcome, tmux) {
        let mut stdout = stdout();
        let _ = stdout.write_all(bytes.as_bytes());
        let _ = stdout.flush();
    }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let tmux = env::var_os("TMUX").is_some_and(|v| !v.is_empty());
    let mut stdout = stdout();
    stdout.write_all(clipboard_sequence(text, tmux).as_bytes())?;
    stdout.flush()?;
    Ok(())
}

fn clipboard_sequence(text: &str, tmux: bool) -> String {
    let payload = general_purpose::STANDARD.encode(text);
    notify::passthrough(&format!("\x1b]52;c;{payload}\x07"), tmux)
}

fn enter_modes(mouse: bool) -> Result<()> {
    let mut out = stdout();
    execute!(out, EnableBracketedPaste, EnableFocusChange)?;
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
    let _ = execute!(
        out,
        DisableBracketedPaste,
        DisableMouseCapture,
        DisableFocusChange
    );
}

impl Screen {
    fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode().context("starting the terminal UI")?;
        execute!(stdout(), EnterAlternateScreen).context("starting the terminal UI")?;
        let frame = FrameBuffer::default();
        let terminal = Terminal::new(CrosstermBackend::new(frame.clone()))
            .context("starting the terminal UI")?;
        terminal::discard_pending();
        enter_modes(mouse)?;
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            leave_modes();
            ratatui::restore();
            previous(info);
        }));
        Ok(Self { terminal, frame })
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
    use crate::tui::notify::Notify;
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
    use crate::tui::keys::Keys;

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
            steering: Steer::default(),
            team: Steering::default(),
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
            notify: Notify::Off,
            math_scale: 1.0,
            keys: Keys::default(),
        })
    }

    #[test]
    fn a_sign_in_runs_off_the_interface_thread_and_answers_on_the_events_channel() {
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
            steering: Steer::default(),
            team: Steering::default(),
        };
        let mut app = an_app();
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
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    }

    #[test]
    fn slash_model_with_no_name_opens_the_list_for_the_provider_in_force() {
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
        c.perform(&mut app, Action::Model(None));
        let login = app.login.as_ref().expect("the picker is open");
        assert_eq!(login.purpose(), login::Purpose::Model);
        assert_eq!(login.choice().name, "synthetic");
        assert!(
            matches!(login.stage, login::Stage::Checking { .. }),
            "the list is being asked for"
        );
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
        assert_eq!(app.header.model, "chosen-at-login");
        assert!(
            app.entries
                .iter()
                .any(|e| matches!(e, Entry::Note(NoteKind::Info, t)
                if t.contains("chosen-at-login") && t.contains("ctrl+s"))),
            "{:?}",
            app.entries
        );
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
        let dir = tempfile::tempdir().unwrap();
        let c = controller(dir.path(), "provider = \"synthetic\"\n");
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
            line: line + 1,
            column: 0,
            id,
            box_: Fit {
                cols: 10,
                rows: 4,
                scaled: false,
            },
            pixels: (100, 80),
            data: "BwcHBwcHBwc=".to_owned(),
        }
    }

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn a_scroll_moves_the_picture_rather_than_sending_it_again() {
        let first = Drawn::default();
        let images = [placed(10, 7)];
        let plan = plan_images(image::Protocol::Kitty, &first, &images, area(), 0, &[]);
        assert!(plan.free.is_empty(), "nothing was on screen yet");
        assert_eq!(plan.transmit.len(), 1, "the file goes once");
        assert_eq!(plan.transmit[0].0, 7);
        assert!(
            plan.transmit[0].1.contains("a=t,"),
            "{}",
            plan.transmit[0].1
        );
        assert_eq!(plan.place.len(), 1);
        assert_eq!(plan.place[0].0, 11, "on the row after its marker");
        assert!(
            plan.place[0].2.contains("a=p,i=7,p=1"),
            "{}",
            plan.place[0].1
        );

        let plan = plan_images(image::Protocol::Kitty, &plan.drawn, &images, area(), 2, &[]);
        assert!(plan.free.is_empty(), "the terminal keeps the data");
        assert!(plan.transmit.is_empty(), "nothing to send a second time");
        assert_eq!(plan.place.len(), 1);
        assert_eq!(plan.place[0].0, 9);
        assert_eq!(plan.drawn.shown, [(9, 0, 7, 0, 4)]);
    }

    #[test]
    fn a_frame_where_nothing_moved_sends_nothing() {
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        let plan = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        assert!(plan.free.is_empty() && plan.transmit.is_empty() && plan.place.is_empty());
    }

    #[test]
    fn a_picture_below_the_fold_is_drawn_in_part_not_dropped() {
        let images = [placed(20, 7)];
        let plan = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        assert_eq!(plan.place.len(), 1, "the visible part is drawn: {plan:?}");
        assert_eq!(plan.drawn.shown, [(21, 0, 7, 0, 3)]);
        let plan = plan_images(image::Protocol::Kitty, &plan.drawn, &images, area(), 2, &[]);
        assert_eq!(
            plan.drawn.shown,
            [(19, 0, 7, 0, 4)],
            "still whole, just higher"
        );
        let plan = plan_images(
            image::Protocol::Kitty,
            &plan.drawn,
            &images,
            area(),
            22,
            &[],
        );
        assert_eq!(
            plan.drawn.shown,
            [(0, 0, 7, 1, 3)],
            "the last three rows only"
        );
    }

    #[test]
    fn a_picture_whose_marker_scrolled_off_is_drawn_from_where_it_is_visible() {
        let images = [placed(8, 7)];
        let plan = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            10,
            &[],
        );
        assert!(plan.free.is_empty(), "nothing was on screen before");
        assert_eq!(plan.place.len(), 1, "the visible part is still drawn");
        assert_eq!(
            plan.drawn.shown,
            [(0, 0, 7, 1, 3)],
            "cropped, at the top of the area"
        );
    }

    #[test]
    fn a_resize_sends_the_pictures_again_because_the_screen_was_cleared() {
        let images = [placed(10, 7)];
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        let taller = Rect::new(0, 0, 80, 30);
        let plan = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &images,
            taller,
            0,
            &[],
        );
        assert_eq!(plan.transmit.len(), 1, "sent again after the clear");
        assert_eq!(plan.place.len(), 1);
        let again = plan_images(image::Protocol::Kitty, &plan.drawn, &images, taller, 0, &[]);
        assert!(again.transmit.is_empty() && again.place.is_empty());
    }

    #[test]
    fn a_picture_a_menu_covers_is_cut_short_rather_than_drawn_over_it() {
        let images = [placed(10, 7)];
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        assert_eq!(first.drawn.shown, [(11, 0, 7, 0, 4)]);
        let menu = Rect::new(0, 12, 80, 8);
        let cropped = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &images,
            area(),
            0,
            &[menu],
        );
        assert_eq!(cropped.drawn.shown, [(11, 0, 7, 0, 1)], "the row above it");
        assert!(cropped.hide.is_empty() && cropped.transmit.is_empty() && cropped.free.is_empty());
        assert_eq!(cropped.place.len(), 1, "cropped, so placed again");
        let back = plan_images(
            image::Protocol::Kitty,
            &cropped.drawn,
            &images,
            area(),
            0,
            &[],
        );
        assert!(back.transmit.is_empty() && back.hide.is_empty() && back.free.is_empty());
        assert_eq!(back.drawn.shown, [(11, 0, 7, 0, 4)]);
    }

    #[test]
    fn a_dialog_takes_a_picture_off_the_screen_and_the_terminal_keeps_it() {
        let images = [placed(10, 7)];
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        let plan = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &images,
            area(),
            0,
            &[area()],
        );
        assert_eq!(plan.hide, [7], "off the cells the dialog covers");
        assert!(plan.free.is_empty(), "the terminal keeps the data");
        assert!(plan.transmit.is_empty() && plan.place.is_empty());
        assert!(plan.drawn.shown.is_empty());
        assert_eq!(plan.drawn.held, [7]);
        let escapes = image_escapes(&plan, area());
        assert!(
            escapes.contains("a=d,d=i,i=7"),
            "delete the placement, not the data: {escapes:?}"
        );
        let back = plan_images(image::Protocol::Kitty, &plan.drawn, &images, area(), 0, &[]);
        assert!(back.transmit.is_empty() && back.hide.is_empty() && back.free.is_empty());
        assert_eq!(back.place.len(), 1, "placed where it was");
    }

    #[test]
    fn a_picture_the_menu_does_not_reach_stays_where_it_is() {
        let images = [placed(2, 7)];
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        let menu = Rect::new(0, 14, 80, 8);
        let plan = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &images,
            area(),
            0,
            &[menu],
        );
        assert!(plan.hide.is_empty() && plan.place.is_empty() && plan.transmit.is_empty());
        assert_eq!(plan.drawn.shown, first.drawn.shown);
    }

    #[test]
    fn a_picture_scrolled_out_of_view_is_removed_and_its_data_forgotten() {
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        let plan = plan_images(image::Protocol::Kitty, &first.drawn, &[], area(), 40, &[]);
        assert_eq!(plan.free, [7]);
        assert!(plan.transmit.is_empty() && plan.place.is_empty());
        assert!(plan.drawn.shown.is_empty());
        let plan = plan_images(
            image::Protocol::Kitty,
            &plan.drawn,
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        assert_eq!(plan.transmit.len(), 1);
        assert_eq!(plan.place.len(), 1);
    }

    #[test]
    fn a_resize_redraws_when_it_moves_the_area_or_changes_what_fits() {
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        let wider = Rect::new(0, 0, 120, 24);
        let plan = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &[placed(10, 7)],
            wider,
            0,
            &[],
        );
        assert_eq!(plan.transmit.len(), 1, "the clear took the data with it");
        assert_eq!(plan.place.len(), 1);
        let shorter = Rect::new(0, 0, 80, 12);
        let plan = plan_images(
            image::Protocol::Kitty,
            &plan.drawn,
            &[placed(10, 7)],
            shorter,
            0,
            &[],
        );
        assert_eq!(plan.transmit.len(), 1, "another clear, another send");
        assert_eq!(plan.place.len(), 1);
        assert_eq!(plan.drawn.shown, [(11, 0, 7, 0, 1)], "one row of four");
        let moved = Rect::new(0, 2, 80, 12);
        let plan = plan_images(
            image::Protocol::Kitty,
            &plan.drawn,
            &[placed(10, 7)],
            moved,
            0,
            &[],
        );
        assert!(plan.free.is_empty() && plan.transmit.is_empty());
        assert_eq!(plan.place.len(), 1);
    }

    #[test]
    fn two_pictures_each_keep_their_own_id() {
        let images = [placed(2, 1), placed(10, 2)];
        let plan = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        assert_eq!(plan.transmit.len(), 2);
        assert_eq!(plan.place.len(), 2);
        assert_eq!(plan.drawn.shown, [(3, 0, 1, 0, 4), (11, 0, 2, 0, 4)]);
        let plan = plan_images(image::Protocol::Kitty, &plan.drawn, &[], area(), 0, &[]);
        assert_eq!(plan.free, [1, 2], "both are taken off, not just the first");
    }

    #[test]
    fn a_picture_that_only_moves_is_not_taken_off_first() {
        let images = [placed(10, 7), placed(20, 9)];
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        let plan = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &images,
            area(),
            1,
            &[],
        );
        assert!(plan.free.is_empty(), "nothing leaves the screen");
        assert!(plan.transmit.is_empty(), "the files are already there");
        assert_eq!(plan.place.len(), 2, "both are placed at their new rows");
    }

    #[test]
    fn a_replaced_transcript_takes_its_pictures_off_and_sends_the_new_ones() {
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        let mut drawn = first.drawn;
        drawn.stale = true;
        let plan = plan_images(
            image::Protocol::Kitty,
            &drawn,
            &[placed(10, 7)],
            area(),
            0,
            &[],
        );
        assert_eq!(plan.free, [7], "the old picture goes");
        assert_eq!(plan.transmit.len(), 1, "and the new one is sent");
        assert_eq!(plan.place.len(), 1);
        assert!(!plan.drawn.stale, "the next frame trusts itself again");
    }

    #[test]
    fn the_escapes_forget_a_picture_that_left_and_move_one_that_stayed() {
        let area = Rect::new(0, 2, 80, 24);
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &[placed(10, 7), placed(20, 8)],
            area,
            0,
            &[],
        );
        let escapes = image_escapes(&first, area);
        assert!(escapes.contains("a=t,f=100,q=2,i=7,"), "sent once");
        assert!(
            escapes.contains("\x1b[14;1H"),
            "the area starts at row 2 and the marker is its line 10: {escapes:?}"
        );
        assert!(escapes.contains("a=p,i=7,p=1"), "placed, not transmitted");
        assert!(
            !escapes.contains("a=d"),
            "nothing to delete on the first frame"
        );
        assert!(
            escapes.ends_with("\x1b8"),
            "the cursor the frame placed is put back after the last placement: {escapes:?}"
        );

        let second = plan_images(
            image::Protocol::Kitty,
            &first.drawn,
            &[placed(20, 8)],
            area,
            0,
            &[],
        );
        let escapes = image_escapes(&second, area);
        assert_eq!(escapes.matches("a=d,d=I,i=").count(), 1, "{escapes:?}");
        assert!(
            escapes.contains("d=I,i=7,"),
            "the one that left: {escapes:?}"
        );
        assert!(
            !escapes.contains("a=p"),
            "the one that stayed is not redrawn"
        );
        assert!(!escapes.contains("a=t"), "and not sent again");
    }

    #[test]
    fn a_picture_inside_a_line_is_placed_at_its_column() {
        let mut picture = placed(10, 7);
        picture.column = 4;
        let images = [picture];
        let plan = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        assert_eq!(plan.place.len(), 1, "placed once");
        let escapes = image_escapes(&plan, area());
        assert!(
            escapes.contains("\x1b[12;5H"),
            "the fifth column of the twelfth row: {escapes:?}"
        );
        let mut moved = images.clone();
        moved[0].column = 5;
        let again = plan_images(image::Protocol::Kitty, &plan.drawn, &moved, area(), 0, &[]);
        assert_eq!(again.place.len(), 1, "the column changed: {again:?}");
        assert!(again.transmit.is_empty(), "the terminal still holds it");
    }

    #[test]
    fn a_move_is_a_placement_at_the_new_row_with_nothing_erased() {
        let area = Rect::new(0, 0, 80, 24);
        let images = [placed(10, 7)];
        let first = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area,
            0,
            &[],
        );
        let moved = plan_images(image::Protocol::Kitty, &first.drawn, &images, area, 3, &[]);
        let escapes = image_escapes(&moved, area);
        assert!(
            !escapes.contains("a=d") && !escapes.contains("a=t"),
            "{escapes:?}"
        );
        assert!(
            escapes.starts_with("\x1b7\x1b[9;1H\x1b_Ga=p,i=7,p=1"),
            "{escapes:?}"
        );
        assert!(escapes.ends_with("\x1b8"), "{escapes:?}");
    }

    #[test]
    fn placing_a_picture_leaves_the_cursor_where_the_frame_put_it() {
        let area = Rect::new(0, 0, 80, 24);
        let images = [placed(10, 7)];
        let plan = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area,
            0,
            &[],
        );
        let escapes = image_escapes(&plan, area);
        assert!(escapes.starts_with("\x1b7"), "{escapes:?}");
        assert!(escapes.ends_with("\x1b8"), "{escapes:?}");
        let quiet = plan_images(image::Protocol::Kitty, &Drawn::default(), &[], area, 0, &[]);
        assert_eq!(image_escapes(&quiet, area), "");
    }

    #[test]
    fn an_iterm_picture_is_sent_every_time_because_nothing_is_stored() {
        let images = [placed(10, 7)];
        let first = plan_images(
            image::Protocol::Iterm2,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        assert!(first.transmit.is_empty());
        assert_eq!(first.place.len(), 1);
        let plan = plan_images(
            image::Protocol::Iterm2,
            &first.drawn,
            &images,
            area(),
            2,
            &[],
        );
        assert!(plan.free.is_empty() && plan.transmit.is_empty());
        assert_eq!(plan.place.len(), 1, "drawn again where it now is");
        let plan = plan_images(image::Protocol::Iterm2, &plan.drawn, &[], area(), 40, &[]);
        assert!(plan.free.is_empty(), "there is nothing to delete");
    }

    #[test]
    fn the_image_log_says_what_the_terminal_was_told() {
        let images = [placed(10, 7)];
        let plan = plan_images(
            image::Protocol::Kitty,
            &Drawn::default(),
            &images,
            area(),
            0,
            &[],
        );
        let line = trace_line(&plan, area(), 0);
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(
            line.starts_with("frame x=0 y=0 80x24 top=0 covered=0"),
            "{line}"
        );
        assert!(line.contains(" transmit 7"), "{line}");
        assert!(line.contains(" place11.0 ESC_Ga=p,i=7,p=1"), "{line}");
        assert!(line.contains(" shown 7@11.0+0/4"), "{line}");
        assert!(!line.contains('\x1b'), "no raw control characters: {line}");
        assert!(
            !line.contains('\u{1b}'),
            "no raw control characters: {line}"
        );
        let empty = plan_images(image::Protocol::Kitty, &plan.drawn, &[], area(), 0, &[]);
        let line = trace_line(&empty, area(), 0);
        assert!(line.contains(" free 7"), "{line}");
        assert!(!line.contains(" place"), "{line}");
    }

    #[test]
    fn the_clipboard_sequence_is_osc_52_with_base64() {
        assert_eq!(clipboard_sequence("hi", false), "\x1b]52;c;aGk=\x07");
        let wrapped = clipboard_sequence("hi", true);
        assert_eq!(wrapped, "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\");
        assert_eq!(clipboard_sequence("", false), "\x1b]52;c;\x07");
    }
}
