//! The status line: a template the user can change, filled from the session (D36).
//!
//! A template is text with `{field}` placeholders. A field the session has no answer for renders
//! as nothing, and takes the separator in front of it along, so leaving out a `[ui] status` field
//! never leaves a dangling `· ` in the line.

use std::fmt;

use crate::provider::Usage;

/// What the line shows when `[ui] status` is not set. It keeps the shape the UI has always had and
/// adds the two figures the template exists for: cost, and the allowance left.
pub const DEFAULT_STATUS: &str = "↑{input} ↓{output} {cache} {cachehit} · ctx {ctx}/{window} · \
                                  {cost} · {quota} · {provider} {model}";

/// The separator an empty field takes with it.
const SEPARATOR: &str = " · ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Provider,
    Model,
    Input,
    Output,
    Cache,
    /// How much of the prompt was served from the provider's cache (D50).
    CacheHit,
    Ctx,
    Window,
    /// The context as a percentage of the window.
    Percent,
    Cost,
    Quota,
    Workspace,
}

impl Field {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "provider" => Field::Provider,
            "model" => Field::Model,
            "input" => Field::Input,
            "output" => Field::Output,
            "cache" => Field::Cache,
            "cachehit" => Field::CacheHit,
            "ctx" => Field::Ctx,
            "window" => Field::Window,
            "percent" => Field::Percent,
            "cost" => Field::Cost,
            "quota" => Field::Quota,
            "workspace" => Field::Workspace,
            _ => return None,
        })
    }

    pub fn names() -> &'static [&'static str] {
        &[
            "provider",
            "model",
            "input",
            "output",
            "cache",
            "cachehit",
            "ctx",
            "window",
            "percent",
            "cost",
            "quota",
            "workspace",
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    Text(String),
    Field(Field),
}

/// A parsed status line. Parsing is where a typo is caught, so a bad `[ui] status` is reported when
/// the config is read rather than showing up as a `{costt}` in the corner later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pieces: Vec<Piece>,
}

impl Default for Template {
    fn default() -> Self {
        Self::parse(DEFAULT_STATUS).unwrap_or(Template { pieces: Vec::new() })
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for piece in &self.pieces {
            match piece {
                Piece::Text(t) => f.write_str(t)?,
                Piece::Field(field) => {
                    let name = Field::names()
                        .iter()
                        .find(|n| Field::parse(n) == Some(*field))
                        .copied()
                        .unwrap_or("?");
                    write!(f, "{{{name}}}")?;
                }
            }
        }
        Ok(())
    }
}

impl Template {
    /// The placeholders are `{name}`, and `{{` and `}}` are literal braces.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut pieces = Vec::new();
        let mut run = String::new();
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    run.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    run.push('}');
                }
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        name.push(c);
                    }
                    if !closed {
                        return Err(format!("`{{` is not closed by `}}` in `{text}`"));
                    }
                    let Some(field) = Field::parse(name.trim()) else {
                        return Err(format!(
                            "`{{{name}}}` is not a field; the fields are {}, and a literal brace is \
                             written `{{{{`",
                            Field::names()
                                .iter()
                                .map(|n| format!("{{{n}}}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    };
                    if !run.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut run)));
                    }
                    pieces.push(Piece::Field(field));
                }
                c => run.push(c),
            }
        }
        if !run.is_empty() {
            pieces.push(Piece::Text(run));
        }
        Ok(Self { pieces })
    }

    /// Whether the template places the field anywhere, so the layout can leave out the copy it
    /// would otherwise draw beside the line (the workspace on the left, D36).
    pub fn contains(&self, field: Field) -> bool {
        self.pieces.contains(&Piece::Field(field))
    }

    /// Fills the template in.
    ///
    /// A field with no answer renders as nothing, and the separator before it is held rather than
    /// written: if a later field has a value the separator goes in front of that one, so
    /// `… {cost}{quota}{model}` reads `… · mock` whether one figure, both or neither are known.
    /// Literal text between them ends the hold, since it is the separator then. A plain space is
    /// not held, only dropped, so `{cache} {cachehit}` left without a hit reads `cache 0` rather
    /// than `cache 0 ` (D50).
    pub fn render(&self, data: &StatusData<'_>) -> String {
        let mut out = String::new();
        let mut held = false;
        for piece in &self.pieces {
            match piece {
                Piece::Text(t) => {
                    out.push_str(t);
                    held = false;
                }
                Piece::Field(field) => {
                    let value = data.value(*field);
                    if value.is_empty() {
                        if out.ends_with(SEPARATOR) {
                            out.truncate(out.len() - SEPARATOR.len());
                            held = true;
                        } else {
                            out.truncate(out.trim_end().len());
                        }
                        continue;
                    }
                    if held {
                        out.push_str(SEPARATOR);
                        held = false;
                    }
                    out.push_str(&value);
                }
            }
        }
        out.trim_end().to_owned()
    }
}

