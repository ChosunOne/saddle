//! transactional private KV rows, never guest-chosen filesystem paths (D156).

use std::collections::BTreeSet;
use std::os::unix::ffi::OsStrExt;
use std::time::Instant;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use saddle_cap::Principal;
use saddle_wit::saddle::host::persist::{Change, Expectation};

use crate::session::Session;

fn open(session: &Session, deadline: Instant) -> Result<Connection> {
    let connection = Connection::open(session.setup.paths.state.join("plugin-state.db"))?;
    connection.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS kv (workspace BLOB NOT NULL, plugin TEXT NOT NULL, key TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(workspace, plugin, key));")?;
    Ok(connection)
}

pub(super) fn get(
    session: &Session,
    who: &Principal,
    key: &str,
    deadline: Instant,
) -> Result<Result<Option<Vec<u8>>, String>> {
    let limits = &session.setup.tool_limits;
    if key.is_empty() || key.len() > limits.persist_key_bytes {
        return Ok(Err("invalid persistent key size".into()));
    }
    let connection = open(session, deadline)?;
    let workspace = session.setup.workspace.as_os_str().as_bytes();
    let value: Option<(i64, Option<Vec<u8>>)> = connection.query_row(
        "SELECT length(value), CASE WHEN length(value)<=? THEN value ELSE NULL END FROM kv WHERE workspace=? AND plugin=? AND key=?",
        params![limits.persist_value_bytes as i64, workspace, who.to_string(), key], |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    match value {
        Some((length, _)) if length > limits.persist_value_bytes as i64 => {
            Ok(Err("stored value exceeds current value budget".into()))
        }
        Some((_, value)) => Ok(Ok(value)),
        None => Ok(Ok(None)),
    }
}

pub(super) fn apply(
    session: &Session,
    who: &Principal,
    changes: Vec<Change>,
    deadline: Instant,
) -> Result<Result<bool, String>> {
    let limits = &session.setup.tool_limits;
    let mut keys = BTreeSet::new();
    if changes.len() > limits.persist_changes || changes.iter().any(|change| change.key.is_empty() || change.key.len() > limits.persist_key_bytes || !keys.insert(change.key.clone()) || change.value.as_ref().is_some_and(|v| v.len() > limits.persist_value_bytes) || matches!(&change.expected, Expectation::Value(v) if v.len() > limits.persist_value_bytes)) { return Ok(Err("invalid persistent batch or configured limit exceeded".into())); }
    let mut connection = open(session, deadline)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let workspace = session.setup.workspace.as_os_str().as_bytes();
    let plugin = who.to_string();
    let counts = || -> Result<(i64, i64)> {
        Ok(transaction.query_row("SELECT count(*), coalesce(sum(length(CAST(key AS BLOB)) + length(value)),0) FROM kv WHERE workspace=? AND plugin=?", params![workspace, plugin], |row| Ok((row.get(0)?, row.get(1)?)))?)
    };
    let (old_count, old_bytes) = counts()?;
    for change in &changes {
        let matches = match &change.expected {
            Expectation::Any => true,
            Expectation::Missing => !transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM kv WHERE workspace=? AND plugin=? AND key=?)",
                params![workspace, plugin, change.key],
                |row| row.get::<_, bool>(0),
            )?,
            Expectation::Value(value) => transaction
                .query_row(
                    "SELECT value = ? FROM kv WHERE workspace=? AND plugin=? AND key=?",
                    params![value, workspace, plugin, change.key],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false),
        };
        if !matches {
            return Ok(Ok(false));
        }
    }
    for change in changes {
        if let Some(value) = change.value {
            transaction.execute("INSERT INTO kv(workspace,plugin,key,value) VALUES (?,?,?,?) ON CONFLICT(workspace,plugin,key) DO UPDATE SET value=excluded.value", params![workspace,plugin,change.key,value])?;
        } else {
            transaction.execute(
                "DELETE FROM kv WHERE workspace=? AND plugin=? AND key=?",
                params![workspace, plugin, change.key],
            )?;
        }
    }
    let (count, bytes) = counts()?;
    let over_quota =
        count > limits.persist_keys as i64 || bytes > limits.persist_total_bytes as i64;
    let shrinking =
        count <= old_count && bytes <= old_bytes && (count < old_count || bytes < old_bytes);
    if over_quota && !shrinking {
        return Ok(Err("persistent storage quota exceeded".into()));
    }
    if Instant::now() >= deadline {
        return Ok(Err("persistent batch timed out before commit".into()));
    }
    transaction.commit()?;
    Ok(Ok(true))
}
