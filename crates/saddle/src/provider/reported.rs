//! The context windows providers state for their models (D34).
//!
//! A `/models` listing is the one place a server says how much a model takes, and the answer is
//! needed before the first request of a session, so it is cached under the state directory. A
//! listing replaces what the same endpoint said before; a model an earlier listing gave a window
//! for keeps it when a later one leaves the field out. The cache is a hint and never authority:
//! a `context-window` in `config.toml` still wins, and so does a `[providers.<name>.pricing]` table
//! over a price the server states (D36).

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::Price;
use super::http::ModelInfo;

/// What one endpoint stated about one model. A cache written before prices were recorded holds a
/// bare number there, which still reads as the window it was (D36).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Entry {
    Window(u64),
    Full {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        price: Option<Price>,
    },
}

impl Entry {
    fn window(&self) -> Option<u64> {
        match self {
            Entry::Window(n) => Some(*n),
            Entry::Full { window, .. } => *window,
        }
    }

    fn price(&self) -> Option<Price> {
        match self {
            Entry::Window(_) => None,
            Entry::Full { price, .. } => *price,
        }
    }

    /// Merges a listing's answer into what was stored: a field this listing left out keeps what an
    /// earlier one said.
    fn update(&mut self, window: Option<u64>, price: Option<Price>) {
        let (mut w, mut p) = (self.window(), self.price());
        if window.is_some() {
            w = window;
        }
        if price.is_some() {
            p = price;
        }
        *self = Entry::Full {
            window: w,
            price: p.filter(Price::is_known),
        };
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Reported {
    /// By base URL, then model id: the same model id can mean different things at two endpoints.
    endpoints: BTreeMap<String, BTreeMap<String, Entry>>,
}

impl Reported {
    /// A cache that is missing or unreadable is not an error to the caller: it just has no answers.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| path.display().to_string()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| path.display().to_string()),
        }
    }

    /// The same, for a caller that only wants to know what a model takes: a cache it cannot read
    /// costs it the default window, not the session.
    pub fn stored(path: &Path) -> Self {
        Self::load(path).unwrap_or_default()
    }

    pub fn window(&self, base_url: &str, model: &str) -> Option<u64> {
        self.endpoints.get(base_url)?.get(model)?.window()
    }

    pub fn price(&self, base_url: &str, model: &str) -> Option<Price> {
        self.endpoints.get(base_url)?.get(model)?.price()
    }

    /// Replaces what `base_url` said, keeping entries for models the server still lists but gave
    /// no window or price for this time.
    pub fn record(&mut self, base_url: &str, models: &[ModelInfo]) {
        let seen = self.endpoints.entry(base_url.to_owned()).or_default();
        seen.retain(|id, _| models.iter().any(|m| &m.id == id));
        for m in models {
            if m.context_window.is_none() && m.price.is_none() {
                continue;
            }
            seen.entry(m.id.clone())
                .and_modify(|e| e.update(m.context_window, m.price))
                .or_insert_with(|| Entry::Full {
                    window: m.context_window,
                    price: m.price,
                });
        }
    }

    /// Written through a temporary file in the same directory, so a crash cannot leave half a
    /// cache for the next session to read.
    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("the model cache has no parent")?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(".models.json.tmp");
        let _ = fs::remove_file(&tmp);
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str, context_window: Option<u64>) -> ModelInfo {
        ModelInfo {
            id: id.to_owned(),
            context_window,
            price: None,
        }
    }

    #[test]
    fn a_listing_replaces_the_endpoint_but_keeps_models_it_left_out() {
        let mut r = Reported::default();
        let first = [info("kimi", Some(262_144)), info("gone", Some(8_192))];
        r.record("https://a/v1", &first);
        assert_eq!(r.window("https://a/v1", "kimi"), Some(262_144));

        // The second listing drops `gone` from the catalogue and says nothing about `kimi`.
        r.record(
            "https://a/v1",
            &[info("kimi", None), info("new", Some(131_072))],
        );
        assert_eq!(r.window("https://a/v1", "gone"), None);
        assert_eq!(r.window("https://a/v1", "kimi"), Some(262_144));
        assert_eq!(r.window("https://a/v1", "new"), Some(131_072));

        // Another endpoint serving the same model id is a separate answer.
        assert_eq!(r.window("https://b/v1", "kimi"), None);
    }

    #[test]
    fn a_cache_written_before_prices_still_reads_as_its_window() {
        // The old shape was a bare number per model.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        fs::write(&path, r#"{"https://a/v1":{"kimi":262144}}"#).unwrap();
        let r = Reported::load(&path).unwrap();
        assert_eq!(r.window("https://a/v1", "kimi"), Some(262_144));
        assert_eq!(r.price("https://a/v1", "kimi"), None);
    }

    #[test]
    fn a_listing_that_states_only_a_price_keeps_the_window_it_knew() {
        let mut r = Reported::default();
        r.record("https://a/v1", &[info("kimi", Some(262_144))]);
        let priced = ModelInfo {
            id: "kimi".into(),
            context_window: None,
            price: Some(Price {
                input: 3e-6,
                output: 1.5e-5,
                cache_read: None,
                cache_write: None,
            }),
        };
        r.record("https://a/v1", &[priced]);
        assert_eq!(r.window("https://a/v1", "kimi"), Some(262_144));
        assert_eq!(
            r.price("https://a/v1", "kimi").map(|p| p.output),
            Some(1.5e-5)
        );
    }

    #[test]
    fn the_cache_round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/models.json");
        let mut r = Reported::default();
        r.record("https://a/v1", &[info("glm", Some(200_000))]);
        r.save(&path).unwrap();
        let back = Reported::load(&path).unwrap();
        assert_eq!(back.window("https://a/v1", "glm"), Some(200_000));
        assert_eq!(
            Reported::load(&dir.path().join("absent.json"))
                .unwrap()
                .window("https://a/v1", "glm"),
            None
        );
    }
}
