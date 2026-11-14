//! standard tool implementations; the core owns effects, guests own presentation (D131).

pub mod listing;
pub mod search;

use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, FilePath, Output, Tool, read_range, resolve_path};
#[cfg(target_arch = "wasm32")]
use serde_json::{Value, json};

const CHUNK_BYTES: u32 = 8192;
const PAGE_BYTES: usize = 50 * 1024;
const PAGE_LINES: u64 = 2000;
const SCAN_BYTES: u64 = 1024 * 1024;

/// a model-facing reader; importing it does not grant any filesystem access.
pub struct Read;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default = "workspace")]
    root: String,
    path: String,
    #[serde(default = "first_line")]
    offset: u64,
    #[serde(default = "page_lines")]
    limit: u64,
}

fn workspace() -> String {
    "workspace".into()
}
fn first_line() -> u64 {
    1
}
fn page_lines() -> u64 {
    PAGE_LINES
}

#[cfg(target_arch = "wasm32")]
impl Tool for Read {
    fn describe() -> Definition {
        Definition {
            name: "read".into(),
            description: "read a UTF-8 text page beneath a granted root; path is workspace-relative or absolute unless root is provided, offset is a 1-based line. At most 2000 lines and 50 KiB per page; scans at most 1 MiB. No images.".into(),
            input_schema: json!({
                "type":"object", "properties": {
                    "root":{"type":"string"}, "path":{"type":"string"},
                    "offset":{"type":"integer","minimum":1},
                    "limit":{"type":"integer","minimum":1}
                }, "required":["path"], "additionalProperties":false
            }).to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
        let arguments = match normalize_path(&arguments, None) {
            Ok(arguments) => arguments,
            Err(text) => {
                return Output {
                    text,
                    is_error: true,
                };
            }
        };
        let result = read_with(&arguments, |root, path, offset, limit| {
            read_range(
                &FilePath {
                    root: root.into(),
                    relative: path.into(),
                },
                offset,
                limit,
            )
            .map(|chunk| (chunk.bytes, chunk.eof))
            .map_err(|error| format!("read failed: {error:?}"))
        });
        match result {
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

#[cfg(target_arch = "wasm32")]
fn normalize_path(arguments: &str, default: Option<&str>) -> Result<String, String> {
    let mut arguments: Value =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let object = arguments
        .as_object_mut()
        .ok_or("arguments must be an object")?;
    if !object.contains_key("root") {
        let path = object
            .get("path")
            .and_then(Value::as_str)
            .or(default)
            .ok_or("missing string field `path`")?;
        let path = resolve_path(path).map_err(|error| format!("resolve failed: {error:?}"))?;
        object.insert("root".into(), json!(path.root));
        object.insert("path".into(), json!(path.relative));
    }
    Ok(arguments.to_string())
}

/// runs the tool against a bounded range service; native tests use an in-memory service.
pub fn read_with(
    arguments: &str,
    mut read: impl FnMut(&str, &str, u64, u32) -> Result<(Vec<u8>, bool), String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.offset == 0 || args.limit == 0 {
        return Err("offset and limit must be positive integers".into());
    }
    // No line numbers: exact text remains suitable for an edit operation (D51).
    let last = args.offset.saturating_add(args.limit.min(PAGE_LINES) - 1);
    let mut line = 1u64;
    let mut offset = 0u64;
    let mut output = Vec::new();
    let mut clipped = false;
    let mut line_start = true;
    loop {
        if offset >= SCAN_BYTES {
            return Err("read would scan more than 1 MiB; request a nearer line offset or use a byte-range tool".into());
        }
        let (bytes, eof) = read(&args.root, &args.path, offset, CHUNK_BYTES)?;
        if bytes.len() > CHUNK_BYTES as usize || (bytes.is_empty() && !eof) {
            return Err("invalid reply from range service".into());
        }
        offset += bytes.len() as u64;
        for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
            if line > last {
                return finish(output, args.offset, line - 1, true, clipped);
            }
            if line >= args.offset {
                let room = PAGE_BYTES.saturating_sub(output.len());
                if room == 0 && line_start {
                    return finish(output, args.offset, line - 1, true, false);
                }
                output.extend_from_slice(&segment[..segment.len().min(room)]);
                if segment.len() > room {
                    clipped = true;
                }
                if clipped {
                    // The current oversized line is deliberately truncated, like native read.
                    return finish(output, args.offset, line, true, true);
                }
            }
            if segment.ends_with(b"\n") {
                line += 1;
                line_start = true;
            } else {
                line_start = false;
            }
        }
        if eof {
            if output.is_empty() {
                return Ok(if offset == 0 {
                    "the file is empty".into()
                } else {
                    format!("offset {} is past the end of the file", args.offset)
                });
            }
            return finish(output, args.offset, line, false, clipped);
        }
    }
}

fn finish(
    mut bytes: Vec<u8>,
    first: u64,
    last: u64,
    more: bool,
    clipped: bool,
) -> Result<String, String> {
    if bytes.contains(&0) {
        return Err("file contains NUL bytes; this tool reads text only".into());
    }
    // A byte cap can split one UTF-8 scalar. Remove only that incomplete suffix, never invalid data.
    let text = loop {
        match String::from_utf8(bytes) {
            Ok(text) => break text,
            Err(error) if clipped && error.utf8_error().error_len().is_none() => {
                let end = error.utf8_error().valid_up_to();
                bytes = error.into_bytes();
                bytes.truncate(end);
            }
            Err(_) => return Err("file page is not UTF-8".into()),
        }
    };
    let mut text = text;
    if clipped {
        text.push_str("\n\n[page byte limit reached; the final line may be truncated]");
    }
    if more {
        text.push_str(&format!(
            "\n\n[showing lines {first}-{last}; call read again with offset={} for more]",
            last.saturating_add(1)
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(bytes: &[u8], arguments: &str) -> Result<String, String> {
        read_with(arguments, |root, path, offset, limit| {
            assert_eq!(root, "workspace");
            assert_eq!(path, "file");
            let start = (offset as usize).min(bytes.len());
            let end = (start + limit as usize).min(bytes.len());
            Ok((bytes[start..end].to_vec(), end == bytes.len()))
        })
    }

    #[test]
    fn whole_text_and_trailing_newlines_are_preserved() {
        for text in ["a\nb\n", "a\nb", "αβ\r\nlast\n"] {
            assert_eq!(page(text.as_bytes(), r#"{"path":"file"}"#).unwrap(), text);
        }
    }

    #[test]
    fn line_selection_and_continuation_are_guest_owned() {
        let text = page(
            b"one\ntwo\nthree\nfour\n",
            r#"{"path":"file","offset":2,"limit":2}"#,
        )
        .unwrap();
        assert!(text.starts_with("two\nthree\n"));
        assert!(text.contains("offset=4"));
        assert_eq!(
            page(b"one\ntwo", r#"{"path":"file","offset":2}"#).unwrap(),
            "two"
        );
    }

    #[test]
    fn pages_cross_host_chunks_without_losing_partial_lines_or_utf8() {
        let text = format!("{}é\nlast\n", "x".repeat(CHUNK_BYTES as usize - 1));
        assert_eq!(page(text.as_bytes(), r#"{"path":"file"}"#).unwrap(), text);
        let large = "hello\n".repeat(20_000);
        assert!(
            page(
                large.as_bytes(),
                r#"{"path":"file","offset":15000,"limit":1}"#
            )
            .unwrap()
            .starts_with("hello\n\n\n[showing")
        );
    }

    #[test]
    fn bounds_binary_errors_empty_files_and_invalid_arguments() {
        let text = page("é".repeat(PAGE_BYTES).as_bytes(), r#"{"path":"file"}"#).unwrap();
        assert!(text.len() < 64 * 1024);
        assert!(text.contains("truncated"));
        assert!(page(b"\xff", r#"{"path":"file"}"#).is_err());
        assert!(page(b"a\0b", r#"{"path":"file"}"#).is_err());
        assert_eq!(
            page(b"", r#"{"path":"file"}"#).unwrap(),
            "the file is empty"
        );
        for args in [
            r#"{"path":"file","limit":0}"#,
            r#"{"path":"file","offset":-1}"#,
            r#"{"path":"file","offset":0}"#,
            r#"{"path":"file","extra":true}"#,
        ] {
            assert!(page(b"text", args).is_err());
        }
    }

    #[test]
    fn exact_byte_and_line_caps_do_not_skip_the_next_line() {
        let text = format!("{}\ntail\n", "a".repeat(PAGE_BYTES - 1));
        let out = page(text.as_bytes(), r#"{"path":"file"}"#).unwrap();
        assert!(out.contains("offset=2"));
        assert!(!out.contains("truncated"));
        let out = page(
            "a\n".repeat(4000).as_bytes(),
            r#"{"path":"file","limit":9999}"#,
        )
        .unwrap();
        assert!(out.contains("offset=2001"));
    }

    #[test]
    fn scanning_and_host_replies_are_bounded_and_failures_are_preserved() {
        let out = page(
            &vec![b'x'; SCAN_BYTES as usize + 1],
            r#"{"path":"file","offset":2}"#,
        );
        assert!(out.unwrap_err().contains("scan more than 1 MiB"));
        assert!(read_with(r#"{"path":"file"}"#, |_, _, _, _| Ok((Vec::new(), false))).is_err());
        assert!(
            read_with(r#"{"path":"file"}"#, |_, _, _, _| Ok((
                vec![b'x'; CHUNK_BYTES as usize + 1],
                true
            )))
            .is_err()
        );
        let out = read_with(r#"{"root":"scratch","path":"file"}"#, |root, _, _, _| {
            assert_eq!(root, "scratch");
            Err("denied by host".into())
        });
        assert_eq!(out.unwrap_err(), "denied by host");
    }
}
