//! ls, find and grep. Each call decides and logs one read of the path it starts from (D10).
//! Everything beneath that path is filtered through the same policy without a record per file, so
//! an excluded file is skipped rather than shown, and the result says how many were skipped.
//!
//! Directories are walked beneath their root with cap-std and symlinks are never followed. find
//! and grep also skip `.git` and whatever `.gitignore` files list.

use std::io::Read as _;
use std::path::Path;

use anyhow::Result;
use cap_std::fs::Dir;
use globset::{Glob, GlobBuilder, GlobMatcher};
use regex::RegexBuilder;
use saddle_cap::{Location, Principal, RelPath, RootName, Scope};
use saddle_policy::Request;
use serde_json::Value;

use super::ToolOutput;
use super::fs::{gate, open_root};
use crate::session::Session;

pub const MAX_LIST: usize = 500;
pub const MAX_FOUND: usize = 1000;
pub const MAX_MATCHES: usize = 200;
const MAX_LINE: usize = 300;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Entries visited before a walk gives up, so a huge tree cannot stall a call.
const MAX_VISITED: usize = 100_000;

/// One `.gitignore` line, matched against paths relative to the root.
struct Rule {
    glob: GlobMatcher,
    negate: bool,
    dir_only: bool,
}

#[derive(Default)]
struct Ignores {
    rules: Vec<Rule>,
}

impl Ignores {
    /// Adds the rules of `<base>/.gitignore`, if there is one.
    fn load(&mut self, root: &Dir, base: &str) {
        let file = if base.is_empty() {
            ".gitignore".to_owned()
        } else {
            format!("{base}/.gitignore")
        };
        let Ok(text) = root.read_to_string(&file) else {
            return;
        };
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negate, line) = match line.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            let (dir_only, line) = match line.strip_suffix('/') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            // A pattern with a slash is anchored to its .gitignore; one without matches at any
            // depth below it.
            let anchored = line.contains('/');
            let line = line.trim_start_matches('/');
            let prefix = if base.is_empty() {
                String::new()
            } else {
                format!("{base}/")
            };
            let pattern = if anchored {
                format!("{prefix}{line}")
            } else {
                format!("{prefix}**/{line}")
            };
            if let Ok(glob) = GlobBuilder::new(&pattern).literal_separator(true).build() {
                self.rules.push(Rule {
                    glob: glob.compile_matcher(),
                    negate,
                    dir_only,
                });
            }
        }
    }

    /// The last matching rule decides.
    fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for r in &self.rules {
            if (!r.dir_only || is_dir) && r.glob.is_match(rel) {
                ignored = !r.negate;
            }
        }
        ignored
    }
}

/// A file found by a walk: its path relative to the root.
struct Found {
    rel: String,
}

struct Walk {
    found: Vec<Found>,
    skipped_by_policy: usize,
    cut_short: bool,
}

/// Walks the files beneath `start` (a directory relative to the root), skipping `.git`, ignored
/// paths, symlinks, and any path `allowed` refuses. Stops after `limit` files.
fn walk(
    root: &Dir,
    start: &str,
    limit: usize,
    gitignore: bool,
    allowed: &dyn Fn(&str) -> bool,
) -> Walk {
    let mut ignores = Ignores::default();
    if gitignore {
        // Rules from the root down to the start apply too.
        let mut base = String::new();
        ignores.load(root, &base);
        for part in start.split('/').filter(|p| !p.is_empty()) {
            if !base.is_empty() {
                base.push('/');
            }
            base.push_str(part);
            ignores.load(root, &base);
        }
    }
    let mut out = Walk {
        found: Vec::new(),
        skipped_by_policy: 0,
        cut_short: false,
    };
    let mut stack = vec![start.to_owned()];
    let mut visited = 0;
    while let Some(dir) = stack.pop() {
        let entries = if dir.is_empty() {
            root.entries()
        } else {
            root.read_dir(&dir)
        };
        let Ok(entries) = entries else { continue };
        let mut names: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|e| {
                let kind = e.file_type().ok()?;
                if kind.is_symlink() {
                    return None;
                }
                Some((e.file_name().to_string_lossy().into_owned(), kind.is_dir()))
            })
            .collect();
        names.sort();
        let mut subdirs = Vec::new();
        for (name, is_dir) in names {
            visited += 1;
            if visited > MAX_VISITED {
                out.cut_short = true;
                return out;
            }
            if name == ".git" {
                continue;
            }
            let rel = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            if gitignore && ignores.ignored(&rel, is_dir) {
                continue;
            }
            if !allowed(&rel) {
                out.skipped_by_policy += 1;
                continue;
            }
            if is_dir {
                if gitignore {
                    ignores.load(root, &rel);
                }
                subdirs.push(rel);
            } else {
                if out.found.len() >= limit {
                    out.cut_short = true;
                    return out;
                }
                out.found.push(Found { rel });
            }
        }
        // Visit subdirectories in name order.
        stack.extend(subdirs.into_iter().rev());
    }
    out
}

