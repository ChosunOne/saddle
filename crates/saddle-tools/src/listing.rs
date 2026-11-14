//! directory listing presentation over a bounded, policy-filtered metadata service (D132).

use saddle_sdk::{DirectoryPage, EntryKind};
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, FilePath, Output, Tool, read_directory};
#[cfg(target_arch = "wasm32")]
use serde_json::json;

#[cfg(target_arch = "wasm32")]
use crate::normalize_path;
use crate::workspace;

const MAX_LIST: usize = 500;
const PAGE_ENTRIES: u32 = 128;
const MAX_TEXT: usize = 50 * 1024;

/// the standard listing tool; all names come from the host's filtered directory pages.
pub struct Ls;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    #[serde(default = "workspace")]
    root: String,
    #[serde(default = "dot")]
    path: String,
}
fn dot() -> String {
    ".".into()
}

#[cfg(target_arch = "wasm32")]
impl Tool for Ls {
    fn describe() -> Definition {
        Definition {
            name: "ls".into(),
            description: "list a directory, sorted by name, with / for directories and @ for symlinks. Path defaults to workspace; accepts absolute paths inside granted roots or explicit root-relative paths. At most 500 entries and 50 KiB. Excluded names are filtered by the host.".into(),
            input_schema: json!({"type":"object","properties":{"root":{"type":"string"},"path":{"type":"string"}},"additionalProperties":false}).to_string(),
        }
    }
    fn invoke(arguments: String) -> Output {
        let result = normalize_path(&arguments, Some(".")).and_then(|arguments| {
            list_with(&arguments, |root, path, after, limit| {
                read_directory(
                    &FilePath {
                        root: root.into(),
                        relative: path.into(),
                    },
                    after,
                    limit,
                )
                .map_err(|error| format!("listing failed: {error:?}"))
            })
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

/// formats bounded pages; native tests substitute metadata without filesystem authority.
pub fn list_with(
    arguments: &str,
    mut read: impl FnMut(&str, &str, &str, u32) -> Result<DirectoryPage, String>,
) -> Result<String, String> {
    let args: Arguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    let mut after = String::new();
    let mut names = Vec::new();
    let mut total = 0;
    let mut skipped = 0;
    while names.len() < MAX_LIST {
        let page = read(&args.root, &args.path, &after, PAGE_ENTRIES)?;
        if page.entries.len() > PAGE_ENTRIES as usize || (page.more && page.entries.is_empty()) {
            return Err("invalid reply from directory service".into());
        }
        total = page.total;
        skipped = page.skipped;
        for entry in page.entries {
            if entry.name <= after
                || entry.name.len() > 1024
                || entry.name.contains(['/', '\0'])
                || matches!(entry.name.as_str(), "." | "..")
            {
                return Err("invalid directory entry or non-advancing cursor".into());
            }
            after = entry.name;
            let suffix = match entry.kind {
                EntryKind::Directory => "/",
                EntryKind::Symlink => "@",
                _ => "",
            };
            if names.len() < MAX_LIST {
                names.push(format!("{after}{suffix}"));
            }
        }
        if !page.more {
            break;
        }
    }
    names.sort();
    let mut text = String::new();
    let mut shown = 0;
    for name in names {
        if text.len() + name.len() + 1 > MAX_TEXT {
            break;
        }
        if shown != 0 {
            text.push('\n');
        }
        text.push_str(&name);
        shown += 1;
    }
    if shown == 0 {
        text.push_str("(empty)");
    }
    if total as usize > shown {
        text.push_str(&format!("\n(first {shown} of {total} entries)"));
    }
    if skipped == 1 {
        text.push_str(
            "\n(1 path skipped: the policy excludes it or its name is not representable)",
        );
    } else if skipped > 1 {
        text.push_str(&format!("\n({skipped} paths skipped: the policy excludes them or their names are not representable)"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use saddle_sdk::DirectoryEntry;

    fn page(entries: Vec<DirectoryEntry>, total: u32, more: bool) -> DirectoryPage {
        DirectoryPage {
            entries,
            total,
            more,
            skipped: 0,
        }
    }
    fn entry(name: &str, kind: EntryKind) -> DirectoryEntry {
        DirectoryEntry {
            name: name.into(),
            kind,
        }
    }
    #[test]
    fn names_are_sorted_and_types_are_presented_without_following_links() {
        let out = list_with("{}", |root, path, after, limit| {
            assert_eq!((root, path, after, limit), ("workspace", ".", "", 128));
            Ok(page(
                vec![
                    entry("a", EntryKind::Directory),
                    entry("b", EntryKind::Symlink),
                    entry("c", EntryKind::File),
                ],
                3,
                false,
            ))
        })
        .unwrap();
        assert_eq!(out, "a/\nb@\nc");
    }
    #[test]
    fn pagination_advances_and_stops_at_five_hundred_entries() {
        let mut calls = 0;
        let out = list_with("{}", |_, _, after, _| {
            let start = if after.is_empty() {
                0
            } else {
                after.parse::<u32>().unwrap() + 1
            };
            calls += 1;
            Ok(page(
                (start..start + 128)
                    .map(|n| entry(&format!("{n:04}"), EntryKind::File))
                    .collect(),
                1000,
                true,
            ))
        })
        .unwrap();
        assert_eq!(calls, 4);
        assert!(out.contains("first 500 of 1000"));
        assert!(!out.contains("0500"));
    }
    #[test]
    fn malformed_pages_errors_and_exclusions_do_not_turn_into_names() {
        assert_eq!(
            list_with("{}", |_, _, _, _| Ok(page(Vec::new(), 0, false))).unwrap(),
            "(empty)"
        );
        assert!(list_with("{}", |_, _, _, _| Ok(page(Vec::new(), 10, true))).is_err());
        assert!(
            list_with("{}", |_, _, _, _| Ok(page(
                vec![entry("../secret", EntryKind::File)],
                1,
                false
            )))
            .is_err()
        );
        assert!(list_with("{}", |_, _, _, _| Err("denied".into())).is_err());
        let out = list_with("{}", |_, _, _, _| {
            Ok(DirectoryPage {
                entries: Vec::new(),
                total: 0,
                more: false,
                skipped: 1,
            })
        })
        .unwrap();
        assert!(out.contains("1 path skipped"));
    }
}
