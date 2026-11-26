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

mod kind {
    pub const ITEM: &str = "item";
    pub const USAGE: &str = "usage";
    pub const TURN: &str = "turn";
    pub const COMPACTION: &str = "compaction";
    pub const DECLARED: &str = "declared";
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub kind: RequestKind,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub cost: f64,
    pub dropped: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Reply,
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

#[derive(Debug, Clone, PartialEq)]
pub struct RequestRow {
    pub node: i64,
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub cost: f64,
    pub dropped: u64,
    pub reasoning_tokens: u64,
    pub at: String,
    pub after: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningRow {
    pub ordinal: usize,
    pub node: i64,
    pub part: usize,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub messages: Vec<(i64, String, String)>,
    pub calls: Vec<CompletedCall>,
    pub requests: Vec<RequestRow>,
    pub reasoning: Vec<ReasoningRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletedCall {
    pub node: i64,
    pub part: usize,
    pub name: String,
    pub arguments: String,
    pub principal: String,
    pub ok: bool,
}

const VERSION: u32 = 3;

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
        reasoning_tokens: body["reasoning_tokens"].as_u64().unwrap_or(0),
        at: node.created.clone(),
        after,
    }
}

fn now() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

pub struct Store {
    conn: Connection,
    pub workspace: PathBuf,
}

impl Store {
    pub fn open(path: &Path, workspace: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
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
        if found < 2 {
            conn.execute_batch("ALTER TABLE sessions ADD COLUMN name TEXT")?;
        }
        if found < 3 {
            conn.execute_batch(
                "ALTER TABLE sessions ADD COLUMN worker INTEGER NOT NULL DEFAULT 0",
            )?;
        }
        conn.pragma_update(None, "user_version", VERSION)?;
        let store = Self { conn, workspace };
        store.import_legacy(path)?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn in_memory(workspace: &str) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id TEXT PRIMARY KEY, workspace TEXT NOT NULL, created TEXT NOT NULL,
                 updated TEXT NOT NULL, provider TEXT NOT NULL DEFAULT '',
                 model TEXT NOT NULL DEFAULT '', leaf INTEGER,
                 from_session TEXT, from_node INTEGER, name TEXT,
                 worker INTEGER NOT NULL DEFAULT 0);
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

    pub fn begin(&self, provider: &str, model: &str) -> Result<String> {
        self.create(provider, model, false)
    }

    pub fn begin_worker(&self, provider: &str, model: &str) -> Result<String> {
        self.create(provider, model, true)
    }

    fn create(&self, provider: &str, model: &str, worker: bool) -> Result<String> {
        let id = session_id();
        let at = now();
        self.conn.execute(
            "INSERT INTO sessions (id, workspace, created, updated, provider, model, worker)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6)",
            params![
                id,
                self.workspace.to_string_lossy(),
                at,
                provider,
                model,
                i64::from(worker)
            ],
        )?;
        Ok(id)
    }

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

    pub fn audit_of(&self, session: &str) -> Result<Option<String>> {
        let body: Option<String> = self
            .conn
            .query_row(
                "SELECT body FROM nodes WHERE session = ?1 AND kind = ?2 ORDER BY id LIMIT 1",
                params![session, kind::TURN],
                |r| r.get(0),
            )
            .optional()?;
        let Some(body) = body else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_str(&body)?;
        Ok(value["audit"].as_str().map(str::to_owned))
    }

    pub fn mark_worker(&self, session: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET worker = 1 WHERE id = ?1",
            params![session],
        )?;
        Ok(())
    }

