//! transcript SQL behind the gated SDK service, not a native tool (D92, D137, D147).
//!
//! Four views over one conversation's active branch: its messages, one row per model request with
//! what it used and what answered it (D94), the reasoning behind those messages (D98), and the
//! conversations of this workspace that may be read (D163).

use saddle_limits as budgets;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use std::time::UNIX_EPOCH;

use anyhow::{Result, bail};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};
use saddle_cap::Principal;
use saddle_cap::policy::Sessions;
use saddle_policy::Request;
use serde_json::{Value, json};

use crate::cancel;
use crate::session::Session;
use crate::store::{Snapshot, Summary};

pub fn execute(
    session: &mut Session,
    who: &Principal,
    sql: &str,
    selector: &str,
    call: Option<&str>,
    deadline: Option<Instant>,
) -> Result<Result<Value, String>> {
    let active = session.query_conversation.clone();
    let mode = session.engine.transcript_sessions(who);
    let wanted = selector.trim();
    // A selector names another conversation; that is the part the grant has to cover, and it is
    // what makes this more than a query of the conversation in hand (D163).
    let others = !wanted.is_empty();
    // Only a grant that covers the workspace resolves a selector or lists the others, so a
    // refusal never says which ids exist (D163).
    let listed = if others || mode == Some(Sessions::Workspace) {
        session.transcript_store()?.list()?
    } else {
        Vec::new()
    };
    let target = match (others, mode) {
        (false, _) => active.clone(),
        (true, Some(Sessions::Workspace)) => match resolve(wanted, &listed) {
            Ok(id) => Some(id.to_owned()),
            Err(why) => return Ok(Err(why)),
        },
        (true, _) => None,
    };
    let id = match (&target, others) {
        (Some(id), _) => id.clone(),
        (None, true) => wanted.to_owned(),
        (None, false) => "none".to_owned(),
    };
    let request = Request::TranscriptQuery {
        session: &id,
        others,
        sql,
    };
    let decision = session.engine.decide(who, &request);
    if let Some(call) = call {
        session
            .audit
            .decision_for_call(who, &request, &decision, call)?;
    } else {
        session.audit.decision(who, &request, &decision)?;
    }
    if !decision.allow {
        return Ok(Err(format!("denied: {}", decision.explanation)));
    }
    // The agent has already built this conversation's snapshot around the call; another
    // conversation, or one it did not build, is loaded here.
    let snapshot = if target == active {
        session.query_snapshot.take()
    } else {
        None
    };
    let Some(snapshot) = snapshot.or(match target.as_deref() {
        Some(id) => Some(session.transcript_store()?.snapshot(id)?),
        None => None,
    }) else {
        return Ok(Err("no saved conversation is active".into()));
    };
    // A grant that covers only this conversation lists only this conversation, so the titles of
    // the others say nothing (D163).
    let allowed: Vec<SessionRow> = match mode {
        Some(Sessions::Workspace) => listed.iter().map(row).collect(),
        _ => listed
            .iter()
            .filter(|s| Some(&s.id) == active.as_ref())
            .map(row)
            .collect(),
    };
    Ok(query_bounded(&snapshot, &allowed, sql, deadline)
        .map_err(|error| format!("session query failed: {error}")))
}

/// One row of the `sessions` view: the saved conversations a query may name (D163).
fn row(summary: &Summary) -> SessionRow {
    SessionRow {
        id: summary.id.clone(),
        name: summary.name.clone().unwrap_or_default(),
        title: summary.title.clone(),
        model: summary.model.clone(),
        updated: summary
            .updated
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64),
        messages: summary.messages as i64,
        forked_from: summary.forked_from.clone().unwrap_or_default(),
    }
}

/// The one conversation `selector` names in this workspace, or why it names none or several
/// (D163). An exact id wins; otherwise the prefix has to be unique, the way `saddle session`
/// takes one, so a partial id is never a guess between conversations.
fn resolve<'a>(selector: &str, listed: &'a [Summary]) -> Result<&'a str, String> {
    if let Some(found) = listed.iter().find(|s| s.id == selector) {
        return Ok(&found.id);
    }
    let matching: Vec<&Summary> = listed
        .iter()
        .filter(|s| s.id.starts_with(selector))
        .collect();
    match matching.as_slice() {
        [] => Err(format!(
            "no conversation in this workspace has an id starting with {selector:?}"
        )),
        [one] => Ok(&one.id),
        many => Err(format!(
            "{} conversations in this workspace start with {selector:?}: give more of the id",
            many.len()
        )),
    }
}

