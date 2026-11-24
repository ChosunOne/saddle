//! Saved conversations, in an embedded database (D53).
//!
//! A conversation is a tree, not a list: `/fork` starts a new conversation from an earlier message,
//! and the message it branched at can hold more than one continuation. A JSON Lines file per
//! session cannot say that without copying everything, so the store is SQLite, one row per message
//! with a parent pointer, and a per-session pointer at the message the conversation is up to. The
//! path from the root to that pointer is what the model is sent; the other branches are kept.
//!
//! The database lives in the state directory, which is outside every root (D16), so the agent
//! cannot read earlier conversations through its own tools. That is the same guarantee the files
//! had. What is new is that a damaged write cannot take the conversation with it: SQLite commits a
//! message or it does not, where a torn last line used to be possible.
//!
//! Older files are imported once, on first open: a working tree that had sessions before this
//! change keeps them, in the same order, with their tokens and cost intact.

use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::provider::{Contribution, Item, Part, ToolCall, ToolResult, Usage, reasoning_text};

/// What a node holds. Every node is one of these, and the kind is a column so a query can pick
/// one out without parsing the body.
mod kind {
    /// One [`crate::provider::Item`], as JSON.
    pub const ITEM: &str = "item";
    /// What one reply used and cost.
    pub const USAGE: &str = "usage";
    /// The start of a turn: which audit log covers it.
    pub const TURN: &str = "turn";
    /// A summary that replaced the items before it.
    pub const COMPACTION: &str = "compaction";
    /// Tools an MCP search declared (D108, D109). Bookkeeping like `usage`, not conversation: it
    /// is replayed as a declaration, never as a message.
    pub const DECLARED: &str = "declared";
}

/// Which provider and model a conversation was last working in (D91).
///
/// The model is a property of the conversation, not of `config.toml`: what a person switched to
/// while working on this one is what it comes back on, and a default changed since — in this
/// session or in another one — does not rewrite a conversation that already exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub provider: String,
    pub model: String,
}

/// What one model request used and what answered it (D94).
///
/// A turn makes one request per reply and a compaction makes one of its own; both are paid for, so
/// both are recorded. The provider and model go with each one because a conversation can switch
/// between them (D91), and a cache hit rate only means anything for the pair that answered.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub kind: RequestKind,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub cost: f64,
    /// Reasoning items the provider said it removed from what it was sent (D97).
    pub dropped: u64,
    /// How many of the output tokens were spent thinking (D99), or 0 when the provider says
    /// nothing about it or has no split to give.
    pub reasoning_tokens: u64,
}

/// Which request a `usage` row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// The reply to the conversation.
    Reply,
    /// A compaction's summary, which is a request the person paid for (D78).
    Summary,
    Plugin,
}

impl RequestKind {
    fn as_str(self) -> &'static str {
        match self {
            RequestKind::Reply => "reply",
            RequestKind::Summary => "summary",
            RequestKind::Plugin => "plugin",
        }
    }
}

/// One recorded request, as `session_query` reads it (D94).
#[derive(Debug, Clone, PartialEq)]
pub struct RequestRow {
    pub node: i64,
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub cost: f64,
    /// Reasoning items the provider said it removed from the prompt (D97).
    pub dropped: u64,
    /// How many of the output tokens were spent thinking (D99).
    pub reasoning_tokens: u64,
    /// When the row was written, to the second.
    pub at: String,
    /// The ordinal of the last message before this request, or 0 when it came first.
    pub after: usize,
}

/// One reasoning block, as `session_query` reads it (D98).
#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningRow {
    /// The ordinal of the message it belongs to, in the same space as `messages`, so the two join.
    pub ordinal: usize,
    pub node: i64,
    /// Its position among that message's parts, so several blocks keep the order they arrived in.
    pub part: usize,
    /// What the block says, not the block itself: an encrypted item has no row at all (D98).
    pub text: String,
}

/// What one `session_query` may see of one conversation: its branch's messages, the requests that
/// produced them (D94), and what the model thought while producing them (D98).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// `(node, role, text)` per message on the branch, in order.
    pub messages: Vec<(i64, String, String)>,
    pub calls: Vec<CompletedCall>,
    pub requests: Vec<RequestRow>,
    /// Reasoning in branch order, each row naming the message it belongs to.
    pub reasoning: Vec<ReasoningRow>,
}

/// a completed call paired with its host-attributed result on the saved branch (D194).
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedCall {
    pub node: i64,
    pub part: usize,
    pub name: String,
    pub arguments: String,
    pub principal: String,
    pub ok: bool,
}

/// The schema version this build writes. A database from a newer build is refused rather than
/// guessed at, because a wrong guess here loses a conversation. Version 2 added `sessions.name`.
const VERSION: u32 = 2;

/// One `usage` row as the transcript tool reports it (D94).
///
/// A row written before the provider, model and kind were recorded still reads: what it cost is
/// still true, and the parts it never had come back empty rather than being guessed at.
fn request_row(node: &Node, after: usize) -> RequestRow {
    let body: serde_json::Value = serde_json::from_str(&node.body).unwrap_or_default();
    RequestRow {
        node: node.id,
        kind: body["kind"].as_str().unwrap_or("unknown").to_owned(),
        provider: body["provider"].as_str().unwrap_or_default().to_owned(),
        model: body["model"].as_str().unwrap_or_default().to_owned(),
        usage: serde_json::from_value(body["usage"].clone()).unwrap_or_default(),
        cost: body["cost"].as_f64().unwrap_or(0.0),
        dropped: body["dropped"].as_u64().unwrap_or(0),
        // A row written before this was recorded reads as 0: unknown is not a claim that the
        // model did no thinking (D99).
        reasoning_tokens: body["reasoning_tokens"].as_u64().unwrap_or(0),
        at: node.created.clone(),
        after,
    }
}

fn now() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// A saved conversation, opened for reading or appending.
///
/// Two of these can point at the same file: the agent thread appends while the UI thread lists.
/// That is what SQLite is for, and it is why the store takes no lock of its own.
pub struct Store {
    conn: Connection,
    pub workspace: PathBuf,
}

impl Store {
    /// Opens (or creates) the database and, on a fresh one, imports any older session files.
    pub fn open(path: &Path, workspace: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        // Two writers are possible (the agent thread and the UI thread); WAL lets a reader in
        // while a write is happening, and the busy timeout waits rather than failing.
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS sessions (
                 id        TEXT PRIMARY KEY,
                 workspace TEXT NOT NULL,
                 created   TEXT NOT NULL,
                 updated   TEXT NOT NULL,
                 provider  TEXT NOT NULL DEFAULT '',
                 model     TEXT NOT NULL DEFAULT '',
                 leaf      INTEGER,
                 -- Where a fork came from, so the tree can say so.
                 from_session TEXT,
                 from_node    INTEGER
             );
             CREATE TABLE IF NOT EXISTS nodes (
                 id      INTEGER PRIMARY KEY AUTOINCREMENT,
                 session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 parent  INTEGER REFERENCES nodes(id),
                 kind    TEXT NOT NULL,
                 body    TEXT NOT NULL,
                 created TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS nodes_session ON nodes(session, id);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             COMMIT;",
        )?;
        let found: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if found > VERSION {
            anyhow::bail!(
                "{} was written by a newer saddle (schema {found}, this build reads {VERSION})",
                path.display()
            );
        }
        // Migrations run after `CREATE TABLE IF NOT EXISTS`, so a database this build made first
        // gets the column added rather than replaced. Each one is written to be repeatable: a
        // migration that runs twice must not fail (D55).
        if found < 2 {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN name TEXT")?;
        }
        conn.pragma_update(None, "user_version", VERSION)?;
        let store = Self { conn, workspace };
        store.import_legacy(path)?;
        Ok(store)
    }

