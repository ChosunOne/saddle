//! A question a tool puts to the person, and the answer they give it (D197).
//!
//! The tool owns the words and the choices; this module owns the wait, the record and one answer
//! per question. A question is not authority: the person may dismiss it, and the guest is told so
//! rather than being handed an answer that nobody gave. A session with nobody in front of it — a
//! piped run, a JSON stream, a worker, `saddle tool` — answers `unavailable` with the reason.
//!
//! The wait is the person's: reading four choices takes longer than the guest's own two-second CPU
//! ceiling and longer than any budget the harness would spend on a machine's work, so nothing bounds
//! it unless the person set `ask-wait-seconds` or the caller set a deadline of its own. The turn's
//! cancellation flag ends the wait too: escape or ctrl+c on a running turn must not leave a dialog
//! owning the keyboard.

use crate::cancel;
use crate::session::Session;
use anyhow::Result;
use saddle_cap::{Principal, Scope};
use saddle_policy::Kind;
use saddle_policy::Request;
use saddle_policy::audit::sha256_hex;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use saddle_wit::saddle::host::ask::{Outcome, Question, Reply, Selection};

/// How long one poll of the answer channel waits before the cancellation flag is read again.
const POLL: Duration = Duration::from_millis(50);

/// One question waiting for its answer in the person's own interface. The interface takes the
/// answer exactly once; dropping the question unanswered is what a closed interface looks like.
#[derive(Debug)]
pub struct Asked {
    pub question: Question,
    reply: Mutex<Option<SyncSender<Outcome>>>,
    /// Whether the wait is still running, so an interface drawing a question it can no longer answer
    /// can take it down: the turn was cancelled, or a deadline the person set ran out (D197).
    open: AtomicBool,
}

impl Asked {
    /// The question, and the end of a channel that carries the person's one answer.
    pub fn pair(question: Question) -> (Arc<Self>, Receiver<Outcome>) {
        let (reply, answers) = mpsc::sync_channel(1);
        (
            Arc::new(Self {
                question,
                reply: Mutex::new(Some(reply)),
                open: AtomicBool::new(true),
            }),
            answers,
        )
    }

    /// Whether the question is still waiting: false once nobody is listening for an answer.
    pub fn open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }

    /// The wait is over, answered or not, so an interface still showing the question drops it.
    pub fn close(&self) {
        self.open.store(false, Ordering::Relaxed);
    }

    /// The person's answer. A second call does nothing: the first answer is the answer.
    pub fn answer(&self, outcome: Outcome) {
        let Ok(mut slot) = self.reply.lock() else {
            return;
        };
        if let Some(reply) = slot.take() {
            let _ = reply.send(outcome);
        }
    }
}

/// Two questions are the same when they would draw the same dialog. The ABI's records carry no
/// comparison of their own, so this is written out here rather than derived (D197).
impl PartialEq for Asked {
    fn eq(&self, other: &Self) -> bool {
        let a = &self.question;
        let b = &other.question;
        a.question == b.question
            && a.allow_other == b.allow_other
            && a.allow_note == b.allow_note
            && a.choices.len() == b.choices.len()
            && a.choices.iter().zip(&b.choices).all(|(a, b)| {
                a.label == b.label && a.description == b.description && a.preview == b.preview
            })
    }
}

impl Eq for Asked {}

/// The interface a session asks on: the TUI's dialog, or the terminal in line mode.
pub trait Questioner: Send {
    /// Put this question to the person. The answer, or `Outcome::Unavailable` when this interface
    /// cannot ask, travels through `asked`; this call returns once it has been shown or refused.
    /// `until` is the whole wait when there is one, for an interface that reads on this thread and
    /// can only notice a deadline or a cancelled turn after the line it is waiting for arrives
    /// (D197).
    fn ask(&mut self, asked: Arc<Asked>, until: Option<Instant>);
}

