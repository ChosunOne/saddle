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

/// The home directory, canonical, if the environment names one (D60).
pub fn home_dir() -> Option<PathBuf> {
    let home = env::var_os("HOME").filter(|h| !h.is_empty())?;
    let home = PathBuf::from(home);
    Some(home.canonicalize().unwrap_or(home))
}

/// Refuses a workspace that is the home directory or an ancestor of it (D60).
///
/// Home is not a directory to hand to an agent: `.ssh`, `.aws`, `.config` and the rest of a person's
/// credentials live directly beneath it, and a root cannot be narrowed from the inside (D20). The
/// refusal is lexical and checked before anything is opened, and `allow-home` in the user's config
/// is the only thing that lifts it — a file no process the agent runs can write (D42).
pub fn check_home(workspace: &Path, allow_home: bool) -> Result<()> {
    if allow_home {
        return Ok(());
    }
    let Some(which) = home_conflict(workspace, home_dir().as_deref()) else {
        return Ok(());
    };
    anyhow::bail!(
        "refusing to open a session at {}: it is {which}, and every credential on this machine is \
         beneath it. Start in a project instead (saddle -C ~/src/project), or set `allow-home = \
         true` in {} if you really mean it",
        workspace.display(),
        Paths::from_env()
            .map(|p| p.user_config().display().to_string())
            .unwrap_or_else(|_| "config.toml".into()),
    )
}

/// How `workspace` relates to home when that is a problem, or `None` when it is fine (D60).
///
/// Split from `check_home` because the environment is process-wide and a test cannot set it
/// without racing every other test.
fn home_conflict(workspace: &Path, home: Option<&Path>) -> Option<&'static str> {
    let home = home?;
    if workspace == home {
        return Some("your home directory");
    }
    // An ancestor of home contains home, and therefore contains the credentials too.
    home.starts_with(workspace)
        .then_some("a directory above your home directory")
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
    fn a_session_may_not_be_rooted_at_home_or_above_it() {
        // D60: home holds every credential, and a root cannot be narrowed from inside (D20).
        let home = Path::new("/home/me");
        assert_eq!(home_conflict(home, Some(home)), Some("your home directory"));
        // An ancestor of home contains it, and so contains .ssh and .config as well.
        for above in ["/", "/home"] {
            assert_eq!(
                home_conflict(Path::new(above), Some(home)),
                Some("a directory above your home directory"),
                "{above}"
            );
        }
        // A project inside home is the ordinary case and is allowed.
        assert_eq!(
            home_conflict(Path::new("/home/me/src/proj"), Some(home)),
            None
        );
        // A sibling that merely shares a prefix is not home.
        assert_eq!(home_conflict(Path::new("/home/melon"), Some(home)), None);
        // Without a home directory there is nothing to compare against.
        assert_eq!(home_conflict(home, None), None);
    }
}
