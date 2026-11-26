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

    pub fn policies_dir(&self) -> PathBuf {
        self.config.join("policies")
    }

    pub fn workspace_policy(&self, workspace: &Path) -> PathBuf {
        self.policies_dir()
            .join(format!("{}.toml", workspace_key(workspace)))
    }

    pub fn session_db(&self) -> PathBuf {
        self.state.join("sessions.db")
    }

    pub fn auth_store(&self) -> PathBuf {
        self.state.join("auth.json")
    }

    pub fn reported_windows(&self) -> PathBuf {
        self.state.join("models.json")
    }
}

pub fn find_workspace(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(start)
        .to_path_buf()
}

pub fn home_dir() -> Option<PathBuf> {
    let home = env::var_os("HOME").filter(|h| !h.is_empty())?;
    let home = PathBuf::from(home);
    Some(home.canonicalize().unwrap_or(home))
}

pub fn sensitive_dirs() -> Vec<(&'static str, PathBuf)> {
    let mut out = Vec::new();
    if let Some(home) = home_dir() {
        out.push(("your home directory", home.clone()));
        if let Ok(paths) = Paths::from_env()
            && let Some(config) = paths.config.parent()
        {
            out.push(("your config directory", config.to_path_buf()));
        }
    }
    out
}

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

pub fn workspace_key(workspace: &Path) -> String {
    let shown = workspace.to_string_lossy();
    let inner = shown.trim_start_matches('/').replace(['/', '\\', ':'], "-");
    format!("--{inner}--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_key_reads_like_the_path_it_names() {
        assert_eq!(
            workspace_key(Path::new("/home/me/proj")),
            "--home-me-proj--"
        );
        assert_eq!(workspace_key(Path::new("/")), "----");
        assert_eq!(
            workspace_key(Path::new("/home/me/with space")),
            "--home-me-with space--"
        );
        for path in ["/../../etc/passwd", "/a\\b", "/c:d"] {
            let key = workspace_key(Path::new(path));
            assert!(!key.contains('/'), "{key}");
            assert!(!key.contains('\\'), "{key}");
            assert!(!key.contains(':'), "{key}");
        }
    }

    #[test]
    fn a_workspace_is_reported_for_the_credentials_it_puts_in_reach() {
        let home = PathBuf::from("/home/me");
        let config = home.join(".config");
        let sensitive = vec![
            ("your home directory", home.clone()),
            ("your config directory", config.clone()),
        ];

        assert_eq!(exposed(&home, &sensitive).len(), 2);
        assert_eq!(exposed(Path::new("/home"), &sensitive).len(), 2);
        assert_eq!(exposed(Path::new("/"), &sensitive).len(), 2);
        let only_config = exposed(&config, &sensitive);
        assert_eq!(only_config.len(), 1);
        assert!(
            only_config[0].contains("your config directory"),
            "{only_config:?}"
        );
        assert!(only_config[0].contains("itself"), "{only_config:?}");

        assert!(exposed(&home.join("src/proj"), &sensitive).is_empty());
        assert!(exposed(Path::new("/home/melon"), &sensitive).is_empty());
    }
}
