//! mutable tool workflows over the same runtime-gated filesystem SDK (D140).

use saddle_sdk::{Change, Expectation};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, FilePath, Output, Tool, apply_changes, read_range, resolve_path};
#[cfg(target_arch = "wasm32")]
use serde_json::json;

use crate::patch::{self, Op};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_CHANGES: usize = 32;

pub struct Write;
pub struct Edit;
pub struct Patch;

pub trait Filesystem {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String>;
    fn apply(&mut self, changes: &[Change]) -> Result<(), String>;
}

#[cfg(target_arch = "wasm32")]
struct HostFilesystem;
#[cfg(target_arch = "wasm32")]
impl Filesystem for HostFilesystem {
    fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let path = resolve_path(path).map_err(|error| format!("resolve failed: {error:?}"))?;
        let path = FilePath {
            root: path.root,
            relative: path.relative,
        };
        let mut bytes = Vec::new();
        loop {
            let want = (MAX_BYTES - bytes.len()).min(65536) as u32;
            if want == 0 {
                return Err("mutation input file exceeds 1 MiB".into());
            }
            let chunk = read_range(&path, bytes.len() as u64, want)
                .map_err(|error| format!("read failed: {error:?}"))?;
            if chunk.bytes.len() > want as usize || (chunk.bytes.is_empty() && !chunk.eof) {
                return Err("invalid range reply".into());
            }
            bytes.extend_from_slice(&chunk.bytes);
            if chunk.eof {
                return Ok(bytes);
            }
        }
    }
    fn apply(&mut self, changes: &[Change]) -> Result<(), String> {
        apply_changes(changes)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArguments {
    path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArguments {
    path: String,
    old: String,
    new: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchArguments {
    patch: String,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn submit(fs: &mut impl Filesystem, changes: &[Change]) -> Result<(), String> {
    if changes.is_empty()
        || changes.len() > MAX_CHANGES
        || changes
            .iter()
            .any(|change| change.path.is_empty() || change.path.len() > 4096)
        || changes
            .iter()
            .map(|change| change.content.as_ref().map_or(0, Vec::len))
            .sum::<usize>()
            > MAX_BYTES
    {
        return Err("mutation exceeds 32 paths/1 MiB or has an invalid path".into());
    }
    fs.apply(changes)
}

pub fn write_with(arguments: &str, fs: &mut impl Filesystem) -> Result<String, String> {
    let args: WriteArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let lines = args.content.lines().count();
    let bytes = args.content.len();
    submit(
        fs,
        &[Change {
            path: args.path.clone(),
            content: Some(args.content.into_bytes()),
            expected: Expectation::Any,
        }],
    )?;
    Ok(format!(
        "wrote {} ({bytes} bytes, {lines} lines)",
        args.path
    ))
}

pub fn edit_with(arguments: &str, fs: &mut impl Filesystem) -> Result<String, String> {
    let args: EditArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.old.is_empty() {
        return Err("old must not be empty".into());
    }
    let bytes = fs.read(&args.path)?;
    if bytes.len() > MAX_BYTES {
        return Err("mutation input file exceeds 1 MiB".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "edit requires UTF-8 text")?;
    match text.matches(&args.old).count() {
        0 => return Err(format!("{}: old does not occur", args.path)),
        1 => {}
        n => {
            return Err(format!(
                "{}: old occurs {n} times; include more context",
                args.path
            ));
        }
    }
    let at = text.find(&args.old).ok_or("old does not occur")?;
    let line = text[..at].bytes().filter(|byte| *byte == b'\n').count() + 1;
    let updated = text.replacen(&args.old, &args.new, 1);
    submit(
        fs,
        &[Change {
            path: args.path.clone(),
            content: Some(updated.into_bytes()),
            expected: Expectation::Sha256(hash(text.as_bytes())),
        }],
    )?;
    Ok(format!("edited {} at line {line}", args.path))
}

pub fn patch_with(arguments: &str, fs: &mut impl Filesystem) -> Result<String, String> {
    let args: PatchArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let ops = patch::parse(&args.patch)?;
    let mut changes = Vec::new();
    let mut report = Vec::new();
    for op in ops {
        match op {
            Op::Add { path, text } => {
                report.push(format!("added {path}"));
                changes.push(Change {
                    path,
                    content: Some(text.into_bytes()),
                    expected: Expectation::Missing,
                });
            }
            Op::Delete { path } => {
                report.push(format!("deleted {path}"));
                changes.push(Change {
                    path,
                    content: None,
                    expected: Expectation::Any,
                });
            }
            Op::Update {
                path,
                destination,
                hunks,
            } => {
                let bytes = fs.read(&path)?;
                if bytes.len() > MAX_BYTES {
                    return Err("mutation input file exceeds 1 MiB".into());
                }
                let text = String::from_utf8(bytes).map_err(|_| "patch requires UTF-8 text")?;
                let updated = patch::apply(&text, &hunks)
                    .map_err(|error| format!("{path}: {error}; nothing was written"))?;
                let expected = Expectation::Sha256(hash(text.as_bytes()));
                if let Some(destination) = destination {
                    report.push(format!("moved {path} to {destination}"));
                    changes.push(Change {
                        path,
                        content: None,
                        expected,
                    });
                    changes.push(Change {
                        path: destination,
                        content: Some(updated.into_bytes()),
                        expected: Expectation::Missing,
                    });
                } else {
                    report.push(format!("updated {path}"));
                    changes.push(Change {
                        path,
                        content: Some(updated.into_bytes()),
                        expected,
                    });
                }
            }
        }
    }
    submit(fs, &changes)?;
    Ok(report.join(", "))
}

#[cfg(target_arch = "wasm32")]
macro_rules! tool {
    ($kind:ident, $name:literal, $description:literal, $properties:expr, $required:expr, $run:ident) => {
        impl Tool for $kind {
            fn describe() -> Definition {
                Definition { name: $name.into(), description: $description.into(), input_schema: json!({"type":"object","properties":$properties,"required":$required,"additionalProperties":false}).to_string() }
            }
            fn invoke(arguments: String) -> Output {
                match $run(&arguments, &mut HostFilesystem) {
                    Ok(text) => Output { text, is_error: false },
                    Err(text) => Output { text, is_error: true },
                }
            }
        }
    };
}
#[cfg(target_arch = "wasm32")]
tool!(
    Write,
    "write",
    "write UTF-8 content, creating parent directories or replacing a file. Requires runtime write permission, not read permission. Paths are workspace-relative, absolute or home-relative. A batch is limited to 1 MiB; tool arguments to 64 KiB. Protected paths and exclusions remain enforced.",
    json!({"path":{"type":"string"},"content":{"type":"string"}}),
    ["path", "content"],
    write_with
);
#[cfg(target_arch = "wasm32")]
tool!(
    Edit,
    "edit",
    "replace exactly one occurrence of old with new in a UTF-8 file. Read the file first; include enough context to make old unique. Requires runtime read and write permissions. Refuses stale contents. Input/result files are limited to 1 MiB; arguments to 64 KiB.",
    json!({"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}}),
    ["path", "old", "new"],
    edit_with
);
#[cfg(target_arch = "wasm32")]
tool!(
    Patch,
    "patch",
    "apply an apply-patch document across files: Begin Patch, Add File, Update File with @@ hunks and optional Move to, Delete File, End Patch. Every hunk must match exactly once. Add/move destinations must be absent. Updates require read and write; additions/deletions require write only. All targets and preconditions are checked before publication. One denial refuses the whole batch. At most 32 paths and 1 MiB of resulting contents. I/O failure during publication may partially apply; reported explicitly.",
    json!({"patch":{"type":"string"}}),
    ["patch"],
    patch_with
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Memory {
        files: BTreeMap<String, Vec<u8>>,
        stale: bool,
    }
    impl Filesystem for Memory {
        fn read(&mut self, path: &str) -> Result<Vec<u8>, String> {
            self.files.get(path).cloned().ok_or("missing file".into())
        }
        fn apply(&mut self, changes: &[Change]) -> Result<(), String> {
            for change in changes {
                if change.path == "denied" {
                    return Err("denied by core".into());
                }
                match &change.expected {
                    Expectation::Missing if self.files.contains_key(&change.path) => {
                        return Err("already exists".into());
                    }
                    Expectation::Sha256(want)
                        if self.stale
                            || self
                                .files
                                .get(&change.path)
                                .map(|bytes| hash(bytes))
                                .as_ref()
                                != Some(want) =>
                    {
                        return Err("file changed".into());
                    }
                    _ => {}
                }
            }
            for change in changes {
                if let Some(bytes) = &change.content {
                    self.files.insert(change.path.clone(), bytes.clone());
                } else {
                    self.files.remove(&change.path);
                }
            }
            Ok(())
        }
    }

    #[test]
    fn write_is_blind_and_edit_matches_exactly_once_and_guards_stale_contents() {
        let mut fs = Memory::default();
        write_with(r#"{"path":"file","content":"first\nold\n"}"#, &mut fs).unwrap();
        assert!(
            edit_with(r#"{"path":"file","old":"old","new":"new"}"#, &mut fs)
                .unwrap()
                .contains("line 2")
        );
        assert_eq!(fs.files["file"], b"first\nnew\n");
        fs.stale = true;
        assert!(
            edit_with(r#"{"path":"file","old":"new","new":"lost"}"#, &mut fs)
                .unwrap_err()
                .contains("changed")
        );
        assert_eq!(fs.files["file"], b"first\nnew\n");
        fs.files.insert("file".into(), b"same same".to_vec());
        assert!(
            edit_with(r#"{"path":"file","old":"same","new":"x"}"#, &mut fs)
                .unwrap_err()
                .contains("2 times")
        );
    }

    #[test]
    fn patches_plan_all_changes_before_submit_and_moves_guard_both_ends() {
        let mut fs = Memory::default();
        fs.files.insert("old".into(), b"before\n".to_vec());
        let args = json!({"patch":"*** Begin Patch\n*** Update File: old\n*** Move to: moved\n@@\n-before\n+after\n*** Add File: new\n+created\n*** End Patch"});
        patch_with(&args.to_string(), &mut fs).unwrap();
        assert!(!fs.files.contains_key("old"));
        assert_eq!(fs.files["moved"], b"after\n");
        let args = json!({"patch":"*** Add File: first\n+ok\n*** Add File: denied\n+bad\n"});
        assert!(patch_with(&args.to_string(), &mut fs).is_err());
        assert!(!fs.files.contains_key("first"));
    }
}