/// Everything the status line can be filled from.
pub struct StatusData<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub usage: Usage,
    pub cost: f64,
    /// Whether a price is known at all, so `$0.00` (nothing spent yet) can be told apart from no
    /// price (D36).
    pub priced: bool,
    pub quota: Option<&'a crate::provider::Quota>,
    pub context: u64,
    pub window: u64,
    pub workspace: &'a str,
    /// The clock a refill is counted down against, passed in rather than read, so the line renders
    /// the same way twice (D49).
    pub now: std::time::SystemTime,
}

impl StatusData<'_> {
    fn value(&self, field: Field) -> String {
        match field {
            Field::Provider if self.provider.is_empty() => "no provider · /login".into(),
            Field::Provider => self.provider.into(),
            Field::Model => self.model.into(),
            Field::Input => tokens(self.usage.input),
            Field::Output => tokens(self.usage.output),
            Field::Cache => self.cache(),
            Field::CacheHit => self.cache_hit(),
            Field::Ctx => tokens(self.context),
            Field::Window => tokens(self.window),
            Field::Percent => match self.window {
                0 => String::new(),
                window => format!("{}%", (self.context.saturating_mul(100) / window).min(100)),
            },
            // No price is not the same as a free session, so neither is shown; but a price with
            // nothing spent yet is `$0.00`, which is the difference the user needs to see.
            Field::Cost if !self.priced => String::new(),
            Field::Cost if self.cost <= 0.0 => "$0.00".into(),
            Field::Cost => money(self.cost),
            Field::Quota => self.quota_text(),
            Field::Workspace => self.workspace.into(),
        }
    }

    /// `cache read/write`, with the write only where the provider counts one (D35).
    fn cache(&self) -> String {
        match self.usage.cache_write {
            0 => format!("cache {}", tokens(self.usage.cache_read)),
            write => format!("cache {}/{}", tokens(self.usage.cache_read), tokens(write)),
        }
    }

    /// The share of the prompt the provider served from its cache: `hit 96%`. Prompt tokens are
    /// the cached ones plus the ones read fresh, so the ratio is of everything sent, and a session
    /// that has sent nothing says nothing rather than `0%` (D50).
    fn cache_hit(&self) -> String {
        let fresh = self.usage.input;
        let cached = self.usage.cache_read;
        let prompt = fresh + cached;
        // A provider that has never written nor read a cache says nothing, rather than claiming
        // `hit 0%` on every request and looking broken (D50).
        if prompt == 0 || cached == 0 && self.usage.cache_write == 0 {
            return String::new();
        }
        // Rounded down, so the line never claims a better share than was measured.
        format!("hit {}%", cached * 100 / prompt)
    }

    /// The allowance in the fewest characters that still say something: `94% ↺1h`, `499/500 req`,
    /// `48.0k/1.0M tok`, `$12.50`. The bar has one row for everything, so a figure that needs a
    /// sentence belongs in `saddle check` instead (D49).
    fn quota_text(&self) -> String {
        let Some(q) = self.quota.filter(|q| !q.is_empty()) else {
            return String::new();
        };
        let mut parts = Vec::new();
        if let Some(left) = q.tokens {
            parts.push(match q.tokens_limit {
                Some(limit) => format!("{}/{} tok", tokens(left), tokens(limit)),
                None => format!("{} tok", tokens(left)),
            });
        }
        if let Some(left) = q.requests {
            parts.push(match q.requests_limit {
                Some(limit) => format!("{left}/{limit} req"),
                None => format!("{left} req"),
            });
        }
        // A balance is the figure a credit-based provider has; it has no limit of its own.
        if let Some(credits) = q.credits {
            parts.push(format!("{credits:.2} cr"));
        }
        if let Some(percent) = q.percent {
            parts.push(format!("{percent:.0}%"));
        }
        let text = parts.join(", ");
        // The refill rides on the figure it belongs to, with no separator: `94% ↺1h` reads as one
        // fact, and `↺` says "until" without a word (D49).
        match (&q.reset, text.is_empty()) {
            (Some(reset), false) => {
                format!("{text} ↺{}", crate::provider::reset_left(reset, self.now))
            }
            (Some(reset), true) => {
                format!("↺{}", crate::provider::reset_left(reset, self.now))
            }
            (None, _) => text,
        }
    }
}

