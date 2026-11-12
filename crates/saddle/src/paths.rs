//! Where saddle keeps its own state. Both directories are protected: no root may contain them
//! (D16), so neither plugins nor sandboxed children can reach the policies or the audit log.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Paths {
    pub state: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        let home = env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let xdg = |var: &str, fallback: &str| -> Result<PathBuf> {
            env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(fallback)))
                .with_context(|| format!("neither {var} nor HOME is set"))
        };
        Ok(Self {
            state: xdg("XDG_STATE_HOME", ".local/state")?.join("saddle"),
            config: xdg("XDG_CONFIG_HOME", ".config")?.join("saddle"),
        })
    }

    pub fn protected(&self) -> Vec<PathBuf> {
        vec![self.state.clone(), self.config.clone()]
    }

    pub fn audit_dir(&self) -> PathBuf {
        self.state.join("audit")
    }

    pub fn sessions_index(&self) -> PathBuf {
        self.state.join("sessions.jsonl")
    }

    pub fn user_config(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// The global policy: the base every workspace inherits from (D30, D42).
    pub fn default_policy(&self) -> PathBuf {
        self.config.join("policy.toml")
    }

    /// Every workspace's own policy. Inside the config directory, so no root contains it and no
    /// sandboxed command can write it (D16, D42).
    pub fn policies_dir(&self) -> PathBuf {
        self.config.join("policies")
    }

    /// The policy that applies in `workspace`, keyed by its canonical path. One file per
    /// workspace, named so a person can read the directory list (D42).
    pub fn workspace_policy(&self, workspace: &Path) -> PathBuf {
        self.policies_dir()
            .join(format!("{}.toml", workspace_key(workspace)))
    }

    /// Saved conversations (D31).
    /// Saved conversations (D53). One database for every workspace, filtered by a column, so a
    /// fork and the session it came from are in one place.
    pub fn session_db(&self) -> PathBuf {
        self.state.join("sessions.db")
    }

    /// Provider keys entered through `/login`, mode 0600 (D29).
    pub fn auth_store(&self) -> PathBuf {
        self.state.join("auth.json")
    }

    /// The context windows providers stated for their models (D34).
    pub fn reported_windows(&self) -> PathBuf {
        self.state.join("models.json")
    }
}

/// The git toplevel above `start` (the nearest ancestor holding `.git`), or `start` itself.
pub fn find_workspace(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(start)
        .to_path_buf()
}

/// A workspace path as one filename component: `/` becomes `%`, so
/// `/home/me/project` is `home%me%project.toml` and the directory listing names the workspaces.
/// Characters that are awkward in a filename become `_`, which is a lossy but readable fallback;
/// the file itself records the path it is for, so a collision is caught rather than guessed at.
pub fn workspace_key(workspace: &Path) -> String {
    let shown = workspace.to_string_lossy();
    let key: String = shown
        .trim_start_matches('/')
        .chars()
        .map(|c| match c {
            '/' => '%',
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '%') => c,
            _ => '_',
        })
        .collect();
    if key.is_empty() { "root".into() } else { key }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_key_names_the_workspace() {
        assert_eq!(workspace_key(Path::new("/home/me/proj")), "home%me%proj");
        assert_eq!(workspace_key(Path::new("/")), "root");
        assert_eq!(
            workspace_key(Path::new("/home/me/with space")),
            "home%me%with_space"
        );
        // No separators survive, so nothing can escape the policies directory.
        assert!(!workspace_key(Path::new("/../../etc/passwd")).contains('/'));
    }
}