/// The filter a walk applies: the agent's read grant on each path, without logging.
fn policy_filter<'a>(session: &'a Session, root: &'a RootName) -> impl Fn(&str) -> bool + 'a {
    move |rel| {
        // D66: a `[sandbox]` directory is not governed by a grant, so every entry beneath it is
        // in reach for the tools its entry lists, and only those.
        if session.is_tool_root(root) {
            return true;
        }
        let Ok(rel) = RelPath::new(rel) else {
            return false;
        };
        let loc = Location::new(root.clone(), rel);
        session
            .engine
            .decide(&Principal::Agent, &Request::FsRead { loc: &loc })
            .allow
    }
}

/// How a path is shown to the model: relative to the workspace when it is in it.
fn shown(session: &Session, root: &RootName, rel: &str) -> String {
    let base = session.base_of(root);
    match base {
        Some(base) => {
            let abs = base.join(rel);
            match abs.strip_prefix(&session.setup.workspace) {
                Ok(inside) if inside.as_os_str().is_empty() => ".".into(),
                Ok(inside) => inside.display().to_string(),
                Err(_) => abs.display().to_string(),
            }
        }
        None => rel.to_owned(),
    }
}

fn optional_str<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    input
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn skipped_note(n: usize) -> String {
    match n {
        0 => String::new(),
        1 => "\n(1 path skipped: the policy excludes it)".into(),
        n => format!("\n({n} paths skipped: the policy excludes them)"),
    }
}

pub fn ls(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let path = optional_str(input, "path").unwrap_or(".");
    let loc = match gate(session, "ls", Scope::FsRead, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(out),
    };
    let session = &*session;
    let root = match open_root(session, &loc) {
        Ok(d) => d,
        Err(e) => return Ok(ToolOutput::err(format!("{path}: {e}"))),
    };
    let dir_rel = loc.rel.as_slash();
    let entries = if loc.rel.is_root() {
        root.entries()
    } else {
        root.read_dir(loc.rel.to_path())
    };
    let entries = match entries {
        Ok(e) => e,
        Err(e) => return Ok(ToolOutput::err(format!("{path}: {e}"))),
    };
    let allowed = policy_filter(session, &loc.root);
    let mut names = Vec::new();
    let mut skipped = 0;
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if dir_rel.is_empty() {
            name.clone()
        } else {
            format!("{dir_rel}/{name}")
        };
        if !allowed(&rel) {
            skipped += 1;
            continue;
        }
        let suffix = match e.file_type() {
            Ok(t) if t.is_symlink() => "@",
            Ok(t) if t.is_dir() => "/",
            _ => "",
        };
        names.push(format!("{name}{suffix}"));
    }
    names.sort();
    let total = names.len();
    names.truncate(MAX_LIST);
    let mut text = if names.is_empty() {
        "(empty)".to_owned()
    } else {
        names.join("\n")
    };
    if total > MAX_LIST {
        text.push_str(&format!("\n(first {MAX_LIST} of {total} entries)"));
    }
    text.push_str(&skipped_note(skipped));
    Ok(ToolOutput::ok(text))
}

/// A glob for find and grep: one without a slash matches file names at any depth; one with a
/// slash matches the path from the search directory.
fn name_glob(pattern: &str) -> Result<(GlobMatcher, bool), String> {
    let by_path = pattern.contains('/');
    GlobBuilder::new(pattern)
        .literal_separator(by_path)
        .build()
        .map(|g: Glob| (g.compile_matcher(), by_path))
        .map_err(|e| format!("bad glob `{pattern}`: {e}"))
}