/// Put a question to this session's person, decide it under the tool's registration, and record it
/// without its words. `allowance` is the whole wait when the person set one or the caller carries a
/// deadline of its own; `None` waits for the person (D197).
pub fn ask(
    session: &mut Session,
    who: &Principal,
    call: &str,
    allowance: Option<Duration>,
    question: Question,
) -> Result<Result<Outcome, String>> {
    // Registration already decided that this principal may run its tool; the question is recorded
    // under that decision like any other host operation, with the words hashed rather than kept.
    let request = Request::Plain(Scope::Tool);
    let decision = session.engine.decide(who, &request);
    session.audit.decision_for_call_details(
        who,
        &request,
        &decision,
        call,
        json!({
            "operation": "ask",
            "question_bytes": question.question.len(),
            "question_sha256": sha256_hex(question.question.as_bytes()),
            "choices": question.choices.len(),
            "allow_other": question.allow_other,
            "allow_note": question.allow_note,
            "deadline_secs": allowance.map(|allowance| allowance.as_secs()),
        }),
    )?;
    if !decision.allow {
        return Ok(Err(format!("denied: {}", decision.explanation)));
    }
    let Some(questioner) = session.questions.as_mut() else {
        return unavailable(
            session,
            who,
            call,
            &question,
            "this session has no interface a person can answer on",
        );
    };
    let (asked, answers) = Asked::pair(question.clone());
    let until = allowance.map(|allowance| Instant::now() + allowance);
    questioner.ask(asked.clone(), until);
    let outcome = wait(answers, until);
    asked.close();
    record(session, who, call, &question, &outcome)?;
    Ok(Ok(outcome))
}

/// The wait itself: one answer, a dismissal when the turn is cancelled, and an explanation when the
/// interface or the allowance goes away first. With no `until` the person has as long as they like.
fn wait(answers: Receiver<Outcome>, until: Option<Instant>) -> Outcome {
    loop {
        match answers.recv_timeout(POLL) {
            Ok(outcome) => return outcome,
            // A turn the person cancelled has no question left to answer (D183).
            Err(RecvTimeoutError::Timeout) if cancel::requested() => return Outcome::Dismissed,
            Err(RecvTimeoutError::Timeout)
                if until.is_some_and(|until| Instant::now() >= until) =>
            {
                return Outcome::Unavailable("the person did not answer in time".into());
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Outcome::Unavailable("the interface closed before answering".into());
            }
        }
    }
}

fn unavailable(
    session: &mut Session,
    who: &Principal,
    call: &str,
    question: &Question,
    why: &str,
) -> Result<Result<Outcome, String>> {
    let outcome = Outcome::Unavailable(why.to_owned());
    record(session, who, call, question, &outcome)?;
    Ok(Ok(outcome))
}

