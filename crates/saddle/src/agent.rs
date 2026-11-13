//! The model loop. A turn sends the conversation, runs the reply's tool calls through the policy
//! gate, sends the results back, and ends when a reply calls no tool.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use saddle_cap::Principal;
use saddle_policy::Kind;
use serde_json::{Value, json};

use crate::cache;
use crate::compact::{self, Reason};
use crate::context::ContextFile;
use crate::provider::{
    Cancelled, Conversation, Item, Part, Provider, Quota, Reply, Sink, Stop, ToolCall, ToolDef,
    ToolResult, Usage,
};
use crate::session::Session;
use crate::store::{self, Store};
use crate::tools::{self, ToolOutput};

/// Tool output past this many bytes is cut before it enters the context.
pub const RESULT_LIMIT: usize = 100 * 1024;

pub trait Ui: Sink {
    fn call(&mut self, call: &ToolCall);
    /// An image was attached to the prompt, for a UI that can show it (D58).
    fn attached(&mut self, _images: &[crate::provider::Image]) {}
    fn result(&mut self, out: &ToolOutput);
    /// A reply, and what it cost as far as the price is known (D36).
    fn reply(&mut self, reply: &Reply, cost: f64);
    fn compaction(&mut self, event: compact::Event);
}

/// Routes a summary request's stream to the UI as compaction events.
struct CompactSink<'a> {
    ui: &'a mut dyn Ui,
}

impl Sink for CompactSink<'_> {
    fn text(&mut self, s: &str) {
        self.ui.compaction(compact::Event::Text(s.to_owned()));
    }

    fn reasoning(&mut self, s: &str) {
        self.ui.compaction(compact::Event::Reasoning(s.to_owned()));
    }

    fn note(&mut self, s: &str) {
        self.ui.note(s);
    }

    fn quota(&mut self, quota: &Quota) {
        self.ui.quota(quota);
    }

    fn cancelled(&self) -> bool {
        self.ui.cancelled()
    }
}

/// How a compaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compacted {
    Done,
    /// Too little history to summarize.
    Nothing,
    Cancelled,
    Failed,
}

/// Total bytes of `@file` attachments in one prompt.
pub const MAX_ATTACHED: usize = 256 * 1024;
/// Marks where attachments start in a prompt as the model receives it.
pub const ATTACHED: &str = "\n\n<file path=\"";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    Answered,
    /// The model declined (a refusal or content filter).
    Declined,
    /// The final reply hit the output limit.
    Cut,
    StepLimit,
    /// The model kept asking for the same call and getting the same answer, so the turn stopped
    /// rather than spinning (D38).
    Repeating,
    /// A request to the provider failed. The conversation so far is kept.
    Failed,
    /// The user cancelled. A reply cut short is dropped; calls not yet run get an error result.
    Cancelled,
}

/// How many times the same call may return the same result before the turn stops (D38). Three is
/// enough that a legitimately repeated call that does change something — polling a build, listing a
/// directory that is filling up — is never cut short.
pub const REPEATS: usize = 3;

/// Notices a model stuck on one call (D38).
///
/// It compares a reply's calls *and* their results: an identical call whose answer changed is
/// progress, and only an identical answer means the conversation cannot move. Anything else — new
/// arguments, a different tool, a changed result — resets the count.
#[derive(Debug, Default)]
struct LoopGuard {
    /// The last batch's calls, and a digest of what they returned.
    previous: Option<(String, String)>,
    /// How many times in a row that exact pair has been seen.
    repeats: usize,
}

impl LoopGuard {
    /// Records one request's calls and results; `true` means the turn should stop.
    fn observe(&mut self, calls: &[ToolCall], results: &[ToolOutput]) -> bool {
        let batch = (signature(calls), digest(results));
        self.repeats = match &self.previous {
            Some(previous) if *previous == batch => self.repeats + 1,
            _ => 1,
        };
        self.previous = Some(batch);
        self.repeats >= REPEATS
    }

    /// How many times the same answer has now come back, for the note the user sees.
    fn seen(&self) -> usize {
        self.repeats
    }
}

