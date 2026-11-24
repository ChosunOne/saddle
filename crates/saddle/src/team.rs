//! live teams of ordinary agent sessions, requested by the shipped team component and owned by the
//! session that started them (D195).
//!
//! A worker is not a second implementation of a session: it is [`Agent`] and [`Session`] on a
//! thread, with the same policy, roots and audit rules. It inherits only the caller's delegated
//! reach by deriving its policy from the same setup, and its model calls are its own principal's,
//! so one worker cannot spend or reach for another. Every worker's full transcript is retained as
//! an attributed branch item on its own conversation, while the model that started it gets one
//! bounded result.

use crate::agent::{self, Agent, End, Steer, Ui};
use crate::cancel;
use crate::compact;
use crate::config::CompactionConfig;
use crate::provider::{Contribution, Image, Quota, Reply, Sink, ToolCall};
use crate::session::{Session, session_id};
use crate::store::Store;
use crate::tools::ToolOutput;
use crate::{provider, tools};
use anyhow::{Context, Result};
use saddle_cap::Principal;
use saddle_limits as budgets;
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
use saddle_wit::saddle::host::workers::Operation;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How much of a worker's transcript is kept for a person to read on screen.
/// How many beats a worker's view keeps. Early ones fall off rather than growing without bound;
/// the conversation itself is in the worker's own saved transcript.
const RENDER_BEATS: usize = 2000;

/// A live handle the interface can use without the agent thread: steering a worker, or asking it to
/// stop, never needs the session that owns it.
#[derive(Clone)]
pub struct Handle {
    steer: Steer,
    /// Ends the turn in flight; the worker stays live and takes the next message.
    interrupt: Arc<AtomicBool>,
    /// Ends the worker for good after the turn in flight.
    stop: Arc<AtomicBool>,
    /// Reaches a command the turn is waiting on, so an interrupt does not wait for it (D195).
    cancel: Arc<AtomicBool>,
}

/// The handles the interface keeps, so a message or an interrupt reaches a worker while the agent
/// that started it is busy (D195).
#[derive(Clone, Default)]
pub struct Steering(Arc<Mutex<BTreeMap<String, Peer>>>);

impl Steering {
    pub fn say(&self, name: &str, text: String) {
        if let Ok(map) = self.0.lock()
            && let Some(peer) = map.get(name)
        {
            peer.handle.steer.push(text);
        }
    }

