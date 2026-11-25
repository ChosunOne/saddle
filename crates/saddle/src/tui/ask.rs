//! The dialog a tool's question opens (D197).
//!
//! A question is answered in two moves at most: pick a choice, then add a note or enter to finish.
//! A preview is shown for the choice under the cursor, which is what makes a choice more than its
//! label. Escape dismisses the question — an answer of its own, not an error — while ctrl+c cancels
//! the turn the question belongs to. Nothing here decides anything: the answer travels back to the
//! waiting tool through the question it arrived with.

use crate::questions::{Asked, Outcome, Reply, Selection};
use saddle_limits as budgets;
use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Where the keyboard is in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Moving through the choices.
    Choosing,
    /// Writing the note that goes with the chosen choice.
    Noting { choice: usize, text: String },
    /// Writing an answer of one's own.
    Typing { text: String },
}

/// What one key did to the question.
#[derive(Debug, Clone)]
pub enum Answer {
    /// The dialog stays open.
    Stay,
    /// The person answered; the dialog closes.
    Given(Outcome),
    /// The person cancelled the turn the question belongs to.
    Cancel,
}

pub struct AskDialog {
    pub asked: Arc<Asked>,
    /// One row per choice, then the row whose answer is typed when it is offered.
    pub selected: usize,
    pub stage: Stage,
    /// Why the last attempt was refused, such as an empty typed answer.
    pub error: Option<String>,
}

impl AskDialog {
    pub fn new(asked: Arc<Asked>) -> Self {
        Self {
            asked,
            selected: 0,
            stage: Stage::Choosing,
            error: None,
        }
    }

    /// How many rows the list has: the choices, and the typed answer when the question offers one.
    pub fn rows(&self) -> usize {
        self.asked.question.choices.len() + usize::from(self.asked.question.allow_other)
    }

    /// The chosen index, or `None` for the typed-answer row.
    pub fn choice(&self, row: usize) -> Option<usize> {
        (row < self.asked.question.choices.len()).then_some(row)
    }

    pub fn key(&mut self, key: KeyEvent) -> Answer {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Answer::Stay;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Answer::Cancel;
        }
        self.error = None;
        match self.stage.clone() {
            Stage::Choosing => self.choosing(key),
            Stage::Noting { choice, mut text } => match key.code {
                KeyCode::Esc => {
                    self.stage = Stage::Choosing;
                    Answer::Stay
                }
                KeyCode::Enter => Answer::Given(if self.asked.question.allow_note {
                    Outcome::Answered(Reply {
                        selection: Selection::Chosen(choice as u32),
                        note: text,
                    })
                } else {
                    Outcome::Answered(Reply {
                        selection: Selection::Chosen(choice as u32),
                        note: String::new(),
                    })
                }),
                _ => {
                    if edit(&mut text, key) {
                        self.stage = Stage::Noting { choice, text };
                    }
                    Answer::Stay
                }
            },
            Stage::Typing { mut text } => match key.code {
                KeyCode::Esc => {
                    self.stage = Stage::Choosing;
                    Answer::Stay
                }
                KeyCode::Enter if text.trim().is_empty() => {
                    self.error = Some("type an answer, or escape to choose again".into());
                    Answer::Stay
                }
                KeyCode::Enter => Answer::Given(Outcome::Answered(Reply {
                    selection: Selection::Typed(text.trim().to_owned()),
                    note: String::new(),
                })),
                _ => {
                    if edit(&mut text, key) {
                        self.stage = Stage::Typing { text };
                    }
                    Answer::Stay
                }
            },
        }
    }

    /// Pasted text goes into the line being written, never into the prompt under the dialog.
    pub fn paste(&mut self, text: &str) {
        let (Stage::Noting { text: note, .. } | Stage::Typing { text: note }) = &mut self.stage
        else {
            return;
        };
        for c in text.chars() {
            if note.len() >= budgets::ask_reply_bytes() {
                break;
            }
            if !c.is_control() {
                note.push(c);
            }
        }
        self.error = None;
    }

    fn choosing(&mut self, key: KeyEvent) -> Answer {
        let rows = self.rows().max(1);
        match key.code {
            KeyCode::Esc => Answer::Given(Outcome::Dismissed),
            KeyCode::Up | KeyCode::Left | KeyCode::Char('k') => {
                self.selected = self.selected.checked_sub(1).unwrap_or(rows - 1);
                Answer::Stay
            }
            KeyCode::Down | KeyCode::Tab | KeyCode::Right | KeyCode::Char('j') => {
                self.selected = (self.selected + 1) % rows;
                Answer::Stay
            }
            // A digit is the shortest way to a choice whose preview the person does not need.
            KeyCode::Char(digit @ '1'..='9') => {
                let row = digit.to_digit(10).unwrap_or(1) as usize - 1;
                if row < rows {
                    self.selected = row;
                    self.choose()
                } else {
                    Answer::Stay
                }
            }
            KeyCode::Enter => self.choose(),
            _ => Answer::Stay,
        }
    }

    /// Enter on the highlighted row: a note first when one is offered, else the answer itself.
    fn choose(&mut self) -> Answer {
        match self.choice(self.selected) {
            Some(choice) if self.asked.question.allow_note => {
                self.stage = Stage::Noting {
                    choice,
                    text: String::new(),
                };
                Answer::Stay
            }
            Some(choice) => Answer::Given(Outcome::Answered(Reply {
                selection: Selection::Chosen(choice as u32),
                note: String::new(),
            })),
            None => {
                self.stage = Stage::Typing {
                    text: String::new(),
                };
                Answer::Stay
            }
        }
    }
}