    /// An in-memory database, for tests.
    #[cfg(test)]
    pub fn in_memory(workspace: &str) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, workspace TEXT NOT NULL, created TEXT NOT NULL,
                 updated TEXT NOT NULL, provider TEXT NOT NULL DEFAULT '',
                 model TEXT NOT NULL DEFAULT '', leaf INTEGER,
                 from_session TEXT, from_node INTEGER, name TEXT);
             CREATE TABLE nodes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session TEXT NOT NULL, parent INTEGER, kind TEXT NOT NULL,
                 body TEXT NOT NULL, created TEXT NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Self {
            conn,
            workspace: PathBuf::from(workspace),
        })
    }

    /// Starts a conversation. The id sorts by time, so the newest session is the first row of a
    /// listing without reading every row's timestamp.
    pub fn begin(&self, provider: &str, model: &str) -> Result<String> {
        let id = session_id();
        let at = now();
        self.conn.execute(
            "INSERT INTO sessions (id, workspace, created, updated, provider, model)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
            params![id, self.workspace.to_string_lossy(), at, provider, model],
        )?;
        Ok(id)
    }

    /// The message the conversation is up to, which is the end of the branch being continued.
    fn leaf(&self, session: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT leaf FROM sessions WHERE id = ?1",
                params![session],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Appends one node under the current leaf and moves the leaf onto it. The parent and the
    /// pointer are written together, so a crash leaves a conversation that is one message shorter
    /// rather than one that is inconsistent.
    fn push(&self, session: &str, kind: &str, body: &str) -> Result<i64> {
        let parent = self.leaf(session)?;
        let at = now();
        self.conn.execute(
            "INSERT INTO nodes (session, parent, kind, body, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session, parent, kind, body, at],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "UPDATE sessions SET leaf = ?2, updated = ?3 WHERE id = ?1",
            params![session, id, at],
        )?;
        Ok(id)
    }

    /// Names a conversation, for a `--resume` list that means something. An empty name clears it,
    /// so `/name` with no argument goes back to showing the first prompt (D55).
    pub fn name(&self, session: &str, name: &str) -> Result<()> {
        let name = name.trim();
        self.conn.execute(
            "UPDATE sessions SET name = ?2 WHERE id = ?1",
            params![session, (!name.is_empty()).then_some(name)],
        )?;
        Ok(())
    }

    pub fn turn(&self, session: &str, audit: &str, provider: &str, model: &str) -> Result<()> {
        let body = serde_json::json!({ "audit": audit, "provider": provider, "model": model });
        self.push(session, kind::TURN, &body.to_string())?;
        Ok(())
    }

    /// Records that the conversation moved to another model (D91).
    ///
    /// The row is what a resumed conversation is read back from, and what the resume list shows, so
    /// a model chosen and then left behind is still the one the next turn is expected to use.
    pub fn model_change(&self, session: &str, provider: &str, model: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET provider = ?2, model = ?3, updated = ?4 WHERE id = ?1",
            params![session, provider, model, now()],
        )?;
        Ok(())
    }

    /// The provider and model this conversation was last working in, if it has been saved and
    /// something was recorded (D91).
    pub fn model_of(&self, session: &str) -> Result<Option<Model>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT provider, model FROM sessions WHERE id = ?1",
                params![session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(provider, model)| {
            (!provider.is_empty() && !model.is_empty()).then_some(Model { provider, model })
        }))
    }

    /// Records one model request: what it used, what it cost, and what answered it (D94).
    pub fn request(&self, session: &str, request: &Request) -> Result<()> {
        self.request_attributed(session, request, None)
    }

    /// retain plugin identity and invocation alongside usage on the saved branch (D156).
    pub fn plugin_request(
        &self,
        session: &str,
        request: &Request,
        principal: &str,
        call: &str,
    ) -> Result<()> {
        self.request_attributed(session, request, Some((principal, call)))
    }

    fn request_attributed(
        &self,
        session: &str,
        request: &Request,
        origin: Option<(&str, &str)>,
    ) -> Result<()> {
        let mut body = serde_json::json!({
            "kind": request.kind.as_str(),
            "provider": request.provider,
            "model": request.model,
            "usage": request.usage,
            "cost": request.cost,
            "dropped": request.dropped,
            "reasoning_tokens": request.reasoning_tokens,
        });
        if let Some((principal, call)) = origin {
            body["principal"] = principal.into();
            body["call_id"] = call.into();
        }
        self.push(session, kind::USAGE, &body.to_string())?;
        Ok(())
    }

    pub fn append(&self, session: &str, item: &Item) -> Result<()> {
        let body = serde_json::to_string(item)?;
        self.push(session, kind::ITEM, &body)?;
        Ok(())
    }

    /// Records that a search declared these tools on `session`'s current branch (D109). A node
    /// rather than a column, so a rewind to before the declaration drops it with everything else
    /// the branch left behind.
    pub fn declare(&self, session: &str, names: &[String]) -> Result<()> {
        let body = serde_json::json!({ "names": names });
        self.push(session, kind::DECLARED, &body.to_string())?;
        Ok(())
    }

    pub fn compaction(&self, session: &str, summary: &str, kept: usize) -> Result<()> {
        let body = serde_json::json!({ "summary": summary, "kept": kept });
        self.push(session, kind::COMPACTION, &body.to_string())?;
        Ok(())
    }

    /// One node as it was stored.
    fn node(&self, id: i64) -> Result<Node> {
        Ok(self.conn.query_row(
            "SELECT id, parent, kind, body, created FROM nodes WHERE id = ?1",
            params![id],
            |r| {
                Ok(Node {
                    id: r.get(0)?,
                    parent: r.get(1)?,
                    kind: r.get(2)?,
                    body: r.get(3)?,
                    created: r.get(4)?,
                })
            },
        )?)
    }

    /// Every node from the root to `leaf` (or to the session's current leaf), in order.
    fn branch_to(&self, leaf: Option<i64>) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut at = leaf;
        while let Some(id) = at {
            let node = self.node(id)?;
            at = node.parent;
            out.push(node);
        }
        out.reverse();
        Ok(out)
    }

    /// The current branch, from the root to where the conversation is.
    fn branch(&self, session: &str) -> Result<Vec<Node>> {
        self.branch_to(self.leaf(session)?)
    }

    /// What `session_query` may see of one conversation — the one in hand, or another of this
    /// workspace when the grant covers it (D163): its branch's text, before compaction as well as
    /// after it (D92), and what each request on that branch used (D94).
    ///
    /// One walk fills all three, which is what lets a request name the message it answered and a
    /// reasoning block name the reply it produced: a request that cost nothing for a 100k-token
    /// prompt is only diagnosable next to that prompt.
    pub fn snapshot(&self, session: &str) -> Result<Snapshot> {
        let mut out = Vec::new();
        let mut calls = Vec::new();
        let mut pending: Vec<ToolCall> = Vec::new();
        let mut requests = Vec::new();
        let mut reasoning = Vec::new();
        for node in self.branch(session)? {
            if node.kind == kind::ITEM {
                let item: Item = serde_json::from_str(&node.body)?;
                let ordinal = out.len() + 1;
                if let Item::ToolResults(results) = &item {
                    for (part, (call, result)) in pending.drain(..).zip(results).enumerate() {
                        if call.id == result.id
                            && let Ok(input) = call.input
                        {
                            calls.push(CompletedCall {
                                node: node.id,
                                part,
                                name: call.name,
                                arguments: input.to_string(),
                                ok: !result.is_error,
                                principal: result
                                    .provenance
                                    .as_ref()
                                    .map(|p| p.principal.clone())
                                    .unwrap_or_default(),
                            });
                        }
                    }
                } else {
                    pending.clear();
                }
                let (role, text) = match item {
                    Item::User(text) | Item::Attached { text, .. } => ("user", text),
                    Item::Contribution(value) => ("plugin", value.content),
                    Item::Summary(text) => ("summary", text),
                    Item::Assistant(parts) => {
                        // D98: the thinking is kept whole to replay it, and only its prose is
                        // readable here, so the row carries the text rather than the block.
                        for (part, block) in parts.iter().enumerate() {
                            if let Some(text) = reasoning_text(block) {
                                reasoning.push(ReasoningRow {
                                    ordinal,
                                    node: node.id,
                                    part,
                                    text,
                                });
                            }
                        }
                        (
                            "assistant",
                            parts
                                .into_iter()
                                .filter_map(|part| match part {
                                    Part::Text(text) => Some(text),
                                    Part::Call(call) => {
                                        let text = serde_json::to_string(&call).unwrap_or_default();
                                        pending.push(call);
                                        Some(text)
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                    Item::ToolResults(results) => (
                        "tool",
                        results
                            .into_iter()
                            .map(|r| format!("{}: {}", r.id, r.content))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                };
                out.push((node.id, role.to_owned(), text));
            } else if node.kind == kind::COMPACTION {
                let body: serde_json::Value = serde_json::from_str(&node.body)?;
                out.push((
                    node.id,
                    "summary".into(),
                    body["summary"].as_str().unwrap_or_default().into(),
                ));
            } else if node.kind == kind::USAGE {
                requests.push(request_row(&node, out.len()));
            }
        }
        Ok(Snapshot {
            messages: out,
            calls,
            requests,
            reasoning,
        })
    }

    /// The current leaf, for a caller that needs to name a point in the tree (D54).
    pub fn current(&self, session: &str) -> Result<Option<i64>> {
        self.leaf(session)
    }

    /// The conversation as it stood at `leaf`, for working out what a rewind leaves behind (D54).
    pub fn items_to(&self, leaf: Option<i64>) -> Result<Vec<Item>> {
        let mut items = Vec::new();
        for node in self.branch_to(leaf)? {
            match node.kind.as_str() {
                kind::ITEM => {
                    if let Ok(item) = serde_json::from_str::<Item>(&node.body) {
                        items.push(item);
                    }
                }
                kind::COMPACTION => {
                    let v: serde_json::Value = serde_json::from_str(&node.body).unwrap_or_default();
                    let kept = v["kept"].as_u64().unwrap_or(0) as usize;
                    let summary = v["summary"].as_str().unwrap_or_default().to_owned();
                    let kept = items.split_off(items.len().saturating_sub(kept));
                    items = vec![Item::Summary(summary)];
                    items.extend(kept);
                }
                _ => {}
            }
        }
        Ok(items)
    }

    /// Reads the current branch back as a conversation: its items, and what its replies cost.
    ///
    /// A tool call left without a result — the process died before it ran — gets one, so the
    /// history is valid to send again.
    pub fn load(&self, session: &str) -> Result<Loaded> {
        let mut items = Vec::new();
        let mut usage = Usage::default();
        let mut cost = 0.0;
        let mut declared = Vec::new();
        for node in self.branch(session)? {
            match node.kind.as_str() {
                kind::ITEM => match serde_json::from_str::<Item>(&node.body) {
                    Ok(item) => items.push(item),
                    // A body that does not parse is a row this build cannot read; skipping it is
                    // better than refusing to open the whole conversation.
                    Err(_) => continue,
                },
                kind::USAGE => {
                    let v: serde_json::Value = serde_json::from_str(&node.body).unwrap_or_default();
                    if let Ok(u) = serde_json::from_value::<Usage>(v["usage"].clone()) {
                        usage.input += u.input;
                        usage.output += u.output;
                        usage.cache_read += u.cache_read;
                        usage.cache_write += u.cache_write;
                    }
                    cost += v["cost"].as_f64().unwrap_or(0.0);
                }
                kind::COMPACTION => {
                    let v: serde_json::Value = serde_json::from_str(&node.body).unwrap_or_default();
                    let kept = v["kept"].as_u64().unwrap_or(0) as usize;
                    let summary = v["summary"].as_str().unwrap_or_default().to_owned();
                    let kept = items.split_off(items.len().saturating_sub(kept));
                    items = vec![Item::Summary(summary)];
                    items.extend(kept);
                }
                kind::DECLARED => {
                    let v: serde_json::Value = serde_json::from_str(&node.body).unwrap_or_default();
                    if let Some(names) = v["names"].as_array() {
                        for name in names.iter().filter_map(serde_json::Value::as_str) {
                            if !declared.iter().any(|d| d == name) {
                                declared.push(name.to_owned());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        repair(&mut items);
        Ok(Loaded {
            items,
            usage,
            cost,
            declared,
        })
    }

    /// This workspace's conversations, most recently updated first.
    pub fn list(&self) -> Result<Vec<Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, updated, model, from_session, name,
                    (SELECT COUNT(*) FROM nodes n
                      WHERE n.session = s.id AND n.kind = 'item'
                        AND json_extract(n.body, '$.user') IS NOT NULL),
                    (SELECT COUNT(*) FROM nodes n WHERE n.session = s.id AND n.kind = 'item')
               FROM sessions s
              WHERE workspace = ?1
              ORDER BY updated DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![self.workspace.to_string_lossy()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, updated, model, from, name, prompts, messages) = row?;
            // A session with no prompt yet is not offered: it has nothing to resume.
            let Some(first) = self.first_prompt(&id)? else {
                continue;
            };
            // A name is what the user chose to recognise it by; without one the first prompt is
            // the best title there is (D55).
            let title = name.clone().unwrap_or(first);
            out.push(Summary {
                id,
                name,
                title,
                prompts: prompts.max(0) as usize,
                messages: messages.max(0) as usize,
                model,
                forked_from: from,
                updated: humantime::parse_rfc3339(&updated).unwrap_or(UNIX_EPOCH),
            });
        }
        Ok(out)
    }

    /// Every message in the session, in the order they were made, with the ones on the active
    /// branch marked (D54).
    ///
    /// It is the whole session rather than the active branch on purpose: a branch left behind by a
    /// rewind has to stay reachable, and after a rewind to the start the active branch is empty —
    /// a view built from that alone would show nothing and strand everything.
    pub fn rows(&self, session: &str) -> Result<Vec<Point>> {
        // The nodes the next request will carry: the path from the leaf to the root.
        let mut active = HashSet::new();
        let mut at = self.leaf(session)?;
        while let Some(id) = at {
            active.insert(id);
            at = self.conn.query_row(
                "SELECT parent FROM nodes WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<i64>>(0),
            )?;
        }
        let leaf = self.leaf(session)?;
        let mut stmt = self.conn.prepare(
            "SELECT id, parent, kind, body, created FROM nodes WHERE session = ?1 ORDER BY id",
        )?;
        let nodes = stmt.query_map(params![session], |r| {
            Ok(Node {
                id: r.get(0)?,
                parent: r.get(1)?,
                kind: r.get(2)?,
                body: r.get(3)?,
                created: r.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for node in nodes {
            let node = node?;
            if let Some(mut point) = self.point(&node, leaf)? {
                point.active = active.contains(&node.id);
                out.push(point);
            }
        }
        Ok(out)
    }

    /// correction ownership includes pre-compaction facts, but only on this saved branch (D156).
    pub fn contributions(&self, session: &str) -> Result<Vec<Contribution>> {
        let mut facts = Vec::new();
        for node in self.branch(session)? {
            if node.kind == kind::ITEM
                && let Ok(Item::Contribution(value)) = serde_json::from_str::<Item>(&node.body)
            {
                facts.push(*value);
            }
        }
        Ok(facts)
    }

    /// One node as a row: its kind, a one-line preview, and the text a user message would be
    /// edited as. `None` for the bookkeeping nodes (`turn`, `usage`), which are not conversation.
    fn point(&self, node: &Node, leaf: Option<i64>) -> Result<Option<Point>> {
        let (kind, preview, text) = match node.kind.as_str() {
            kind::ITEM => match serde_json::from_str::<Item>(&node.body) {
                Ok(Item::User(t)) => (PointKind::User, first_line(&t), t),
                Ok(Item::Assistant(parts)) => (
                    PointKind::Assistant,
                    assistant_preview(&parts),
                    String::new(),
                ),
                Ok(Item::ToolResults(results)) => {
                    let failed = results.iter().filter(|r| r.is_error).count();
                    let n = results.len();
                    let plural = if n == 1 { "" } else { "s" };
                    let label = match failed {
                        0 => format!("{n} tool result{plural}"),
                        f if f == n => format!("{n} tool result{plural}, failed"),
                        f => format!("{n} tool result{plural}, {f} failed"),
                    };
                    (PointKind::Tool, label, String::new())
                }
                Ok(Item::Contribution(value)) => (
                    PointKind::Tool,
                    format!("{} contribution", value.provenance.principal),
                    String::new(),
                ),
                Ok(Item::Summary(t)) => (PointKind::Summary, first_line(&t), String::new()),
                Ok(Item::Attached { text, images }) => {
                    let mut preview = first_line(&text);
                    if preview.is_empty() && images.is_empty() {
                        preview = "(empty prompt)".to_owned();
                    }
                    if !images.is_empty() {
                        let plural = if images.len() == 1 { "" } else { "s" };
                        preview = format!("{preview} [{} image{plural}]", images.len())
                            .trim()
                            .to_owned();
                    }
                    (PointKind::User, preview, text)
                }
                Err(_) => return Ok(None),
            },
            // The turn markers and usage records are bookkeeping: they are what `/audit` and the
            // cost line read, not what a person navigates.
            _ => return Ok(None),
        };
        Ok(Some(Point {
            node: node.id,
            parent: node.parent,
            kind,
            preview,
            text,
            here: leaf == Some(node.id),
            active: false,
        }))
    }

    /// Moves the conversation to a node, or to before the first one when `None`, without deleting
    /// anything: what follows stays in the database and stays in the tree view (D54).
    pub fn rewind(&self, session: &str, node: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET leaf = ?2, updated = ?3 WHERE id = ?1",
            params![session, node, now()],
        )?;
        Ok(())
    }

    /// Starts a new conversation from the prompt at `at`, or from the end of the branch when it is
    /// absent. Every node up to that point is copied, so the fork stands on its own and the two can
    /// be resumed separately.
    ///
    /// Forking at the prompt the conversation is currently up to copies the whole branch: that is
    /// the answer a person expects from the row marked `here`, and it keeps the replies that came
    /// after that prompt.
    pub fn fork(&self, session: &str, at: Option<i64>) -> Result<String> {
        let chain = match at {
            None => self.branch(session)?,
            Some(id) => {
                // The chain to the chosen node, so a fork from the middle of a branch is possible.
                let mut out = Vec::new();
                let mut at = Some(id);
                while let Some(id) = at {
                    let node = self.node(id)?;
                    at = node.parent;
                    out.push(node);
                }
                out.reverse();
                out
            }
        };
        let from_node = chain.last().map(|n| n.id);
        let id = session_id();
        let at = now();
        self.conn.execute(
            "INSERT INTO sessions (id, workspace, created, updated, provider, model, from_session, from_node)
             SELECT ?1, workspace, ?2, ?2, provider, model, id, ?3 FROM sessions WHERE id = ?4",
            params![id, at, from_node, session],
        )?;
        let mut parent: Option<i64> = None;
        for node in &chain {
            self.conn.execute(
                "INSERT INTO nodes (session, parent, kind, body, created) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, parent, node.kind, node.body, at],
            )?;
            parent = Some(self.conn.last_insert_rowid());
        }
        self.conn.execute(
            "UPDATE sessions SET leaf = ?2 WHERE id = ?1",
            params![id, parent],
        )?;
        Ok(id)
    }

    /// The first prompt on the branch, trimmed to one line.
    fn first_prompt(&self, session: &str) -> Result<Option<String>> {
        for node in self.branch(session)? {
            if node.kind != kind::ITEM {
                continue;
            }
            if let Ok(Item::User(text)) = serde_json::from_str::<Item>(&node.body) {
                let line = text.lines().next().unwrap_or_default().trim();
                return Ok(Some(line.to_owned()));
            }
        }
        Ok(None)
    }

    /// Imports the JSON Lines files earlier builds wrote, once. A file that fails to parse is left
    /// where it is and reported, rather than deleted: losing a conversation to a migration is the
    /// worst outcome available here.
    fn import_legacy(&self, db: &Path) -> Result<()> {
        let done: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'legacy'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if done.is_some() {
            return Ok(());
        }
        let Some(dir) = db.parent().map(|d| d.join("sessions")) else {
            return Ok(());
        };
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(());
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();
        for file in files {
            match import_one(&self.conn, &file) {
                Ok(()) => {
                    // Renamed, not removed: if the import was wrong the file is still there.
                    let mut moved = file.clone().into_os_string();
                    moved.push(".imported");
                    let _ = fs::rename(&file, PathBuf::from(moved));
                }
                Err(e) => eprintln!("saddle: could not import {}: {e:#}", file.display()),
            }
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('legacy', ?1)",
            params![now()],
        )?;
        Ok(())
    }

    /// Removes a conversation and everything in it. The nodes go with it, because a message
    /// without the session it belongs to is not anything (D55).
    pub fn delete(&self, session: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM nodes WHERE session = ?1", params![session])?;
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![session])?;
        Ok(())
    }

    /// The most recent conversation in this workspace, for `--continue`.
    pub fn latest(&self) -> Result<Option<Summary>> {
        Ok(self.list()?.into_iter().next())
    }
}

/// One stored node.
#[derive(Debug, Clone)]
struct Node {
    id: i64,
    parent: Option<i64>,
    kind: String,
    body: String,
    created: String,
}

/// One message in the session, as the tree shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
    pub node: i64,
    pub parent: Option<i64>,
    pub kind: PointKind,
    /// One line, for the row.
    pub preview: String,
    /// The whole message, for a user message chosen to be edited again. Empty otherwise.
    pub text: String,
    /// Whether the conversation is at this message.
    pub here: bool,
    /// Whether this message is on the active branch, which is what the next request carries.
    pub active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointKind {
    User,
    Assistant,
    Tool,
    Summary,
}

impl PointKind {
    /// The mark in front of a row, so the kind is readable at a glance.
    pub fn mark(self) -> &'static str {
        match self {
            PointKind::User => "❯",
            PointKind::Assistant => "◆",
            PointKind::Tool => "⚙",
            PointKind::Summary => "⋯",
        }
    }
}

/// An id that sorts by time, so a listing is a query and not a scan of every timestamp. The
/// counter keeps two sessions opened in the same millisecond apart, which a fork can easily be.
fn session_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{ms:012x}-{:x}-{n:x}", process::id())
}

/// A conversation read back: its items, and the tokens its replies have already cost.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    pub items: Vec<Item>,
    pub usage: Usage,
    pub cost: f64,
    /// What this branch declared, in the order it was declared (D109).
    pub declared: Vec<String>,
}

/// One saved conversation, as a listing shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    /// What the user named it, if they named it (D55).
    pub name: Option<String>,
    /// The name, or the first prompt's first line.
    pub title: String,
    pub prompts: usize,
    /// Every message on the branch, for `/session` (D55).
    pub messages: usize,
    pub model: String,
    /// The session this one was forked from, if it was.
    pub forked_from: Option<String>,
    pub updated: SystemTime,
}

/// The first line of a message that has anything on it, so a prompt that opens with a blank line is
/// still recognisable in the tree.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// One line saying what a reply did (D54).
///
/// A reply that only calls tools has no text part at all, and in a working session those are the
/// most common rows — one per round trip. Saying `(no text)` for each of them makes the tree a
/// wall of identical rows that say nothing; the tool names say what kind of work it was.
fn assistant_preview(parts: &[Part]) -> String {
    if let Some(text) = parts.iter().find_map(|p| match p {
        Part::Text(t) => Some(t.as_str()),
        _ => None,
    }) {
        let line = first_line(text);
        if !line.is_empty() {
            return line;
        }
    }
    let mut names: Vec<&str> = Vec::new();
    for part in parts {
        if let Part::Call(call) = part
            && !names.contains(&call.name.as_str())
        {
            names.push(&call.name);
        }
    }
    if !names.is_empty() {
        let more = if names.len() > 3 { "…" } else { "" };
        let list = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        return format!("calls {list}{more}");
    }
    // A thinking model can reply with reasoning and nothing else, which is a real state.
    if parts.iter().any(|p| matches!(p, Part::Native { .. })) {
        return "thought, said nothing".to_owned();
    }
    "(empty reply)".to_owned()
}

/// Gives every tool call left without a result one, so a history cut short is still valid to send.
fn repair(items: &mut Vec<Item>) {
    let mut i = 0;
    while i < items.len() {
        if let Item::Assistant(parts) = &items[i] {
            let calls: Vec<String> = parts
                .iter()
                .filter_map(|p| match p {
                    Part::Call(c) => Some(c.id.clone()),
                    _ => None,
                })
                .collect();
            let answered = matches!(items.get(i + 1), Some(Item::ToolResults(_)));
            if !calls.is_empty() && !answered {
                let results = calls
                    .into_iter()
                    .map(|id| ToolResult {
                        provenance: None,
                        id,
                        content: "not run: the session ended before this call ran".into(),
                        is_error: true,
                        diff: None,
                        images: Vec::new(),
                    })
                    .collect();
                items.insert(i + 1, Item::ToolResults(results));
            }
        }
        i += 1;
    }
}

/// The records an older build wrote, so they can be read one last time.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum Legacy {
    Header {
        id: String,
        workspace: PathBuf,
        created: String,
    },
    Turn {
        audit: String,
        provider: String,
        model: String,
    },
    Item {
        item: Item,
    },
    Usage {
        usage: Usage,
        #[serde(default)]
        cost: f64,
    },
    Compaction {
        summary: String,
        kept: usize,
    },
}

/// Reads one JSON Lines session into the database, keeping its id so a `--continue` that mentions
/// it still works.
fn import_one(conn: &Connection, path: &Path) -> Result<()> {
    let file = File::open(path).with_context(|| format!("{}", path.display()))?;
    let mut lines = BufReader::new(file).lines();
    let Some(Ok(first)) = lines.next() else {
        anyhow::bail!("no header");
    };
    let Legacy::Header {
        id,
        workspace,
        created,
    } = serde_json::from_str(&first)?
    else {
        anyhow::bail!("the first line is not a header");
    };
    conn.execute(
        "INSERT OR IGNORE INTO sessions (id, workspace, created, updated)
         VALUES (?1, ?2, ?3, ?3)",
        params![id, workspace.to_string_lossy(), created],
    )?;
    let mut leaf: Option<i64> = None;
    let mut push = |kind: &str, body: String| -> Result<()> {
        conn.execute(
            "INSERT INTO nodes (session, parent, kind, body, created) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, leaf, kind, body, now()],
        )?;
        leaf = Some(conn.last_insert_rowid());
        Ok(())
    };
    for line in lines {
        let line = line?;
        // A torn last line is dropped, the way a load used to skip it.
        match serde_json::from_str::<Legacy>(&line) {
            Ok(Legacy::Item { item }) => push(kind::ITEM, serde_json::to_string(&item)?)?,
            Ok(Legacy::Usage { usage, cost }) => push(
                kind::USAGE,
                serde_json::json!({ "usage": usage, "cost": cost }).to_string(),
            )?,
            Ok(Legacy::Turn {
                audit,
                provider,
                model,
            }) => push(
                kind::TURN,
                serde_json::json!({ "audit": audit, "provider": provider, "model": model })
                    .to_string(),
            )?,
            Ok(Legacy::Compaction { summary, kept }) => push(
                kind::COMPACTION,
                serde_json::json!({ "summary": summary, "kept": kept }).to_string(),
            )?,
            Ok(Legacy::Header { .. }) => {}
            Err(_) => continue,
        }
    }
    conn.execute(
        "UPDATE sessions SET leaf = ?2 WHERE id = ?1",
        params![id, leaf],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Api, Provenance, ToolCall};
    use saddle_cap::Scope;
    use serde_json::json;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    fn store() -> Store {
        Store::in_memory("/ws").unwrap()
    }

    fn conversation(s: &Store, id: &str) {
        s.append(id, &Item::User("fix the build".into())).unwrap();
        s.append(
            id,
            &Item::Assistant(vec![
                Part::Native {
                    api: Api::Openai,
                    block: json!({"reasoning_content": "hmm"}),
                },
                Part::Call(ToolCall {
                    id: "c".into(),
                    name: "bash".into(),
                    input: Err("{bad".into()),
                }),
            ]),
        )
        .unwrap();
        s.request(
            id,
            &Request {
                kind: RequestKind::Reply,
                provider: "synthetic".into(),
                model: "glm".into(),
                usage: Usage {
                    input: 10,
                    output: 2,
                    cache_read: 300,
                    cache_write: 0,
                },
                cost: 0.25,
                dropped: 0,
                reasoning_tokens: 0,
            },
        )
        .unwrap();
    }

    #[test]
    fn a_snapshot_carries_the_reasoning_beside_the_message_it_produced() {
        // D98: the reasoning was kept on the wire (D95, D96) and in the store, and every query
        // dropped it on the floor, so "what did it think before it chose that tool" was an
        // assumption about the transcript rather than something it could be asked.
        let s = store();
        let id = s.begin("synthetic", "glm").unwrap();
        conversation(&s, &id);
        let snapshot = s.snapshot(&id).unwrap();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(
            snapshot.reasoning,
            vec![ReasoningRow {
                // The reply is the branch's second message, and the reasoning is its first part.
                ordinal: 2,
                node: snapshot.messages[1].0,
                part: 0,
                text: "hmm".into(),
            }]
        );
    }

    #[test]
    fn plugin_provenance_and_wrappers_survive_fork_and_rewind_without_rewriting() {
        let store = store();
        let id = store.begin("p", "m").unwrap();
        let items = vec![
            Item::User("read a file".into()),
            Item::Assistant(vec![Part::Call(ToolCall {
                id: "c".into(),
                name: "plugin__reader__reader".into(),
                input: Ok(json!({})),
            })]),
            Item::ToolResults(vec![ToolResult {
                id: "c".into(),
                content: "<plugin-output-original plugin=\"reader\">data</plugin-output-original>"
                    .into(),
                is_error: false,
                images: Vec::new(),
                diff: None,
                provenance: Some(Box::new(Provenance {
                    principal: "plugin.reader".into(),
                    scope: Scope::Tool,
                    call_id: "host-call-1".into(),
                    component_sha256: "0".repeat(64),
                    version: "0.1.0".into(),
                })),
            }]),
        ];
        for item in &items {
            store.append(&id, item).unwrap();
        }
        let point = store.current(&id).unwrap();
        let fork = store.fork(&id, point).unwrap();
        assert_eq!(store.load(&fork).unwrap().items, items);
        store.append(&id, &Item::User("later".into())).unwrap();
        store.rewind(&id, point).unwrap();
        assert_eq!(store.load(&id).unwrap().items, items);
    }

    #[test]
    fn a_conversation_round_trips_with_its_blocks_usage_and_cost() {
        let s = store();
        let id = s.begin("synthetic", "glm").unwrap();
        conversation(&s, &id);
        let loaded = s.load(&id).unwrap();
        // The call in the reply had no result stored, so one is filled in on the way (D31).
        assert_eq!(loaded.items.len(), 3);
        assert!(matches!(&loaded.items[0], Item::User(t) if t == "fix the build"));
        assert!(matches!(&loaded.items[1], Item::Assistant(p) if p.len() == 2));
        assert!(matches!(&loaded.items[2], Item::ToolResults(r) if r[0].is_error));
        assert_eq!(loaded.usage.cache_read, 300);
        assert!((loaded.cost - 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_request_is_read_back_with_what_answered_it_and_which_message_it_followed() {
        // D94: the numbers were kept per turn and thrown away from view, which is why a poor cache
        // hit rate could not be diagnosed. A request now says what it used, which provider and
        // model answered, and where in the conversation it landed.
        let s = store();
        let id = s.begin("synthetic", "glm").unwrap();
        s.append(&id, &Item::User("first".into())).unwrap();
        s.append(&id, &Item::Assistant(vec![Part::Text("hi".into())]))
            .unwrap();
        s.request(
            &id,
            &Request {
                kind: RequestKind::Reply,
                provider: "synthetic".into(),
                model: "glm".into(),
                usage: Usage {
                    input: 1_000,
                    output: 20,
                    cache_read: 900,
                    cache_write: 0,
                },
                cost: 0.02,
                dropped: 3,
                reasoning_tokens: 0,
            },
        )
        .unwrap();
        s.append(&id, &Item::User("second".into())).unwrap();
        s.request(
            &id,
            &Request {
                kind: RequestKind::Summary,
                provider: "openai".into(),
                model: "gpt".into(),
                usage: Usage {
                    input: 200,
                    output: 30,
                    cache_read: 0,
                    cache_write: 200,
                },
                cost: 0.01,
                dropped: 0,
                reasoning_tokens: 0,
            },
        )
        .unwrap();

        let snapshot = s.snapshot(&id).unwrap();
        let rows = &snapshot.requests;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].provider, "synthetic");
        assert_eq!(rows[0].model, "glm");
        assert_eq!(rows[0].kind, "reply");
        assert_eq!(rows[0].usage.cache_read, 900);
        assert!((rows[0].cost - 0.02).abs() < 1e-9);
        // What the provider said it removed from the prompt travels with the row (D97).
        assert_eq!(rows[0].dropped, 3);
        // The second request follows the third message, and a compaction is recorded as one.
        assert_eq!(rows[0].after, 2);
        assert_eq!(rows[1].after, 3);
        assert_eq!(rows[1].kind, "summary");
        assert_eq!(rows[1].usage.cache_write, 200);
        assert!(!rows[0].at.is_empty(), "a request is dated: {}", rows[0].at);
    }

    #[test]
    fn a_request_recorded_before_the_model_was_kept_still_reads() {
        // Rows written by an older build (and imported from the JSON Lines files) have only what
        // they cost. They are reported as unknown rather than dropped or guessed at: the cost is
        // still true, and inventing a provider would make a hit rate lie.
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("hello".into())).unwrap();
        // Written as the older build wrote it: a usage row, with no provider, model or kind.
        s.request(
            &id,
            &Request {
                kind: RequestKind::Summary,
                provider: "openai".into(),
                model: "gpt".into(),
                usage: Usage::default(),
                cost: 0.0,
                dropped: 0,
                reasoning_tokens: 0,
            },
        )
        .unwrap();
        let node = s.current(&id).unwrap().unwrap();
        s.conn
            .execute(
                "UPDATE nodes SET body = ?2 WHERE id = ?1",
                params![
                    node,
                    json!({"usage": {"input": 5, "output": 1, "cache_read": 0, "cache_write": 0},
                           "cost": 0.5})
                    .to_string()
                ],
            )
            .unwrap();
        let snapshot = s.snapshot(&id).unwrap();
        assert_eq!(snapshot.requests.len(), 1);
        let row = &snapshot.requests[0];
        assert_eq!(row.kind, "unknown");
        assert_eq!(row.provider, "");
        assert_eq!(row.model, "");
        assert_eq!(row.usage.input, 5);
        assert_eq!(row.reasoning_tokens, 0);
        assert!((row.cost - 0.5).abs() < 1e-9);
    }

    #[test]
    fn a_request_keeps_the_split_of_output_that_was_thinking() {
        // D99: the reply's own count, kept with the request it belongs to rather than inside
        // `Usage`, whose fields add up to the bill — this one is part of `output`.
        let s = store();
        let id = s.begin("openai", "gpt").unwrap();
        s.append(&id, &Item::User("hello".into())).unwrap();
        s.request(
            &id,
            &Request {
                kind: RequestKind::Reply,
                provider: "openai".into(),
                model: "gpt".into(),
                usage: Usage {
                    input: 10,
                    output: 900,
                    cache_read: 0,
                    cache_write: 0,
                },
                cost: 0.1,
                dropped: 0,
                reasoning_tokens: 850,
            },
        )
        .unwrap();
        let snapshot = s.snapshot(&id).unwrap();
        assert_eq!(snapshot.requests[0].reasoning_tokens, 850);
        assert_eq!(snapshot.requests[0].usage.output, 900);
    }

    #[test]
    fn a_tool_call_without_a_result_is_repaired_on_load() {
        // The process died between the reply and the call: the history must still be valid.
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("go".into())).unwrap();
        s.append(
            &id,
            &Item::Assistant(vec![Part::Call(ToolCall {
                id: "c1".into(),
                name: "ls".into(),
                input: Ok(json!({})),
            })]),
        )
        .unwrap();
        let loaded = s.load(&id).unwrap();
        assert!(
            matches!(&loaded.items[2], Item::ToolResults(r) if r[0].id == "c1" && r[0].is_error)
        );
    }

    #[test]
    fn a_compaction_replaces_the_items_before_it() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("one".into())).unwrap();
        s.append(&id, &Item::User("two".into())).unwrap();
        s.compaction(&id, "we did both", 1).unwrap();
        s.append(&id, &Item::User("three".into())).unwrap();
        assert_eq!(
            s.load(&id).unwrap().items,
            [
                Item::Summary("we did both".into()),
                Item::User("two".into()),
                Item::User("three".into()),
            ]
        );
    }

    #[test]
    fn a_fork_copies_the_branch_up_to_the_chosen_prompt_and_stands_alone() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("first".into())).unwrap();
        s.append(&id, &Item::User("second".into())).unwrap();
        s.append(&id, &Item::User("third".into())).unwrap();
        let nodes = entries(&s, &id);
        assert_eq!(nodes.len(), 3);

        // Fork at the second prompt: the new conversation has the first two and nothing after.
        let fork = s.fork(&id, Some(nodes[1])).unwrap();
        assert_eq!(
            s.load(&fork).unwrap().items,
            [Item::User("first".into()), Item::User("second".into())]
        );
        // Copying, not sharing: appending to the fork leaves the original alone.
        s.append(&fork, &Item::User("another way".into())).unwrap();
        assert_eq!(s.load(&id).unwrap().items.len(), 3);
        assert_eq!(s.load(&fork).unwrap().items.len(), 3);

        let listed = s.list().unwrap();
        let from = listed.iter().find(|x| x.id == fork).unwrap();
        assert_eq!(from.forked_from.as_deref(), Some(id.as_str()));
        assert_eq!(from.title, "first");
    }

    #[test]
    fn query_history_keeps_compacted_text_but_not_other_branches_or_sessions() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("original secret word".into()))
            .unwrap();
        let first = s.current(&id).unwrap();
        s.append(&id, &Item::User("abandoned".into())).unwrap();
        s.rewind(&id, first).unwrap();
        s.compaction(&id, "short summary", 0).unwrap();
        s.append(&id, &Item::User("current".into())).unwrap();
        let other = s.begin("p", "m").unwrap();
        s.append(&other, &Item::User("other conversation".into()))
            .unwrap();
        let text: Vec<_> = s
            .snapshot(&id)
            .unwrap()
            .messages
            .into_iter()
            .map(|(_, _, text)| text)
            .collect();
        assert_eq!(text, ["original secret word", "short summary", "current"]);
    }

    /// The node ids of the rows that are messages, in order.
    fn entries(s: &Store, id: &str) -> Vec<i64> {
        s.rows(id).unwrap().into_iter().map(|p| p.node).collect()
    }

    #[test]
    fn the_tree_shows_every_message_and_marks_the_one_the_conversation_is_at() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("fix the build".into())).unwrap();
        s.append(
            &id,
            &Item::Assistant(vec![Part::Text("looking at it".into())]),
        )
        .unwrap();
        s.append(
            &id,
            &Item::ToolResults(vec![ToolResult {
                provenance: None,
                id: "c1".into(),
                content: "ok".into(),
                images: Vec::new(),
                is_error: false,
                diff: None,
            }]),
        )
        .unwrap();
        s.append(&id, &Item::User("now run it".into())).unwrap();
        s.append(&id, &Item::Assistant(vec![Part::Text("done".into())]))
            .unwrap();

        let points = s.rows(&id).unwrap();
        let points: Vec<&Point> = points.iter().collect();
        // The turn marker and the usage record are bookkeeping and are not shown (D54).
        assert_eq!(points.len(), 5);
        let kinds: Vec<PointKind> = points.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            [
                PointKind::User,
                PointKind::Assistant,
                PointKind::Tool,
                PointKind::User,
                PointKind::Assistant
            ]
        );
        // The leaf is the last reply, so that is the marked one.
        assert_eq!(points.iter().filter(|p| p.here).count(), 1);
        assert!(points[4].here);
        // A prompt carries its whole text, for the editor; a reply carries only a preview.
        assert_eq!(points[0].text, "fix the build");
        assert_eq!(points[1].preview, "looking at it");
        assert_eq!(points[1].text, "");
    }

    #[test]
    fn a_reply_that_only_calls_tools_says_what_it_called() {
        // These are the most common rows in a working session: one per tool round trip, with no
        // text part at all. `(no text)` for each of them is a wall of rows saying nothing (D54).
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("build it".into())).unwrap();
        s.append(
            &id,
            &Item::Assistant(vec![
                Part::Native {
                    api: Api::Openai,
                    block: json!({"reasoning_content": "hmm"}),
                },
                Part::Call(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: Ok(json!({"path": "Cargo.toml"})),
                }),
            ]),
        )
        .unwrap();
        s.append(
            &id,
            &Item::ToolResults(vec![
                ToolResult {
                    provenance: None,
                    id: "c1".into(),
                    content: "ok".into(),
                    images: Vec::new(),
                    is_error: false,
                    diff: None,
                },
                ToolResult {
                    provenance: None,
                    id: "c2".into(),
                    content: "denied".into(),
                    images: Vec::new(),
                    is_error: true,
                    diff: None,
                },
            ]),
        )
        .unwrap();
        s.append(
            &id,
            &Item::Assistant(vec![Part::Call(ToolCall {
                id: "c3".into(),
                name: "read".into(),
                input: Ok(json!({})),
            })]),
        )
        .unwrap();
        s.append(
            &id,
            &Item::Assistant(vec![
                Part::Call(ToolCall {
                    id: "c4".into(),
                    name: "bash".into(),
                    input: Ok(json!({})),
                }),
                Part::Call(ToolCall {
                    id: "c5".into(),
                    name: "edit".into(),
                    input: Ok(json!({})),
                }),
                Part::Call(ToolCall {
                    id: "c6".into(),
                    name: "write".into(),
                    input: Ok(json!({})),
                }),
                Part::Call(ToolCall {
                    id: "c7".into(),
                    name: "grep".into(),
                    input: Ok(json!({})),
                }),
            ]),
        )
        .unwrap();
        // A thinking model that answered with reasoning and nothing else.
        s.append(
            &id,
            &Item::Assistant(vec![Part::Native {
                api: Api::Openai,
                block: json!({"reasoning_content": "still thinking"}),
            }]),
        )
        .unwrap();

        let previews: Vec<String> = s
            .rows(&id)
            .unwrap()
            .into_iter()
            .map(|p| p.preview)
            .collect();
        assert_eq!(
            previews,
            [
                "build it",
                "calls read",
                "2 tool results, 1 failed",
                "calls read",
                "calls bash, edit, write…",
                "thought, said nothing",
            ]
        );
        // Not one of them says "no text".
        assert!(
            !previews.iter().any(|p| p.contains("no text")),
            "{previews:?}"
        );
    }

    #[test]
    fn a_reply_with_text_shows_it_and_skips_a_leading_blank_line() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(
            &id,
            &Item::User("\n\n  the real prompt  \nsecond line".into()),
        )
        .unwrap();
        s.append(
            &id,
            &Item::Assistant(vec![
                Part::Text("\n  Here is what I found.  \nMore detail.".into()),
                Part::Call(ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    input: Ok(json!({})),
                }),
            ]),
        )
        .unwrap();
        let rows = s.rows(&id).unwrap();
        assert_eq!(rows[0].preview, "the real prompt");
        // Text wins over the calls: the calls are still there, the sentence says more.
        assert_eq!(rows[1].preview, "Here is what I found.");
        // The whole prompt is kept for the editor, blank lines and all.
        assert_eq!(rows[0].text, "\n\n  the real prompt  \nsecond line");
    }

    #[test]
    fn a_declaration_is_recorded_on_the_branch_and_rewinds_with_it() {
        // D109: what a search declared is branch state, so a resume brings it back, a fork carries
        // it, and a jump to before it forgets it — the conversation itself never changes.
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("find me a tool".into())).unwrap();
        s.declare(&id, &["mcp__fake__search".to_owned()]).unwrap();
        assert_eq!(
            s.load(&id).unwrap().declared,
            ["mcp__fake__search".to_owned()],
            "a resume puts back what the branch declared"
        );
        assert_eq!(
            s.load(&id).unwrap().items.len(),
            1,
            "and it is not a message: replaying it costs nothing"
        );

        // A point in the tree is a place a conversation can continue from, so a declaration is
        // not one.
        assert!(
            s.rows(&id)
                .unwrap()
                .iter()
                .all(|p| p.preview != "declared" && !p.preview.is_empty()),
            "a declaration does not appear as a node a person could pick"
        );

        // Forking copies the branch, declaration included.
        let nodes = entries(&s, &id);
        let fork = s.fork(&id, Some(nodes[0])).unwrap();
        assert!(
            s.load(&fork).unwrap().declared.is_empty(),
            "the fork was taken before it"
        );

        // Rewinding to before the declaration forgets it, without touching the conversation.
        s.rewind(&id, Some(nodes[0])).unwrap();
        assert!(s.load(&id).unwrap().declared.is_empty());
        assert_eq!(
            s.load(&id).unwrap().items,
            [Item::User("find me a tool".into())]
        );
    }

    #[test]
    fn rewinding_keeps_the_branch_it_left_and_the_row_says_so() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("one".into())).unwrap();
        s.append(&id, &Item::User("two".into())).unwrap();
        let nodes = entries(&s, &id);

        // Go back to before the second prompt, the way `/tree` does, and send something else.
        s.rewind(&id, Some(nodes[0])).unwrap();
        assert_eq!(s.load(&id).unwrap().items, [Item::User("one".into())]);
        s.append(&id, &Item::User("two, differently".into()))
            .unwrap();

        // The branch that was left is still there and still listed, marked as not active (D54).
        let rows = s.rows(&id).unwrap();
        assert_eq!(rows.len(), 3);
        let left = rows
            .iter()
            .find(|p| p.preview == "two")
            .expect("the branch that was left is still in the tree");
        assert!(
            !left.active,
            "it is not on the branch the next request carries"
        );
        assert!(rows.iter().any(|p| p.preview == "two, differently"));

        // Choosing it puts the conversation back on the old branch.
        s.rewind(&id, Some(left.node)).unwrap();
        assert_eq!(
            s.load(&id).unwrap().items,
            [Item::User("one".into()), Item::User("two".into())]
        );
    }

    #[test]
    fn rewinding_to_nothing_empties_the_history_without_deleting_it() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("one".into())).unwrap();
        s.append(&id, &Item::User("two".into())).unwrap();
        s.rewind(&id, None).unwrap();
        assert!(s.load(&id).unwrap().items.is_empty());
        // The rows still show both messages, because a rewound conversation can be walked forward
        // again from the tree.
        let rows = s.rows(&id).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!rows.iter().any(|p| p.here));
    }

    #[test]
    fn a_model_change_is_what_the_conversation_is_read_back_as_using() {
        // D91: the row a resumed conversation is read from says which model it was last working in,
        // so a default changed since does not follow it there.
        let s = store();
        let id = s.begin("synthetic", "begun-on").unwrap();
        assert_eq!(
            s.model_of(&id).unwrap(),
            Some(Model {
                provider: "synthetic".into(),
                model: "begun-on".into(),
            })
        );
        s.append(&id, &Item::User("a prompt".into())).unwrap();
        s.model_change(&id, "openai", "moved-to").unwrap();
        assert_eq!(
            s.model_of(&id).unwrap().map(|m| m.model),
            Some("moved-to".into())
        );
        assert_eq!(
            s.model_of(&id).unwrap().map(|m| m.provider),
            Some("openai".into())
        );
        // The resume list shows the same thing, because it reads the same row.
        assert_eq!(s.list().unwrap()[0].model, "moved-to");
        // A conversation nothing is known about has no model rather than an empty one.
        assert_eq!(s.model_of("no-such-session").unwrap(), None);
    }

    #[test]
    fn listing_is_per_workspace_newest_first_and_skips_an_empty_session() {
        let s = store();
        let a = s.begin("p", "m").unwrap();
        s.append(&a, &Item::User("first task\nwith detail".into()))
            .unwrap();
        s.append(&a, &Item::User("second".into())).unwrap();
        let empty = s.begin("p", "m").unwrap();
        // The ids sort by time; make the order unambiguous.
        thread::sleep(Duration::from_millis(5));
        let b = s.begin("p", "m").unwrap();
        s.append(&b, &Item::User("later task".into())).unwrap();

        let listed = s.list().unwrap();
        let titles: Vec<&str> = listed.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, ["later task", "first task"]);
        assert_eq!(listed[1].prompts, 2);
        assert!(!listed.iter().any(|x| x.id == empty));
        assert_eq!(s.latest().unwrap().unwrap().id, b);
    }

    #[test]
    fn a_name_replaces_the_first_prompt_and_clearing_it_brings_the_prompt_back() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("the actual first prompt".into()))
            .unwrap();
        assert_eq!(s.list().unwrap()[0].title, "the actual first prompt");
        assert_eq!(s.list().unwrap()[0].name, None);

        s.name(&id, "the sandbox refactor").unwrap();
        let listed = &s.list().unwrap()[0];
        assert_eq!(listed.name.as_deref(), Some("the sandbox refactor"));
        // The title is what a picker shows, and a name wins over the prompt (D55).
        assert_eq!(listed.title, "the sandbox refactor");

        // `/name` with no argument clears it, and the prompt is the title again.
        s.name(&id, "").unwrap();
        assert_eq!(s.list().unwrap()[0].name, None);
        assert_eq!(s.list().unwrap()[0].title, "the actual first prompt");
        // Whitespace is not a name.
        s.name(&id, "   ").unwrap();
        assert_eq!(s.list().unwrap()[0].name, None);
    }

    #[test]
    fn deleting_a_conversation_takes_its_messages_with_it() {
        let s = store();
        let a = s.begin("p", "m").unwrap();
        s.append(&a, &Item::User("keep me".into())).unwrap();
        let b = s.begin("p", "m").unwrap();
        s.append(&b, &Item::User("delete me".into())).unwrap();

        s.delete(&b).unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, a);
        // Nothing is left pointing at the deleted session.
        let orphans: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE session = ?1",
                params![b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0);
        // And the other conversation is untouched.
        assert_eq!(s.load(&a).unwrap().items, [Item::User("keep me".into())]);
    }

    #[test]
    fn a_database_from_the_previous_schema_gains_the_name_column() {
        // The version before names existed: the file on disk is already there and must open (D55).
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sessions.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, workspace TEXT NOT NULL, created TEXT NOT NULL,
                     updated TEXT NOT NULL, provider TEXT NOT NULL DEFAULT '',
                     model TEXT NOT NULL DEFAULT '', leaf INTEGER,
                     from_session TEXT, from_node INTEGER);
                 CREATE TABLE nodes (
                     id INTEGER PRIMARY KEY AUTOINCREMENT, session TEXT NOT NULL,
                     parent INTEGER, kind TEXT NOT NULL, body TEXT NOT NULL,
                     created TEXT NOT NULL);
                 CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 PRAGMA user_version = 1;
                 INSERT INTO sessions (id, workspace, created, updated)
                   VALUES ('old', '/ws', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
                 INSERT INTO nodes (session, parent, kind, body, created)
                   VALUES ('old', NULL, 'item', '{\"user\":\"a prompt\"}', '2026-01-01T00:00:00Z');
                 INSERT INTO sessions (id, workspace, created, updated)
                   VALUES ('old2', '/ws', '2026-01-02T00:00:00Z', '2026-01-02T00:00:00Z');
                 INSERT INTO nodes (session, parent, kind, body, created)
                   VALUES ('old2', NULL, 'item', '{\"user\":\"another\"}', '2026-01-02T00:00:00Z');
                 UPDATE sessions SET leaf = (SELECT MIN(id) FROM nodes) WHERE id = 'old';
                 UPDATE sessions SET leaf = (SELECT MAX(id) FROM nodes) WHERE id = 'old2';",
            )
            .unwrap();
        }
        let s = Store::open(&db, PathBuf::from("/ws")).unwrap();
        // The old conversations are there, and naming one now works.
        assert_eq!(
            s.load("old").unwrap().items,
            [Item::User("a prompt".into())]
        );
        s.name("old", "named after the fact").unwrap();
        let listed = s.list().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(
            listed
                .iter()
                .any(|x| x.name.as_deref() == Some("named after the fact"))
        );
        // Opening it again does not try to add the column twice.
        drop(s);
        let again = Store::open(&db, PathBuf::from("/ws")).unwrap();
        assert_eq!(again.list().unwrap().len(), 2);
    }

    #[test]
    fn the_legacy_files_are_imported_once_and_kept_under_a_new_name() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("sessions");
        fs::create_dir_all(&legacy).unwrap();
        let id = "000000000001-1";
        let mut text = String::new();
        text.push_str(
            &json!({"type":"header","version":1,"id":id,
            "workspace":"/ws","created":"2026-01-01T00:00:00Z"})
            .to_string(),
        );
        text.push('\n');
        text.push_str(&json!({"type":"item","item":{"user":"old task"}}).to_string());
        text.push('\n');
        text.push_str(
            &json!({"type":"usage",
            "usage":{"input":1,"output":2,"cache_read":3,"cache_write":0},"cost":0.5})
            .to_string(),
        );
        text.push('\n');
        // A torn last line, as an old crash left it.
        text.push_str("{\"type\":\"item\",\"item\":{\"us");
        fs::write(legacy.join(format!("{id}.jsonl")), text).unwrap();

        let db = dir.path().join("sessions.db");
        let s = Store::open(&db, PathBuf::from("/ws")).unwrap();
        let loaded = s.load(id).unwrap();
        assert_eq!(loaded.items, [Item::User("old task".into())]);
        assert_eq!(loaded.usage.cache_read, 3);
        assert!((loaded.cost - 0.5).abs() < 1e-9);
        // The file is renamed, not deleted, and a second open does not import it twice.
        assert!(legacy.join(format!("{id}.jsonl.imported")).exists());
        assert!(!legacy.join(format!("{id}.jsonl")).exists());
        let again = Store::open(&db, PathBuf::from("/ws")).unwrap();
        assert_eq!(again.load(id).unwrap().items.len(), 1);
        assert_eq!(again.list().unwrap().len(), 1);
    }
}
