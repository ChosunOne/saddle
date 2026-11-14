//! guest-side traversal, globs and regex matching; host services supply only scoped data (D133).

use globset::{GlobBuilder, GlobMatcher};
use regex::RegexBuilder;
use saddle_ignore::{FILES, Rules};
use saddle_sdk::{DirectoryPage, EntryKind};
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use crate::normalize_path;
use crate::workspace;
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, FilePath, Output, Tool, read_directory, read_range};
#[cfg(target_arch = "wasm32")]
use serde_json::json;

const MAX_VISITED: usize = 100_000;
const MAX_PENDING: usize = 2048;
const MAX_PATH_BYTES: usize = 1024 * 1024;
const MAX_TEXT: usize = 50 * 1024;
const MAX_FILE: usize = 4 * 1024 * 1024;
const MAX_IGNORES: usize = 128 * 1024;
const MAX_FOUND: usize = 1000;
const MAX_MATCHES: usize = 200;

pub struct Find;
pub struct Grep;

/// the guest has no native filesystem; tests provide in-memory data through the same contract.
pub trait Filesystem {
    fn directory(&mut self, root: &str, path: &str, after: &str) -> Result<DirectoryPage, String>;
    fn file(&mut self, root: &str, path: &str, maximum: usize) -> Result<FileRead, String>;
}

pub enum FileRead {
    Bytes(Vec<u8>),
    Unavailable,
    TooLarge,
}

#[cfg(target_arch = "wasm32")]
struct HostFilesystem;
#[cfg(target_arch = "wasm32")]
impl Filesystem for HostFilesystem {
    fn directory(&mut self, root: &str, path: &str, after: &str) -> Result<DirectoryPage, String> {
        read_directory(
            &FilePath {
                root: root.into(),
                relative: path.into(),
            },
            after,
            128,
        )
        .map_err(|error| format!("directory read failed: {error:?}"))
    }
    fn file(&mut self, root: &str, path: &str, maximum: usize) -> Result<FileRead, String> {
        let path = FilePath {
            root: root.into(),
            relative: path.into(),
        };
        let mut bytes = Vec::new();
        loop {
            let limit = (maximum - bytes.len()).min(65536) as u32;
            if limit == 0 {
                return Ok(FileRead::TooLarge);
            }
            let chunk = match read_range(&path, bytes.len() as u64, limit) {
                Ok(chunk) => chunk,
                Err(_) => return Ok(FileRead::Unavailable),
            };
            if chunk.bytes.len() > limit as usize || (chunk.bytes.is_empty() && !chunk.eof) {
                return Err("invalid reply from range service".into());
            }
            bytes.extend_from_slice(&chunk.bytes);
            if chunk.eof {
                return Ok(FileRead::Bytes(bytes));
            }
        }
    }
}

