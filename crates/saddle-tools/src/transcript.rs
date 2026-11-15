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
}

pub fn query_with(
    arguments: &str,
    query: impl FnOnce(&str) -> Result<String, String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.sql.len() > budgets::query_bytes() {
        return Err("transcript SQL exceeds configured byte limit".into());
    }
    query(&args.sql)
}

#[cfg(target_arch = "wasm32")]
impl Tool for SessionQuery {
    fn describe() -> Definition {
        Definition {
            name: "session_query".into(),
            description: "read-only SQL over the current conversation branch, including pre-compaction history. Views: messages(ordinal,id,role,text); requests(ordinal,node,kind,provider,model,input,output,cache_read,cache_write,cached,cost,reasoning_dropped,reasoning_tokens,at,after); reasoning(ordinal,node,part,text). Ordinals start at 1; join reasoning to messages on ordinal. Request kind is reply or summary; after is the preceding message ordinal. Cached is 1 for a cache hit. Only prose reasoning is readable. No other conversations, images, internal tables or files are accessible. Requires transcript-read. Results are JSON with columns, rows, truncated; capped at 100 rows and 50 KiB. Use WHERE, ORDER BY ordinal and LIMIT/OFFSET to search and page. Example: SELECT ordinal,role,text FROM messages WHERE text LIKE '%decision%' LIMIT 10".into(),
            input_schema: json!({
                "type":"object",
                "properties":{"sql":{"type":"string"}},
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
            query_with(r#"{"sql":"SELECT 1"}"#, |sql| {
                assert_eq!(sql, "SELECT 1");
                Ok("result".into())
            })
            .unwrap(),
            "result"
        );
        assert_eq!(
            query_with(r#"{"sql":"SELECT 1"}"#, |_| {
                Err("denied by core".into())
            })
            .unwrap_err(),
            "denied by core"
        );
        for arguments in [
            "{}".into(),
            r#"{"sql":1}"#.into(),
            r#"{"sql":"SELECT 1","session":"other"}"#.into(),
            json!({"sql":"x".repeat(16385)}).to_string(),
        ] {
            assert!(
                query_with(&arguments, |_| panic!(
                    "must not dispatch invalid arguments"
                ))
                .is_err()
            );
        }
    }
}
