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

use crate::provider::Tokens;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct Credential {
    /// A key typed by the user, or read from the provider's environment variable.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_key: String,
    /// Tokens from an OAuth login, when the provider authenticates that way (D85).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<Tokens>,
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
        self.providers
            .get(provider)
            .map(|c| c.api_key.as_str())
            .filter(|k| !k.is_empty())
    }

    /// The last OAuth login for a provider (D85).
    pub fn oauth(&self, provider: &str) -> Option<&Tokens> {
        self.providers.get(provider).and_then(|c| c.oauth.as_ref())
    }

    pub fn set(&mut self, provider: &str, api_key: String) {
        self.providers.insert(
            provider.to_owned(),
            Credential {
                api_key,
                oauth: None,
            },
        );
    }

    /// Replaces a provider's OAuth tokens, keeping any key it also has.
    pub fn set_oauth(&mut self, provider: &str, tokens: Tokens) {
        let entry = self.providers.entry(provider.to_owned()).or_default();
        entry.oauth = Some(tokens);
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

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_key_file_from_before_oauth_still_loads() {
        // D85: the store is the user's own file, and one written before this change must keep
        // working. `api-key` alone is a complete credential.
        let dir = tempdir();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"openai":{"api-key":"sk-old"}}"#).unwrap();
        let store = AuthStore::load(&path).unwrap();
        assert_eq!(store.key("openai"), Some("sk-old"));
        assert!(store.oauth("openai").is_none());
        // An absent or empty key is not a key.
        assert_eq!(store.key("nonesuch"), None);
    }

    #[test]
    fn oauth_tokens_are_stored_beside_the_key_and_round_trip() {
        let dir = tempdir();
        let path = dir.path().join("auth.json");
        let mut store = AuthStore::default();
        let tokens = Tokens {
            access: "at".into(),
            refresh: "rt".into(),
            expires_at: 123,
            client_id: Some("cid".into()),
            account_id: Some("acct".into()),
        };
        store.set_oauth("openai-chatgpt", tokens.clone());
        store.save(&path).unwrap();
        let back = AuthStore::load(&path).unwrap();
        assert_eq!(back.oauth("openai-chatgpt"), Some(&tokens));
        // An OAuth entry carries no key, and a key entry carries no tokens: neither leaks the
        // other's shape into the file.
        store.set("openai", "sk-1".to_owned());
        store.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(saved["openai"]["api-key"], "sk-1");
        assert!(
            saved["openai"].get("oauth").is_none(),
            "the key entry grew an oauth field: {text}"
        );
        assert_eq!(saved["openai-chatgpt"]["oauth"]["access"], "at");
        assert!(
            saved["openai-chatgpt"].get("api-key").is_none(),
            "the oauth entry grew a key field: {text}"
        );
        // The file stays private.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
        // Logging out forgets it.
        assert!(store.remove("openai-chatgpt"));
        assert!(!store.remove("openai-chatgpt"));
    }
}
