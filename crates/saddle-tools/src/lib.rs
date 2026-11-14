//! standard tool implementations; the core owns effects, guests own presentation (D131).

pub mod listing;
mod pagination;
pub mod search;
pub mod transcript;
pub use pagination::read_with;

use saddle_sdk::ImageInfo;
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, FilePath, Output, Tool, emit_image, read_range, resolve_path};
#[cfg(target_arch = "wasm32")]
use serde_json::{Value, json};

const CHUNK_BYTES: u32 = 8192;
const PAGE_BYTES: usize = 50 * 1024;
const PAGE_LINES: u64 = 2000;
const SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// a model-facing reader; importing it does not grant any filesystem access.
pub struct Read;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default = "workspace")]
    root: String,
    path: String,
    #[serde(default)]
    format: ReadFormat,
    #[serde(default = "first_line")]
    offset: u64,
    #[serde(default = "page_lines")]
    limit: u64,
}

#[derive(Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum ReadFormat {
    #[default]
    Text,
    Image,
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
            description: "read a file beneath a granted root in the explicitly selected format. Format defaults to text (UTF-8); use image for PNG/JPEG/GIF/WebP attachments. No automatic detection or fallback between modes. Path is workspace-relative or absolute unless root is provided. Offset is a 1-based text line; text pages have at most 2000 lines and 50 KiB with total/continuation notes. Text scans are limited to 64 MiB; images to 5 MiB.".into(),
            input_schema: json!({
                "type":"object", "properties": {
                    "root":{"type":"string"}, "path":{"type":"string"},
                    "format":{"type":"string","enum":["text","image"],"default":"text"},
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
        let result = read_with_media(
            &arguments,
            |root, path, offset, limit| {
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
            },
            |name, bytes| emit_image(name, bytes),
        );
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

/// reads the requested format without detection or fallback between text and images (D136).
pub fn read_with_media(
    arguments: &str,
    mut read: impl FnMut(&str, &str, u64, u32) -> Result<(Vec<u8>, bool), String>,
    mut emit: impl FnMut(&str, &[u8]) -> Result<ImageInfo, String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.format == ReadFormat::Text {
        return read_with(arguments, read);
    }
    if args.offset == 0 || args.limit == 0 {
        return Err("offset and limit must be positive integers".into());
    }
    let (mut bytes, mut eof) = read(&args.root, &args.path, 0, CHUNK_BYTES)?;
    if bytes.len() > CHUNK_BYTES as usize || (bytes.is_empty() && !eof) {
        return Err("invalid reply from range service".into());
    }
    while !eof {
        let want = (MAX_IMAGE_BYTES - bytes.len()).min(65536) as u32;
        if want == 0 {
            return Err("image is over the 5120 KiB limit".into());
        }
        let (chunk, end) = read(&args.root, &args.path, bytes.len() as u64, want)?;
        if chunk.len() > want as usize || (chunk.is_empty() && !end) {
            return Err("invalid reply from range service".into());
        }
        bytes.extend_from_slice(&chunk);
        eof = end;
    }
    let name = args.path.rsplit('/').next().unwrap_or("image");
    let info = emit(name, &bytes)?;
    Ok(format!(
        "{} is an image: {}×{}, {}",
        args.path, info.width, info.height, info.media_type
    ))
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
            .starts_with("hello\n\n[showing")
        );
    }

    #[test]
    fn bounds_binary_errors_empty_files_and_invalid_arguments() {
        let text = page("é".repeat(PAGE_BYTES).as_bytes(), r#"{"path":"file"}"#).unwrap();
        assert!(text.len() < 64 * 1024);
        assert!(text.contains("bytes; the first"));
        assert!(page(b"\xff", r#"{"path":"file"}"#).is_err());
        assert!(page(b"a\0b", r#"{"path":"file"}"#).is_err());
        assert_eq!(
            page(b"", r#"{"path":"file"}"#).unwrap(),
            "\n\nthe file is empty"
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
        assert!(out.unwrap_err().contains("scan more than 64 MiB"));
        assert!(read_with(r#"{"path":"file"}"#, |_, _, _, _| Ok((Vec::new(), false))).is_err());
        assert!(
            read_with(r#"{"path":"file"}"#, |_, _, _, _| Ok((
                vec![b'x'; 65537],
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

    #[test]
    fn format_is_explicit_defaults_to_text_and_never_falls_back() {
        for arguments in [
            r#"{"path":"notes.png"}"#,
            r#"{"path":"notes.png","format":"text"}"#,
        ] {
            let text = read_with_media(
                arguments,
                |_, _, _, _| Ok((b"GIF89a is also a valid text prefix\n".to_vec(), true)),
                |_, _| panic!("text mode must never emit an image"),
            )
            .unwrap();
            assert_eq!(text, "GIF89a is also a valid text prefix\n");
        }
        let error = read_with_media(
            r#"{"path":"picture.png"}"#,
            |_, _, _, _| Ok((b"\x89PNG\r\n\x1a\n".to_vec(), true)),
            |_, _| panic!("invalid text must not switch to image mode"),
        )
        .unwrap_err();
        assert!(error.contains("UTF-8"));
        let error = read_with_media(
            r#"{"path":"notes.png","format":"image"}"#,
            |_, _, _, _| Ok((b"plain text".to_vec(), true)),
            |_, bytes| {
                assert_eq!(bytes, b"plain text");
                Err("invalid image from host".into())
            },
        )
        .unwrap_err();
        assert_eq!(error, "invalid image from host");
        for arguments in [
            r#"{"path":"file","format":"auto"}"#,
            r#"{"path":"file","format":null}"#,
        ] {
            assert!(
                read_with_media(
                    arguments,
                    |_, _, _, _| panic!("invalid format must fail before reading"),
                    |_, _| panic!("invalid format must not emit an image"),
                )
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_images_are_assembled_through_bounded_ranges() {
        let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
        data.resize(100_000, 0);
        let mut emitted = 0;
        let text = read_with_media(
            r#"{"path":"misnamed.txt","format":"image"}"#,
            |_, _, offset, limit| {
                let start = offset as usize;
                let end = (start + limit as usize).min(data.len());
                Ok((data[start..end].to_vec(), end == data.len()))
            },
            |name, bytes| {
                assert_eq!(name, "misnamed.txt");
                assert_eq!(bytes, data);
                emitted += 1;
                Ok(ImageInfo {
                    media_type: "image/png".into(),
                    width: 1,
                    height: 2,
                })
            },
        )
        .unwrap();
        assert_eq!(emitted, 1);
        assert!(text.contains("1×2, image/png"));
        assert!(
            read_with_media(
                r#"{"path":"file","format":"image"}"#,
                |_, _, offset, limit| {
                    let mut bytes = vec![0; limit as usize];
                    if offset == 0 {
                        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
                    }
                    Ok((bytes, false))
                },
                |_, _| panic!("oversized images must not be emitted")
            )
            .unwrap_err()
            .contains("5120 KiB")
        );
    }
}