fn glob_matches(glob: &(GlobMatcher, bool), rel: &str, start: &str) -> bool {
    let (matcher, by_path) = glob;
    let from_start = rel
        .strip_prefix(start)
        .map(|r| r.trim_start_matches('/'))
        .unwrap_or(rel);
    if *by_path {
        matcher.is_match(from_start)
    } else {
        matcher.is_match(rel.rsplit('/').next().unwrap_or(rel))
    }
}

pub fn find(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let Some(pattern) = optional_str(input, "pattern") else {
        return Ok(ToolOutput::err("missing string field `pattern`"));
    };
    let glob = match name_glob(pattern) {
        Ok(g) => g,
        Err(e) => return Ok(ToolOutput::err(e)),
    };
    let path = optional_str(input, "path").unwrap_or(".");
    let loc = match gate(session, "find", Scope::FsRead, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(out),
    };
    let session = &*session;
    let root = match open_root(session, &loc) {
        Ok(d) => d,
        Err(e) => return Ok(ToolOutput::err(format!("{path}: {e}"))),
    };
    let start = loc.rel.as_slash();
    let allowed = policy_filter(session, &loc.root);
    let walked = walk(&root, &start, usize::MAX, true, &allowed);
    let mut hits: Vec<String> = walked
        .found
        .iter()
        .filter(|f| glob_matches(&glob, &f.rel, &start))
        .map(|f| shown(session, &loc.root, &f.rel))
        .collect();
    let total = hits.len();
    hits.truncate(MAX_FOUND);
    let mut text = if hits.is_empty() {
        format!("no files match `{pattern}`")
    } else {
        hits.join("\n")
    };
    if total > MAX_FOUND {
        text.push_str(&format!(
            "\n(first {MAX_FOUND} of {total}; narrow the pattern or path)"
        ));
    }
    if walked.cut_short {
        text.push_str("\n(the tree is too large; search a subdirectory)");
    }
    text.push_str(&skipped_note(walked.skipped_by_policy));
    Ok(ToolOutput::ok(text))
}

/// Reads a file for grep: `None` for binary files (a NUL in the first 8 KiB), files that are not
/// UTF-8, and files over the size limit.
fn searchable(root: &Dir, rel: &str) -> Option<String> {
    let file = root.open(rel).ok()?;
    if file.metadata().ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES).read_to_end(&mut bytes).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub fn grep(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let Some(pattern) = optional_str(input, "pattern") else {
        return Ok(ToolOutput::err("missing string field `pattern`"));
    };
    let ignore_case = input
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let regex = match RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .size_limit(1 << 20)
        .build()
    {
        Ok(r) => r,
        Err(e) => return Ok(ToolOutput::err(format!("bad pattern: {e}"))),
    };
    let glob = match optional_str(input, "glob").map(name_glob).transpose() {
        Ok(g) => g,
        Err(e) => return Ok(ToolOutput::err(e)),
    };
    let path = optional_str(input, "path").unwrap_or(".");
    let loc = match gate(session, "grep", Scope::FsRead, path)? {
        Ok(loc) => loc,
        Err(out) => return Ok(out),
    };
    let session = &*session;
    let root = match open_root(session, &loc) {
        Ok(d) => d,
        Err(e) => return Ok(ToolOutput::err(format!("{path}: {e}"))),
    };
    let start = loc.rel.as_slash();
    let allowed = policy_filter(session, &loc.root);
    // A file path searches that file alone.
    let single = root
        .symlink_metadata(loc.rel.to_path())
        .is_ok_and(|m| m.is_file());
    let walked = if single {
        Walk {
            found: vec![Found { rel: start.clone() }],
            skipped_by_policy: 0,
            cut_short: false,
        }
    } else {
        walk(&root, &start, usize::MAX, true, &allowed)
    };
    let mut lines = Vec::new();
    let mut files_with_matches = 0;
    let mut more = false;
    'files: for f in &walked.found {
        if let Some(g) = &glob
            && !glob_matches(g, &f.rel, &start)
        {
            continue;
        }
        let Some(text) = searchable(&root, &f.rel) else {
            continue;
        };
        let mut matched = false;
        for (n, line) in text.lines().enumerate() {
            if !regex.is_match(line) {
                continue;
            }
            if lines.len() >= MAX_MATCHES {
                more = true;
                break 'files;
            }
            matched = true;
            let mut line = line.trim_end().to_owned();
            if line.len() > MAX_LINE {
                let mut cut = MAX_LINE;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
                line.push('…');
            }
            lines.push(format!(
                "{}:{}: {line}",
                shown(session, &loc.root, &f.rel),
                n + 1
            ));
        }
        files_with_matches += usize::from(matched);
    }
    let mut text = if lines.is_empty() {
        format!("no matches for `{pattern}`")
    } else {
        lines.join("\n")
    };
    if more {
        text.push_str(&format!(
            "\n(stopped at {MAX_MATCHES} matches; narrow the pattern, path or glob)"
        ));
    } else if !lines.is_empty() {
        let plural = if files_with_matches == 1 { "" } else { "s" };
        text.push_str(&format!(
            "\n({} matches in {files_with_matches} file{plural})",
            lines.len()
        ));
    }
    if walked.cut_short {
        text.push_str("\n(the tree is too large; search a subdirectory)");
    }
    text.push_str(&skipped_note(walked.skipped_by_policy));
    Ok(ToolOutput::ok(text))
}