/// What a batch of calls asked for, in a form two batches can be compared by. The arguments are
/// serialised rather than inspected, so a tool's own schema decides what counts as the same call.
fn signature(calls: &[ToolCall]) -> String {
    calls
        .iter()
        .map(|c| {
            let input = match &c.input {
                Ok(v) => v.to_string(),
                Err(raw) => raw.clone(),
            };
            format!("{}({input})", c.name)
        })
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// A digest of what a batch of calls returned, so a long result is compared cheaply.
fn digest(results: &[ToolOutput]) -> String {
    let mut all = String::new();
    for r in results {
        all.push_str(if r.ok { "ok" } else { "err" });
        all.push_str(&r.content);
        // A diff never reaches the model, so it is not part of what the model has seen.
        all.push('\u{1}');
    }
    saddle_policy::audit::sha256_hex(all.as_bytes())
}

/// A saved conversation read back, with what it has cost so far (D35).
#[derive(Debug, Clone, PartialEq)]
pub struct Resumed {
    pub items: Vec<Item>,
    pub usage: Usage,
    /// Dollars spent before this run, summed from what each reply recorded (D36).
    pub cost: f64,
    /// Tokens the next request would carry, from the same estimate compaction uses.
    pub context: u64,
}

/// What a rewind did: the conversation it moved to, and the items it left behind (D54).
pub struct Rewound {
    pub resumed: Resumed,
    pub abandoned: Vec<Item>,
}

pub struct Agent {
    /// `None` until the user picks one with /login.
    provider: Option<Provider>,
    /// The prompt sent with every request: `base` followed by the current grants (D47).
    system: String,
    /// The prompt without its grants section, so a reload replaces only that part.
    base: String,
    tools: Vec<ToolDef>,
    items: Vec<Item>,
    /// Model requests one prompt may take, or `None` for as many as it needs (D37).
    max_steps: Option<usize>,
    /// Where conversations are saved; `None` once saving has failed.
    store: Option<Store>,
    /// The conversation being appended to, created at its first prompt (D53).
    saved: Option<String>,
    compaction: compact::Settings,
    /// Tokens in the context at the last reply, and how many items it covered.
    context_tokens: u64,
    measured: usize,
    /// Tokens used by the last turn.
    pub turn_usage: Usage,
    /// Dollars this session has spent, as far as prices are known (D36).
    pub cost: f64,
    /// Whether a reply's cache went unused, which the user is told and the model is not (D50).
    misses: cache::Watcher,
}

impl Agent {
    pub fn new(
        provider: Option<Provider>,
        system: String,
        base: String,
        tools: Vec<ToolDef>,
        max_steps: Option<usize>,
        store: Option<Store>,
        compaction: compact::Settings,
    ) -> Self {
        Self {
            provider,
            system,
            base,
            tools,
            items: Vec::new(),
            max_steps,
            store,
            saved: None,
            compaction,
            context_tokens: 0,
            measured: 0,
            turn_usage: Usage::default(),
            cost: 0.0,
            misses: cache::Watcher::default(),
        }
    }

    /// A limit of zero is no limit: a prompt that may not take even one request would fail
    /// without saying anything useful (D37).
    pub fn set_max_steps(&mut self, max_steps: Option<usize>) {
        self.max_steps = max_steps.filter(|n| *n > 0);
    }

    pub fn set_compaction(&mut self, settings: compact::Settings) {
        self.compaction = settings;
    }

    /// Replaces the grants in the system prompt after the policy changed (D47). The prompt is sent
    /// with every request, so this is how the model is told: its next request carries the new
    /// grants, and a call that was refused under the old ones may now be allowed.
    pub fn set_grants(&mut self, grants: String) {
        self.system = format!("{}{grants}", self.base);
    }

    /// A window the endpoint stated after this session started (D34). It goes to the provider, so
    /// compaction and the header keep reading the one number they are settled from.
    pub fn set_window(&mut self, window: u64) {
        if let Some(provider) = &mut self.provider {
            provider.set_window(window);
        }
    }

    /// What the model takes, settled when the provider was built (D34), so compaction and the
    /// header cannot disagree about it.
    fn context_window(&self) -> u64 {
        self.provider
            .as_ref()
            .map_or(compact::DEFAULT_CONTEXT_WINDOW, Provider::window)
    }

    /// Tokens the next request would carry: the last reply's count plus an estimate for what was
    /// added since, or an estimate of everything when no reply has been counted.
    fn context_estimate(&self) -> u64 {
        let since = self.measured.min(self.items.len());
        let added: usize = self.items[since..].iter().map(compact::chars).sum();
        if self.context_tokens == 0 {
            let fixed = self.system.len()
                + self
                    .tools
                    .iter()
                    .map(|t| t.description.len() + t.schema.to_string().len())
                    .sum::<usize>();
            compact::tokens(fixed + added)
        } else {
            self.context_tokens + compact::tokens(added)
        }
    }

    /// Replaces the older part of the conversation with a summary the model writes. The model's
    /// reasoning and the summary stream to the UI as they come in.
    pub fn compact(
        &mut self,
        session: &mut Session,
        reason: Reason,
        focus: Option<&str>,
        ui: &mut dyn Ui,
    ) -> Result<Compacted> {
        let Some(provider) = &self.provider else {
            ui.note("no provider chosen yet: /login picks one");
            return Ok(Compacted::Failed);
        };
        let cut = compact::cut_point(&self.items, self.compaction.keep_recent);
        if cut == 0 {
            if reason == Reason::Manual {
                ui.note("nothing to compact yet");
            }
            return Ok(Compacted::Nothing);
        }
        let window = self.context_window();
        let before = self.context_estimate();
        ui.compaction(compact::Event::Started {
            reason,
            tokens: before,
            window,
            messages: cut,
        });
        // Leave room for the instructions and the summary itself.
        let room = window.saturating_sub(self.compaction.reserve).max(8_000) as usize * 3;
        let text = compact::serialize(&self.items[..cut], room);
        let request = [Item::User(compact::request(
            &text,
            &session.setup.workspace,
            focus,
        ))];
        let conv = Conversation {
            system: compact::SYSTEM,
            tools: &[],
            items: &request,
        };
        let result = provider.complete(&conv, &mut CompactSink { ui: &mut *ui });
        let reply = match result {
            Ok(reply) => reply,
            Err(e) => {
                let cancelled = e.is::<Cancelled>();
                let msg = if cancelled {
                    "cancelled".to_owned()
                } else {
                    format!("{e:#}")
                };
                self.log(
                    session,
                    json!({ "compaction": format!("{reason:?}"), "error": msg }),
                    "compaction failed",
                )?;
                ui.compaction(compact::Event::Failed(msg));
                return Ok(if cancelled {
                    Compacted::Cancelled
                } else {
                    Compacted::Failed
                });
            }
        };
        let summary: String = reply
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            .to_owned();
        self.log(
            session,
            json!({
                "compaction": format!("{reason:?}"),
                "model": reply.model,
                "usage": reply.usage,
                "summarized": cut,
            }),
            "conversation compacted",
        )?;
        let cost = self
            .provider
            .as_ref()
            .and_then(Provider::price)
            .map_or(0.0, |p| p.cost(&reply.usage));
        self.cost += cost;
        if let (Some(store), Some(id)) = (&self.store, &self.saved)
            && let Err(e) = store.usage(id, &reply.usage, cost)
        {
            self.saving_failed(e, ui);
        }
        if summary.is_empty() || reply.stop == Stop::Refusal {
            let why = format!("the model returned no summary ({})", reply.stop_raw);
            ui.compaction(compact::Event::Failed(why));
            return Ok(Compacted::Failed);
        }
        let kept = self.items.split_off(cut);
        let kept_len = kept.len();
        self.items = vec![Item::Summary(summary.clone())];
        self.items.extend(kept);
        if let (Some(store), Some(id)) = (&self.store, &self.saved)
            && let Err(e) = store.compaction(id, &summary, kept_len)
        {
            self.saving_failed(e, ui);
        }
        self.context_tokens = 0;
        self.measured = 0;
        let after = self.context_estimate();
        ui.compaction(compact::Event::Done {
            summarized: cut,
            kept: kept_len,
            before,
            after,
        });
        Ok(Compacted::Done)
    }

    /// Runs a command the user typed after `!`, under the agent's exec grant and sandbox. With
    /// `share`, the command and its output join the conversation as a user message.
    pub fn shell(
        &mut self,
        session: &mut Session,
        command: &str,
        share: bool,
        ui: &mut dyn Ui,
    ) -> Result<End> {
        let input = json!({ "command": command });
        ui.call(&ToolCall {
            id: "shell".into(),
            name: "bash".into(),
            input: Ok(input.clone()),
        });
        let out = tools::call(session, "bash", &input)?;
        ui.result(&out);
        if ui.cancelled() {
            return Ok(End::Cancelled);
        }
        if share {
            self.start_turn(session, ui);
            let shared = format!(
                "I ran a command in the workspace:\n$ {command}\n{}",
                limit(out.content)
            );
            self.record(Item::User(shared), ui);
        } else {
            ui.note("the output stays out of the conversation");
        }
        Ok(End::Answered)
    }

    /// Appends the files a prompt mentions as `@path`, each read through the policy as the agent.
    /// A token that is not an existing file is left alone.
    /// Attaches the `@path` files to the prompt, and the pictures as image parts (D58).
    fn attach(
        &mut self,
        session: &mut Session,
        prompt: &str,
        ui: &mut dyn Ui,
    ) -> Result<(String, Vec<crate::provider::Image>)> {
        let mut out = prompt.to_owned();
        let mut seen = std::collections::HashSet::new();
        let mut total = 0;
        // Pictures are not text, so they cannot go in the prompt string; they ride beside it.
        let mut images: Vec<crate::provider::Image> = Vec::new();
        for token in prompt.split_whitespace() {
            let Some(raw) = token.strip_prefix('@') else {
                continue;
            };
            let trimmed = raw.trim_end_matches([',', '.', ';', ':', ')', '?', '!', '"', '\'']);
            let is_file = |p: &str| !p.is_empty() && session.setup.workspace.join(p).is_file();
            let Some(path) = [raw, trimmed].into_iter().find(|p| is_file(p)) else {
                continue;
            };
            if !seen.insert(path.to_owned()) {
                continue;
            }
            // A file that is an image is attached as one, before anything tries to read it as
            // text (D58).
            match tools::fs::read_bytes(session, path)? {
                Ok(bytes) if tools::fs::looks_like_image(&bytes) => {
                    match crate::provider::Image::read(std::path::Path::new(path), bytes) {
                        Ok(image) => {
                            total += image.data.len();
                            // The UI decides how to show it: a picture where the terminal can
                            // draw one, a line where it cannot (D58, D59).
                            ui.attached(std::slice::from_ref(&image));
                            images.push(image);
                        }
                        Err(why) => ui.note(&format!("@{path} not attached: {why}")),
                    }
                    continue;
                }
                _ => {}
            }
            match tools::fs::read_text(session, path)? {
                Ok(text) if total + text.len() > MAX_ATTACHED => {
                    ui.note(&format!(
                        "@{path} not attached: attachments are limited to {} KiB a prompt",
                        MAX_ATTACHED / 1024
                    ));
                }
                Ok(text) => {
                    total += text.len();
                    let _ = write!(out, "{ATTACHED}{path}\">\n{}\n</file>", text.trim_end());
                    ui.note(&format!("attached {path} ({} lines)", text.lines().count()));
                }
                Err(why) => ui.note(&format!("@{path} not attached: {}", why.content)),
            }
        }
        Ok((out, images))
    }

    /// Continues a saved conversation: its items become the history, and new ones are appended
    /// to the same file.
    ///
    /// The tokens it has already spent and an estimate of what its next request would carry come
    /// back with it, so a resumed session opens on its own numbers rather than on zero (D35).
    pub fn resume(&mut self, id: &str) -> Result<Resumed> {
        let store = self
            .store
            .as_ref()
            .context("this session is not being saved, so there is nothing to resume")?;
        let loaded = store.load(id)?;
        self.saved = Some(id.to_owned());
        self.items = loaded.items.clone();
        self.context_tokens = 0;
        self.measured = 0;
        let context = self.context_estimate();
        self.cost = loaded.cost;
        Ok(Resumed {
            items: loaded.items,
            usage: loaded.usage,
            cost: loaded.cost,
            context,
        })
    }

    /// Appends to the history and to the saved transcript. A failure to save is reported once
    /// and stops saving; the conversation itself goes on.
    fn record(&mut self, item: Item, ui: &mut dyn Ui) {
        if let (Some(store), Some(id)) = (&self.store, &self.saved)
            && let Err(e) = store.append(id, &item)
        {
            self.saving_failed(e, ui);
        }
        self.items.push(item);
    }

    fn saving_failed(&mut self, e: anyhow::Error, ui: &mut dyn Ui) {
        ui.note(&format!("this session is no longer being saved: {e:#}"));
        self.store = None;
        self.saved = None;
    }

    /// Opens the transcript at a conversation's first prompt and marks the start of a turn.
    fn start_turn(&mut self, session: &Session, ui: &mut dyn Ui) {
        let (name, model) = self
            .provider
            .as_ref()
            .map(|p| (p.name.clone(), p.config.model.clone()))
            .unwrap_or_default();
        if self.saved.is_none()
            && let Some(store) = &self.store
        {
            match store.begin(&name, &model) {
                Ok(id) => self.saved = Some(id),
                Err(e) => return self.saving_failed(e, ui),
            }
        }
        if let (Some(store), Some(id)) = (&self.store, &self.saved)
            && let Err(e) = store.turn(id, session.audit.session(), &name, &model)
        {
            self.saving_failed(e, ui);
        }
    }

    /// Switches provider or model between turns, and logs the change.
    pub fn set_provider(&mut self, session: &mut Session, provider: Provider) -> Result<()> {
        let model = provider.config.model.clone();
        self.provider = Some(provider);
        self.log(session, json!({ "model": model }), "provider changed")
    }

    /// Starts a new conversation. The audit log carries on; the next prompt opens a new
    /// saved session.
    pub fn clear(&mut self) {
        self.items.clear();
        self.saved = None;
    }

    /// Continues a conversation at the point a fork was taken, with the same store: what is
    /// appended from here lands on the new session (D53).
    pub fn adopt(&mut self, id: &str) -> Result<Resumed> {
        self.saved = None;
        self.resume(id)
    }

    /// Names this conversation, or clears the name with `None` (D55).
    pub fn name(&mut self, name: Option<&str>) -> Result<()> {
        let store = self
            .store
            .as_ref()
            .context("this session is not being saved, so there is nothing to name")?;
        let id = self
            .saved
            .as_deref()
            .context("this conversation has not been saved yet, so there is nothing to name")?;
        store.name(id, name.unwrap_or_default())
    }

    /// Removes a saved conversation, refusing the one this session is writing to: deleting that
    /// would leave the next message with nowhere to go (D55).
    pub fn delete_session(&self, id: &str) -> Result<()> {
        if self.saved.as_deref() == Some(id) {
            anyhow::bail!(
                "that is the conversation this session is writing to; /clear first, then delete it"
            );
        }
        let store = self
            .store
            .as_ref()
            .context("this session is not being saved")?;
        store.delete(id)
    }

    /// What this conversation is, for `/session` (D55): which record it is, how much it holds, and
    /// what it has cost so far.
    pub fn session_info(&self, session: &mut Session) -> Result<String> {
        let store = self
            .store
            .as_ref()
            .context("this session is not being saved")?;
        let Some(id) = self.saved.as_deref() else {
            return Ok(format!(
                "this conversation is not saved yet; it will be at its first prompt.\n\
                 audit log: {}",
                session.audit.path().display()
            ));
        };
        let rows = store.rows(id)?;
        let prompts = rows
            .iter()
            .filter(|p| p.kind == store::PointKind::User)
            .count();
        let loaded = store.load(id)?;
        let u = loaded.usage;
        let name = store
            .list()?
            .into_iter()
            .find(|s| s.id == id)
            .and_then(|s| s.name);
        Ok(format!(
            "id        {id}\n\
             name      {}\n\
             messages  {} on this branch, {prompts} of them prompts\n\
             tokens    {} in ({} cache read, {} cache write), {} out\n\
             cost      ${:.4}\n\
             audit     {}",
            name.as_deref()
                .unwrap_or("(none: the first prompt is the title)"),
            rows.len(),
            u.input,
            u.cache_read,
            u.cache_write,
            u.output,
            self.cost,
            session.audit.path().display()
        ))
    }

    /// The session's messages, as rows for `/tree` (D54).
    pub fn rows(&self) -> Result<Vec<store::Point>> {
        match (&self.store, &self.saved) {
            (Some(store), Some(id)) => store.rows(id),
            // A conversation that has not been saved yet has nothing to navigate.
            _ => Ok(Vec::new()),
        }
    }

    /// Moves the conversation to a point, or to the very start when `node` is `None`, and rebuilds
    /// the history from there. Nothing is deleted: what followed stays in the database and is
    /// reachable from the tree (D54).
    /// Moves the conversation to a point, or to the very start when `node` is `None`, and rebuilds
    /// the history from there. Nothing is deleted: what followed stays in the database and is
    /// reachable from the tree (D54).
    ///
    /// The branch being left is summarized onto the new one, so what it established is not thrown
    /// away. That is a model call, so a failure is reported and the rewind stands: the move is what
    /// the user asked for, and losing it to a provider error would be the worse outcome.
    pub fn rewind(&mut self, node: Option<i64>) -> Result<Rewound> {
        let store = self
            .store
            .as_ref()
            .context("this session is not being saved, so there is nothing to go back to")?;
        let id = self.saved.clone().context(
            "this conversation has not been saved yet, so there is nothing to go back to",
        )?;
        // What the branch holds before the move, so the difference after it is exactly what was
        // abandoned. The caller decides whether that is worth a model request (D54).
        let mut abandoned = store.items_to(store.current(&id)?)?;
        store.rewind(&id, node)?;
        // The new branch is a prefix of the old one, so its length is the split point: everything
        // after it is what the move left behind.
        let kept = store.items_to(store.current(&id)?)?.len();
        let loaded = store.load(&id)?;
        self.items = loaded.items.clone();
        self.context_tokens = 0;
        self.measured = 0;
        self.cost = loaded.cost;
        let left = if abandoned.len() > kept {
            abandoned.split_off(kept)
        } else {
            Vec::new()
        };
        let resumed = Resumed {
            items: loaded.items,
            usage: loaded.usage,
            cost: loaded.cost,
            context: self.context_estimate(),
        };
        Ok(Rewound {
            resumed,
            abandoned: left,
        })
    }

    /// What the conversation looks like now, for a caller that changed it and has to say so.
    pub fn state(&mut self) -> Result<Resumed> {
        let usage = match (&self.store, &self.saved) {
            (Some(store), Some(id)) => store.load(id)?.usage,
            _ => Usage::default(),
        };
        Ok(Resumed {
            items: self.items.clone(),
            usage,
            cost: self.cost,
            context: self.context_estimate(),
        })
    }

    /// Asks the model for a short note on what a branch that is being left established, and
    /// records it on the branch that continues (D54).
    pub fn summarize_left_behind(
        &mut self,
        session: &mut Session,
        ui: &mut dyn Ui,
        abandoned: &[Item],
    ) -> Result<bool> {
        // Only work that produced something is worth a request: a prompt typed and left before any
        // reply has nothing to report.
        if !abandoned
            .iter()
            .any(|i| matches!(i, Item::Assistant(_) | Item::ToolResults(_)))
        {
            return Ok(false);
        }
        let Some(provider) = &self.provider else {
            return Ok(false);
        };
        let room = self.context_window().saturating_sub(8_000).max(4_000) as usize * 3;
        let text = compact::serialize(abandoned, room);
        let request = [Item::User(compact::branch_request(
            &text,
            &session.setup.workspace,
        ))];
        let conv = Conversation {
            system: compact::SYSTEM,
            tools: &[],
            items: &request,
        };
        let result = provider.complete(&conv, &mut CompactSink { ui: &mut *ui });
        let reply = match result {
            Ok(reply) => reply,
            Err(e) => {
                if e.is::<Cancelled>() {
                    ui.note("the branch was left without a summary: cancelled");
                } else {
                    ui.note(&format!(
                        "the branch was left without a summary: {e:#} (the work is still in /tree)"
                    ));
                }
                return Ok(false);
            }
        };
        let summary: String = reply
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<String>()
            .trim()
            .to_owned();
        let cost = provider.price().map_or(0.0, |p| p.cost(&reply.usage));
        self.cost += cost;
        let u = &mut self.turn_usage;
        u.input += reply.usage.input;
        u.output += reply.usage.output;
        u.cache_read += reply.usage.cache_read;
        u.cache_write += reply.usage.cache_write;
        if summary.is_empty() || summary == compact::BRANCH_NOTHING {
            return Ok(false);
        }
        let note = format!("what the other branch established:\n\n{summary}");
        self.log(
            session,
            json!({ "branch summary": summary.len(), "model": reply.model }),
            "branch summarized",
        )?;
        // Recorded, so a resume sees the same history, and as a summary, so the model reads it as
        // context rather than as something the user said (D33).
        let item = Item::Summary(note);
        if let (Some(store), Some(id)) = (&self.store, &self.saved)
            && let Err(e) = store.append(id, &item)
        {
            self.saving_failed(e, ui);
        }
        self.items.push(item);
        ui.note("the branch you left was summarized onto this one");
        Ok(true)
    }

    /// Copies the branch up to `node` into a new conversation and continues that one. The
    /// conversation that was being continued is left where it was, so the other branch is still
    /// there to resume (D53).
    pub fn fork(&mut self, node: Option<i64>) -> Result<Resumed> {
        let store = self
            .store
            .as_ref()
            .context("this session is not being saved, so there is nothing to fork")?;
        let from = self
            .saved
            .as_deref()
            .context("this conversation has not been saved yet, so there is nothing to fork")?;
        let id = store.fork(from, node)?;
        self.adopt(&id)
    }

    /// The reasoning effort in force, if one is set (D57).
    pub fn effort(&self) -> Option<String> {
        self.provider
            .as_ref()
            .and_then(|p| p.effort().map(str::to_owned))
    }

    /// Sets how much the model reasons, for this session, and says so (D57).
    pub fn set_effort(&mut self, effort: Option<String>) -> Option<String> {
        let provider = self.provider.as_mut()?;
        provider.set_effort(effort);
        provider.effort().map(str::to_owned)
    }

    /// Adopts a price the endpoint stated after the provider was built (D36).
    pub fn set_price(&mut self, price: crate::provider::Price) {
        if let Some(provider) = &mut self.provider {
            provider.set_price(price);
        }
    }

    pub fn turn(&mut self, session: &mut Session, prompt: &str, ui: &mut dyn Ui) -> Result<End> {
        self.turn_usage = Usage::default();
        // A miss last turn does not silence this one (D50).
        self.misses.turn_end();
        if self.provider.is_none() {
            ui.note("no provider chosen yet: /login picks one");
            return Ok(End::Failed);
        }
        self.start_turn(session, ui);
        let (prompt, images) = self.attach(session, prompt, ui)?;
        // A prompt with pictures is its own kind of item, so every place that reads an item has to
        // say what an attachment means there (D58).
        if images.is_empty() {
            self.record(Item::User(prompt), ui);
        } else {
            self.record(
                Item::Attached {
                    text: prompt,
                    images,
                },
                ui,
            );
        }
        let mut overflow_retried = false;
        let mut requests = 0usize;
        let mut guard = LoopGuard::default();
        loop {
            // Unlimited by default, so a long task is not cut short; a limit the user set still
            // stops the loop and says so (D37).
            if let Some(limit) = self.max_steps
                && requests >= limit
            {
                ui.note(&format!(
                    "stopped after {limit} model requests; send another message to continue"
                ));
                return Ok(End::StepLimit);
            }
            requests += 1;
            if ui.cancelled() {
                return Ok(End::Cancelled);
            }
            let threshold = self
                .context_window()
                .saturating_sub(self.compaction.reserve);
            if self.compaction.auto
                && self.context_estimate() > threshold
                && self.compact(session, Reason::Threshold, None, ui)? == Compacted::Cancelled
            {
                return Ok(End::Cancelled);
            }
            let conv = Conversation {
                system: &self.system,
                tools: &self.tools,
                items: &self.items,
            };
            let provider = self
                .provider
                .as_ref()
                .expect("checked at the start of the turn");
            let reply = match provider.complete(&conv, ui) {
                Ok(reply) => reply,
                Err(e) if e.is::<Cancelled>() => {
                    self.log(session, json!({}), "model request cancelled by the user")?;
                    return Ok(End::Cancelled);
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    self.log(session, json!({ "error": msg }), "model request failed")?;
                    // Too long for the window: compact and try once more.
                    if compact::is_overflow(&msg) && self.compaction.auto && !overflow_retried {
                        overflow_retried = true;
                        match self.compact(session, Reason::Overflow, None, ui)? {
                            Compacted::Done => continue,
                            Compacted::Cancelled => return Ok(End::Cancelled),
                            Compacted::Nothing | Compacted::Failed => {}
                        }
                    }
                    ui.note(&format!("model request failed: {msg}"));
                    return Ok(End::Failed);
                }
            };
            self.log(
                session,
                json!({ "model": reply.model, "stop": reply.stop_raw, "usage": reply.usage }),
                &format!("model replied ({})", reply.stop_raw),
            )?;
            let u = &mut self.turn_usage;
            u.input += reply.usage.input;
            u.output += reply.usage.output;
            u.cache_read += reply.usage.cache_read;
            u.cache_write += reply.usage.cache_write;
            // What this reply cost, from whatever price the config or the endpoint gave (D36).
            let cost = self
                .provider
                .as_ref()
                .and_then(Provider::price)
                .map_or(0.0, |p| p.cost(&reply.usage));
            self.cost += cost;
            ui.reply(&reply, cost);
            // A cache that has stopped being read costs several times what it should, and no
            // token count shows it. The user is told; the model is not (D50).
            let price = self.provider.as_ref().and_then(Provider::price);
            if let Some(note) = self.misses.reply(&reply.usage, price) {
                ui.note(&note);
            }
            if let (Some(store), Some(id)) = (&self.store, &self.saved)
                && let Err(e) = store.usage(id, &reply.usage, cost)
            {
                self.saving_failed(e, ui);
            }

            let calls: Vec<ToolCall> = reply.calls().cloned().collect();
            let stop = reply.stop;
            let u = reply.usage;
            self.record(Item::Assistant(reply.parts), ui);
            self.context_tokens = u.input + u.cache_read + u.cache_write + u.output;
            self.measured = self.items.len();
            if calls.is_empty() {
                return Ok(match stop {
                    Stop::Refusal => {
                        ui.note("the model declined to answer");
                        End::Declined
                    }
                    Stop::MaxTokens => {
                        ui.note("the reply hit the output limit");
                        End::Cut
                    }
                    _ => End::Answered,
                });
            }

            // Every call gets a result, so the history stays valid for the next request. A call
            // in a reply that was cut off or declined may be incomplete and is not run.
            let mut outputs = Vec::with_capacity(calls.len());
            let mut results = Vec::with_capacity(calls.len());
            for call in &calls {
                let out = match stop {
                    Stop::Refusal => ToolOutput::err("not run: the reply was declined"),
                    Stop::MaxTokens => ToolOutput::err(
                        "not run: the reply hit the output limit, so this call may be incomplete",
                    ),
                    _ if ui.cancelled() => ToolOutput::err("not run: cancelled by the user"),
                    _ => run_call(session, call, ui)?,
                };
                let content = limit(out.content.clone());
                // The diff is for the transcript only; the guard compares what the model saw.
                let seen = ToolOutput {
                    diff: None,
                    ..out.clone()
                };
                results.push(ToolResult {
                    id: call.id.clone(),
                    images: out.images.clone(),
                    content,
                    is_error: !out.ok,
                    diff: out.diff,
                });
                outputs.push(seen);
            }
            self.record(Item::ToolResults(results), ui);
            if stop == Stop::Refusal {
                ui.note("the model declined to continue");
                return Ok(End::Declined);
            }
            if ui.cancelled() {
                return Ok(End::Cancelled);
            }
            // The same call answered the same way REPEATS times running means the conversation
            // cannot move on its own, so stop instead of spending the window on it (D38).
            if guard.observe(&calls, &outputs) {
                let seen = guard.seen();
                ui.note(&format!(
                    "stopped: the model asked for the same call {} times and got the same answer \
                     each time; send another message to continue differently",
                    seen
                ));
                return Ok(End::Repeating);
            }
        }
    }

    fn log(&self, session: &mut Session, mut args: Value, explanation: &str) -> Result<()> {
        args["provider"] = self
            .provider
            .as_ref()
            .map_or("none", |p| p.name.as_str())
            .into();
        session
            .audit
            .event(
                Kind::Model,
                Some(&Principal::Agent),
                None,
                None,
                args,
                explanation,
            )
            .context("writing the audit log")
    }
}

/// What a call is about, for display: its command or its path.
pub fn call_detail(call: &ToolCall) -> String {
    let Ok(v) = &call.input else {
        return "(input is not valid JSON)".to_owned();
    };
    let field = |k: &str| v[k].as_str().filter(|s| !s.is_empty());
    match (field("pattern"), field("path"), field("command")) {
        (Some(pattern), Some(path), _) => format!("{pattern}  in {path}"),
        (Some(pattern), None, _) => pattern.to_owned(),
        (None, Some(path), _) => path.to_owned(),
        (None, None, Some(command)) => command.to_owned(),
        (None, None, None) => String::new(),
    }
}

fn run_call(session: &mut Session, call: &ToolCall, ui: &mut dyn Ui) -> Result<ToolOutput> {
    ui.call(call);
    let out = match &call.input {
        Err(raw) => ToolOutput::err(json!({ "INVALID_JSON": raw }).to_string()),
        Ok(_) if !tools::NAMES.contains(&call.name.as_str()) => ToolOutput::err(format!(
            "unknown tool `{}`; the tools are {}",
            call.name,
            tools::NAMES.join(", ")
        )),
        Ok(input) => tools::call(session, &call.name, input)?,
    };
    ui.result(&out);
    Ok(out)
}

fn limit(mut content: String) -> String {
    if content.is_empty() {
        return "(no output)".into();
    }
    if content.len() > RESULT_LIMIT {
        let mut cut = RESULT_LIMIT;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        let total = content.len();
        content.truncate(cut);
        let _ = write!(content, "\n(cut at {cut} of {total} bytes)");
    }
    content
}

/// The system prompt: what the tools do, how the policy answers, and what this session grants.
/// It holds no timestamps or other per-request values, so the cached prefix stays stable.
/// The grants part of the system prompt, which is the part a mid-session change replaces (D47).
pub fn grants_section(session: &Session) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\nGrants from {}:", session.setup.policy_path.display());
    for line in session.engine.policy().describe() {
        if line.starts_with("agent.") {
            let _ = writeln!(s, "  {line}");
        }
    }
    let extras = &session.setup.extras;
    // Paths, shown as written when they are inside home so the prompt stays readable.
    let names = |list: &[std::path::PathBuf]| {
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        list.iter()
            .map(|p| match &home {
                Some(home) => match p.strip_prefix(home) {
                    Ok(rest) => format!("~/{}", rest.display()),
                    Err(_) => p.display().to_string(),
                },
                None => p.display().to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !extras.read.is_empty() {
        let _ = writeln!(s, "\nCommands can also read: {}.", names(&extras.read));
    }
    if !extras.write.is_empty() {
        let _ = writeln!(s, "Commands can also write: {}.", names(&extras.write));
    }
    s
}

/// The prompt sent with every request: everything about the session, then its grants. Split so a
/// mid-session policy change replaces the grants and nothing else (D47).
pub fn system_prompt(session: &Session, context: &[ContextFile]) -> String {
    format!(
        "{}{}",
        base_prompt(session, context),
        grants_section(session)
    )
}

/// [`system_prompt`] without the grants.
pub fn base_prompt(session: &Session, context: &[ContextFile]) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "You are a coding agent working in {}. Tool paths are absolute or relative to that \
         directory.",
        session.setup.workspace.display()
    );
    s.push_str(
        "\nread, write and edit work on files; ls, find and grep explore the workspace. bash runs \
         `sh -c` with the workspace as the working directory.\n\nEvery tool call is checked against a policy file that the user \
         owns and the agent cannot write. The policy can change during a session: when the user \
         changes a grant, the grants below are updated in this prompt before your next request. A \
         refused call returns a result starting with \"denied:\" and the reason; if the grant it \
         needs has since been added, the same call works. Work within the grants, or stop and tell \
         the user which grant the task needs. Do not offer to add a grant, and do not ask for one as part of a task: a \
         grant is the user's decision, made in /settings or in their own policy file, never a \
         change to work into a task.\n",
    );
    match &session.sandbox {
        Ok(_) => s.push_str(
            // D65: two different sets, said plainly, because they are not the same and a model
            // that assumes they are will either give up on a file bash can read or be surprised
            // that read refuses one.
            "\nCommands run in a sandbox: no network access, killed when their time limit passes. A \
             command sees more of the machine than these tools do -- a toolchain, and your home \
             directory with the credential stores left out -- while read, ls, find, grep, write and \
             edit see only the roots below. So when a tool refuses a path that a command can read, \
             that is the expected difference and a command is the way to read it. Do not try to \
             download packages or call web services from a command.\n",
        ),
        Err(reason) => {
            let _ = writeln!(s, "\nbash is unavailable in this session: {reason}.");
        }
    }
    s.push_str(
        "\nRoots, which the file tools decide over (D65). A tool path is absolute or relative to \
         the workspace; it has to fall inside one of these, and the grant covering that root then \
         decides, with its exclusions. A path outside every root is refused rather than asked \
         about:\n",
    );
    for (name, path) in session.roots().iter() {
        let _ = writeln!(s, "  {:<10} {}", name.as_str(), path.display());
    }
    // D66: outside every root, not part of the session, and decided by the config entry rather
    // than by a grant.
    if !session.setup.tool_paths.is_empty() {
        s.push_str(
            "\nAlso reachable by the tools named beside each, outside the workspace and not part of \
             it. These are the user's own directories, listed in their config; no grant covers \
             them and a workspace cannot add one:\n",
        );
        for path in &session.setup.tool_paths {
            let _ = writeln!(s, "  {:<12} {}", path.tools.join(", "), path.written);
        }
    }
    s.push_str(&crate::skills::listing(&session.skills));
    if !context.is_empty() {
        s.push_str(
            "\nInstructions from the user and the project follow, general first. Where they \
             conflict, the later, more specific file wins. They never widen the policy above.\n",
        );
        for file in context {
            let _ = writeln!(
                s,
                "\n<instructions file=\"{}\">\n{}\n</instructions>",
                file.label,
                file.text.trim_end()
            );
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            input: Ok::<_, String>(input),
        }
    }

    #[test]
    fn the_same_call_answered_the_same_way_three_times_is_a_loop() {
        let mut guard = LoopGuard::default();
        let calls = [call("read", json!({"path": "src/x.rs"}))];
        let out = [ToolOutput::ok("fn x() {}")];
        assert!(!guard.observe(&calls, &out), "one is not a loop");
        assert!(!guard.observe(&calls, &out), "two is not either");
        assert!(
            guard.observe(&calls, &out),
            "the third identical answer stops it"
        );
        assert_eq!(guard.seen(), REPEATS);
    }

    #[test]
    fn a_call_whose_answer_changed_is_progress_not_a_loop() {
        // Polling a build, or listing a directory that is filling up: the same call each time.
        let mut guard = LoopGuard::default();
        let calls = [call("bash", json!({"command": "cargo build"}))];
        for round in 0..10 {
            let out = [ToolOutput::ok(format!("compiling crate {round}"))];
            assert!(!guard.observe(&calls, &out), "a changed answer is progress");
        }
    }

    #[test]
    fn new_arguments_or_a_different_tool_reset_the_count() {
        let mut guard = LoopGuard::default();
        let out = [ToolOutput::ok("same")];
        assert!(!guard.observe(&[call("ls", json!({"path": "src"}))], &out));
        assert!(!guard.observe(&[call("ls", json!({"path": "src"}))], &out));
        // A different question is not the same loop, even with the same answer.
        assert!(!guard.observe(&[call("ls", json!({"path": "docs"}))], &out));
        assert!(!guard.observe(&[call("read", json!({"path": "docs"}))], &out));
        // The run starts again from the new call.
        assert!(!guard.observe(&[call("read", json!({"path": "docs"}))], &out));
        assert!(guard.observe(&[call("read", json!({"path": "docs"}))], &out));
    }

    #[test]
    fn a_diff_for_the_transcript_does_not_make_an_answer_look_different() {
        let mut with_diff = ToolOutput::ok("wrote 3 bytes");
        with_diff.diff = Some("--- a\n+++ b\n".into());
        // The model never sees the diff, so two identical results compare equal.
        assert_eq!(
            digest(&[ToolOutput::ok("wrote 3 bytes")]),
            digest(&[with_diff])
        );
    }

    #[test]
    fn a_batch_of_calls_is_compared_as_a_whole() {
        let batch = [
            call("read", json!({"path": "a"})),
            call("grep", json!({"pattern": "x"})),
        ];
        let out = [ToolOutput::ok("a"), ToolOutput::ok("b")];
        let mut guard = LoopGuard::default();
        assert!(!guard.observe(&batch, &out));
        assert!(!guard.observe(&batch, &out));
        assert!(guard.observe(&batch, &out));
        // Half the batch changing is a new request.
        let mut guard = LoopGuard::default();
        guard.observe(&batch, &out);
        guard.observe(&batch, &out);
        let other = [
            call("read", json!({"path": "a"})),
            call("grep", json!({"pattern": "y"})),
        ];
        assert!(!guard.observe(&other, &out));
    }

    #[test]
    fn a_reload_replaces_the_grants_and_leaves_the_rest_of_the_prompt_alone() {
        // D47: the prompt is sent with every request, so swapping its grants section is how the
        // model is told. Everything before that section has to survive untouched.
        let mut agent = Agent::new(
            None,
            "You are a coding agent.\nGrants:\n  agent.fs-read[0]".into(),
            "You are a coding agent.\n".into(),
            Vec::new(),
            None,
            None,
            compact::Settings::default(),
        );
        agent.set_grants("Grants:\n  agent.fs-read[0]\n  agent.net[0]".into());
        assert!(agent.system.starts_with("You are a coding agent.\n"));
        assert!(agent.system.contains("agent.fs-read[0]"));
        assert!(
            agent.system.contains("agent.net[0]"),
            "the new grant is in the prompt"
        );
        // Setting it again replaces rather than appends.
        agent.set_grants("Grants:\n  agent.exec".into());
        assert_eq!(
            agent.system,
            "You are a coding agent.\nGrants:\n  agent.exec"
        );
        assert!(
            !agent.system.contains("agent.net"),
            "no stale grant is left behind"
        );
    }

    #[test]
    fn a_prompt_has_no_request_limit_unless_one_is_set() {
        let mut agent = Agent::new(
            None,
            String::new(),
            String::new(),
            Vec::new(),
            None,
            None,
            compact::Settings::default(),
        );
        assert_eq!(agent.max_steps, None, "the default is no limit (D37)");

        agent.set_max_steps(Some(7));
        assert_eq!(agent.max_steps, Some(7));
        // Zero requests would fail without saying anything useful, so it reads as no limit.
        agent.set_max_steps(Some(0));
        assert_eq!(agent.max_steps, None);
    }

    #[test]
    fn a_resumed_session_comes_back_with_its_cost_and_its_context() {
        // One store, in memory: the session is written and then read back through the same API a
        // resumed session uses (D53).
        let store = Store::in_memory("/ws").unwrap();
        let id = store.begin("synthetic", "glm").unwrap();
        store
            .append(&id, &Item::User("what changed?".into()))
            .unwrap();
        store
            .append(&id, &Item::Assistant(vec![Part::Text("two files".into())]))
            .unwrap();
        store
            .usage(
                &id,
                &Usage {
                    input: 1_200,
                    output: 40,
                    cache_read: 800,
                    cache_write: 0,
                },
                0.0,
            )
            .unwrap();

        let mut agent = Agent::new(
            None,
            "the system prompt".into(),
            "the system prompt".into(),
            Vec::new(),
            Some(5),
            Some(store),
            compact::Settings::default(),
        );
        let saved = agent.resume(&id).unwrap();
        assert_eq!(saved.items.len(), 2);
        assert_eq!(saved.usage.input, 1_200);
        assert_eq!(saved.usage.cache_read, 800);
        // The estimate covers the system prompt and both items, so the status line opens on a
        // number rather than on zero (D35).
        assert!(saved.context > 0);
        assert_eq!(saved.context, agent.context_estimate());
    }

    fn agent_with(store: Store, id: &str) -> Agent {
        let mut agent = Agent::new(
            None,
            "the system prompt".into(),
            "the system prompt".into(),
            Vec::new(),
            Some(5),
            Some(store),
            compact::Settings::default(),
        );
        agent.resume(id).unwrap();
        agent
    }

    #[test]
    fn a_fork_continues_the_new_conversation_and_leaves_the_old_branch_alone() {
        let store = Store::in_memory("/ws").unwrap();
        let id = store.begin("p", "m").unwrap();
        store.append(&id, &Item::User("first".into())).unwrap();
        store.append(&id, &Item::User("second".into())).unwrap();
        let mut agent = agent_with(store, &id);
        assert_eq!(agent.items.len(), 2);

        // Fork at the first prompt: the conversation continues in a new conversation (D53).
        let nodes: Vec<i64> = agent.rows().unwrap().into_iter().map(|p| p.node).collect();
        let forked = agent.fork(Some(nodes[0])).unwrap();
        assert_eq!(forked.items, [Item::User("first".into())]);
        assert_eq!(agent.items, [Item::User("first".into())]);
        // What is appended now goes to the new conversation, not the one that was continued.
        assert_eq!(agent.rows().unwrap().len(), 1);
    }

    #[test]
    fn rewinding_rebuilds_the_history_and_keeps_the_branch_it_left() {
        let store = Store::in_memory("/ws").unwrap();
        let id = store.begin("p", "m").unwrap();
        store.append(&id, &Item::User("first".into())).unwrap();
        store
            .append(&id, &Item::Assistant(vec![Part::Text("one".into())]))
            .unwrap();
        store.append(&id, &Item::User("second".into())).unwrap();
        let mut agent = agent_with(store, &id);
        assert_eq!(agent.items.len(), 3);

        // Back to before the second prompt — what `/tree` does when a prompt is chosen, so that
        // sending it again makes another branch (D54).
        let second = agent
            .rows()
            .unwrap()
            .iter()
            .find(|p| p.preview == "second")
            .map(|p| p.parent.unwrap())
            .unwrap();
        let back = agent.rewind(Some(second)).unwrap();
        assert_eq!(
            back.resumed.items,
            [
                Item::User("first".into()),
                Item::Assistant(vec![Part::Text("one".into())])
            ]
        );
        assert_eq!(agent.items.len(), 2);
        // What was left behind is handed back, so the caller can have it summarized (D54).
        assert_eq!(back.abandoned, [Item::User("second".into())]);
        // Every message is still there to walk back through, including the one just left.
        assert!(agent.rows().unwrap().len() >= 3);
    }

    #[test]
    fn results_are_capped_on_a_char_boundary() {
        assert_eq!(limit(String::new()), "(no output)");
        let long = "é".repeat(RESULT_LIMIT);
        let cut = limit(long);
        assert!(cut.len() < RESULT_LIMIT + 64);
        assert!(cut.ends_with(&format!("of {} bytes)", 2 * RESULT_LIMIT)));
    }
}