/// What a person answered, never the words they typed: a note and an answer of their own are their
/// own text, so the record keeps their size and hash (D197).
fn record(
    session: &mut Session,
    who: &Principal,
    call: &str,
    question: &Question,
    outcome: &Outcome,
) -> Result<()> {
    let answered = match outcome {
        Outcome::Answered(Reply {
            selection: Selection::Chosen(index),
            note,
        }) => json!({
            "selection": "chosen",
            "choice": index,
            "note_bytes": note.len(),
            "note_sha256": sha256_hex(note.as_bytes()),
        }),
        Outcome::Answered(Reply {
            selection: Selection::Typed(text),
            ..
        }) => json!({
            "selection": "typed",
            "typed_bytes": text.len(),
            "typed_sha256": sha256_hex(text.as_bytes()),
        }),
        Outcome::Dismissed => json!({ "selection": "dismissed" }),
        Outcome::Unavailable(why) => json!({
            "selection": "unavailable",
            "why_sha256": sha256_hex(why.as_bytes()),
        }),
    };
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(Scope::Tool),
        Some(true),
        json!({
            "call_id": call,
            "operation": "asked",
            "question_sha256": sha256_hex(question.question.as_bytes()),
            "answer": answered,
        }),
        "the person answered the question",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use saddle_wit::saddle::host::ask::Choice;

    fn question() -> Question {
        Question {
            question: "which one?".into(),
            choices: vec![Choice {
                label: "first".into(),
                description: String::new(),
                preview: String::new(),
                recommended: false,
            }],
            allow_other: true,
            allow_note: true,
        }
    }

    #[test]
    fn a_question_carries_exactly_one_answer() {
        let (asked, answers) = Asked::pair(question());
        asked.answer(Outcome::Dismissed);
        asked.answer(Outcome::Unavailable("second".into()));
        // The second answer is dropped: one question has one answer.
        assert!(matches!(answers.try_recv(), Ok(Outcome::Dismissed)));
        assert!(answers.try_recv().is_err());
    }

    #[test]
    fn the_answer_arrives_but_a_closed_interface_or_a_cancelled_turn_does_not_wait_forever() {
        let (asked, answers) = Asked::pair(question());
        asked.answer(Outcome::Dismissed);
        assert!(matches!(
            wait(answers, Some(Instant::now() + Duration::from_secs(1))),
            Outcome::Dismissed
        ));

        // Nobody answers before the allowance runs out.
        let (asked, answers) = Asked::pair(question());
        drop(asked);
        assert!(matches!(
            wait(answers, Some(Instant::now() + Duration::from_secs(1))),
            Outcome::Unavailable(ref why) if why.contains("closed")
        ));

        // A cancelled turn ends the wait at the next poll rather than at the allowance.
        let (_asked, answers) = Asked::pair(question());
        cancel::request();
        let started = Instant::now();
        let outcome = wait(answers, Some(Instant::now() + Duration::from_secs(30)));
        cancel::reset();
        assert!(matches!(outcome, Outcome::Dismissed));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn without_an_allowance_the_person_has_as_long_as_they_like() {
        use std::thread;
        use std::time::Duration;

        let (asked, answers) = Asked::pair(question());
        assert!(asked.open(), "the question is open while it waits");
        // Longer than any poll or allowance, and shorter than a person reading four choices.
        let later = asked.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            later.answer(Outcome::Dismissed);
        });
        let started = Instant::now();
        assert!(matches!(wait(answers, None), Outcome::Dismissed));
        // Dismissed only because it was answered, never because the clock ran out.
        assert!(started.elapsed() >= Duration::from_millis(250));
        asked.close();
        assert!(
            !asked.open(),
            "a closed question is one no interface should keep drawing"
        );
    }

    /// The whole host side of D197, against a real session: the standard component's principal is
    /// decided by the engine, the question reaches the interface, the person's answer comes back, and
    /// the audit keeps the words' size and hash rather than the words.
    #[test]
    fn a_session_records_the_question_and_returns_the_answer_its_person_gave() {
        use crate::config::Overrides;
        use crate::session::Session;
        use saddle_cap::PluginId;
        use std::env;
        use std::fs;

        struct Answering;
        impl Questioner for Answering {
            fn ask(&mut self, asked: Arc<Asked>, _until: Option<Instant>) {
                assert_eq!(asked.question.choices.len(), 2, "the question as written");
                asked.answer(Outcome::Answered(Reply {
                    selection: Selection::Chosen(1),
                    note: "only on Tuesdays".into(),
                }));
            }
        }

        let ws = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        let config = xdg.path().join("config");
        fs::create_dir_all(config.join("saddle")).unwrap();
        fs::write(
            config.join("saddle/config.toml"),
            "[standard-tools]\nask = []\n",
        )
        .unwrap();
        // SAFETY: one process per test (nextest), and nothing else reads these.
        unsafe {
            env::set_var("XDG_CONFIG_HOME", &config);
            env::set_var("XDG_STATE_HOME", xdg.path().join("state"));
            env::set_var("HOME", xdg.path().join("home"));
        }
        let mut session = Session::open_with(ws.path(), None, &Overrides::default()).unwrap();
        session.questions = Some(Box::new(Answering));
        let who = Principal::Plugin(PluginId::new("standard-ask").unwrap());
        let question = Question {
            question: "which day should the report run?".into(),
            choices: vec![
                Choice {
                    label: "Monday".into(),
                    description: String::new(),
                    preview: String::new(),
                    recommended: false,
                },
                Choice {
                    label: "Tuesday".into(),
                    description: String::new(),
                    preview: "the quiet day".into(),
                    recommended: true,
                },
            ],
            allow_other: true,
            allow_note: true,
        };
        let outcome = ask(&mut session, &who, "call-1", None, question)
            .unwrap()
            .unwrap();
        match outcome {
            Outcome::Answered(reply) => {
                assert!(matches!(reply.selection, Selection::Chosen(1)));
                assert_eq!(reply.note, "only on Tuesdays");
            }
            _ => panic!("the person answered"),
        }
        let log = fs::read_to_string(
            fs::read_dir(session.setup.paths.audit_dir())
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
        )
        .unwrap();
        assert!(log.contains("question_sha256"), "{log}");
        assert!(log.contains("\"choice\":1"), "{log}");
        assert!(log.contains("note_sha256"), "{log}");
        assert!(!log.contains("which day should the report run?"), "{log}");
        assert!(!log.contains("only on Tuesdays"), "{log}");
        session.close().unwrap();
    }

    #[test]
    fn dropping_the_question_is_what_a_closed_interface_looks_like() {
        let (asked, answers) = Asked::pair(question());
        drop(asked);
        assert!(matches!(
            answers.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }
}
