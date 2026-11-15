//! host-owned optional wall-clock deadlines across tool transports (D152).

use std::cell::Cell;
use std::error::Error;
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{Value, json};

use crate::provider::ToolDef;

#[derive(Debug)]
pub struct TimedOut;
impl Display for TimedOut {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str("tool call timed out")
    }
}
impl Error for TimedOut {}

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

struct Restore(Option<Instant>);
impl Drop for Restore {
    fn drop(&mut self) {
        DEADLINE.set(self.0);
    }
}

pub fn deadline() -> Option<Instant> {
    DEADLINE.get()
}

pub fn expired() -> bool {
    deadline().is_some_and(|end| Instant::now() >= end)
}

pub fn remaining(ceiling: Duration) -> Result<Duration> {
    let left = deadline().map_or(ceiling, |end| {
        ceiling.min(end.saturating_duration_since(Instant::now()))
    });
    if left.is_zero() {
        return Err(TimedOut.into());
    }
    Ok(left)
}

pub fn parse(input: &Value) -> Result<Option<Instant>, String> {
    let Some(value) = input.get("timeout_secs") else {
        return Ok(None);
    };
    let seconds = value
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .ok_or("timeout_secs must be a positive integer")?;
    Instant::now()
        .checked_add(Duration::from_secs(seconds))
        .map(Some)
        .ok_or_else(|| "timeout_secs is too large".into())
}

pub fn using<T>(end: Option<Instant>, operation: impl FnOnce() -> T) -> T {
    let end = match (deadline(), end) {
        (Some(old), Some(new)) => Some(old.min(new)),
        (old, new) => old.or(new),
    };
    let _restore = Restore(DEADLINE.replace(end));
    operation()
}

/// add a host execution option without making it required or replacing a tool's own field.
pub fn definition(mut tool: ToolDef) -> ToolDef {
    if let Some(schema) = tool.schema.as_object_mut() {
        let properties = schema.entry("properties").or_insert_with(|| json!({}));
        if let Some(properties) = properties.as_object_mut() {
            properties.entry("timeout_secs").or_insert_with(|| json!({
                "type": "integer", "minimum": 1,
                "description": "Optional total wall-clock timeout in seconds; expiry stops local execution. Existing limits may stop it sooner. Remote MCP cancellation is best-effort."
            }));
        }
    }
    tool
}

/// consume the host option unless the underlying tool already declares it (notably bash).
pub fn arguments(input: &Value, schema: &Value) -> Value {
    let mut arguments = input.clone();
    if schema
        .get("properties")
        .and_then(|p| p.get("timeout_secs"))
        .is_none()
        && let Some(object) = arguments.as_object_mut()
    {
        object.remove("timeout_secs");
    }
    arguments
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn timeout_is_optional_strict_and_overflow_checked() {
        assert_eq!(parse(&json!({})).unwrap(), None);
        for value in [
            json!(0),
            json!(-1),
            json!(0.5),
            json!("1"),
            json!(null),
            json!(u64::MAX),
        ] {
            assert!(parse(&json!({"timeout_secs":value})).is_err(), "{value}");
        }
        assert!(parse(&json!({"timeout_secs":1})).unwrap().is_some());
    }

    #[test]
    fn nested_deadlines_never_extend_and_restore_even_on_unwind() {
        let end = Instant::now() + Duration::from_secs(1);
        using(Some(end), || {
            assert_eq!(deadline(), Some(end));
            using(Some(end + Duration::from_secs(1)), || {
                assert_eq!(deadline(), Some(end))
            });
            let _ = catch_unwind(AssertUnwindSafe(|| {
                using(Some(Instant::now()), || panic!("probe"))
            }));
            assert_eq!(deadline(), Some(end));
            assert!(remaining(Duration::from_secs(30)).unwrap() <= Duration::from_secs(1));
        });
        assert_eq!(deadline(), None);
        using(Some(Instant::now()), || {
            assert!(remaining(Duration::from_secs(30)).is_err())
        });
    }

    #[test]
    fn host_option_is_advertised_optional_and_not_leaked_into_strict_tool_arguments() {
        let original = json!({"type":"object", "properties":{"path":{"type":"string"}}, "required":["path"], "additionalProperties":false});
        let tool = definition(ToolDef {
            name: "read".into(),
            description: "read".into(),
            schema: original.clone(),
        });
        assert_eq!(tool.schema["properties"]["timeout_secs"]["minimum"], 1);
        assert_eq!(tool.schema["required"], json!(["path"]));
        let input = json!({"path":"file", "timeout_secs":1});
        assert_eq!(arguments(&input, &original), json!({"path":"file"}));
        assert_eq!(arguments(&input, &tool.schema), input);
    }
}
