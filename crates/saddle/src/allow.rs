//! A session may use a workspace that puts credentials in reach, once a person has allowed that
//! session id to use it (D60).
//!
//! Nothing here is reachable from inside a session: the files live in the state directory, which no
//! root may contain (D16), so the agent cannot write an allowance for itself and a sandboxed child
//! cannot either. An allowance is written by `saddle session allow <id>` from a terminal the person
//! is sitting at, and it is removed when the session it names ends — so it is a permission for one
//! session, not a property of the workspace.

use std::cmp::Reverse;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What an allowance says: the workspace it covers, and who asked for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub workspace: PathBuf,
    /// The process that asked, so `session pending` can show whether it is still running.
    pub pid: u32,
    /// Seconds since the epoch, for a person reading the list.
    pub when: u64,
}

impl Entry {
    /// Whether this entry covers `workspace`. Paths are compared after canonicalising, because the
    /// same directory reaches the prompt as `/home/me`, `/home/me/` and `/home/me/.` and all three
    /// are the same permission.
    pub fn covers(&self, workspace: &Path) -> bool {
        let same = |a: &Path, b: &Path| {
            let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            canon(a) == canon(b)
        };
        same(&self.workspace, workspace)
    }
}

/// The directory of session allowances, and the ids waiting for one.
pub struct Allowance {
    dir: PathBuf,
}

impl Allowance {
    /// `state` is saddle's state directory.
    pub fn new(state: &Path) -> Self {
        Self {
            dir: state.join("allowances"),
        }
    }

    fn pending_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.pending"))
    }

    fn allow_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.allow"))
    }

    /// Records that session `id` is waiting to use `workspace`, which is what makes an id findable
    /// after the terminal that printed it has scrolled away.
    pub fn request(&self, id: &str, workspace: &Path) -> Result<()> {
        let entry = Entry {
            workspace: workspace.to_path_buf(),
            pid: process::id(),
            when: now(),
        };
        write(&self.pending_path(id), &entry)
    }

    /// The entry for `id`, if a person has allowed it.
    pub fn granted(&self, id: &str) -> Option<Entry> {
        read(&self.allow_path(id))
    }

    /// Whether session `id` may use `workspace`: an allowance exists and names that workspace.
    pub fn allows(&self, id: &str, workspace: &Path) -> bool {
        self.granted(id)
            .is_some_and(|entry| entry.covers(workspace))
    }

    /// Allows session `id` to use `workspace`. Called by `saddle session allow`, which only a
    /// person runs: the file is outside every root, so nothing the agent starts can write it.
    pub fn allow(&self, id: &str, workspace: &Path) -> Result<()> {
        let entry = Entry {
            workspace: workspace.to_path_buf(),
            pid: process::id(),
            when: now(),
        };
        write(&self.allow_path(id), &entry)
    }

    /// Every allowance, most recently made first, so a person can pick one instead of remembering
    /// an id (D61).
    pub fn allowed(&self) -> Vec<(String, Entry)> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Entry)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "allow") {
                    let id = path.file_stem()?.to_str()?.to_owned();
                    Some((id, read(&path)?))
                } else {
                    None
                }
            })
            .collect();
        out.sort_by_key(|(_, entry)| Reverse(entry.when));
        out
    }

    /// The allowances that cover `workspace`, which is what the picker offers when a session in a
    /// sensitive workspace is refused (D61).
    pub fn for_workspace(&self, workspace: &Path) -> Vec<(String, Entry)> {
        let mut out: Vec<(String, Entry)> = self
            .allowed()
            .into_iter()
            .filter(|(_, entry)| entry.covers(workspace))
            .collect();
        out.sort_by_key(|(_, entry)| Reverse(entry.when));
        out
    }

    /// Resolves what a person typed to one of `candidates`: a 1-based number from the list, or an
    /// id or unique id prefix. `None` when nothing matches, and an error when a prefix is
    /// ambiguous, so a typo cannot silently pick the wrong session.
    pub fn resolve(typed: &str, candidates: &[(String, Entry)]) -> Result<Option<String>> {
        let typed = typed.trim();
        if typed.is_empty() {
            return Ok(None);
        }
        if let Ok(n) = typed.parse::<usize>()
            && n >= 1
            && let Some((id, _)) = candidates.get(n - 1)
        {
            return Ok(Some(id.clone()));
        }
        if let Some((id, _)) = candidates.iter().find(|(id, _)| id == typed) {
            return Ok(Some(id.clone()));
        }
        let matches: Vec<&String> = candidates
            .iter()
            .map(|(id, _)| id)
            .filter(|id| id.starts_with(typed))
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some((*one).clone())),
            many => bail!(
                "`{typed}` matches {} sessions ({}); type more of the id",
                many.len(),
                many.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// Forgets everything about `id`: its allowance and its pending request, so an allowance is
    /// gone when a person says so rather than when a process exits (D61).
    pub fn revoke(&self, id: &str) {
        let _ = fs::remove_file(self.allow_path(id));
        let _ = fs::remove_file(self.pending_path(id));
    }

    /// The sessions waiting for an allowance, oldest first.
    pub fn waiting(&self) -> Vec<(String, Entry)> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<(String, Entry)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if path.extension().is_some_and(|x| x == "pending") {
                    let id = path.file_stem()?.to_str()?.to_owned();
                    Some((id, read(&path)?))
                } else {
                    None
                }
            })
            .collect();
        out.sort_by_key(|(_, entry)| entry.when);
        out
    }
}