fn dot() -> String {
    ".".into()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArguments {
    #[serde(default = "workspace")]
    root: String,
    #[serde(default = "dot")]
    path: String,
    pattern: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArguments {
    #[serde(default = "workspace")]
    root: String,
    #[serde(default = "dot")]
    path: String,
    pattern: String,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
}

#[cfg(target_arch = "wasm32")]
fn output(result: Result<String, String>) -> Output {
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
#[cfg(target_arch = "wasm32")]
impl Tool for Find {
    fn describe() -> Definition {
        Definition { name: "find".into(), description: "find files using a glob under a granted directory. A glob without / matches filenames at any depth; one with / matches paths from the search directory. Honors .gitignore, .ignore and .fdignore; never follows symlinks. At most 1000 results and 50 KiB.".into(), input_schema: json!({"type":"object","properties":{"root":{"type":"string"},"path":{"type":"string"},"pattern":{"type":"string"}},"required":["pattern"],"additionalProperties":false}).to_string() }
    }
    fn invoke(arguments: String) -> Output {
        output(
            normalize_path(&arguments, Some("."))
                .and_then(|arguments| find_with(&arguments, &mut HostFilesystem)),
        )
    }
}
#[cfg(target_arch = "wasm32")]
impl Tool for Grep {
    fn describe() -> Definition {
        Definition { name: "grep".into(), description: "search UTF-8 files with a regular expression, optional filename/path glob and ignore_case. Returns path:line: text. Honors ignore files, skips .git, symlinks, binary/unreadable files and files above 4 MiB. At most 200 matches and 50 KiB; search a subdirectory for large trees.".into(), input_schema: json!({"type":"object","properties":{"root":{"type":"string"},"path":{"type":"string"},"pattern":{"type":"string"},"glob":{"type":"string"},"ignore_case":{"type":"boolean"}},"required":["pattern"],"additionalProperties":false}).to_string() }
    }
    fn invoke(arguments: String) -> Output {
        output(
            normalize_path(&arguments, Some("."))
                .and_then(|arguments| grep_with(&arguments, &mut HostFilesystem)),
        )
    }
}

fn relative(path: &str) -> Result<String, String> {
    if path.starts_with('/') {
        return Err("path must be relative to its root".into());
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err("path escapes its root".into());
                }
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}
fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.into()
    } else {
        format!("{base}/{name}")
    }
}
fn glob(pattern: &str) -> Result<(GlobMatcher, bool), String> {
    let by_path = pattern.contains('/');
    let glob = GlobBuilder::new(pattern)
        .literal_separator(by_path)
        .build()
        .map_err(|error| format!("bad glob: {error}"))?;
    Ok((glob.compile_matcher(), by_path))
}
fn matches(glob: &(GlobMatcher, bool), path: &str, start: &str) -> bool {
    let path = if glob.1 {
        path.strip_prefix(start)
            .unwrap_or(path)
            .trim_start_matches('/')
    } else {
        path.rsplit('/').next().unwrap_or(path)
    };
    glob.0.is_match(path)
}
fn shown(root: &str, path: &str) -> String {
    if root == "workspace" || root.starts_with("@tool:") {
        path.into()
    } else {
        format!("{root}:{path}")
    }
}

#[derive(Default)]
struct Walk {
    skipped: u32,
    tree_limit: bool,
    result_limit: bool,
}
fn ignores(
    fs: &mut impl Filesystem,
    root: &str,
    base: &str,
    rules: &mut Rules,
    budget: &mut usize,
) -> Result<(), String> {
    for name in FILES {
        match fs.file(root, &join(base, name), 65536)? {
            FileRead::Bytes(bytes) => {
                *budget += bytes.len();
                if *budget > MAX_IGNORES {
                    return Err(
                        "ignore files exceed the 128 KiB parsing budget; search a subdirectory"
                            .into(),
                    );
                }
                if let Ok(text) = String::from_utf8(bytes) {
                    rules.add(&text, base);
                }
            }
            FileRead::TooLarge => {
                return Err("an ignore file exceeds 64 KiB; narrow the search".into());
            }
            FileRead::Unavailable => {}
        }
    }
    Ok(())
}

fn walk<F: Filesystem>(
    fs: &mut F,
    root: &str,
    start: &str,
    mut visit: impl FnMut(&mut F, &str) -> Result<bool, String>,
) -> Result<Walk, String> {
    let mut rules = Rules::default();
    let mut ignore_bytes = 0;
    if !start.is_empty() {
        ignores(fs, root, "", &mut rules, &mut ignore_bytes)?;
        let mut parent = String::new();
        let parts: Vec<_> = start.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            parent = join(&parent, part);
            ignores(fs, root, &parent, &mut rules, &mut ignore_bytes)?;
        }
    }
    let mut result = Walk::default();
    let mut stack = vec![start.to_owned()];
    let mut queued_bytes = start.len();
    let mut visited = 0;
    while let Some(directory) = stack.pop() {
        queued_bytes -= directory.len();
        ignores(fs, root, &directory, &mut rules, &mut ignore_bytes)?;
        let mut after = String::new();
        let mut children = Vec::new();
        let mut child_bytes = 0;
        let mut observed_skips = 0;
        loop {
            let page = fs.directory(root, &directory, &after)?;
            if page.entries.len() > 128 || (page.more && page.entries.is_empty()) {
                return Err("invalid directory page".into());
            }
            result.skipped = result
                .skipped
                .saturating_add(page.skipped.saturating_sub(observed_skips));
            observed_skips = observed_skips.max(page.skipped);
            for entry in page.entries {
                if entry.name <= after
                    || entry.name.contains(['/', '\0'])
                    || matches!(entry.name.as_str(), "." | "..")
                    || entry.name.len() > 1024
                {
                    return Err("invalid directory cursor or entry".into());
                }
                after = entry.name;
                visited += 1;
                if visited > MAX_VISITED {
                    result.tree_limit = true;
                    return Ok(result);
                }
                if after == ".git" || matches!(entry.kind, EntryKind::Symlink | EntryKind::Other) {
                    continue;
                }
                let path = join(&directory, &after);
                let is_dir = matches!(entry.kind, EntryKind::Directory);
                if rules.ignored(&path, is_dir) {
                    continue;
                }
                if is_dir {
                    if stack.len() + children.len() >= MAX_PENDING
                        || queued_bytes + child_bytes + path.len() > MAX_PATH_BYTES
                    {
                        result.tree_limit = true;
                        return Ok(result);
                    }
                    child_bytes += path.len();
                    children.push(path);
                } else if !visit(fs, &path)? {
                    result.result_limit = true;
                    return Ok(result);
                }
            }
            if !page.more {
                break;
            }
        }
        queued_bytes += child_bytes;
        stack.extend(children.into_iter().rev());
    }
    Ok(result)
}
fn notes(text: &mut String, walked: &Walk) {
    if walked.tree_limit {
        text.push_str("\n(the tree is too large; search a subdirectory)");
    }
    if walked.skipped > 0 {
        text.push_str(&format!(
            "\n({} paths skipped by policy or unrepresentable names)",
            walked.skipped
        ));
    }
}