    pub fn interrupt(&self, name: &str) {
        if let Ok(map) = self.0.lock()
            && let Some(peer) = map.get(name)
        {
            peer.handle.interrupt.store(true, Ordering::SeqCst);
            peer.handle.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Ends a worker for good, as the list's `x` does. The interface holds the lead's map, so it
    /// is the conversation that owns the team doing it (D195).
    pub fn stop(&self, name: &str) {
        if let Ok(map) = self.0.lock()
            && let Some(peer) = map.get(name)
        {
            peer.handle.stop.store(true, Ordering::SeqCst);
            peer.handle.interrupt.store(true, Ordering::SeqCst);
            peer.handle.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// What the interface draws, read straight from the shared handles so a worker streams while
    /// the lead's thread is busy (D195).
    pub fn views(&self) -> Vec<Render> {
        match self.0.lock() {
            Ok(map) => map.values().map(Peer::render).collect(),
            Err(_) => Vec::new(),
        }
    }
}

/// One piece of a worker's conversation, kept structured rather than as finished lines: the
/// interface draws it with the same renderer as the lead's transcript — markdown, code, tool
/// blocks and collapsed output — instead of a plain tail (D195).
#[derive(Debug, Clone, PartialEq)]
pub enum Beat {
    /// A prompt the worker was given: its brief, or a message from a person or a peer.
    Prompt(String),
    Text(String),
    Thinking(String),
    Call {
        name: String,
        detail: String,
    },
    Result {
        ok: bool,
        content: String,
        diff: Option<String>,
        /// Pictures the call returned, drawn where the terminal can (D58).
        images: Vec<Image>,
    },
    Note(String),
    /// What one reply used, as the status line of the lead's transcript states it.
    Spend {
        input: u64,
        output: u64,
        cost: f64,
    },
}

/// What a person sees of one worker: its live transcript and what it is doing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Render {
    pub name: String,
    pub brief: String,
    pub state: String,
    pub turns: usize,
    pub beats: Vec<Beat>,
    pub error: Option<String>,
    pub finished: bool,
}

impl Render {
    /// Adds one piece, joining the streamed text and reasoning onto the block already at the end
    /// so a token does not become a beat of its own.
    fn push(&mut self, beat: Beat) {
        match (&mut self.beats.last_mut(), &beat) {
            (Some(Beat::Text(held)), Beat::Text(more)) => held.push_str(more),
            (Some(Beat::Thinking(held)), Beat::Thinking(more)) => held.push_str(more),
            _ => self.beats.push(beat),
        }
        let over = self.beats.len().saturating_sub(RENDER_BEATS);
        if over > 0 {
            self.beats.drain(..over);
        }
    }

    /// The conversation as plain lines, for `agent status`: the whole transcript is in the
    /// interface, and a line-mode reader needs the words rather than the rendering.
    fn plain(&self) -> Vec<String> {
        let mut out = Vec::new();
        for beat in &self.beats {
            match beat {
                Beat::Prompt(text) => {
                    out.push(String::new());
                    out.push(format!("▸ {text}"));
                }
                Beat::Text(text) => out.extend(text.lines().map(str::to_owned)),
                Beat::Thinking(text) => {
                    out.push("thinking:".into());
                    out.extend(text.lines().map(str::to_owned));
                }
                Beat::Call { name, detail } => out.push(format!("⚙ {name} {detail}")),
                Beat::Result { ok, content, .. } => {
                    let mark = if *ok { "✓" } else { "✗" };
                    out.push(format!("{mark} {}", first_line(content)));
                }
                Beat::Note(text) => out.push(format!("· {text}")),
                Beat::Spend { input, output, .. } => {
                    out.push(format!("· {input} in · {output} out"))
                }
            }
        }
        out
    }
}

/// What runs on a worker's thread. The brief is the conversation's first prompt; a message arrives
/// as an ordinary turn with whose words it is stated first.
enum Job {
    Start(String),
    /// A message is waiting on the worker's steering handle; look for it (D195).
    Wake,
}

/// One live worker as the team shares it: a worker's own session sees its peers through the same
/// map, which is what lets two workers message each other without a lead relay (D195).
#[derive(Clone)]
struct Peer {
    conversation: String,
    handle: Handle,
    render: Arc<Mutex<Render>>,
    /// Kept so the worker's thread stays alive between turns: dropping it would end the worker
    /// after its first turn, before a peer or the interface could address it (D195).
    jobs: Sender<Job>,
}

impl Peer {
    fn render(&self) -> Render {
        self.render.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

/// The live team of one session. Held on the session's thread, so starting a worker is a session
/// effect and never a guest operation on its own. A worker's session holds a view of the same map
/// with `owns` false: it can address peers but not start or stop the team.
pub struct Manager {
    shared: Arc<Mutex<BTreeMap<String, Peer>>>,
    owns: bool,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            shared: Default::default(),
            owns: true,
        }
    }
}

impl Manager {
    /// The view of the team a worker's own session gets (D195).
    pub(crate) fn child(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            owns: false,
        }
    }
    /// A handle map the interface can use without the agent thread. It reads the peers' handles
    /// each time, so a worker started after the interface was built is still reachable.
    pub fn steering(&self) -> Steering {
        Steering(self.shared.clone())
    }

    pub fn views(&self) -> Vec<Render> {
        match self.shared.lock() {
            Ok(map) => map.values().map(Peer::render).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The saved conversation a worker writes, for the interface to open it (D195).
    pub fn conversation(&self, name: &str) -> Option<String> {
        self.lookup(name).map(|peer| peer.conversation)
    }

    fn lookup(&self, name: &str) -> Option<Peer> {
        self.shared.lock().ok()?.get(name).cloned()
    }

    /// Stops everything and says how many were stopped; used before a policy replacement and at
    /// session close, when the grants a worker was built on no longer exist (D47, D153).
    pub fn stop_all(&mut self) -> usize {
        if !self.owns {
            return 0;
        }
        let Ok(map) = self.shared.lock() else {
            return 0;
        };
        let count = map.len();
        for peer in map.values() {
            peer.handle.stop.store(true, Ordering::SeqCst);
            peer.handle.interrupt.store(true, Ordering::SeqCst);
            peer.handle.cancel.store(true, Ordering::SeqCst);
            cancel::request();
        }
        count
    }

    pub fn interrupt(&self, name: &str) -> Result<String, String> {
        let peer = self
            .lookup(name)
            .ok_or_else(|| format!("no worker {name:?} in this team"))?;
        peer.handle.interrupt.store(true, Ordering::SeqCst);
        peer.handle.cancel.store(true, Ordering::SeqCst);
        Ok(format!("interrupting {name}"))
    }

    pub fn stop(&self, name: &str) -> Result<String, String> {
        if !self.owns {
            return Err("only the conversation that started the team stops a worker".into());
        }
        let peer = self
            .lookup(name)
            .ok_or_else(|| format!("no worker {name:?} in this team"))?;
        peer.handle.stop.store(true, Ordering::SeqCst);
        peer.handle.interrupt.store(true, Ordering::SeqCst);
        peer.handle.cancel.store(true, Ordering::SeqCst);
        Ok(format!(
            "stopping {name}; its conversation and findings are kept"
        ))
    }

    pub fn say(&self, from: &str, to: &str, text: &str) -> Result<String, String> {
        let peer = self.lookup(to).ok_or_else(|| {
            format!("no worker {to:?}; workers exchange messages directly, without a lead relay")
        })?;
        if text.trim().is_empty() {
            return Err("empty message".into());
        }
        if peer.render().finished {
            return Err(format!("{to} has finished; start a new worker"));
        }
        // A running worker takes it at its next request boundary; an idle one is woken by this,
        // and the handle is shared with its thread so a peer or the interface can do it while the
        // agent that started the team is busy (D195).
        peer.jobs
            .send(Job::Wake)
            .map_err(|_| format!("{to} has finished; start a new worker"))?;
        peer.handle
            .steer
            .push(format!("[message from {from}]\n{text}"));
        Ok(format!("delivered to {to}"))
    }

    pub fn inspect(&self, name: &str) -> Result<String, String> {
        let peer = self
            .lookup(name)
            .ok_or_else(|| format!("no worker {name:?} in this team"))?;
        let render = peer.render();
        let mut out = format!(
            "{} · {}\nbrief: {}\nturns: {}\n",
            render.name, render.state, render.brief, render.turns
        );
        if let Some(error) = &render.error {
            out.push_str(&format!("error: {error}\n"));
        }
        let lines = render.plain();
        let recent: Vec<&String> = lines.iter().rev().take(12).collect();
        for line in recent.into_iter().rev() {
            out.push_str(line);
            out.push('\n');
        }
        Ok(out)
    }

    pub fn roster(&self) -> String {
        let Ok(map) = self.shared.lock() else {
            return "the team is unavailable".into();
        };
        if map.is_empty() {
            return "no live workers in this session".into();
        }
        let mut out = String::new();
        for (name, peer) in map.iter() {
            let render = peer.render();
            out.push_str(&format!(
                "{name} · {} · {} turns\n",
                render.state, render.turns
            ));
        }
        out
    }

    /// Starts a worker. The brief is written inside the session's temporary root and read back
    /// through the read gate, so the exact text the worker was given is a branch item attributable
    /// to a real file rather than an unrecorded argument (D195).
    pub fn start(
        &mut self,
        session: &mut Session,
        who: &Principal,
        origin_principal: &str,
        name: &str,
        brief: &str,
    ) -> Result<Result<String, String>> {
        if origin_principal != "plugin.standard-agent"
            && origin_principal != "plugin.standard-swarm"
        {
            return Ok(Err(format!(
                "workers are requested by the shipped agent and swarm tools, not {origin_principal}"
            )));
        }
        match valid_name(name) {
            Ok(()) => {}
            Err(why) => return Ok(Err(why)),
        }
        if brief.trim().is_empty() {
            return Ok(Err("a worker needs a brief".into()));
        }
        let team_workers = session.setup.tool_limits.team_workers;
        // A worker turn has no request limit unless `tool-limits.team-steps` sets one, as the
        // lead's turn has none unless `[agent] max-steps` does (D37).
        let team_steps = match session.setup.tool_limits.team_steps {
            0 => None,
            steps => Some(steps),
        };
        if self.views().len() >= team_workers {
            return Ok(Err(format!(
                "this session already has {} workers (tool-limits.team-workers)",
                self.views().len()
            )));
        }
        if self.lookup(name).is_some() {
            return Ok(Err(format!("a worker named {name:?} is already live")));
        }
        if session.engine.policy().grants(&Principal::Agent).is_none() {
            return Ok(Err("the agent has no grants to derive a worker from".into()));
        }
        let grant = Request::ModelInvoke { tokens: 0 };
        let decision = session.engine.decide(&Principal::Agent, &grant);
        session
            .audit
            .decision(&Principal::Agent, &grant, &decision)?;
        if !decision.allow {
            return Ok(Err(format!(
                "a worker is a model conversation of its own, which needs model-invoke: {}",
                decision.explanation
            )));
        }
        // The host writes and reads the brief itself, in this session's private 0700 temporary
        // directory: no root is involved, so no grant is needed to read what the harness chose,
        // and the exact text is still recorded by hash rather than by argument (D195).
        let dir = session.tmp_dir().join("team").join(name);
        if let Err(error) = fs::create_dir_all(&dir) {
            return Ok(Err(format!(
                "could not make the worker's directory: {error}"
            )));
        }
        let brief_path = dir.join("brief.md");
        if let Err(error) = fs::write(&brief_path, brief) {
            return Ok(Err(format!("could not record the worker's brief: {error}")));
        }
        let text = match fs::read(&brief_path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) => return Ok(Err(format!("could not read the worker's brief: {error}"))),
        };
        let brief_label = brief_path.display().to_string();
        session.audit.event(
            Kind::Decision,
            Some(who),
            Some(saddle_cap::Scope::Tool),
            Some(true),
            json!({
                "operation": "worker-start",
                "name": name,
                "brief": brief_label,
                "brief_sha256": sha256_hex(text.as_bytes()),
                "brief_bytes": text.len(),
                "workers": self.views().len() + 1,
            }),
            "live worker started from a delegated brief",
        )?;
        let id = session_id();
        // `session.team` is this manager, temporarily taken out of the session, so the child's
        // view is taken from here rather than from the session it is derived from (D195).
        let mut child = session.derive(&id)?;
        child.team = self.child();
        // Peers address each other by name; the random id names the audit log, not a mailbox.
        child.team_label = name.to_owned();
        let store = Store::open(
            &child.setup.paths.session_db(),
            child.setup.workspace.clone(),
        )
        .context("opening the worker's conversation store")?;
        let provider_name = session
            .provider_twin()
            .map(|provider| provider.name.clone())
            .unwrap_or_else(|| "worker".into());
        let conversation = store.begin(&provider_name, "")?;
        store.name(&conversation, name)?;
        let Some(provider) = session.provider_twin() else {
            return Ok(Err(
                "no provider is active in this session to give a worker".into(),
            ));
        };
        if provider.missing_key().is_some() {
            return Ok(Err(
                "the provider needs a credential a worker cannot hold".into()
            ));
        }
        let system = agent::system_prompt(&child, &[]);
        let mut declared = tools::definitions();
        declared.extend(child.declared_tools());
        let settings = compact::Settings::from_config(&CompactionConfig::default());
        let mut agent = Agent::new(
            Some(provider),
            system.clone(),
            agent::base_prompt(&child, &[]),
            declared,
            team_steps,
            Some(store),
            settings,
        );
        agent.set_context(Vec::new());
        agent.set_conversation(&conversation);
        let steer = agent.steering();
        let handle = Handle {
            steer: steer.clone(),
            interrupt: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let render = Arc::new(Mutex::new(Render {
            name: name.to_owned(),
            brief: first_line(&text),
            state: "starting".into(),
            ..Render::default()
        }));
        let (jobs, receive) = mpsc::channel();
        let view = render.clone();
        let watch = handle.clone();
        if let Err(error) = thread::Builder::new()
            .name(format!("saddle-worker-{name}"))
            .spawn(move || run(agent, child, receive, view, watch))
        {
            return Ok(Err(format!("could not start {name}: {error}")));
        }
        if let Ok(mut map) = self.shared.lock() {
            map.insert(
                name.to_owned(),
                Peer {
                    conversation: conversation.clone(),
                    handle,
                    render,
                    jobs: jobs.clone(),
                },
            );
        }
        let _ = jobs.send(Job::Start(text));
        Ok(Ok(format!(
            "started {name} (conversation {conversation}) · inspect, interrupt or stop it from the team view (ctrl+b); peers reach it with agent send"
        )))
    }
}

/// The thread a worker lives on: one turn per job, and a session closed with its audit log.
fn run(
    mut agent: Agent,
    mut session: Session,
    jobs: Receiver<Job>,
    view: Arc<Mutex<Render>>,
    handle: Handle,
) {
    let Handle {
        steer,
        interrupt,
        stop,
        cancel,
    } = handle;
    session.cancel = cancel.clone();
    let mut ui = WorkerUi {
        view: view.clone(),
        interrupt: interrupt.clone(),
        stop: stop.clone(),
    };
    let mut first = true;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // A message from the interface or a peer arrives through the same handle a person's
        // steering does; with no turn running it starts one (D149, D195).
        let prompt = if first {
            first = false;
            match jobs.recv() {
                Ok(Job::Start(brief)) => brief,
                Ok(Job::Wake) => continue,
                Err(_) => break,
            }
        } else {
            let mut text = None;
            loop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                match jobs.try_recv() {
                    Ok(Job::Start(brief)) => {
                        text = Some(brief);
                        break;
                    }
                    Ok(Job::Wake) => continue,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        finish(agent, session, &view);
                        return;
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                }
                let pending = steer.take();
                if !pending.is_empty() {
                    text = Some(pending.join("\n\n"));
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
            match text {
                Some(text) => text,
                None => break,
            }
        };
        set(&view, |render| {
            render.state = "running".into();
            render.turns += 1;
            render.push(Beat::Prompt(prompt.clone()));
        });
        let end = agent.turn(&mut session, &prompt, &mut ui);
        match end {
            Ok(End::Cancelled) => set(&view, |r| r.state = "idle (interrupted)".into()),
            Ok(End::Answered) => set(&view, |r| r.state = "idle".into()),
            Ok(end) => set(&view, |r| {
                r.state = format!("stopped ({end:?})").to_lowercase()
            }),
            Err(error) => {
                let text = format!("{error:#}");
                set(&view, |r| {
                    r.state = "failed".into();
                    r.error = Some(text.clone());
                    r.push(Beat::Note(format!("error: {text}")));
                });
                break;
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // An interrupt ends the turn, not the worker: it takes its next message.
        interrupt.store(false, Ordering::SeqCst);
        cancel.store(false, Ordering::SeqCst);
    }
    finish(agent, session, &view);
}

fn finish(mut agent: Agent, session: Session, view: &Arc<Mutex<Render>>) {
    agent.release_provider();
    let closed = session.close();
    set(view, |render| {
        render.state = "finished".into();
        render.finished = true;
        match &closed {
            Ok(head) => render.push(Beat::Note(format!("audit closed at {head}"))),
            Err(error) => render.push(Beat::Note(format!(
                "closing the worker's audit failed: {error:#}"
            ))),
        }
    });
}

fn set(view: &Arc<Mutex<Render>>, change: impl FnOnce(&mut Render)) {
    if let Ok(mut render) = view.lock() {
        change(&mut render);
    }
}

fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > budgets::directory_name_bytes() {
        return Err("a worker name needs 1 to a directory-name's bytes".into());
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
    {
        return Err("a worker name is lowercase letters, digits, dash or underscore".into());
    }
    Ok(())
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut out: String = line.chars().take(160).collect();
    if line.chars().count() > 160 {
        out.push('…');
    }
    out
}

/// One service call from the shipped team component. Every operation addresses only workers this
/// live session started; none takes a path, command or executable.
pub fn manage(
    session: &mut Session,
    who: &Principal,
    op: Operation,
    origin: &provider::Provenance,
) -> Result<Result<String, String>> {
    let caller = origin.principal.clone();
    let record = arguments(&op);
    let result = match op {
        Operation::Start(message) => {
            let mut team = mem::take(&mut session.team);
            let result = team.start(session, who, &caller, &message.target, &message.text);
            session.team = team;
            result?
        }
        Operation::Send(message) => {
            let from = session.team_label.clone();
            session.team.say(&from, &message.target, &message.text)
        }
        Operation::Inspect(name) => session.team.inspect(&name),
        Operation::Interrupt(name) => session.team.interrupt(&name),
        Operation::Stop(name) => session.team.stop(&name),
        Operation::Roster => Ok(session.team.roster()),
    };
    let allowed = result.is_ok();
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(saddle_cap::Scope::Tool),
        Some(allowed),
        record,
        result
            .as_ref()
            .err()
            .map(String::as_str)
            .unwrap_or("live team operation"),
    )?;
    Ok(result)
}

/// What the audit records: who it addresses and how much it says, never the words themselves.
pub fn arguments(op: &Operation) -> serde_json::Value {
    match op {
        Operation::Start(message) | Operation::Send(message) => json!({
            "target": message.target,
            "text_bytes": message.text.len(),
            "text_sha256": sha256_hex(message.text.as_bytes()),
        }),
        Operation::Inspect(name) | Operation::Interrupt(name) | Operation::Stop(name) => {
            json!({ "name": name })
        }
        Operation::Roster => json!({}),
    }
}

/// A worker's interface: full streaming and reasoning into its own view, bounded by the same
/// limits as any other transcript.
struct WorkerUi {
    view: Arc<Mutex<Render>>,
    interrupt: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Sink for WorkerUi {
    fn text(&mut self, text: &str) {
        set(&self.view, |render| {
            render.push(Beat::Text(text.to_owned()))
        });
    }

    fn note(&mut self, text: &str) {
        set(&self.view, |render| {
            render.push(Beat::Note(text.to_owned()))
        });
    }

    fn reasoning(&mut self, text: &str) {
        set(&self.view, |render| {
            render.push(Beat::Thinking(text.to_owned()))
        });
    }

    fn quota(&mut self, _: &Quota) {}

    fn cancelled(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.interrupt.load(Ordering::SeqCst)
    }
}

impl Ui for WorkerUi {
    fn call(&mut self, call: &ToolCall) {
        let detail = agent::call_detail(call);
        set(&self.view, |render| {
            render.push(Beat::Call {
                name: call.name.clone(),
                detail,
            })
        });
    }

    fn result(&mut self, out: &ToolOutput) {
        let visible = out.visible_content().to_owned();
        set(&self.view, |render| {
            render.push(Beat::Result {
                ok: out.ok,
                content: visible.clone(),
                diff: out.diff.clone(),
                images: out.images.clone(),
            })
        });
    }

    fn reply(&mut self, reply: &Reply, cost: f64) {
        set(&self.view, |render| {
            render.push(Beat::Spend {
                input: reply.usage.input,
                output: reply.usage.output,
                cost,
            })
        });
    }

    fn compaction(&mut self, _: compact::Event) {}
    fn contribution(&mut self, _: &Contribution) {}
}

/// The provider twin a worker starts from. Kept as a method so the whole path from the active
/// provider to a worker is visible in one place.
impl Session {
    pub(crate) fn provider_twin(&self) -> Option<provider::Provider> {
        self.extensions
            .provider
            .as_ref()
            .map(provider::Provider::twin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saddle_wit::saddle::host::workers::Message;

    #[test]
    fn names_are_narrow_and_briefs_keep_their_first_line() {
        for name in ["a", "runtime", "tests-2", "a_b"] {
            assert!(valid_name(name).is_ok(), "{name}");
        }
        for name in ["", "Runtime", "a/b", "../x", "a.b"] {
            assert!(valid_name(name).is_err(), "{name}");
        }
        assert_eq!(
            first_line("\n  think from the test\nmore"),
            "think from the test"
        );
    }

    #[test]
    fn an_audit_record_names_the_target_and_never_the_words() {
        let op = Operation::Send(Message {
            target: "runtime".into(),
            text: "secret finding".into(),
        });
        let value = arguments(&op);
        assert_eq!(value["target"], "runtime");
        assert!(!value.to_string().contains("secret"));
    }
}
