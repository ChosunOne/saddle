//! human file-picker enumeration, not an agent filesystem tool (D147).
use crate::ignore::{self, Rules};
use cap_std::fs::Dir;
use saddle_limits as budgets;
use std::path::Path;
/// A file found by a walk: its path relative to the root.
struct Found {
    rel: String,
}

struct Walk {
    found: Vec<Found>,
    skipped_by_policy: usize,
    cut_short: bool,
}

/// Adds the rules of every ignore file in `base`, which is a directory relative to `root`.
fn load_ignores(root: &Dir, rules: &mut Rules, base: &str) {
    for name in ignore::FILES {
        let file = if base.is_empty() {
            (*name).to_owned()
        } else {
            format!("{base}/{name}")
        };
        if let Ok(text) = root.read_to_string(&file) {
            rules.add(&text, base);
        }
    }
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
    let mut ignores = Rules::default();
    if gitignore {
        // Rules from the root down to the start apply too.
        load_ignores(root, &mut ignores, "");
        let mut base = String::new();
        for part in start.split('/').filter(|p| !p.is_empty()) {
            if !base.is_empty() {
                base.push('/');
            }
            base.push_str(part);
            load_ignores(root, &mut ignores, &base);
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
            if visited > budgets::search_visited() {
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
                    load_ignores(root, &mut ignores, &rel);
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
