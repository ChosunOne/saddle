//! D125: recover a Responses stream once, before any text or reasoning has been shown.
//!
//! Only failures marked by the transport or parser are transient. A rejected request, malformed
//! event, cancellation or partial reply is not a reason to silently generate another answer.

use std::error::Error;
use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{Cancelled, Quota, Reply, Sink};

const DELAY: Duration = Duration::from_secs(1);

/// a failure for which a fresh request may succeed, subject to the output and retry guards.
#[derive(Debug)]
pub struct Transient {
    message: String,
    reason: &'static str,
    delay: Duration,
}

impl Transient {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            reason: "transient provider error",
            delay: DELAY,
        }
    }

    /// identifies the failure using host-chosen labels, never backend text or request data (D130).
    pub fn with_reason(mut self, reason: &'static str) -> Self {
        self.reason = reason;
        self
    }

    pub fn with_retry_after(mut self, seconds: Option<u64>) -> Self {
        if let Some(seconds) = seconds {
            self.delay = Duration::from_secs(seconds.min(60));
        }
        self
    }
}

impl fmt::Display for Transient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for Transient {}

/// retries once over HTTP with the whole history, never replaying a visible partial response.
pub fn once(
    sink: &mut dyn Sink,
    mut attempt: impl FnMut(bool, &mut dyn Sink) -> Result<Reply>,
) -> Result<Reply> {
    check_cancelled(sink)?;
    let mut tracked = AttemptSink {
        sink,
        emitted: false,
    };
    match attempt(false, &mut tracked) {
        Ok(reply) => Ok(reply),
        Err(error) => {
            check_cancelled(tracked.sink)?;
            if tracked.emitted || error.downcast_ref::<Cancelled>().is_some() {
                return Err(error);
            }
            let Some(transient) = error.downcast_ref::<Transient>() else {
                return Err(error);
            };
            let delay = transient.delay;
            tracked.note(&format!("response stream interrupted ({}); retrying once over HTTP with the full conversation in {}s", transient.reason, delay.as_secs()));
            pause(tracked.sink, delay)?;
            attempt(true, tracked.sink)
        }
    }
}

fn check_cancelled(sink: &dyn Sink) -> Result<()> {
    if sink.cancelled() {
        return Err(Cancelled.into());
    }
    Ok(())
}

fn pause(sink: &dyn Sink, duration: Duration) -> Result<()> {
    let end = Instant::now() + duration;
    loop {
        check_cancelled(sink)?;
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(Duration::from_millis(50)));
    }
}

struct AttemptSink<'a> {
    sink: &'a mut dyn Sink,
    emitted: bool,
}

impl Sink for AttemptSink<'_> {
    fn text(&mut self, text: &str) {
        self.emitted |= !text.is_empty();
        self.sink.text(text);
    }

    fn reasoning(&mut self, text: &str) {
        self.emitted |= !text.is_empty();
        self.sink.reasoning(text);
    }

    fn note(&mut self, text: &str) {
        self.sink.note(text);
    }

    fn quota(&mut self, quota: &Quota) {
        self.sink.quota(quota);
    }

    fn cancelled(&self) -> bool {
        self.sink.cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[derive(Default)]
    struct Capture {
        text: String,
        reasoning: String,
        notes: Vec<String>,
        quota: Option<Quota>,
        cancel: bool,
        cancel_on_note: bool,
    }

    impl Sink for Capture {
        fn text(&mut self, text: &str) {
            self.text.push_str(text);
        }
        fn reasoning(&mut self, text: &str) {
            self.reasoning.push_str(text);
        }
        fn note(&mut self, text: &str) {
            self.notes.push(text.to_owned());
            self.cancel |= self.cancel_on_note;
        }
        fn quota(&mut self, quota: &Quota) {
            self.quota = Some(quota.clone());
        }
        fn cancelled(&self) -> bool {
            self.cancel
        }
    }

    #[test]
    fn recovery_is_bounded_and_uses_http_only_on_the_second_attempt() {
        let mut sink = Capture::default();
        let mut attempts = Vec::new();
        let error = once(&mut sink, |http, _| {
            attempts.push(http);
            Err(Transient::new("connection closed").into())
        })
        .unwrap_err();
        assert_eq!(attempts, [false, true]);
        assert_eq!(sink.notes.len(), 1);
        assert!(error.to_string().contains("connection closed"));
    }

    #[test]
    fn recovery_reports_only_the_host_chosen_failure_category() {
        let mut sink = Capture::default();
        let _ = once(&mut sink, |_, _| {
            Err(
                Transient::new("private backend details and request text\x1b[2J")
                    .with_reason("websocket read timed out")
                    .with_retry_after(Some(0))
                    .into(),
            )
        });
        assert_eq!(
            sink.notes,
            [
                "response stream interrupted (websocket read timed out); retrying once over HTTP with the full conversation in 0s"
            ]
        );
    }

    #[test]
    fn text_or_reasoning_prevents_replaying_a_partial_reply() {
        for reasoning in [false, true] {
            let mut sink = Capture::default();
            let mut attempts = 0;
            let error = once(&mut sink, |_, sink| {
                attempts += 1;
                if reasoning {
                    sink.reasoning("thinking");
                } else {
                    sink.text("answer");
                }
                sink.quota(&Quota::default());
                Err(Transient::new("connection closed").into())
            })
            .unwrap_err();
            assert_eq!(attempts, 1);
            assert!(sink.notes.is_empty());
            assert_eq!(sink.text, if reasoning { "" } else { "answer" });
            assert_eq!(sink.reasoning, if reasoning { "thinking" } else { "" });
            assert!(sink.quota.is_some());
            assert!(error.downcast_ref::<Transient>().is_some());
        }
    }

    #[test]
    fn permanent_errors_and_cancellation_are_not_retried() {
        for cancelled in [false, true] {
            let mut sink = Capture::default();
            let mut attempts = 0;
            let error = once(&mut sink, |_, _| {
                attempts += 1;
                if cancelled {
                    Err(Cancelled.into())
                } else {
                    Err(anyhow!("invalid request"))
                }
            })
            .unwrap_err();
            assert_eq!(attempts, 1);
            assert!(sink.notes.is_empty());
            assert_eq!(
                error.to_string(),
                if cancelled {
                    "cancelled"
                } else {
                    "invalid request"
                }
            );
        }
    }

    #[test]
    fn cancellation_before_request_or_during_backoff_stops_recovery() {
        for already_cancelled in [false, true] {
            let mut sink = Capture {
                cancel: already_cancelled,
                cancel_on_note: true,
                ..Capture::default()
            };
            let mut attempts = 0;
            let error = once(&mut sink, |_, _| {
                attempts += 1;
                Err(Transient::new("connection closed").into())
            })
            .unwrap_err();
            assert!(error.downcast_ref::<Cancelled>().is_some());
            assert_eq!(attempts, if already_cancelled { 0 } else { 1 });
        }
    }

    #[test]
    fn retry_after_overrides_the_default_and_has_a_bound() {
        assert_eq!(Transient::new("test").delay, DELAY);
        assert_eq!(
            Transient::new("test").with_retry_after(Some(0)).delay,
            Duration::ZERO
        );
        assert_eq!(
            Transient::new("test").with_retry_after(Some(30)).delay,
            Duration::from_secs(30)
        );
        assert_eq!(
            Transient::new("test")
                .with_retry_after(Some(u64::MAX))
                .delay,
            Duration::from_secs(60)
        );
    }
}
