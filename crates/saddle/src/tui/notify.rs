//! D164: the end of a turn the person is not watching is announced, with the terminal's bell or a
//! desktop notification the terminal draws itself.
//!
//! Whether the person is watching is the terminal's focus, which it reports once mode 1004 is on.
//! A terminal that has not reported it is treated as unfocused: one that never reports would
//! otherwise never announce anything, and an extra bell is the cheaper mistake.

use crate::agent::End;
use serde::Deserialize;

/// `[ui] notify`: how the end of a turn is announced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Notify {
    Off,
    /// BEL. Every terminal accepts it; most mark the tab or window, and some play a sound.
    #[default]
    Bell,
    /// OSC 9, a desktop notification in iTerm2, WezTerm, Ghostty and kitty.
    Osc9,
    /// OSC 777, a desktop notification in foot and Ghostty.
    Osc777,
}

/// What the event loop knows about the terminal's focus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    /// The terminal has not said, which may mean it never will.
    #[default]
    Unknown,
    In,
    Out,
}

/// What to say when a turn ends this way, or `None` when nothing should be said: a cancelled turn
/// was cancelled by the person, who is therefore watching.
pub fn ended(end: End) -> Option<&'static str> {
    match end {
        End::Cancelled => None,
        End::Answered => Some("finished"),
        End::Declined => Some("the model declined"),
        End::Cut => Some("the reply hit the output limit"),
        End::StepLimit => Some("stopped at the request limit"),
        End::Repeating => Some("stopped: the model kept repeating a call"),
        End::Failed => Some("a request to the provider failed"),
    }
}

/// The bytes that announce `message` under `title`, or `None` when the setting is off or the
/// terminal is focused. Under tmux an OSC goes through tmux's passthrough; the bell needs none,
/// because tmux passes a pane's bell on by itself.
pub fn sequence(
    how: Notify,
    focus: Focus,
    title: &str,
    message: &str,
    tmux: bool,
) -> Option<String> {
    if focus == Focus::In {
        return None;
    }
    // OSC 777 separates the title from the body with `;`, so the title cannot carry one.
    let title = clean(title).replace(';', ",");
    let message = clean(message);
    match how {
        Notify::Off => None,
        Notify::Bell => Some("\x07".into()),
        Notify::Osc9 => Some(passthrough(&format!("\x1b]9;{title}: {message}\x07"), tmux)),
        Notify::Osc777 => Some(passthrough(
            &format!("\x1b]777;notify;{title};{message}\x07"),
            tmux,
        )),
    }
}

/// tmux forwards an escape sequence to the outer terminal only inside one of its own, with every
/// ESC inside doubled.
pub fn passthrough(sequence: &str, tmux: bool) -> String {
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence.to_owned()
    }
}

/// The text without control characters, so a workspace name cannot end the sequence early or
/// start another one.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Ui {
        notify: Notify,
    }

    #[test]
    fn the_setting_is_one_of_four_words() {
        let parse = |text: &str| toml::from_str::<Ui>(text).map(|ui| ui.notify);
        assert_eq!(parse("notify = \"osc777\"").unwrap(), Notify::Osc777);
        assert_eq!(parse("notify = \"osc9\"").unwrap(), Notify::Osc9);
        assert_eq!(parse("notify = \"off\"").unwrap(), Notify::Off);
        assert!(parse("notify = \"growl\"").is_err());
    }

    #[test]
    fn a_focused_terminal_or_the_off_setting_hears_nothing() {
        assert_eq!(sequence(Notify::Bell, Focus::In, "t", "m", false), None);
        assert_eq!(sequence(Notify::Off, Focus::Out, "t", "m", false), None);
        assert_eq!(sequence(Notify::Off, Focus::Unknown, "t", "m", false), None);
    }

    #[test]
    fn a_terminal_that_never_said_where_focus_is_still_hears_the_bell() {
        assert_eq!(
            sequence(Notify::Bell, Focus::Unknown, "t", "m", false).as_deref(),
            Some("\x07")
        );
        assert_eq!(
            sequence(Notify::Bell, Focus::Out, "t", "m", true).as_deref(),
            Some("\x07")
        );
    }

    #[test]
    fn desktop_notifications_name_the_workspace_and_the_outcome() {
        assert_eq!(
            sequence(Notify::Osc9, Focus::Out, "saddle web", "finished", false).as_deref(),
            Some("\x1b]9;saddle web: finished\x07")
        );
        assert_eq!(
            sequence(Notify::Osc777, Focus::Out, "saddle web", "finished", false).as_deref(),
            Some("\x1b]777;notify;saddle web;finished\x07")
        );
    }

    #[test]
    fn a_name_cannot_end_the_sequence_or_split_the_title() {
        let text = sequence(
            Notify::Osc777,
            Focus::Out,
            "saddle a;b\x07\x1b]0;x\u{9c}",
            "done\x1b",
            false,
        );
        assert_eq!(
            text.as_deref(),
            Some("\x1b]777;notify;saddle a,b]0,x;done\x07")
        );
    }

    #[test]
    fn under_tmux_an_osc_is_wrapped_with_its_escapes_doubled() {
        assert_eq!(
            sequence(Notify::Osc9, Focus::Out, "s", "m", true).as_deref(),
            Some("\x1bPtmux;\x1b\x1b]9;s: m\x07\x1b\\")
        );
    }

    #[test]
    fn only_a_cancelled_turn_goes_unannounced() {
        assert_eq!(ended(End::Cancelled), None);
        for end in [
            End::Answered,
            End::Declined,
            End::Cut,
            End::StepLimit,
            End::Repeating,
            End::Failed,
        ] {
            assert!(ended(end).is_some(), "{end:?}");
        }
    }
}
