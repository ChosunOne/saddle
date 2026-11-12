//! Provider keys entered through `/login`, in `$XDG_STATE_HOME/saddle/auth.json` with mode 0600.
//! The state directory is outside every root (D16), so neither the agent nor a sandboxed command
//! can read it. A key in the provider's environment variable takes precedence (D29).

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct Credential {
    pub api_key: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthStore {
    providers: BTreeMap<String, Credential>,
}

impl AuthStore {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("{}", path.display())),
        }
    }

    pub fn key(&self, provider: &str) -> Option<&str> {
        self.providers.get(provider).map(|c| c.api_key.as_str())
    }

    pub fn set(&mut self, provider: &str, api_key: String) {
        self.providers
            .insert(provider.to_owned(), Credential { api_key });
    }

    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers.remove(provider).is_some()
    }

    /// Writes a 0600 temporary file and renames it over the store.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path
            .parent()
            .context("auth store has no parent directory")?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(".auth.json.tmp");
        let _ = fs::remove_file(&tmp);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn keys_round_trip_in_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/auth.json");
        let mut store = AuthStore::load(&path).unwrap();
        assert_eq!(store.key("deepseek"), None);
        store.set("deepseek", "sk-1".into());
        store.save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut back = AuthStore::load(&path).unwrap();
        assert_eq!(back.key("deepseek"), Some("sk-1"));
        assert!(back.remove("deepseek"));
        back.save(&path).unwrap();
        assert_eq!(AuthStore::load(&path).unwrap().key("deepseek"), None);
    }
}