/// Applies one editing key, answering whether the text changed enough to redraw.
fn edit(text: &mut String, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Backspace => text.pop().is_some(),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let changed = !text.is_empty();
            text.clear();
            changed
        }
        KeyCode::Char(c) if !c.is_control() && text.len() < budgets::ask_reply_bytes() => {
            text.push(c);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;
    use saddle_wit::saddle::host::ask::{Choice, Question};
    use std::sync::mpsc::Receiver;

    fn dialog() -> (AskDialog, Receiver<Outcome>) {
        let question = Question {
            question: "which one?".into(),
            choices: vec![
                Choice {
                    label: "first".into(),
                    description: String::new(),
                    preview: "the first preview".into(),
                },
                Choice {
                    label: "second".into(),
                    description: "a line about it".into(),
                    preview: String::new(),
                },
            ],
            allow_other: true,
            allow_note: true,
        };
        let (asked, answers) = Asked::pair(question);
        (AskDialog::new(asked), answers)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn a_choice_takes_a_note_and_the_note_is_the_answer() {
        let (mut dialog, _answers) = dialog();
        // Down to the second choice, enter to choose it.
        assert!(matches!(dialog.key(key(KeyCode::Down)), Answer::Stay));
        assert_eq!(dialog.selected, 1);
        assert!(matches!(dialog.key(key(KeyCode::Enter)), Answer::Stay));
        assert_eq!(
            dialog.stage,
            Stage::Noting {
                choice: 1,
                text: String::new()
            }
        );
        for c in "keep this".chars() {
            dialog.key(key(KeyCode::Char(c)));
        }
        let Answer::Given(Outcome::Answered(reply)) = dialog.key(key(KeyCode::Enter)) else {
            panic!("the note finishes the answer");
        };
        assert!(matches!(reply.selection, Selection::Chosen(1)));
        assert_eq!(reply.note, "keep this");
    }

    #[test]
    fn a_digit_picks_that_choice_and_escape_dismisses_the_question() {
        let (mut dialog, _answers) = dialog();
        assert!(
            matches!(dialog.key(key(KeyCode::Char('1'))), Answer::Stay),
            "a choice with a note offered asks for the note first"
        );
        // The note stage is where escape goes back, not away.
        assert!(matches!(dialog.key(key(KeyCode::Esc)), Answer::Stay));
        assert_eq!(dialog.stage, Stage::Choosing);
        assert!(matches!(
            dialog.key(key(KeyCode::Esc)),
            Answer::Given(Outcome::Dismissed)
        ));
    }

    #[test]
    fn the_last_row_is_an_answer_of_one_s_own_and_needs_words() {
        let (mut dialog, _answers) = dialog();
        // Three rows: two choices and the typed one.
        assert_eq!(dialog.rows(), 3);
        // Up from the first row wraps to the typed-answer row, which is the last one.
        dialog.key(key(KeyCode::Up));
        assert_eq!(dialog.selected, 2);
        assert!(matches!(dialog.key(key(KeyCode::Enter)), Answer::Stay));
        assert!(matches!(dialog.stage, Stage::Typing { .. }));
        assert!(matches!(dialog.key(key(KeyCode::Enter)), Answer::Stay));
        assert!(dialog.error.is_some());
        for c in "neither".chars() {
            dialog.key(key(KeyCode::Char(c)));
        }
        let Answer::Given(Outcome::Answered(reply)) = dialog.key(key(KeyCode::Enter)) else {
            panic!("the typed answer finishes the question");
        };
        assert!(matches!(reply.selection, Selection::Typed(ref text) if text == "neither"));
        assert_eq!(reply.note, "");
    }

    #[test]
    fn a_question_without_a_note_answers_on_the_first_enter_and_without_other_has_one_row() {
        let (asked, _answers) = Asked::pair(Question {
            question: "confirm?".into(),
            choices: vec![Choice {
                label: "yes".into(),
                description: String::new(),
                preview: String::new(),
            }],
            allow_other: false,
            allow_note: false,
        });
        let mut dialog = AskDialog::new(asked);
        assert_eq!(dialog.rows(), 1);
        let Answer::Given(Outcome::Answered(reply)) = dialog.key(key(KeyCode::Enter)) else {
            panic!("a question without a note answers at once");
        };
        assert!(matches!(reply.selection, Selection::Chosen(0)));
        assert_eq!(reply.note, "");
    }

    #[test]
    fn pasted_words_go_into_the_line_the_question_is_asking_for() {
        let (mut dialog, _answers) = dialog();
        dialog.paste("not while choosing");
        assert_eq!(dialog.stage, Stage::Choosing);
        dialog.key(key(KeyCode::Enter));
        dialog.paste("a pasted note \u{1b}[2J");
        assert_eq!(
            dialog.stage,
            Stage::Noting {
                choice: 0,
                text: "a pasted note [2J".into()
            }
        );
    }

    #[test]
    fn ctrl_c_cancels_the_turn_rather_than_answering() {
        let (mut dialog, _answers) = dialog();
        assert!(matches!(
            dialog.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Answer::Cancel
        ));
    }
}
