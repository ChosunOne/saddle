//! transcript tool schema and presentation; the gated core service owns SQL execution (D137).

use saddle_limits as budgets;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::configure_limits;
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, Output, Tool, query_transcript};
#[cfg(target_arch = "wasm32")]
use serde_json::json;

pub struct SessionQuery;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    sql: String,
    /// The conversation to read: absent for the one in hand, or the id (or unique prefix) of
    /// another session of this workspace, which the grant has to cover (D163).
    #[serde(default)]
    session: Option<String>,
}

pub fn query_with(
    arguments: &str,
    query: impl FnOnce(&str, &str) -> Result<String, String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.sql.len() > budgets::query_bytes() {
        return Err("transcript SQL exceeds configured byte limit".into());
    }
    if args
        .session
        .as_ref()
        .is_some_and(|s| s.len() > budgets::directory_name_bytes())
    {
        return Err("session selector exceeds configured byte limit".into());
    }
    query(&args.sql, args.session.as_deref().unwrap_or_default())
}

#[cfg(target_arch = "wasm32")]
impl Tool for SessionQuery {
    fn describe() -> Definition {
        Definition {
            name: "session_query".into(),
            description: "read-only SQL over a conversation's active branch, including pre-compaction history. Views: messages(ordinal,id,role,text); requests(ordinal,node,kind,provider,model,input,output,cache_read,cache_write,cached,cost,reasoning_dropped,reasoning_tokens,at,after); reasoning(ordinal,node,part,text); sessions(id,name,title,model,updated,messages,forked_from). Ordinals start at 1; join reasoning to messages on ordinal. cached=1 is a cache hit, and only prose reasoning is readable. Omit `session` for this conversation, or give an id (or unique prefix) from `sessions` to read another one of this workspace, which the transcript-read grant has to cover. No images, internal tables or files. Results are JSON {columns,rows,truncated}, capped at 100 rows and 50 KiB. Use WHERE, ORDER BY ordinal and LIMIT/OFFSET.".into(),
            input_schema: json!({
                "type":"object",
                "properties":{
                    "sql":{"type":"string"},
                    "session":{"type":"string","description":"a session id or unique prefix from the sessions view; omitted reads this conversation"}
                },
                "required":["sql"], "additionalProperties":false
            }).to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
        if let Err(text) = configure_limits() {
            return Output {
                text,
                is_error: true,
            };
        }
        match query_with(&arguments, query_transcript) {
            Ok(text) => Output {
                text,
                is_error: false,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_are_checked_before_host_dispatch_and_results_are_preserved() {
        assert_eq!(
            query_with(r#"{"sql":"SELECT 1"}"#, |sql, session| {
                assert_eq!(sql, "SELECT 1");
                assert_eq!(session, "", "the conversation in hand");
                Ok("result".into())
            })
            .unwrap(),
            "result"
        );
        assert_eq!(
            query_with(r#"{"sql":"SELECT 1","session":"abc"}"#, |_, session| {
                assert_eq!(session, "abc");
                Ok("other".into())
            })
            .unwrap(),
            "other"
        );
        assert_eq!(
            query_with(r#"{"sql":"SELECT 1"}"#, |_, _| {
                Err("denied by core".into())
            })
            .unwrap_err(),
            "denied by core"
        );
        for arguments in [
            "{}".into(),
            r#"{"sql":1}"#.into(),
            json!({"sql":"SELECT 1","session":"x".repeat(4097)}).to_string(),
            json!({"sql":"x".repeat(16385)}).to_string(),
        ] {
            assert!(
                query_with(&arguments, |_, _| panic!(
                    "must not dispatch invalid arguments"
                ))
                .is_err()
            );
        }
    }
}