    pub fn adopt_workers(&self, audit_dir: &Path, sessions: &mut [Summary]) -> Result<usize> {
        let mut marked = 0;
        for summary in sessions.iter_mut().filter(|summary| !summary.worker) {
            let Some(audit) = self.audit_of(&summary.id)? else {
                continue;
            };
            let path = audit_dir.join(format!("{audit}.jsonl"));
            let Ok(file) = File::open(&path) else {
                continue;
            };
            let mut first = String::new();
            if BufReader::new(file).read_line(&mut first).is_err() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
                continue;
            };
            if record["args"].get("worker").is_some() {
                self.mark_worker(&summary.id)?;
                summary.worker = true;
                marked += 1;
            }
        }
        Ok(marked)
    }

    pub fn model_change(&self, session: &str, provider: &str, model: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET provider = ?2, model = ?3, updated = ?4 WHERE id = ?1",
            params![session, provider, model, now()],
        )?;
        Ok(())
    }

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

    pub fn request(&self, session: &str, request: &Request) -> Result<()> {
        self.request_attributed(session, request, None)
    }

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

    fn branch_to(&self, leaf: Option<i64>) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        if leaf.is_none() {
            return Ok(out);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT id, parent, kind, body, created FROM nodes WHERE id = ?1")?;
        let mut at = leaf;
        while let Some(id) = at {
            let node = stmt.query_row(params![id], |r| {
                Ok(Node {
                    id: r.get(0)?,
                    parent: r.get(1)?,
                    kind: r.get(2)?,
                    body: r.get(3)?,
                    created: r.get(4)?,
                })
            })?;
            at = node.parent;
            out.push(node);
        }
        out.reverse();
        Ok(out)
    }

    fn branch(&self, session: &str) -> Result<Vec<Node>> {
        self.branch_to(self.leaf(session)?)
    }

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

    pub fn current(&self, session: &str) -> Result<Option<i64>> {
        self.leaf(session)
    }

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

    pub fn load(&self, session: &str) -> Result<Loaded> {
        let mut items = Vec::new();
        let mut usage = Usage::default();
        let mut cost = 0.0;
        let mut declared = Vec::new();
        for node in self.branch(session)? {
            match node.kind.as_str() {
                kind::ITEM => match serde_json::from_str::<Item>(&node.body) {
                    Ok(item) => items.push(item),
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

    pub fn list(&self) -> Result<Vec<Summary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, updated, model, from_session, name, worker,
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
                r.get::<_, i64>(7)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, updated, model, from, name, worker, prompts, messages) = row?;
            let Some(first) = self.first_prompt(&id)? else {
                continue;
            };
            let title = name.clone().unwrap_or(first);
            out.push(Summary {
                id,
                name,
                title,
                prompts: prompts.max(0) as usize,
                messages: messages.max(0) as usize,
                model,
                forked_from: from,
                worker: worker != 0,
                updated: humantime::parse_rfc3339(&updated).unwrap_or(UNIX_EPOCH),
            });
        }
        Ok(out)
    }

    pub fn rows(&self, session: &str) -> Result<Vec<Point>> {
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

    pub fn rewind(&self, session: &str, node: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET leaf = ?2, updated = ?3 WHERE id = ?1",
            params![session, node, now()],
        )?;
        Ok(())
    }

    pub fn fork(&self, session: &str, at: Option<i64>) -> Result<String> {
        let chain = match at {
            None => self.branch(session)?,
            Some(id) => self.branch_to(Some(id))?,
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

    pub fn delete(&self, session: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM nodes WHERE session = ?1", params![session])?;
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![session])?;
        Ok(())
    }

    pub fn latest(&self) -> Result<Option<Summary>> {
        Ok(self.list()?.into_iter().next())
    }
}

#[derive(Debug, Clone)]
struct Node {
    id: i64,
    parent: Option<i64>,
    kind: String,
    body: String,
    created: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Point {
    pub node: i64,
    pub parent: Option<i64>,
    pub kind: PointKind,
    pub preview: String,
    pub text: String,
    pub here: bool,
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
    pub fn mark(self) -> &'static str {
        match self {
            PointKind::User => "❯",
            PointKind::Assistant => "◆",
            PointKind::Tool => "⚙",
            PointKind::Summary => "⋯",
        }
    }
}

fn session_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("{ms:012x}-{:x}-{n:x}", process::id())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Loaded {
    pub items: Vec<Item>,
    pub usage: Usage,
    pub cost: f64,
    pub declared: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    pub name: Option<String>,
    pub title: String,
    pub prompts: usize,
    pub messages: usize,
    pub model: String,
    pub forked_from: Option<String>,
    pub worker: bool,
    pub updated: SystemTime,
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned()
}

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
    if parts.iter().any(|p| matches!(p, Part::Native { .. })) {
        return "thought, said nothing".to_owned();
    }
    "(empty reply)".to_owned()
}

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
        let s = store();
        let id = s.begin("synthetic", "glm").unwrap();
        conversation(&s, &id);
        let snapshot = s.snapshot(&id).unwrap();
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(
            snapshot.reasoning,
            vec![ReasoningRow {
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
        assert_eq!(loaded.items.len(), 3);
        assert!(matches!(&loaded.items[0], Item::User(t) if t == "fix the build"));
        assert!(matches!(&loaded.items[1], Item::Assistant(p) if p.len() == 2));
        assert!(matches!(&loaded.items[2], Item::ToolResults(r) if r[0].is_error));
        assert_eq!(loaded.usage.cache_read, 300);
        assert!((loaded.cost - 0.25).abs() < 1e-9);
    }

    #[test]
    fn a_request_is_read_back_with_what_answered_it_and_which_message_it_followed() {
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
        assert_eq!(rows[0].dropped, 3);
        assert_eq!(rows[0].after, 2);
        assert_eq!(rows[1].after, 3);
        assert_eq!(rows[1].kind, "summary");
        assert_eq!(rows[1].usage.cache_write, 200);
        assert!(!rows[0].at.is_empty(), "a request is dated: {}", rows[0].at);
    }

    #[test]
    fn a_request_recorded_before_the_model_was_kept_still_reads() {
        let s = store();
        let id = s.begin("p", "m").unwrap();
        s.append(&id, &Item::User("hello".into())).unwrap();
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

        let fork = s.fork(&id, Some(nodes[1])).unwrap();
        assert_eq!(
            s.load(&fork).unwrap().items,
            [Item::User("first".into()), Item::User("second".into())]
        );
        s.append(&fork, &Item::User("another way".into())).unwrap();
        assert_eq!(s.load(&id).unwrap().items.len(), 3);
        assert_eq!(s.load(&fork).unwrap().items.len(), 3);

        let listed = s.list().unwrap();
        let from = listed.iter().find(|x| x.id == fork).unwrap();
        assert_eq!(from.forked_from.as_deref(), Some(id.as_str()));
        assert_eq!(from.title, "first");
    }

    #[test]
    fn an_older_worker_conversation_is_recognised_by_its_audit_log() {
        let s = store();
        let mine = s.begin("p", "m").unwrap();
        s.append(&mine, &Item::User("mine".into())).unwrap();
        s.turn(&mine, "audit-mine", "p", "m").unwrap();
        let theirs = s.begin("p", "m").unwrap();
        s.append(&theirs, &Item::User("the brief".into())).unwrap();
        s.turn(&theirs, "audit-worker", "p", "m").unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("audit-worker.jsonl"),
            "{\"args\":{\"worker\":\"runtime\"},\"kind\":\"session-start\"}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("audit-mine.jsonl"),
            "{\"args\":{\"workspace\":\"/ws\"},\"kind\":\"session-start\"}\n",
        )
        .unwrap();

        let mut listed = s.list().unwrap();
        assert_eq!(s.adopt_workers(dir.path(), &mut listed).unwrap(), 1);
        assert!(listed.iter().find(|x| x.id == theirs).unwrap().worker);
        assert!(!listed.iter().find(|x| x.id == mine).unwrap().worker);
        assert!(
            s.list()
                .unwrap()
                .iter()
                .find(|x| x.id == theirs)
                .unwrap()
                .worker
        );
        let mut again = s.list().unwrap();
        assert_eq!(s.adopt_workers(dir.path(), &mut again).unwrap(), 0);
    }

    #[test]
    fn a_worker_conversation_is_saved_but_not_offered_for_resume() {
        let s = store();
        let mine = s.begin("p", "m").unwrap();
        s.append(&mine, &Item::User("my own question".into()))
            .unwrap();
        let worker = s.begin_worker("p", "m").unwrap();
        s.append(&worker, &Item::User("the brief".into())).unwrap();
        s.name(&worker, "runtime").unwrap();

        let listed = s.list().unwrap();
        assert!(!listed.iter().find(|x| x.id == mine).unwrap().worker);
        let kept = listed.iter().find(|x| x.id == worker).unwrap();
        assert!(kept.worker);
        assert_eq!(kept.name.as_deref(), Some("runtime"));
        assert_eq!(s.load(&worker).unwrap().items.len(), 1);
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
        assert_eq!(points.iter().filter(|p| p.here).count(), 1);
        assert!(points[4].here);
        assert_eq!(points[0].text, "fix the build");
        assert_eq!(points[1].preview, "looking at it");
        assert_eq!(points[1].text, "");
    }

    #[test]
    fn a_reply_that_only_calls_tools_says_what_it_called() {
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
        assert_eq!(rows[1].preview, "Here is what I found.");
        assert_eq!(rows[0].text, "\n\n  the real prompt  \nsecond line");
    }

    #[test]
    fn a_declaration_is_recorded_on_the_branch_and_rewinds_with_it() {
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

        assert!(
            s.rows(&id)
                .unwrap()
                .iter()
                .all(|p| p.preview != "declared" && !p.preview.is_empty()),
            "a declaration does not appear as a node a person could pick"
        );

        let nodes = entries(&s, &id);
        let fork = s.fork(&id, Some(nodes[0])).unwrap();
        assert!(
            s.load(&fork).unwrap().declared.is_empty(),
            "the fork was taken before it"
        );

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

        s.rewind(&id, Some(nodes[0])).unwrap();
        assert_eq!(s.load(&id).unwrap().items, [Item::User("one".into())]);
        s.append(&id, &Item::User("two, differently".into()))
            .unwrap();

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
        let rows = s.rows(&id).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(!rows.iter().any(|p| p.here));
    }

    #[test]
    fn a_model_change_is_what_the_conversation_is_read_back_as_using() {
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
        assert_eq!(s.list().unwrap()[0].model, "moved-to");
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
        assert_eq!(listed.title, "the sandbox refactor");

        s.name(&id, "").unwrap();
        assert_eq!(s.list().unwrap()[0].name, None);
        assert_eq!(s.list().unwrap()[0].title, "the actual first prompt");
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
        let orphans: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE session = ?1",
                params![b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0);
        assert_eq!(s.load(&a).unwrap().items, [Item::User("keep me".into())]);
    }

    #[test]
    fn a_database_from_the_previous_schema_gains_the_name_column() {
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
        text.push_str("{\"type\":\"item\",\"item\":{\"us");
        fs::write(legacy.join(format!("{id}.jsonl")), text).unwrap();

        let db = dir.path().join("sessions.db");
        let s = Store::open(&db, PathBuf::from("/ws")).unwrap();
        let loaded = s.load(id).unwrap();
        assert_eq!(loaded.items, [Item::User("old task".into())]);
        assert_eq!(loaded.usage.cache_read, 3);
        assert!((loaded.cost - 0.5).abs() < 1e-9);
        assert!(legacy.join(format!("{id}.jsonl.imported")).exists());
        assert!(!legacy.join(format!("{id}.jsonl")).exists());
        let again = Store::open(&db, PathBuf::from("/ws")).unwrap();
        assert_eq!(again.load(id).unwrap().items.len(), 1);
        assert_eq!(again.list().unwrap().len(), 1);
    }
}