/// A dollar amount: cents matter for one call, fractions of a cent for a session of small ones.
pub fn money(amount: f64) -> String {
    match amount {
        a if a <= 0.0 => String::new(),
        a if a < 0.01 => format!("${a:.4}"),
        a if a < 1.0 => format!("${a:.3}"),
        a => format!("${a:.2}"),
    }
}

/// 950, 12.0k, 1.2M — the same shape the tokens have always had in the status line.
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Price, Quota};

    fn data<'a>(usage: Usage, cost: f64, quota: Option<&'a Quota>) -> StatusData<'a> {
        StatusData {
            provider: "mock",
            model: "mock-model",
            usage,
            cost,
            priced: cost > 0.0,
            quota,
            context: 1_300,
            window: 128_000,
            // A fixed clock, so the line renders the same way twice (D49).
            now: std::time::UNIX_EPOCH,
            workspace: "/ws",
        }
    }

    #[test]
    fn the_default_line_keeps_its_shape_and_adds_nothing_it_does_not_know() {
        let d = data(Usage::default(), 0.0, None);
        let line = Template::default().render(&d);
        assert_eq!(line, "↑0 ↓0 cache 0 · ctx 1.3k/128.0k · mock mock-model");
        assert!(!line.contains(" ·  ·"), "{line}");
    }

    #[test]
    fn cost_and_quota_appear_when_they_are_known() {
        let quota = Quota {
            tokens: Some(48_000),
            tokens_limit: Some(1_000_000),
            ..Quota::default()
        };
        let d = data(
            Usage {
                input: 1_200,
                output: 80,
                cache_read: 400,
                cache_write: 0,
            },
            0.0123,
            Some(&quota),
        );
        let line = Template::default().render(&d);
        assert_eq!(
            line,
            "↑1.2k ↓80 cache 400 hit 25% · ctx 1.3k/128.0k · $0.012 · 48.0k/1.0M tok · \
             mock mock-model"
        );
    }

    #[test]
    fn a_template_can_be_anything_and_a_typo_is_an_error() {
        let t = Template::parse("{percent} of {window} · {workspace}").unwrap();
        let line = t.render(&data(Usage::default(), 0.0, None));
        assert_eq!(line, "1% of 128.0k · /ws");

        // Round-trips, so /settings can show what is set.
        assert_eq!(t.to_string(), "{percent} of {window} · {workspace}");

        let e = Template::parse("{costt}").unwrap_err();
        assert!(e.contains("{cost}"), "the error lists the fields: {e}");
        let e = Template::parse("{cost").unwrap_err();
        assert!(e.contains("not closed"), "{e}");

        // A literal brace is written twice and is not a field.
        let t = Template::parse("{{{model}}}").unwrap();
        assert_eq!(t.render(&data(Usage::default(), 0.0, None)), "{mock-model}");
    }

    #[test]
    fn an_empty_field_takes_the_separator_before_it() {
        let t = Template::parse("{model} · {cost}").unwrap();
        let line = t.render(&data(Usage::default(), 0.0, None));
        assert_eq!(line, "mock-model", "no dangling separator");
        // With a price it is back.
        let line = t.render(&data(Usage::default(), 0.004, None));
        assert_eq!(line, "mock-model · $0.0040");
    }

    #[test]
    fn a_template_that_places_the_workspace_is_not_drawn_twice() {
        let t = Template::parse("{provider} {model}").unwrap();
        assert!(!t.contains(Field::Workspace));
        let t = Template::parse("{workspace} · {model}").unwrap();
        assert!(t.contains(Field::Workspace));
    }

    #[test]
    fn a_known_price_with_nothing_spent_reads_as_zero_rather_than_vanishing() {
        let mut d = data(Usage::default(), 0.0, None);
        d.priced = true;
        assert_eq!(
            Template::default().render(&d),
            "↑0 ↓0 cache 0 · ctx 1.3k/128.0k · $0.00 · mock mock-model"
        );
        // No price at all is still nothing: an unpriced model is not a free one.
        d.priced = false;
        assert_eq!(
            Template::default().render(&d),
            "↑0 ↓0 cache 0 · ctx 1.3k/128.0k · mock mock-model"
        );
    }

    #[test]
    fn a_credit_balance_shows_beside_tokens_and_requests() {
        let quota = Quota {
            tokens: Some(48_000),
            credits: Some(12.5),
            reset: Some("6m0s".into()),
            ..Quota::default()
        };
        let line = Template::default().render(&data(Usage::default(), 0.01, Some(&quota)));
        assert!(line.contains("48.0k tok"), "{line}");
        assert!(line.contains("12.50 cr"), "{line}");
        // A duration is already short, so it rides after the figure as the provider wrote it.
        assert!(line.contains("↺6m0s"), "{line}");
    }

    #[test]
    fn the_cache_hit_rate_is_the_share_of_the_prompt_that_was_cached() {
        let with = |input, read, write| {
            let d = data(
                Usage {
                    input,
                    cache_read: read,
                    cache_write: write,
                    ..Usage::default()
                },
                0.0,
                None,
            );
            Template::parse("{cachehit}").unwrap().render(&d)
        };
        // 9.6k cached against 400 fresh is 96% of the prompt.
        assert_eq!(with(400, 9_600, 0), "hit 96%");
        // A cache that is written but never read is the shape of a prefix that broke: the rate
        // falls rather than the line going quiet (D50).
        assert_eq!(with(10_000, 0, 9_000), "hit 0%");
        // Rounded down, so the line never claims a better share than was measured.
        assert_eq!(with(2, 1, 0), "hit 33%");
        // A provider that has never cached says nothing rather than `hit 0%` on every reply.
        assert_eq!(with(4_000, 0, 0), "");
        // And nothing sent yet says nothing.
        assert_eq!(with(0, 0, 0), "");
    }

    #[test]
    fn a_plan_percentage_counts_down_to_its_refill() {
        // What Synthetic's page yields, and the line the bar shows for it: the percentage, then a
        // countdown marked `↺` — `94% ↺1h`, with no words (D49).
        let quota = Quota {
            percent: Some(94.4),
            reset: Some("2026-11-12T08:26:09.000Z".into()),
            ..Quota::default()
        };
        let render = |now: std::time::SystemTime| {
            let mut d = data(Usage::default(), 0.0, Some(&quota));
            d.now = now;
            Template::parse("{quota}").unwrap().render(&d)
        };
        // An hour before the refill.
        assert_eq!(
            render(humantime::parse_rfc3339("2026-11-12T07:26:09.000Z").unwrap()),
            "94% ↺1h"
        );
        // A few seconds, and a state that has already passed.
        assert_eq!(
            render(humantime::parse_rfc3339("2026-11-12T08:26:04.000Z").unwrap()),
            "94% ↺5s"
        );
        assert_eq!(
            render(humantime::parse_rfc3339("2026-11-12T09:00:00.000Z").unwrap()),
            "94% ↺now"
        );
    }

    #[test]
    fn a_request_pool_reads_as_a_fraction() {
        // A provider that states a pool rather than a plan, which is the shape the response
        // headers carry for Anthropic and OpenAI (D36).
        let pool = Quota {
            requests: Some(499),
            requests_limit: Some(500),
            ..Quota::default()
        };
        let line =
            Template::parse("{quota}")
                .unwrap()
                .render(&data(Usage::default(), 0.0, Some(&pool)));
        assert_eq!(line, "499/500 req");
    }

    #[test]
    fn money_and_tokens_read_the_way_the_status_line_needs() {
        assert_eq!(money(0.0), "");
        assert_eq!(money(0.0042), "$0.0042");
        assert_eq!(money(0.0123), "$0.012");
        assert_eq!(money(1.234), "$1.23");
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(12_000), "12.0k");
        assert_eq!(tokens(1_200_000), "1.2M");
    }

    #[test]
    fn a_price_charges_the_millions_the_config_states() {
        // $3.00 per million in, $15.00 out, as a model card states them.
        let price = Price {
            input: 3.0 / 1_000_000.0,
            output: 15.0 / 1_000_000.0,
            cache_read: Some(0.3 / 1_000_000.0),
            cache_write: None,
        };
        let usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cache_read: 1_000_000,
            cache_write: 0,
        };
        // 3.00 + 1.50 + 0.30
        assert!((price.cost(&usage) - 4.8).abs() < 1e-9);
        // An unstated cache price is charged at the input price rather than for free.
        let no_cache_price = Price {
            cache_read: None,
            ..price
        };
        assert!((no_cache_price.cost(&usage) - 7.5).abs() < 1e-9);
        assert!(!Price::default().is_known());
    }
}
