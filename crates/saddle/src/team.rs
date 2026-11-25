//! live teams of ordinary agent sessions, requested by the shipped team component and owned by the
//! session that started them (D195), with the team's message bus and its serially claimed command
//! queue (D196).
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
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// How much of a worker's transcript is kept for a person to read on screen.
/// How many beats a worker's view keeps. Early ones fall off rather than growing without bound;
/// the conversation itself is in the worker's own saved transcript.
const RENDER_BEATS: usize = 2000;

/// One delivery on the team's live message bus, kept for the page's bus view (D195). The words are
/// held in memory only: the audit records who spoke to whom, never what was said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusMessage {
    pub from: String,
    pub to: String,
    pub text: String,
}

/// What a queued command is doing: waiting its turn, claimed by one member (so the queue runs one
/// command at a time), finished, or lost when a claim's lease ran out (D195).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueState {
    Pending,
    Claimed { by: String },
    Done { ok: bool },
    Lost,
}

/// One item of the team's shared command queue (D195).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueItem {
    pub id: u64,
    pub command: String,
    pub why: String,
    pub owner: String,
    pub state: QueueState,
}

impl Queue {
    /// The commands the queue is holding or has finished, oldest first (D195).
    fn view(&self) -> Vec<QueueItem> {
        self.items
            .iter()
            .map(|item| QueueItem {
                id: item.id,
                command: item.command.clone(),
                why: item.why.clone(),
                owner: item.owner.clone(),
                state: item.state.clone(),
            })
            .collect()
    }

    fn waiting(&self) -> usize {
        self.items
            .iter()
            .filter(|item| matches!(item.state, QueueState::Pending | QueueState::Claimed { .. }))
            .count()
    }

    /// Keeps the finished history bounded; waiting commands are the team's and are never dropped.
    fn trim(&mut self) {
        let done = self
            .items
            .iter()
            .filter(|item| matches!(item.state, QueueState::Done { .. } | QueueState::Lost))
            .count();
        let mut over = done.saturating_sub(QUEUE_DONE);
        if over == 0 {
            return;
        }
        self.items.retain(|item| {
            let finished = matches!(item.state, QueueState::Done { .. } | QueueState::Lost);
            if finished && over > 0 {
                over -= 1;
                false
            } else {
                true
            }
        });
    }
}

/// One queued command as the host holds it, with when it was claimed, so a dead member's claim
/// does not stop the queue for good.
struct Queued {
    id: u64,
    command: String,
    why: String,
    owner: String,
    state: QueueState,
    claimed_at: Option<Instant>,
}

#[derive(Default)]
struct Queue {
    items: Vec<Queued>,
    next: u64,
}

/// How many bus deliveries are kept for the page.
const BUS_MESSAGES: usize = 200;
/// How much of one message the bus keeps: the full finding in practice, bounded so a team cannot
/// grow the bus without limit.
const BUS_CHARS: usize = 8000;
/// How many commands may wait at once.
const QUEUE_ITEMS: usize = 64;
/// How many finished commands the queue keeps for its view.
const QUEUE_DONE: usize = 16;
/// How long a claim holds the queue before another member may take it: a worker that dies mid-build
/// must not stop the team for good.
const QUEUE_LEASE: Duration = Duration::from_secs(600);

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
pub struct Steering {
    shared: Arc<Mutex<BTreeMap<String, Peer>>>,
    bus: Arc<Mutex<VecDeque<BusMessage>>>,
    queue: Arc<Mutex<Queue>>,
}

impl Steering {
    pub fn say(&self, name: &str, text: String) {
        if let Ok(map) = self.shared.lock()
            && let Some(peer) = map.get(name)
        {
            peer.handle.steer.push(text);
            // An idle worker waits on its channel, so the message itself is what wakes it (D195).
            let _ = peer.jobs.send(Job::Wake);
        }
    }