/// How long ago `when` was, for a person reading the pending list.
pub fn ago(when: u64) -> String {
    let secs = now().saturating_sub(when);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write(path: &Path, entry: &Entry) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("{}", dir.display()))?;
    }
    let text = toml::to_string(entry).context("writing an allowance")?;
    fs::write(path, text).with_context(|| format!("{}", path.display()))
}

fn read(path: &Path) -> Option<Entry> {
    let text = fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Checks that an allowance may be written for `id`, so a typo is refused rather than stored.
pub fn check_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !ok {
        bail!("`{id}` is not a session id (they look like 19c4f3a2-3f9b)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn an_allowance_covers_the_workspace_it_names_and_nothing_else() {
        let state = temp();
        let ws = temp();
        let other = temp();
        let allowance = Allowance::new(state.path());
        assert!(!allowance.allows("abc-1", ws.path()), "nothing allowed yet");
        allowance.allow("abc-1", ws.path()).unwrap();
        assert!(allowance.allows("abc-1", ws.path()));
        // The same directory spelled differently is the same permission.
        assert!(allowance.allows("abc-1", &ws.path().join(".")));
        // Another workspace, or another session id, is not covered.
        assert!(!allowance.allows("abc-1", other.path()));
        assert!(!allowance.allows("def-2", ws.path()));
    }

    #[test]
    fn an_allowance_belongs_to_one_session_and_is_dropped_with_it() {
        let state = temp();
        let ws = temp();
        let allowance = Allowance::new(state.path());
        allowance.request("abc-1", ws.path()).unwrap();
        allowance.allow("abc-1", ws.path()).unwrap();
        assert_eq!(allowance.waiting().len(), 1, "the request is still listed");
        allowance.revoke("abc-1");
        assert!(
            !allowance.allows("abc-1", ws.path()),
            "the allowance is gone"
        );
        assert!(allowance.waiting().is_empty(), "and so is the request");
    }

    #[test]
    fn a_waiting_session_is_findable_by_id() {
        // The id is printed once, so it has to be recoverable afterwards: this is what
        // `saddle session pending` shows.
        let state = temp();
        let ws = temp();
        let allowance = Allowance::new(state.path());
        allowance.request("19c4f3a2-3f9b", ws.path()).unwrap();
        let waiting = allowance.waiting();
        assert_eq!(waiting.len(), 1);
        let (id, entry) = &waiting[0];
        assert_eq!(id, "19c4f3a2-3f9b");
        assert_eq!(entry.workspace, ws.path());
        assert_eq!(entry.pid, process::id());
    }

    #[test]
    fn an_allowance_is_picked_by_number_or_by_prefix() {
        // D61: the point of persisting an allowance is not having to copy a 16-character id.
        let state = temp();
        let ws = temp();
        let allowance = Allowance::new(state.path());
        allowance.allow("9f2c1a4b7e0d3355", ws.path()).unwrap();
        allowance.allow("9f2c2b0000000000", ws.path()).unwrap();
        let candidates = allowance.for_workspace(ws.path());
        assert_eq!(candidates.len(), 2);

        // A number picks by position, which the printed list shows.
        let by_number = Allowance::resolve("2", &candidates).unwrap();
        assert_eq!(by_number.as_deref(), Some(candidates[1].0.as_str()));
        // An exact id and a unique prefix both work.
        let exact = Allowance::resolve("9f2c2b0000000000", &candidates).unwrap();
        assert_eq!(exact.as_deref(), Some("9f2c2b0000000000"));
        let unique = Allowance::resolve("9f2c2b", &candidates).unwrap();
        assert_eq!(unique.as_deref(), Some("9f2c2b0000000000"));
        // An ambiguous prefix is an error rather than a guess, so a typo cannot pick a session.
        let ambiguous = Allowance::resolve("9f2c", &candidates);
        assert!(ambiguous.is_err(), "{ambiguous:?}");
        // Nothing, out of range, and no match are all "nothing chosen".
        for none in ["", "0", "3", "deadbeef"] {
            assert_eq!(
                Allowance::resolve(none, &candidates).unwrap(),
                None,
                "{none:?}"
            );
        }
    }

    #[test]
    fn a_session_id_has_to_look_like_one() {
        for good in ["19c4f3a2-3f9b", "abc", "a_b-1"] {
            check_id(good).unwrap();
        }
        for bad in ["", "../etc/passwd", "a/b", "a b", &"x".repeat(65)] {
            assert!(check_id(bad).is_err(), "{bad:?} must be refused");
        }
    }
}