/// Files under `dir` for the `@` picker: names only, so no policy check (attaching a file goes
/// through the read gate). Skips `.git`, ignored paths and symlinks.
pub fn list_files(dir: &Path, limit: usize) -> Vec<String> {
    let Ok(root) = Dir::open_ambient_dir(dir, cap_std::ambient_authority()) else {
        return Vec::new();
    };
    walk(&root, "", limit, true, &|_| true)
        .found
        .into_iter()
        .map(|f| f.rel)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        for (path, text) in [
            (".gitignore", "target/\n*.log\n!keep.log\n/root-only.txt\n"),
            ("src/main.rs", "fn main() {}\n"),
            ("src/lib.rs", "pub fn x() {}\n"),
            ("src/nested/.gitignore", "gen.rs\n"),
            ("src/nested/gen.rs", "generated\n"),
            ("src/nested/mod.rs", "mod x;\n"),
            ("target/debug/out.rs", "built\n"),
            ("app.log", "noise\n"),
            ("keep.log", "kept\n"),
            ("root-only.txt", "x\n"),
            ("docs/root-only.txt", "y\n"),
            (".git/config", "[core]\n"),
            (".env", "SECRET=1\n"),
        ] {
            std::fs::create_dir_all(p.join(path).parent().unwrap()).unwrap();
            std::fs::write(p.join(path), text).unwrap();
        }
        std::os::unix::fs::symlink("/etc", p.join("etc-link")).unwrap();
        d
    }

    #[test]
    fn walks_honour_gitignore_negation_anchoring_and_the_filter() {
        let d = tree();
        let root = Dir::open_ambient_dir(d.path(), cap_std::ambient_authority()).unwrap();
        let no_env = |rel: &str| rel != ".env";
        let walked = walk(&root, "", 100, true, &no_env);
        let found: Vec<&str> = walked.found.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(
            found,
            [
                ".gitignore",
                "keep.log",
                "docs/root-only.txt",
                "src/lib.rs",
                "src/main.rs",
                "src/nested/.gitignore",
                "src/nested/mod.rs",
            ]
        );
        assert_eq!(walked.skipped_by_policy, 1);

        let from_nested = walk(&root, "src/nested", 100, true, &|_| true);
        let found: Vec<&str> = from_nested.found.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(found, ["src/nested/.gitignore", "src/nested/mod.rs"]);

        let picker = list_files(d.path(), 3);
        assert_eq!(picker.len(), 3);
    }

    #[test]
    fn name_and_path_globs() {
        let by_name = name_glob("*.rs").unwrap();
        assert!(glob_matches(&by_name, "src/nested/mod.rs", ""));
        let by_path = name_glob("nested/*.rs").unwrap();
        assert!(glob_matches(&by_path, "src/nested/mod.rs", "src"));
        assert!(!glob_matches(&by_path, "src/nested/mod.rs", ""));
    }
}
