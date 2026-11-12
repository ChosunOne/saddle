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

    /// One file per workspace, holding the grants that directory overrides (D63). Inside the
    /// config directory, so no root contains it and no sandboxed command can write it (D16, D42).
    pub fn policies_dir(&self) -> PathBuf {
        self.config.join("policies")
    }

    /// The grants that apply in `workspace` on top of the base in `config.toml`, keyed by its
    /// canonical path. One file per workspace, so the authority for a directory is a file named
    /// after it rather than a sub-table of the global one (D63).
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

/// The home directory, canonical, if the environment names one (D60).
pub fn home_dir() -> Option<PathBuf> {
    let home = env::var_os("HOME").filter(|h| !h.is_empty())?;
    let home = PathBuf::from(home);
    Some(home.canonicalize().unwrap_or(home))
}

/// The directories that hold credentials: what each is called, and where it is (D60).
pub fn sensitive_dirs() -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        out.push(("your home directory", home.clone()));
        // The default config directory is inside home; XDG_CONFIG_HOME may move it out.
        if let Ok(paths) = Paths::from_env()
            && let Some(config) = paths.config.parent()
        {
            out.push(("your config directory", config.to_path_buf()));
        }
    }
    out
}

/// What a workspace would put in reach, for the warning before a session opens (D60).
///
/// This is deliberately not a rule about what a workspace may be: nothing here refuses anything,
/// and no directory is named as invalid. It reports the sensitive directories the workspace *is*
/// or *contains*, so the person starting the session can decide with the facts in front of them.
/// A project inside home is reported only if it actually contains one of them.
pub fn exposed(workspace: &Path, sensitive: &[(&str, PathBuf)]) -> Vec<String> {
    sensitive
        .iter()
        .filter(|(_, path)| path == workspace || path.starts_with(workspace))
        .map(|(what, path)| {
            if path == workspace {
                format!("{what} itself ({})", path.display())
            } else {
                format!("{what}, and it is inside ({})", path.display())
            }
        })
        .collect()
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

    #[test]
    fn a_workspace_is_reported_for_the_credentials_it_puts_in_reach() {
        // D60: nothing is refused. What the person is told is which sensitive directory the
        // workspace is, or contains, so that approving it is a decision rather than a guess.
        let home = PathBuf::from("/home/me");
        let config = home.join(".config");
        let sensitive = vec![
            ("your home directory", home.clone()),
            ("your config directory", config.clone()),
        ];

        // Home itself, and anything above it, puts both in reach.
        assert_eq!(exposed(&home, &sensitive).len(), 2);
        assert_eq!(exposed(Path::new("/home"), &sensitive).len(), 2);
        assert_eq!(exposed(Path::new("/"), &sensitive).len(), 2);
        // The config directory alone is enough to report.
        let only_config = exposed(&config, &sensitive);
        assert_eq!(only_config.len(), 1);
        assert!(
            only_config[0].contains("your config directory"),
            "{only_config:?}"
        );
        assert!(only_config[0].contains("itself"), "{only_config:?}");

        // An ordinary project inside home is not a warning: it contains none of them.
        assert!(exposed(&home.join("src/proj"), &sensitive).is_empty());
        // A sibling sharing a prefix is not home either.
        assert!(exposed(Path::new("/home/melon"), &sensitive).is_empty());
    }
}
