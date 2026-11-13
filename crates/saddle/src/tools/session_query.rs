//! A branch-only, read-only SQL surface; untrusted SQL never reaches the saved store (D92).

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};
use saddle_cap::Principal;
use saddle_policy::Request;
use serde_json::{Value, json};

use super::{ToolOutput, str_field};
use crate::session::Session;

pub fn run(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let sql = match str_field(input, "sql") {
        Ok(sql) => sql,
        Err(out) => return Ok(out),
    };
    let id = session
        .query_conversation
        .as_deref()
        .unwrap_or("none")
        .to_owned();
    let decision = session.decide(
        &Principal::Agent,
        &Request::TranscriptQuery { session: &id, sql },
    )?;
    if !decision.allow {
        return Ok(ToolOutput::denied(&decision.explanation));
    }
    let Some(messages) = &session.query_snapshot else {
        return Ok(ToolOutput::err("no saved conversation is active"));
    };
    Ok(match query(messages, sql) {
        Ok(value) => ToolOutput::ok(value.to_string()),
        Err(error) => ToolOutput::err(format!("session query failed: {error}")),
    })
}

fn authorize(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Read { table_name, .. }
            if table_name == "messages"
                || (table_name == "snapshot" && ctx.accessor == Some("messages")) =>
        {
            Authorization::Allow
        }
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

fn query(messages: &[(i64, String, String)], sql: &str) -> Result<Value> {
    if sql.len() > 16 * 1024 {
        bail!("SQL exceeds 16 KiB");
    }
    let mut conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "PRAGMA temp_store=MEMORY;
        CREATE TABLE snapshot(ordinal INTEGER, id INTEGER, role TEXT, text TEXT);
        CREATE VIEW messages AS SELECT ordinal, id, role, text FROM snapshot;",
    )?;
    {
        let mut insert = conn.prepare("INSERT INTO snapshot VALUES (?1, ?2, ?3, ?4)")?;
        for (ordinal, (id, role, text)) in messages.iter().enumerate() {
            insert.execute(params![(ordinal + 1) as i64, id, role, text])?;
        }
    }
    conn.pragma_update(None, "query_only", true)?;
    conn.set_limit(Limit::SQLITE_LIMIT_LENGTH, 1024 * 1024)?;
    conn.set_limit(Limit::SQLITE_LIMIT_SQL_LENGTH, 16 * 1024)?;
    conn.set_limit(Limit::SQLITE_LIMIT_COLUMN, 32)?;
    conn.set_limit(Limit::SQLITE_LIMIT_EXPR_DEPTH, 100)?;
    conn.set_limit(Limit::SQLITE_LIMIT_VDBE_OP, 100_000)?;
    let steps = AtomicUsize::new(0);
    conn.progress_handler(
        1000,
        Some(move || steps.fetch_add(1, Ordering::Relaxed) >= 1000),
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
    let mut cursor = statement.query([])?;
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = cursor.next()? {
        if rows.len() == 100 {
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
        if bytes + size > 50 * 1024 {
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

    fn messages() -> Vec<(i64, String, String)> {
        vec![(7, "user".into(), "remember teal".into())]
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
    fn cannot_write_attach_inspect_or_call_unapproved_functions() {
        for sql in [
            "DELETE FROM messages",
            "UPDATE snapshot SET text='changed'",
            "DROP VIEW messages",
            "ATTACH '/tmp/other.db' AS other",
            "PRAGMA database_list",
            "SELECT * FROM sqlite_schema",
            "SELECT * FROM snapshot",
            "SELECT * FROM pragma_database_list",
            "SELECT load_extension('anything')",
            "SELECT readfile('/etc/passwd')",
            "SELECT 1); DELETE FROM snapshot; --",
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
        let rows = vec![(1, "user".into(), "hi".into()); 101];
        let out = query(&rows, "SELECT * FROM messages").unwrap();
        assert_eq!(out["rows"].as_array().unwrap().len(), 100);
        assert_eq!(out["truncated"], true);
        assert!(query(&[], "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n) SELECT sum(x) FROM n").is_err());
    }

    #[test]
    fn caps_serialized_bytes_and_rejects_oversized_sql() {
        let rows = vec![(1, "user".into(), "\"".repeat(30_000))];
        let out = query(&rows, "SELECT text FROM messages").unwrap();
        assert_eq!(out["truncated"], true);
        assert!(out.to_string().len() <= 50 * 1024);
        assert!(query(&[], &" ".repeat(16 * 1024 + 1)).is_err());
    }
}
