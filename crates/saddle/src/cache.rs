use crate::provider::{Price, Usage};
use crate::tui::status;

pub const CACHEABLE: u64 = 1024;

#[derive(Debug, Default)]
pub struct Watcher {
    warm: bool,
    told: bool,
}

impl Watcher {
    pub fn reply(&mut self, usage: &Usage, price: Option<Price>) -> Option<String> {
        let warm = self.warm;
        self.warm |= usage.cache_read > 0 || usage.cache_write > 0;
        if !warm || self.told || usage.cache_read > 0 || usage.input < CACHEABLE {
            return None;
        }
        self.told = true;
        Some(text(usage.input, price))
    }

    pub fn turn_end(&mut self) {
        self.told = false;
    }
}

fn text(tokens: u64, price: Option<Price>) -> String {
    let head = format!(
        "cache miss: {} tokens read at full price",
        status::tokens(tokens)
    );
    match price.and_then(|p| extra(tokens, p)) {
        Some(extra) => format!("{head}, ${extra:.4} more than cached"),
        None => head,
    }
}

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
        let first = Usage {
            input: 9000,
            cache_write: 9000,
            ..Usage::default()
        };
        assert_eq!(w.reply(&first, Some(PRICE)), None);
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
        assert_eq!(w.reply(&usage(10_000, 0), Some(PRICE)), None);
        w.turn_end();
        assert!(w.reply(&usage(10_000, 0), Some(PRICE)).is_some());
    }

    #[test]
    fn a_hit_and_a_small_prompt_say_nothing() {
        let mut w = Watcher::default();
        assert!(w.reply(&usage(9000, 9000), Some(PRICE)).is_none());
        assert_eq!(w.reply(&usage(1000, 9000), Some(PRICE)), None);
        assert_eq!(w.reply(&usage(500, 0), Some(PRICE)), None);
    }

    #[test]
    fn the_money_is_worked_out_from_the_prices() {
        let more = extra(10_000, PRICE).unwrap();
        assert!((more - 0.0057).abs() < 1e-9, "{more}");
        let no_read = Price {
            cache_read: None,
            ..PRICE
        };
        assert_eq!(extra(10_000, no_read), None);
        let same = Price {
            cache_read: Some(PRICE.input),
            ..PRICE
        };
        assert_eq!(extra(10_000, same), None);
    }
}