fn authorize(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read { table_name, .. } => match (table_name, ctx.accessor) {
            // The three views, and the backing table each of them reads (D92, D94, D98).
            ("messages" | "requests" | "reasoning" | "sessions", _) => Authorization::Allow,
            ("message_snapshot", Some("messages"))
            | ("request_snapshot", Some("requests"))
            | ("reasoning_snapshot", Some("reasoning"))
            | ("session_snapshot", Some("sessions")) => Authorization::Allow,
            _ => Authorization::Deny,
        },
        AuthAction::Function {
            function_name:
                "like" | "glob" | "lower" | "upper" | "length" | "substr" | "substring" | "instr"
                | "trim" | "ltrim" | "rtrim" | "replace" | "coalesce" | "ifnull" | "nullif" | "count"
                | "min" | "max" | "sum" | "avg" | "total" | "group_concat" | "abs" | "round" | "typeof"
                | "json_extract" | "json_valid",
        } => Authorization::Allow,
        _ => Authorization::Deny,
    }
}

#[cfg(test)]
fn query(snapshot: &Snapshot, sql: &str) -> Result<Value> {
    query_bounded(snapshot, &[], sql, None)
}

fn check_deadline(deadline: Option<Instant>) -> Result<()> {
    if cancel::FLAG.load(Ordering::Relaxed) || deadline.is_some_and(|end| Instant::now() >= end) {
        bail!("transcript query cancelled or timed out");
    }
    Ok(())
}

/// One saved conversation as the `sessions` view says it (D163).
struct SessionRow {
    id: String,
    name: String,
    title: String,
    model: String,
    updated: i64,
    messages: i64,
    forked_from: String,
}

