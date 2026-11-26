use std::cmp::Reverse;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub workspace: PathBuf,
    pub pid: u32,
    pub when: u64,
}

impl Entry {
    pub fn covers(&self, workspace: &Path) -> bool {
        let same = |a: &Path, b: &Path| {
            let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            canon(a) == canon(b)
        };
        same(&self.workspace, workspace)
    }
}

pub struct Allowance {
    dir: PathBuf,
}

impl Allowance {
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

    pub fn request(&self, id: &str, workspace: &Path) -> Result<()> {
        let entry = Entry {
            workspace: workspace.to_path_buf(),
            pid: process::id(),
            when: now(),
        };
        write(&self.pending_path(id), &entry)
    }

    pub fn granted(&self, id: &str) -> Option<Entry> {
        read(&self.allow_path(id))
    }

    pub fn allows(&self, id: &str, workspace: &Path) -> bool {
        self.granted(id)
            .is_some_and(|entry| entry.covers(workspace))
    }

    pub fn allow(&self, id: &str, workspace: &Path) -> Result<()> {
        let entry = Entry {
            workspace: workspace.to_path_buf(),
            pid: process::id(),
            when: now(),
        };
        write(&self.allow_path(id), &entry)
    }

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

    pub fn for_workspace(&self, workspace: &Path) -> Vec<(String, Entry)> {
        let mut out: Vec<(String, Entry)> = self
            .allowed()
            .into_iter()
            .filter(|(_, entry)| entry.covers(workspace))
            .collect();
        out.sort_by_key(|(_, entry)| Reverse(entry.when));
        out
    }

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

    pub fn revoke(&self, id: &str) {
        let _ = fs::remove_file(self.allow_path(id));
        let _ = fs::remove_file(self.pending_path(id));
    }

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
        assert!(allowance.allows("abc-1", &ws.path().join(".")));
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
        let state = temp();
        let ws = temp();
        let allowance = Allowance::new(state.path());
        allowance.allow("9f2c1a4b7e0d3355", ws.path()).unwrap();
        allowance.allow("9f2c2b0000000000", ws.path()).unwrap();
        let candidates = allowance.for_workspace(ws.path());
        assert_eq!(candidates.len(), 2);

        let by_number = Allowance::resolve("2", &candidates).unwrap();
        assert_eq!(by_number.as_deref(), Some(candidates[1].0.as_str()));
        let exact = Allowance::resolve("9f2c2b0000000000", &candidates).unwrap();
        assert_eq!(exact.as_deref(), Some("9f2c2b0000000000"));
        let unique = Allowance::resolve("9f2c2b", &candidates).unwrap();
        assert_eq!(unique.as_deref(), Some("9f2c2b0000000000"));
        let ambiguous = Allowance::resolve("9f2c", &candidates);
        assert!(ambiguous.is_err(), "{ambiguous:?}");
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