    pub fn interrupt(&self, name: &str) {
        if let Ok(map) = self.shared.lock()
            && let Some(peer) = map.get(name)
            && peer.running()
        {
            peer.handle.interrupt.store(true, Ordering::SeqCst);
            peer.handle.cancel.store(true, Ordering::SeqCst);
        }
    }

    /// Ends a worker for good, as the list's `x` does. The interface holds the lead's map, so it
    /// is the conversation that owns the team doing it (D195).
    pub fn stop(&self, name: &str) {
        if let Ok(map) = self.shared.lock()
            && let Some(peer) = map.get(name)
            && peer.live()
        {
            peer.handle.stop.store(true, Ordering::SeqCst);
            peer.handle.interrupt.store(true, Ordering::SeqCst);
            peer.handle.cancel.store(true, Ordering::SeqCst);
            let _ = peer.jobs.send(Job::Wake);
        }
    }

    /// What the interface draws, read straight from the shared handles so a worker streams while
    /// the lead's thread is busy (D195).
    pub fn views(&self) -> Vec<Render> {
        match self.shared.lock() {
            Ok(map) => map.values().map(Peer::render).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The team's message bus, for the page's bus view (D195).
    pub fn bus(&self) -> Vec<BusMessage> {
        self.bus
            .lock()
            .map(|bus| bus.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The shared command queue, for the page's queue view (D195).
    pub fn queue(&self) -> Vec<QueueItem> {
        self.queue
            .lock()
            .map(|queue| queue.view())
            .unwrap_or_default()
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

/// How many peers still hold a thread. A finished worker is kept for inspection but not counted
/// against `tool-limits.team-workers` (D195).
fn live_count(map: &BTreeMap<String, Peer>) -> usize {
    map.values().filter(|peer| peer.live()).count()
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

    /// Whether this worker still has a thread to reach. A finished one is kept so its conversation
    /// can still be inspected, but it is not a live worker: it does not use a team slot and its
    /// name can be used again (D195).
    fn live(&self) -> bool {
        self.render.lock().map(|r| !r.finished).unwrap_or(false)
    }

    /// Whether a turn is in flight, which is the only thing an interrupt can end.
    fn running(&self) -> bool {
        self.render
            .lock()
            .map(|r| r.state == "running")
            .unwrap_or(false)
    }
}

/// The live team of one session. Held on the session's thread, so starting a worker is a session
/// effect and never a guest operation on its own. A worker's session holds a view of the same map
/// with `owns` false: it can address peers but not start or stop the team.
pub struct Manager {
    shared: Arc<Mutex<BTreeMap<String, Peer>>>,
    /// What the team has said to itself, newest last, for the page's bus view (D195).
    bus: Arc<Mutex<VecDeque<BusMessage>>>,
    /// Commands the team wants run one at a time, whoever gets to them (D195).
    queue: Arc<Mutex<Queue>>,
    /// What the workers have finished since the lead last read it, oldest first, so the lead is
    /// woken by an event rather than by asking (D195).
    completions: Arc<Mutex<VecDeque<Completion>>>,
    owns: bool,
}

/// One thing a worker did that its lead has not been told yet: it came to rest after a turn, or it
/// failed. The notice names the worker and its state, never its words; the lead reads those with
/// `status` or `inspect` and answers with `send` (D195).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Completion {
    name: String,
    state: String,
    /// Whether the lead's model has been shown it.
    announced: bool,
    /// Whether the person's transcript has shown it.
    shown: bool,
}

/// How many completions are kept before the oldest already-shown one is dropped.
const COMPLETIONS: usize = 32;

impl Default for Manager {
    fn default() -> Self {
        Self {
            shared: Default::default(),
            bus: Default::default(),
            queue: Default::default(),
            completions: Default::default(),
            owns: true,
        }
    }
}

impl Manager {
    /// The view of the team a worker's own session gets (D195).
    pub(crate) fn child(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            bus: self.bus.clone(),
            queue: self.queue.clone(),
            completions: self.completions.clone(),
            owns: false,
        }
    }
    /// A handle map the interface can use without the agent thread. It reads the peers' handles
    /// each time, so a worker started after the interface was built is still reachable.
    pub fn steering(&self) -> Steering {
        Steering {
            shared: self.shared.clone(),
            bus: self.bus.clone(),
            queue: self.queue.clone(),
        }
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

    /// Records that a worker came to rest, so the lead's next turn carries it: an event, never
    /// something the lead has to poll for (D195). The same worker resting in the same state is one
    /// notice until it is read, so a worker that idles twice is not announced twice.
    pub fn completed(&self, name: &str, state: &str) {
        let Ok(mut done) = self.completions.lock() else {
            return;
        };
        if let Some(last) = done.back()
            && last.name == name
            && last.state == state
            && !last.announced
            && !last.shown
        {
            return;
        }
        done.push_back(Completion {
            name: name.to_owned(),
            state: state.to_owned(),
            announced: false,
            shown: false,
        });
        while done.len() > COMPLETIONS {
            // Drop the oldest one both readers have seen; with none, the oldest anyway.
            match done
                .iter()
                .position(|completion| completion.announced && completion.shown)
            {
                Some(index) => {
                    done.remove(index);
                }
                None => {
                    done.pop_front();
                }
            }
        }
    }

    /// Whether a worker's completion is waiting for the lead's next turn (D195).
    pub fn pending(&self) -> bool {
        self.completions
            .lock()
            .map(|done| done.iter().any(|completion| !completion.announced))
            .unwrap_or(false)
    }

    /// Whether a worker is still starting or mid-turn, so a one-shot run waits for it instead of
    /// closing the session out from under it (D174's rule, for a team — D195).
    pub fn working(&self) -> bool {
        match self.shared.lock() {
            Ok(map) => map.values().any(|peer| {
                let state = peer.render().state;
                state == "starting" || state == "running"
            }),
            Err(_) => false,
        }
    }

    /// What has not been told yet: the model's notices are marked announced so one turn carries
    /// them, the person's are shown in the transcript (D153's shape, for workers — D195).
    pub fn notices(&self, model: bool) -> Vec<String> {
        let Ok(mut done) = self.completions.lock() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for completion in done.iter_mut() {
            let unseen = if model {
                !completion.announced
            } else {
                !completion.shown
            };
            if !unseen {
                continue;
            }
            if model {
                completion.announced = true;
            } else {
                completion.shown = true;
            }
            out.push(notice_line(&completion.name, &completion.state));
        }
        out
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
            let _ = peer.jobs.send(Job::Wake);
        }
        cancel::request();
        count
    }

    pub fn interrupt(&self, name: &str) -> Result<String, String> {
        self.owner()?;
        let peer = self
            .lookup(name)
            .ok_or_else(|| format!("no worker {name:?} in this team"))?;
        if !peer.live() {
            return Ok(format!("{name} has finished"));
        }
        // An interrupt at rest would be waiting for the next message and cancel that turn instead:
        // it ends a turn in flight or it is nothing (D195).
        if !peer.running() {
            return Ok(format!("{name} is idle; nothing to interrupt"));
        }
        peer.handle.interrupt.store(true, Ordering::SeqCst);
        peer.handle.cancel.store(true, Ordering::SeqCst);
        Ok(format!("interrupting {name}"))
    }

    pub fn stop(&self, name: &str) -> Result<String, String> {
        self.owner()?;
        let peer = self
            .lookup(name)
            .ok_or_else(|| format!("no worker {name:?} in this team"))?;
        if !peer.live() {
            return Ok(format!("{name} has finished"));
        }
        peer.handle.stop.store(true, Ordering::SeqCst);
        peer.handle.interrupt.store(true, Ordering::SeqCst);
        peer.handle.cancel.store(true, Ordering::SeqCst);
        let _ = peer.jobs.send(Job::Wake);
        Ok(format!(
            "stopping {name}; its conversation and findings are kept"
        ))
    }

    pub fn say(&self, from: &str, to: &str, text: &str) -> Result<String, String> {
        let peer = self.lookup(to).ok_or_else(|| {
            if to == "lead" {
                "a worker cannot message the lead: the lead is told when a worker comes to rest \
                 and reads it with status or inspect, so put findings and questions in your reply"
                    .to_owned()
            } else {
                format!(
                    "no worker {to:?}; workers exchange messages directly, without a lead relay"
                )
            }
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
        // The bus keeps who spoke to whom and the words, for the page and for peers that ask; the
        // conversation itself holds the full text (D195).
        if let Ok(mut bus) = self.bus.lock() {
            bus.push_back(BusMessage {
                from: from.to_owned(),
                to: to.to_owned(),
                text: text.chars().take(BUS_CHARS).collect(),
            });
            while bus.len() > BUS_MESSAGES {
                bus.pop_front();
            }
        }
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
        // What the lead is supposed to do about it, so a notice and a status answer the same
        // question the same way (D195).
        if render.state.starts_with("idle") {
            out.push_str(&format!(
                "waiting for you: reply with agent send to {name}\n"
            ));
        } else if render.state == "failed" {
            out.push_str(
                "a failed worker cannot be sent to; start another if the work continues\n",
            );
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

    /// Adds a command to the team's shared queue. Whoever gets to it runs it through their own
    /// `bash`; the queue only decides the order (D195).
    pub fn enqueue(&self, from: &str, command: &str, why: &str) -> Result<String, String> {
        self.member(from)?;
        let command = command.trim();
        if command.is_empty() {
            return Err("a queued command needs a command".into());
        }
        if command.len() > 4096 {
            return Err("a queued command is at most 4096 bytes".into());
        }
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| "the command queue is unavailable".to_string())?;
        if queue.waiting() >= QUEUE_ITEMS {
            return Err(format!(
                "the command queue is full ({QUEUE_ITEMS} commands waiting)"
            ));
        }
        queue.next += 1;
        let id = queue.next;
        queue.items.push(Queued {
            id,
            command: command.to_owned(),
            why: why.trim().to_owned(),
            owner: from.to_owned(),
            state: QueueState::Pending,
            claimed_at: None,
        });
        queue.trim();
        Ok(format!("queued #{id} for the team: {command}"))
    }

    /// Claims the next command, which holds the queue until it is finished or the lease runs out,
    /// so two workers never run the queue's commands at the same time (D195).
    pub fn claim(&self, from: &str) -> Result<String, String> {
        self.member(from)?;
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| "the command queue is unavailable".to_string())?;
        let now = Instant::now();
        if let Some(held) = queue
            .items
            .iter_mut()
            .find(|item| matches!(item.state, QueueState::Claimed { .. }))
        {
            let expired = held
                .claimed_at
                .is_some_and(|at| now.saturating_duration_since(at) >= QUEUE_LEASE);
            if !expired {
                let QueueState::Claimed { by } = &held.state else {
                    return Err("the command queue is unavailable".into());
                };
                return Err(format!(
                    "the queue runs one command at a time: #{} is claimed by {by}",
                    held.id
                ));
            }
            held.state = QueueState::Lost;
            held.claimed_at = None;
        }
        let Some(item) = queue
            .items
            .iter_mut()
            .find(|item| item.state == QueueState::Pending)
        else {
            return Ok("nothing queued".into());
        };
        item.state = QueueState::Claimed {
            by: from.to_owned(),
        };
        item.claimed_at = Some(now);
        let why = if item.why.is_empty() {
            String::new()
        } else {
            format!("\nwhy: {}", item.why)
        };
        Ok(format!(
            "claimed #{}: {}{why}\nrun it with your own bash, then finish it with {{\"action\":\"finish\",\"id\":{},\"ok\":true}}",
            item.id, item.command, item.id
        ))
    }

    /// Marks a claimed command finished, which lets the next one be claimed (D195).
    pub fn finish(&self, from: &str, id: u64, ok: bool) -> Result<String, String> {
        self.member(from)?;
        let mut queue = self
            .queue
            .lock()
            .map_err(|_| "the command queue is unavailable".to_string())?;
        let Some(item) = queue.items.iter_mut().find(|item| item.id == id) else {
            return Err(format!("no queued command #{id}"));
        };
        match &item.state {
            QueueState::Claimed { by } if by == from => {}
            QueueState::Claimed { by } => {
                return Err(format!("#{id} is claimed by {by}, not {from}"));
            }
            _ => return Err(format!("#{id} is not claimed")),
        }
        item.state = QueueState::Done { ok };
        item.claimed_at = None;
        queue.trim();
        Ok(format!("#{id} {}", if ok { "done" } else { "failed" }))
    }

    /// The queue as the model reads it: what is waiting, what is running, what has finished
    /// (D195).
    pub fn queue_text(&self) -> String {
        let Ok(queue) = self.queue.lock() else {
            return "the command queue is unavailable".into();
        };
        if queue.items.is_empty() {
            return "the command queue is empty".into();
        }
        let mut out = String::new();
        for item in &queue.items {
            let state = match &item.state {
                QueueState::Pending => "pending".to_owned(),
                QueueState::Claimed { by } => format!("running ({by})"),
                QueueState::Done { ok: true } => "done".to_owned(),
                QueueState::Done { ok: false } => "failed".to_owned(),
                QueueState::Lost => "lost (its claim expired)".to_owned(),
            };
            out.push_str(&format!(
                "#{:<3} {:<18} {}  · enqueued by {}",
                item.id, state, item.command, item.owner
            ));
            if !item.why.is_empty() {
                out.push_str(&format!(" · {}", item.why));
            }
            out.push('\n');
        }
        out
    }

    /// The bus as the model reads it: the last messages, oldest first, attributed (D195).
    pub fn bus_text(&self) -> String {
        let Ok(bus) = self.bus.lock() else {
            return "the message bus is unavailable".into();
        };
        if bus.is_empty() {
            return "no messages on the team bus yet".into();
        }
        let mut out = String::new();
        for message in bus
            .iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            out.push_str(&format!(
                "{} → {}: {}\n",
                message.from, message.to, message.text
            ));
        }
        out
    }

    /// Whether a name is the lead or a live worker, so a finished worker's old tool call cannot
    /// claim or finish in the team's queue (D195).
    fn member(&self, name: &str) -> Result<(), String> {
        if name == "lead" {
            return Ok(());
        }
        match self.lookup(name) {
            Some(peer) if peer.live() => Ok(()),
            _ => Err(format!("{name:?} is not a live worker of this team")),
        }
    }

    /// Whether this conversation is the one that started the team. A worker sees its peers and can
    /// message them, but cannot create a team of its own or stop one (D195).
    fn owner(&self) -> Result<(), String> {
        if self.owns {
            Ok(())
        } else {
            Err(
                "a worker cannot start or stop the team; only the conversation that started it can"
                    .into(),
            )
        }
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
        if let Err(why) = self.owner() {
            return Ok(Err(why));
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
        let live = self.shared.lock().map(|map| live_count(&map)).unwrap_or(0);
        if live >= team_workers {
            return Ok(Err(format!(
                "this session already has {live} live workers (tool-limits.team-workers)"
            )));
        }
        if self.lookup(name).is_some_and(|peer| peer.live()) {
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
                "workers": live + 1,
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
        let conversation = store.begin_worker(&provider_name, "")?;
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
    let name = view
        .lock()
        .map(|render| render.name.clone())
        .unwrap_or_default();
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
                // The thread sleeps on its channel rather than polling the steering handle: every
                // message, interrupt and stop sends a job to wake it (D195).
                match jobs.recv() {
                    Ok(Job::Start(brief)) => {
                        text = Some(brief);
                        break;
                    }
                    Ok(Job::Wake) => {}
                    Err(_) => {
                        finish(agent, session, &view);
                        return;
                    }
                }
                let pending = steer.take();
                if !pending.is_empty() {
                    text = Some(pending.join("\n\n"));
                    break;
                }
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
        let (state, failure) = match end {
            Ok(End::Cancelled) => ("idle (interrupted)".to_owned(), None),
            Ok(End::Answered) => ("idle".to_owned(), None),
            Ok(end) => (format!("stopped ({end:?})").to_lowercase(), None),
            Err(error) => {
                let text = format!("{error:#}");
                ("failed".to_owned(), Some(text))
            }
        };
        set(&view, |render| {
            render.state = state.clone();
            if let Some(text) = &failure {
                render.error = Some(text.clone());
                render.push(Beat::Note(format!("error: {text}")));
            }
        });
        // A worker that comes to rest is an event for the lead's next turn, which may be idle now:
        // this is what replaces the lead having to ask. A stop the lead asked for is not announced
        // (D195).
        if !stop.load(Ordering::SeqCst) {
            session.team.completed(&name, &state);
        }
        if failure.is_some() {
            break;
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

/// What one notice says: which worker, what it is now, and how the lead reads it and answers. A
/// worker's own words stay in its conversation, as a task's output stays in its record (D153), so
/// the notice carries host-owned state only (D195).
fn notice_line(name: &str, state: &str) -> String {
    let what = match state {
        "idle" => "finished its turn and is waiting",
        "idle (interrupted)" => "was interrupted and is waiting",
        "failed" => "failed",
        other if other.starts_with("stopped") => "stopped its turn",
        other => other,
    };
    if state == "failed" {
        format!(
            "worker {name} {what}; read it with the agent tool (action: status or inspect), then \
             start another if the work continues"
        )
    } else {
        format!(
            "worker {name} {what}; read it with the agent tool (action: status or inspect) and \
             answer with action send"
        )
    }
}

fn valid_name(name: &str) -> Result<(), String> {
    // "lead" is what a peer message from the conversation in hand is called, so a worker with that
    // name would make whose words they are ambiguous (D195).
    if name == "lead" {
        return Err(
            "`lead` names the conversation that started the team; pick another name".into(),
        );
    }
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
        Operation::Enqueue(job) => {
            session
                .team
                .enqueue(&session.team_label, &job.command, &job.why)
        }
        Operation::Claim => session.team.claim(&session.team_label),
        Operation::Finish(done) => session.team.finish(&session.team_label, done.id, done.ok),
        Operation::Queue => Ok(session.team.queue_text()),
        Operation::Bus => Ok(session.team.bus_text()),
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
        Operation::Roster | Operation::Claim | Operation::Queue | Operation::Bus => json!({}),
        Operation::Enqueue(job) => json!({
            "command_bytes": job.command.len(),
            "command_sha256": sha256_hex(job.command.as_bytes()),
            "why_bytes": job.why.len(),
        }),
        Operation::Finish(done) => json!({ "id": done.id, "ok": done.ok }),
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
    use saddle_wit::saddle::host::workers::{Job, Message};
    use std::sync::Mutex;

    #[test]
    fn names_are_narrow_and_briefs_keep_their_first_line() {
        for name in ["a", "runtime", "tests-2", "a_b"] {
            assert!(valid_name(name).is_ok(), "{name}");
        }
        for name in ["", "lead", "Runtime", "a/b", "../x", "a.b"] {
            assert!(valid_name(name).is_err(), "{name}");
        }
        assert_eq!(
            first_line("\n  think from the test\nmore"),
            "think from the test"
        );
    }

    fn peer(state: &str, finished: bool) -> Peer {
        let (jobs, receive) = mpsc::channel();
        // The test keeps no thread to receive: hold the receiver open so a delivery is a delivery.
        mem::forget(receive);
        let flag = || Arc::new(AtomicBool::new(false));
        Peer {
            conversation: "conversation".into(),
            handle: Handle {
                steer: Steer::default(),
                interrupt: flag(),
                stop: flag(),
                cancel: flag(),
            },
            render: Arc::new(Mutex::new(Render {
                state: state.into(),
                finished,
                ..Render::default()
            })),
            jobs,
        }
    }

    fn with_peers(peers: Vec<(&str, &str, bool)>) -> Manager {
        let mut map = BTreeMap::new();
        for (name, state, finished) in peers {
            map.insert(name.to_owned(), peer(state, finished));
        }
        Manager {
            shared: Arc::new(Mutex::new(map)),
            bus: Default::default(),
            queue: Default::default(),
            completions: Default::default(),
            owns: true,
        }
    }

    #[test]
    fn a_worker_at_rest_is_one_notice_each_reader_sees_once() {
        // D195: the lead is woken by an event, not by asking, and a repeated rest is not a second
        // wakeup while the first is still unread.
        let team = Manager::default();
        assert!(!team.pending());
        team.completed("runtime", "idle");
        team.completed("runtime", "idle");
        assert!(team.pending());
        let model = team.notices(true);
        assert_eq!(model.len(), 1);
        assert!(model[0].contains("runtime"), "{model:?}");
        assert!(model[0].contains("is waiting"), "{model:?}");
        assert!(model[0].contains("status or inspect"), "{model:?}");
        assert!(model[0].contains("action send"), "{model:?}");
        assert!(!team.pending());
        assert!(team.notices(true).is_empty(), "one turn carries it");
        // The person's own transcript still shows the line, once.
        assert_eq!(team.notices(false).len(), 1);
        assert!(team.notices(false).is_empty());
    }

    #[test]
    fn a_failed_worker_says_so_and_is_not_answered() {
        let team = Manager::default();
        team.completed("runtime", "failed");
        let notice = team.notices(true).remove(0);
        assert!(notice.contains("runtime failed"), "{notice}");
        assert!(notice.contains("start another"), "{notice}");
    }

    #[test]
    fn only_a_worker_mid_turn_keeps_a_one_shot_run_waiting() {
        assert!(with_peers(vec![("a", "running", false)]).working());
        assert!(with_peers(vec![("a", "starting", false)]).working());
        assert!(!with_peers(vec![("a", "idle", false)]).working());
        assert!(!with_peers(vec![("a", "finished", true)]).working());
    }

    #[test]
    fn a_worker_cannot_address_the_lead_and_is_told_where_its_answer_goes() {
        let team = with_peers(vec![("a", "idle", false)]);
        let refused = team.say("a", "lead", "a finding").unwrap_err();
        assert!(refused.contains("cannot message the lead"), "{refused}");
        assert!(refused.contains("status or inspect"), "{refused}");
        assert!(refused.contains("reply"), "{refused}");
    }

    #[test]
    fn an_interrupt_only_ends_a_turn_that_is_running() {
        // At rest there is no turn to end, and marking one would cancel the message that comes
        // next instead (D195).
        let idle = with_peers(vec![("idle", "idle", false)]);
        assert_eq!(
            idle.interrupt("idle").unwrap(),
            "idle is idle; nothing to interrupt"
        );
        let running = with_peers(vec![("busy", "running", false)]);
        assert_eq!(running.interrupt("busy").unwrap(), "interrupting busy");
        let finished = with_peers(vec![("done", "finished", true)]);
        assert_eq!(finished.interrupt("done").unwrap(), "done has finished");
    }

    #[test]
    fn a_finished_worker_keeps_its_conversation_but_not_a_slot_or_its_name() {
        let team = with_peers(vec![
            ("done", "finished", true),
            ("idle", "idle", false),
            ("busy", "running", false),
        ]);
        assert_eq!(live_count(&team.shared.lock().unwrap()), 2);
        // A message to a finished worker is refused rather than delivered to no thread.
        let refused = team.say("lead", "done", "hello").unwrap_err();
        assert_eq!(refused, "done has finished; start a new worker");
    }

    #[test]
    fn a_worker_session_cannot_start_or_stop_the_team() {
        let mut team = with_peers(vec![("peer", "idle", false)]);
        team.owns = false;
        let refused = team.owner().unwrap_err();
        assert!(
            refused.contains("only the conversation that started it"),
            "{refused}"
        );
        assert!(team.stop("peer").is_err());
        assert!(team.interrupt("peer").is_err());
        team.owns = true;
        assert!(team.owner().is_ok());
    }

    #[test]
    fn a_message_at_rest_clears_an_older_interrupt() {
        let team = with_peers(vec![("idle", "idle", false)]);
        team.interrupt("idle").unwrap();
        team.say("lead", "idle", "next task").unwrap();
        let peer = team.lookup("idle").unwrap();
        assert!(!peer.handle.interrupt.load(Ordering::SeqCst));
        assert!(!peer.handle.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn the_shared_queue_runs_one_claim_at_a_time() {
        // Two workers share one queue: a claim holds it until the command is finished, so the
        // commands that must not overlap are run serially (D195).
        let team = with_peers(vec![("runtime", "idle", false), ("tests", "idle", false)]);
        assert_eq!(
            team.enqueue("lead", "cargo test -p saddle-cap", "not two at once")
                .unwrap(),
            "queued #1 for the team: cargo test -p saddle-cap"
        );
        team.enqueue("lead", "cargo test -p saddle-policy", "")
            .unwrap();
        let claimed = team.claim("runtime").unwrap();
        assert!(claimed.contains("cargo test -p saddle-cap"), "{claimed}");
        let busy = team.claim("tests").unwrap_err();
        assert!(busy.contains("one command at a time"), "{busy}");
        let wrong = team.finish("tests", 1, true).unwrap_err();
        assert!(wrong.contains("claimed by runtime"), "{wrong}");
        assert_eq!(team.finish("runtime", 1, true).unwrap(), "#1 done");
        let next = team.claim("tests").unwrap();
        assert!(next.contains("cargo test -p saddle-policy"), "{next}");
        let view = team.steering().queue();
        assert_eq!(view.len(), 2);
        assert_eq!(view[1].state, QueueState::Claimed { by: "tests".into() });
        assert_eq!(team.finish("tests", 2, false).unwrap(), "#2 failed");
        assert_eq!(
            team.steering().queue()[1].state,
            QueueState::Done { ok: false }
        );
        // Only a live member may queue or claim; a finished worker's late call is refused.
        let gone = with_peers(vec![("done", "finished", true)]);
        assert!(gone.enqueue("done", "true", "").is_err());
        assert!(gone.claim("done").is_err());
    }

    #[test]
    fn a_claim_whose_lease_ran_out_frees_the_queue() {
        // A worker that dies mid-command must not hold the queue for ever: after the lease another
        // member may take the next command, and the abandoned one is marked lost (D196).
        let team = with_peers(vec![("runtime", "idle", false), ("tests", "idle", false)]);
        team.enqueue("lead", "cargo test -p saddle-cap", "")
            .unwrap();
        team.enqueue("lead", "cargo test -p saddle-policy", "")
            .unwrap();
        team.claim("runtime").unwrap();
        {
            let mut queue = team.queue.lock().unwrap();
            queue.items[0].claimed_at = Some(Instant::now() - QUEUE_LEASE);
        }
        let next = team.claim("tests").unwrap();
        assert!(next.contains("saddle-policy"), "{next}");
        let view = team.steering().queue();
        assert_eq!(view[0].state, QueueState::Lost);
        assert_eq!(view[1].state, QueueState::Claimed { by: "tests".into() });
    }

    #[test]
    fn the_bus_keeps_who_spoke_to_whom() {
        let team = with_peers(vec![("runtime", "idle", false)]);
        team.say("lead", "runtime", "check the loader\nsecond line")
            .unwrap();
        let bus = team.steering().bus();
        assert_eq!(bus.len(), 1);
        assert_eq!(bus[0].from, "lead");
        assert_eq!(bus[0].to, "runtime");
        assert!(bus[0].text.starts_with("check the loader"));
        assert!(team.bus_text().contains("lead → runtime"));
        // A queue entry's audit names the length and hash, never the command.
        let op = Operation::Enqueue(Job {
            command: "rm -rf secret".into(),
            why: "test".into(),
        });
        assert!(!arguments(&op).to_string().contains("secret"));
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