/// finds names using only metadata and ignore-file bytes from the supplied service.
pub fn find_with(arguments: &str, fs: &mut impl Filesystem) -> Result<String, String> {
    let args: FindArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.pattern.is_empty() {
        return Err("pattern must not be empty".into());
    }
    let start = relative(&args.path)?;
    let pattern = glob(&args.pattern)?;
    let mut text = String::new();
    let mut found = 0;
    let walked = walk(fs, &args.root, &start, |_, path| {
        if !matches(&pattern, path, &start) {
            return Ok(true);
        }
        let path = shown(&args.root, path);
        if found >= MAX_FOUND || text.len() + path.len() + 1 > MAX_TEXT {
            return Ok(false);
        }
        if found > 0 {
            text.push('\n');
        }
        text.push_str(&path);
        found += 1;
        Ok(true)
    })?;
    if found == 0 {
        text = format!("no files match `{}`", args.pattern);
    }
    if walked.result_limit {
        text.push_str("\n(result limit reached; narrow the pattern or path)");
    }
    notes(&mut text, &walked);
    Ok(text)
}

/// searches bounded UTF-8 file contents; regular expressions and line formatting remain in guest code.
pub fn grep_with<F: Filesystem>(arguments: &str, fs: &mut F) -> Result<String, String> {
    let args: GrepArguments =
        serde_json::from_str(arguments).map_err(|error| format!("invalid arguments: {error}"))?;
    if args.pattern.is_empty() {
        return Err("pattern must not be empty".into());
    }
    let start = relative(&args.path)?;
    let regex = RegexBuilder::new(&args.pattern)
        .case_insensitive(args.ignore_case)
        .size_limit(1 << 20)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|error| format!("bad pattern: {error}"))?;
    let filter = args.glob.as_deref().map(glob).transpose()?;
    let mut text = String::new();
    let mut count = 0;
    let mut files = 0;
    let mut inspect =
        |fs: &mut F, path: &str, contents: Option<FileRead>| -> Result<bool, String> {
            if filter
                .as_ref()
                .is_some_and(|filter| !matches(filter, path, &start))
            {
                return Ok(true);
            }
            let contents = match contents {
                Some(contents) => contents,
                None => fs.file(&args.root, path, MAX_FILE)?,
            };
            let FileRead::Bytes(bytes) = contents else {
                return Ok(true);
            };
            if bytes.len() > MAX_FILE || bytes.iter().take(8192).any(|byte| *byte == 0) {
                return Ok(true);
            }
            let Ok(contents) = String::from_utf8(bytes) else {
                return Ok(true);
            };
            let mut matched = false;
            for (number, line) in contents.lines().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                let line = line.trim_end();
                let mut end = line.len().min(300);
                while !line.is_char_boundary(end) {
                    end -= 1;
                }
                let suffix = if end < line.len() { "…" } else { "" };
                let row = format!(
                    "{}:{}: {}{suffix}",
                    shown(&args.root, path),
                    number + 1,
                    &line[..end]
                );
                if count >= MAX_MATCHES || text.len() + row.len() + 1 > MAX_TEXT {
                    return Ok(false);
                }
                if count > 0 {
                    text.push('\n');
                }
                text.push_str(&row);
                count += 1;
                matched = true;
            }
            files += usize::from(matched);
            Ok(true)
        };
    let direct = if start.is_empty() {
        FileRead::Unavailable
    } else {
        fs.file(&args.root, &start, MAX_FILE)?
    };
    let walked = match direct {
        FileRead::Bytes(_) | FileRead::TooLarge => Walk {
            result_limit: !inspect(fs, &start, Some(direct))?,
            ..Walk::default()
        },
        FileRead::Unavailable => walk(fs, &args.root, &start, |fs, path| inspect(fs, path, None))?,
    };
    if count == 0 {
        text = format!("no matches for `{}`", args.pattern);
    }
    if walked.result_limit {
        text.push_str("\n(match or byte limit reached; narrow the pattern, path or glob)");
    } else if count > 0 {
        text.push_str(&format!("\n({count} matches in {files} files)"));
    }
    notes(&mut text, &walked);
    Ok(text)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use saddle_sdk::DirectoryEntry;

    use super::*;

    #[derive(Default)]
    struct Memory {
        files: BTreeMap<String, Vec<u8>>,
        links: Vec<String>,
    }
    impl Memory {
        fn put(&mut self, path: &str, text: &str) {
            self.files.insert(path.into(), text.as_bytes().to_vec());
        }
    }
    impl Filesystem for Memory {
        fn directory(
            &mut self,
            root: &str,
            path: &str,
            after: &str,
        ) -> Result<DirectoryPage, String> {
            assert_eq!(root, "workspace");
            if self.files.contains_key(path) {
                return Err("not a directory".into());
            }
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{path}/")
            };
            let mut names = BTreeMap::new();
            for key in self.files.keys() {
                if let Some(rest) = key.strip_prefix(&prefix) {
                    let name = rest.split('/').next().unwrap();
                    names.insert(
                        name.to_owned(),
                        if rest.contains('/') {
                            EntryKind::Directory
                        } else {
                            EntryKind::File
                        },
                    );
                }
            }
            for key in &self.links {
                if let Some(rest) = key.strip_prefix(&prefix)
                    && !rest.contains('/')
                {
                    names.insert(rest.into(), EntryKind::Symlink);
                }
            }
            let total = names.len() as u32;
            let mut entries: Vec<_> = names
                .into_iter()
                .filter(|(name, _)| name.as_str() > after)
                .map(|(name, kind)| DirectoryEntry { name, kind })
                .collect();
            let more = entries.len() > 128;
            entries.truncate(128);
            Ok(DirectoryPage {
                entries,
                total,
                more,
                skipped: 0,
            })
        }
        fn file(&mut self, root: &str, path: &str, maximum: usize) -> Result<FileRead, String> {
            assert_eq!(root, "workspace");
            Ok(match self.files.get(path) {
                Some(bytes) if bytes.len() > maximum => FileRead::TooLarge,
                Some(bytes) => FileRead::Bytes(bytes.clone()),
                None => FileRead::Unavailable,
            })
        }
    }

    #[test]
    fn glob_depth_ignore_files_and_symlinks_match_native_search_rules() {
        let mut fs = Memory::default();
        fs.put(".gitignore", "*.tmp\nignored/\n");
        fs.put(".ignore", "*.log\n");
        fs.put(".fdignore", "*.bak\n");
        fs.put("src/.gitignore", "hidden.rs\n");
        for path in [
            "main.rs",
            "src/lib.rs",
            "src/hidden.rs",
            "src/x.tmp",
            "ignored/secret.rs",
            ".git/config",
            "x.log",
            "x.bak",
        ] {
            fs.put(path, "hello");
        }
        fs.links.push("link.rs".into());
        assert_eq!(
            find_with(r#"{"pattern":"*.rs"}"#, &mut fs).unwrap(),
            "main.rs\nsrc/lib.rs"
        );
        assert_eq!(
            find_with(r#"{"pattern":"src/*.rs"}"#, &mut fs).unwrap(),
            "src/lib.rs"
        );
        assert_eq!(
            find_with(r#"{"path":"src","pattern":"*.rs"}"#, &mut fs).unwrap(),
            "src/lib.rs"
        );
        let out = find_with(r#"{"pattern":"*.log"}"#, &mut fs).unwrap();
        assert!(out.starts_with("no files match"));
    }

    #[test]
    fn regex_case_globs_line_numbers_and_explicit_files_are_guest_owned() {
        let mut fs = Memory::default();
        fs.put("a.rs", "first\nHELLO\nhello world\n");
        fs.put("b.txt", "hello");
        let out = grep_with(
            r#"{"pattern":"^hello","ignore_case":true,"glob":"*.rs"}"#,
            &mut fs,
        )
        .unwrap();
        assert!(out.contains("a.rs:2: HELLO"));
        assert!(out.contains("a.rs:3: hello world"));
        assert!(!out.contains("b.txt"));
        assert!(out.contains("2 matches in 1 files"));
        fs.put(".gitignore", "a.rs");
        assert!(
            grep_with(r#"{"path":"a.rs","pattern":"HELLO"}"#, &mut fs)
                .unwrap()
                .contains("a.rs:2: HELLO")
        );
        assert!(grep_with(r#"{"pattern":"["}"#, &mut fs).is_err());
    }

    #[test]
    fn results_files_ignore_data_and_binary_content_are_bounded() {
        let mut fs = Memory::default();
        for n in 0..1100 {
            fs.put(&format!("{n:04}.rs"), "hit");
        }
        let out = find_with(r#"{"pattern":"*.rs"}"#, &mut fs).unwrap();
        assert!(out.contains("result limit reached"));
        assert!(!out.contains("1000.rs"));
        let out = grep_with(r#"{"pattern":"hit"}"#, &mut fs).unwrap();
        assert!(out.contains("match or byte limit reached"));
        assert!(out.len() < 65536);
        let mut fs = Memory::default();
        fs.files.insert("binary".into(), b"hit\0".to_vec());
        fs.files.insert("invalid".into(), vec![255]);
        fs.files.insert("large".into(), vec![b'x'; MAX_FILE + 1]);
        assert!(
            grep_with(r#"{"pattern":"hit"}"#, &mut fs)
                .unwrap()
                .starts_with("no matches")
        );
        fs.put(".gitignore", &"x".repeat(65537));
        assert!(
            find_with(r#"{"pattern":"*"}"#, &mut fs)
                .unwrap_err()
                .contains("ignore file exceeds")
        );
    }
}