fn query_bounded(
    snapshot: &Snapshot,
    sessions: &[SessionRow],
    sql: &str,
    deadline: Option<Instant>,
) -> Result<Value> {
    check_deadline(deadline)?;
    if sql.len() > budgets::query_bytes() {
        bail!("SQL exceeds configured byte limit");
    }
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "PRAGMA temp_store=MEMORY;
        CREATE TABLE message_snapshot(ordinal INTEGER, id INTEGER, role TEXT, text TEXT);
        CREATE VIEW messages AS SELECT ordinal, id, role, text FROM message_snapshot;
        CREATE TABLE request_snapshot(
            ordinal INTEGER, node INTEGER, kind TEXT, provider TEXT, model TEXT,
            input INTEGER, output INTEGER, cache_read INTEGER, cache_write INTEGER,
            cached INTEGER, cost REAL, dropped INTEGER, reasoning_tokens INTEGER,
            at TEXT, after INTEGER);
        CREATE VIEW requests AS SELECT ordinal, node, kind, provider, model, input, output,
            cache_read, cache_write, cached, cost, dropped AS reasoning_dropped,
            reasoning_tokens, at, after
            FROM request_snapshot;
        CREATE TABLE reasoning_snapshot(ordinal INTEGER, node INTEGER, part INTEGER, text TEXT);
        CREATE VIEW reasoning AS SELECT ordinal, node, part, text FROM reasoning_snapshot;
        CREATE TABLE session_snapshot(
            id TEXT, name TEXT, title TEXT, model TEXT, updated INTEGER, messages INTEGER,
            forked_from TEXT);
        CREATE VIEW sessions AS SELECT id, name, title, model, updated, messages, forked_from
            FROM session_snapshot;",
    )?;
    {
        let mut insert = conn.prepare("INSERT INTO message_snapshot VALUES (?1, ?2, ?3, ?4)")?;
        for (ordinal, (id, role, text)) in snapshot.messages.iter().enumerate() {
            check_deadline(deadline)?;
            insert.execute(params![(ordinal + 1) as i64, id, role, text])?;
        }
    }
    {
        let mut insert = conn.prepare("INSERT INTO request_snapshot VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)")?;
        for (ordinal, row) in snapshot.requests.iter().enumerate() {
            check_deadline(deadline)?;
            insert.execute(params![
                (ordinal + 1) as i64,
                row.node,
                row.kind,
                row.provider,
                row.model,
                row.usage.input as i64,
                row.usage.output as i64,
                row.usage.cache_read as i64,
                row.usage.cache_write as i64,
                // Whether the prompt was served from a provider cache at all. The counts stay the
                // source of truth; this is the question a hit rate is about (D94).
                i64::from(row.usage.cache_read > 0),
                row.cost,
                row.dropped as i64,
                row.reasoning_tokens as i64,
                row.at,
                row.after as i64,
            ])?;
        }
    }
    {
        let mut insert = conn.prepare("INSERT INTO reasoning_snapshot VALUES (?1, ?2, ?3, ?4)")?;
        for row in &snapshot.reasoning {
            check_deadline(deadline)?;
            insert.execute(params![
                row.ordinal as i64,
                row.node,
                row.part as i64,
                row.text
            ])?;
        }
    }
    {
        let mut insert =
            conn.prepare("INSERT INTO session_snapshot VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?;
        for row in sessions {
            insert.execute(params![
                row.id,
                row.name,
                row.title,
                row.model,
                row.updated,
                row.messages,
                row.forked_from
            ])?;
        }
    }
    conn.pragma_update(None, "query_only", true)?;
    conn.set_limit(
        Limit::SQLITE_LIMIT_LENGTH,
        budgets::query_value_bytes().min(i32::MAX as usize) as i32,
    )?;
    conn.set_limit(
        Limit::SQLITE_LIMIT_SQL_LENGTH,
        budgets::query_bytes().min(i32::MAX as usize) as i32,
    )?;
    conn.set_limit(
        Limit::SQLITE_LIMIT_COLUMN,
        budgets::query_columns().min(i32::MAX as usize) as i32,
    )?;
    conn.set_limit(
        Limit::SQLITE_LIMIT_EXPR_DEPTH,
        budgets::query_expr_depth().min(i32::MAX as usize) as i32,
    )?;
    conn.set_limit(
        Limit::SQLITE_LIMIT_VDBE_OP,
        budgets::query_program_ops().min(i32::MAX as usize) as i32,
    )?;
    let steps = AtomicUsize::new(0);
    conn.progress_handler(
        1000,
        Some(move || {
            steps.fetch_add(1000, Ordering::Relaxed) >= budgets::query_steps()
                || check_deadline(deadline).is_err()
        }),
    )?;
    conn.authorizer(Some(authorize))?;
    // Wrapping accepts exactly one query, not a batch or a PRAGMA. The authorizer still decides
    // every operation: lexical SELECT checks would not protect functions or virtual tables.
    let sql = sql.trim().trim_end_matches(';');
    let mut statement = conn.prepare(&format!("SELECT * FROM ({sql}\n)"))?;
    if !statement.readonly() {
        bail!("only read-only queries are allowed");
    }
    let columns: Vec<String> = statement.column_names().map(String::from).collect();
    let mut bytes = serde_json::to_vec(&columns)?.len() + 64;
    if bytes > budgets::query_result_bytes() {
        bail!("query column names exceed configured result limit");
    }
    let mut cursor = statement.query([])?;
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = cursor.next()? {
        check_deadline(deadline)?;
        if rows.len() == budgets::query_rows() {
            truncated = true;
            break;
        }
        let mut values = Vec::new();
        for col in 0..columns.len() {
            values.push(match row.get_ref(col)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(text) => json!(String::from_utf8_lossy(text)),
                ValueRef::Blob(_) => bail!("blob results are not supported"),
            });
        }
        let size = serde_json::to_vec(&values)?.len() + 1;
        if bytes + size > budgets::query_result_bytes() {
            truncated = true;
            break;
        }
        bytes += size;
        rows.push(values);
    }
    Ok(json!({"columns": columns, "rows": rows, "truncated": truncated}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Usage;
    use crate::store::{ReasoningRow, RequestRow};
    use std::time::Duration;

    fn messages() -> Snapshot {
        Snapshot {
            messages: vec![(7, "user".into(), "remember teal".into())],
            requests: Vec::new(),
            reasoning: Vec::new(),
        }
    }

    /// A request row, as the store would export it (D94).
    fn request(provider: &str, model: &str, input: u64, cache_read: u64) -> RequestRow {
        RequestRow {
            node: 9,
            kind: "reply".into(),
            provider: provider.into(),
            model: model.into(),
            usage: Usage {
                input,
                output: 5,
                cache_read,
                cache_write: 0,
            },
            cost: 0.01,
            dropped: 0,
            reasoning_tokens: 0,
            at: "2026-01-01T00:00:00Z".into(),
            after: 1,
        }
    }

    #[test]
    fn searches_the_view() {
        let out = query(
            &messages(),
            "SELECT ordinal, role, text FROM messages WHERE text LIKE '%teal%'",
        )
        .unwrap();
        assert_eq!(out["rows"], json!([[1, "user", "remember teal"]]));
    }

    #[test]
    fn query_checks_its_deadline_and_bounds_escaped_column_names() {
        let error = query_bounded(&messages(), &[], "SELECT 1", Some(Instant::now())).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let sql = format!("SELECT 1 AS \"{}\"", "\u{1}".repeat(10_000));
        let error = query(&messages(), &sql).unwrap_err();
        assert!(error.to_string().contains("column names"), "{error}");
    }

    fn summary(id: &str, title: &str) -> Summary {
        Summary {
            id: id.to_owned(),
            name: None,
            title: title.to_owned(),
            prompts: 1,
            messages: 2,
            model: "glm".to_owned(),
            forked_from: None,
            updated: UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    #[test]
    fn a_selector_is_an_id_or_a_prefix_that_names_exactly_one_conversation() {
        // D163: an id is taken exactly, a prefix only when it is the only one, so a partial id is
        // never a guess between two conversations.
        let listed = [
            summary("abcdefghij", "first"),
            summary("abcxyz", "second"),
            summary("zzz", "third"),
        ];
        assert_eq!(resolve("zzz", &listed).unwrap(), "zzz");
        assert_eq!(resolve("abcx", &listed).unwrap(), "abcxyz");
        let ambiguous = resolve("abc", &listed).unwrap_err();
        assert!(ambiguous.contains("2 conversations"), "{ambiguous}");
        let none = resolve("nope", &listed).unwrap_err();
        assert!(none.contains("no conversation"), "{none}");
    }

    #[test]
    fn the_sessions_view_names_the_conversations_and_nothing_else_is_readable() {
        // D163: what a query can find out about the other conversations is what the host put in
        // the table — id, name, title, model, update time and size — and no table of the database.
        let rows = [SessionRow {
            id: "abcdefghij".into(),
            name: String::new(),
            title: "remember teal".into(),
            model: "glm".into(),
            updated: 1_700_000_000,
            messages: 4,
            forked_from: String::new(),
        }];
        let out = query_bounded(
            &messages(),
            &rows,
            "SELECT id, title, messages FROM sessions",
            None,
        )
        .unwrap();
        assert_eq!(out["rows"], json!([["abcdefghij", "remember teal", 4]]));
        // The backing table has no name of its own to guess at: only the view is authorized.
        let error = query_bounded(&messages(), &rows, "SELECT * FROM sessions", None)
            .and_then(|_| {
                query_bounded(
                    &messages(),
                    &rows,
                    "SELECT * FROM sessions; DROP TABLE sessions",
                    None,
                )
            })
            .unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn reports_what_each_request_used_and_whether_it_read_the_cache() {
        // D94: the store had the numbers and the tool could not see them, so "why does this
        // provider's cache miss" was answerable only from the audit log. This is the query the
        // view exists for.
        let snapshot = Snapshot {
            messages: messages().messages,
            requests: vec![
                request("synthetic", "glm", 1_000, 0),
                request("synthetic", "glm", 1_000, 900),
                request("openai", "gpt", 2_000, 0),
            ],
            reasoning: Vec::new(),
        };
        let out = query(
            &snapshot,
            "SELECT provider, sum(cache_read), sum(cached), count(*) FROM requests
             GROUP BY provider ORDER BY provider",
        )
        .unwrap();
        assert_eq!(
            out["rows"],
            json!([["openai", 0, 0, 1], ["synthetic", 900, 1, 2]])
        );
        // One request, with the columns a person joins the messages on.
        let out = query(
            &snapshot,
            "SELECT ordinal, kind, model, input, cache_read, cached, after FROM requests
             WHERE cache_read > 0",
        )
        .unwrap();
        assert_eq!(out["rows"], json!([[2, "reply", "glm", 1_000, 900, 1, 1]]));
    }

    #[test]
    fn a_request_carries_the_reasoning_the_provider_dropped() {
        // D97: the number that answers "is the API eating my reasoning" from a query.
        let mut dropped = request("openai", "gpt", 500, 0);
        dropped.dropped = 2;
        let snapshot = Snapshot {
            messages: messages().messages,
            requests: vec![
                request("openai", "gpt", 500, 400),
                dropped,
                request("openai", "gpt", 500, 0),
            ],
            reasoning: Vec::new(),
        };
        let out = query(
            &snapshot,
            "SELECT ordinal, reasoning_dropped FROM requests ORDER BY ordinal",
        )
        .unwrap();
        assert_eq!(out["rows"], json!([[1, 0], [2, 2], [3, 0]]));
    }

    #[test]
    fn a_request_says_how_much_of_its_output_was_thinking() {
        // D99: the split, beside the output it is part of, so "what did we pay for that we never
        // saw" is a sum rather than an estimate from character counts.
        let mut thinking = request("openai", "gpt", 500, 400);
        thinking.reasoning_tokens = 850;
        let snapshot = Snapshot {
            messages: messages().messages,
            requests: vec![request("openai", "gpt", 500, 400), thinking],
            reasoning: Vec::new(),
        };
        let out = query(
            &snapshot,
            "SELECT sum(output), sum(reasoning_tokens) FROM requests",
        )
        .unwrap();
        assert_eq!(out["rows"], json!([[10, 850]]));
        let out = query(
            &snapshot,
            "SELECT ordinal, output, reasoning_tokens FROM requests WHERE reasoning_tokens > 0",
        )
        .unwrap();
        assert_eq!(out["rows"], json!([[2, 5, 850]]));
    }

    #[test]
    fn reasoning_is_searchable_beside_the_text_it_produced() {
        // D98: reasoning was replayed to the model and stored on the item (D95, D96) and no view
        // could see it, so the transcript was searchable and the thinking behind it was not.
        let snapshot = Snapshot {
            messages: vec![
                (7, "user".into(), "how many rows".into()),
                (8, "assistant".into(), "checking".into()),
            ],
            requests: Vec::new(),
            reasoning: vec![ReasoningRow {
                ordinal: 2,
                node: 8,
                part: 0,
                text: "count the rows, then subtract the ones the filter took".into(),
            }],
        };
        let out = query(
            &snapshot,
            "SELECT m.ordinal, m.role, r.text FROM messages m JOIN reasoning r USING (ordinal)
             WHERE r.text LIKE '%subtract%'",
        )
        .unwrap();
        assert_eq!(
            out["rows"],
            json!([[
                2,
                "assistant",
                "count the rows, then subtract the ones the filter took"
            ]])
        );
    }

    #[test]
    fn the_backing_tables_are_as_private_as_the_views() {
        let snapshot = Snapshot {
            messages: messages().messages,
            requests: vec![request("openai", "gpt", 10, 0)],
            reasoning: Vec::new(),
        };
        for sql in [
            "SELECT * FROM message_snapshot",
            "SELECT * FROM request_snapshot",
            "SELECT * FROM reasoning_snapshot",
            "SELECT * FROM session_snapshot",
        ] {
            assert!(query(&snapshot, sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn cannot_write_attach_inspect_or_call_unapproved_functions() {
        for sql in [
            "DELETE FROM messages",
            "UPDATE message_snapshot SET text='changed'",
            "DROP VIEW messages",
            "ATTACH '/tmp/other.db' AS other",
            "PRAGMA database_list",
            "SELECT * FROM sqlite_schema",
            "SELECT * FROM message_snapshot",
            "SELECT * FROM pragma_database_list",
            "SELECT load_extension('anything')",
            "SELECT readfile('/etc/passwd')",
            "SELECT 1); DELETE FROM message_snapshot; --",
            "SELECT randomblob(1000000000)",
        ] {
            assert!(query(&messages(), sql).is_err(), "{sql}");
        }
        assert_eq!(
            query(&messages(), "SELECT text FROM messages").unwrap()["rows"],
            json!([["remember teal"]])
        );
    }

    #[test]
    fn bounds_rows_and_recursive_work() {
        let snapshot = Snapshot {
            messages: vec![(1, "user".into(), "hi".into()); 101],
            requests: Vec::new(),
            reasoning: Vec::new(),
        };
        let out = query(&snapshot, "SELECT * FROM messages").unwrap();
        assert_eq!(out["rows"].as_array().unwrap().len(), 100);
        assert_eq!(out["truncated"], true);
        assert!(query(&Snapshot::default(), "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT sum(x) FROM n").is_err());
    }

    #[test]
    fn caps_serialized_bytes_and_rejects_oversized_sql() {
        let snapshot = Snapshot {
            messages: vec![(1, "user".into(), "\"".repeat(30_000))],
            requests: Vec::new(),
            reasoning: Vec::new(),
        };
        let out = query(&snapshot, "SELECT text FROM messages").unwrap();
        assert_eq!(out["truncated"], true);
        assert!(out.to_string().len() <= budgets::query_result_bytes());
        assert!(query(&Snapshot::default(), &" ".repeat(16 * 1024 + 1)).is_err());
    }
}
