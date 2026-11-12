//! Cache misses, told to the person at the terminal and never to the model (D50).
//!
//! A provider that caches the prompt prefix bills the reused part at a fraction of the input price.
//! When the prefix changes — the system prompt is reloaded, the tool list moves, an earlier message
//! is rewritten — every request of that turn reads the whole prompt again at full price, which is
//! several times the cost and invisible in the token counts the status line shows. This module is
//! the notice: one line per turn, saying how much was re-read and what the miss cost, because a
//! cache that has quietly stopped working is the difference between a cent and a dime a turn.
//!
//! The notice is display only. It goes through [`crate::provider::Sink::note`], which reaches the
//! transcript in the TUI and stderr in line mode; the conversation sent to the model is built from
//! [`crate::provider::Item`]s and never from a note.

use crate::provider::{Price, Usage};

/// The smallest prompt a provider caches: Anthropic's floor is 1024 tokens, and below it nothing is
/// cacheable, so a warning there would be noise on every short request.
pub const CACHEABLE: u64 = 1024;

/// Watches replies for a cache that has stopped being used.
#[derive(Debug, Default)]
pub struct Watcher {
    /// Caching has worked in this session, so a request that reads nothing from it is a miss rather
    /// than a provider that does not cache at all.
    warm: bool,
    /// This turn has already been told. A turn that misses at every step is one story, not ten.
    told: bool,
}

impl Watcher {
    /// What to tell the user about one reply, if anything (D50).
    ///
    /// A cold start is not a miss: the first request writes the cache it will read next time, so it
    /// reads nothing and nothing is wrong. Only a request that reads nothing after caching has been
    /// seen working is worth a line.
    pub fn reply(&mut self, usage: &Usage, price: Option<Price>) -> Option<String> {
        let warm = self.warm;
        self.warm |= usage.cache_read > 0 || usage.cache_write > 0;
        if !warm || self.told || usage.cache_read > 0 || usage.input < CACHEABLE {
            return None;
        }
        self.told = true;
        Some(text(usage.input, price))
    }

    /// A new turn may report again: the prefix that broke is worth one line each time it happens.
    pub fn turn_end(&mut self) {
        self.told = false;
    }
}

/// `cache miss: 41.2k tokens read at full price, $0.0057 more than cached`. The money is left out
/// when no cache price is stated, since then the difference is not known rather than zero (D36).
fn text(tokens: u64, price: Option<Price>) -> String {
    let head = format!(
        "cache miss: {} tokens read at full price",
        crate::tui::status::tokens(tokens)
    );
    match price.and_then(|p| extra(tokens, p)) {
        Some(extra) => format!("{head}, ${extra:.4} more than cached"),
        None => head,
    }
}

/// What the miss cost above what a cache read would have cost, if the server states one.
fn extra(tokens: u64, price: Price) -> Option<f64> {
    let read = price.cache_read?;
    let more = tokens as f64 * (price.input - read);
    (more > 0.0).then_some(more)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRICE: Price = Price {
        input: 0.0000006,
        output: 0.0000012,
        cache_read: Some(0.00000003),
        cache_write: None,
    };

    fn usage(input: u64, read: u64) -> Usage {
        Usage {
            input,
            cache_read: read,
            ..Usage::default()
        }
    }

    #[test]
    fn a_cold_start_is_not_a_miss() {
        let mut w = Watcher::default();
        // The first request writes the cache it will read next time, so reading nothing is right.
        let first = Usage {
            input: 9000,
            cache_write: 9000,
            ..Usage::default()
        };
        assert_eq!(w.reply(&first, Some(PRICE)), None);
        // From then on, a reply that reads nothing from the cache is a miss.
        let miss = w.reply(&usage(10_000, 0), Some(PRICE));
        assert!(
            miss.as_deref()
                .is_some_and(|m| m.starts_with("cache miss: 10.0k tokens")),
            "{miss:?}"
        );
    }

    #[test]
    fn a_provider_that_never_caches_is_never_warned_about() {
        let mut w = Watcher::default();
        for _ in 0..5 {
            assert_eq!(w.reply(&usage(50_000, 0), Some(PRICE)), None);
        }
    }

    #[test]
    fn one_line_per_turn_and_a_new_turn_reports_again() {
        let mut w = Watcher::default();
        w.reply(&usage(9000, 9000), Some(PRICE));
        assert!(w.reply(&usage(10_000, 0), Some(PRICE)).is_some());
        // The rest of the turn's requests miss too, and saying so again adds nothing.
        assert_eq!(w.reply(&usage(10_000, 0), Some(PRICE)), None);
        w.turn_end();
        assert!(w.reply(&usage(10_000, 0), Some(PRICE)).is_some());
    }

    #[test]
    fn a_hit_and_a_small_prompt_say_nothing() {
        let mut w = Watcher::default();
        assert!(w.reply(&usage(9000, 9000), Some(PRICE)).is_none());
        // A hit is the good case.
        assert_eq!(w.reply(&usage(1000, 9000), Some(PRICE)), None);
        // Below the cacheable floor nothing could have been cached, so a miss is not a fault.
        assert_eq!(w.reply(&usage(500, 0), Some(PRICE)), None);
    }

    #[test]
    fn the_money_is_worked_out_from_the_prices() {
        // 10k tokens at 0.6 per million instead of 0.03 is 0.0057 more.
        let more = extra(10_000, PRICE).unwrap();
        assert!((more - 0.0057).abs() < 1e-9, "{more}");
        // Nothing stated for a cache read means the difference is not known, not zero.
        let no_read = Price {
            cache_read: None,
            ..PRICE
        };
        assert_eq!(extra(10_000, no_read), None);
        // A cache read costing what a read costs is no miss at all.
        let same = Price {
            cache_read: Some(PRICE.input),
            ..PRICE
        };
        assert_eq!(extra(10_000, same), None);
    }
}
