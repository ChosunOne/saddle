//! ask the person a question, with choices, previews and an optional note (D197).
//!
//! The guest owns the schema, the bounds and how the answer is put to the model; the host owns the
//! question surface, the audit and the wait, because a question reaches a person rather than a file
//! or a socket. Asking grants nothing: the person may dismiss the question, and that dismissal is an
//! answer the model is told not to ask again. The preview of a choice is shown while it is selected,
//! so a choice can be judged by a diff or a paragraph rather than by its label alone.

use saddle_limits as budgets;
use saddle_sdk::question::{Outcome, Reply, Selection};
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use saddle_sdk::question::{self, Choice as WireChoice, Question};
#[cfg(target_arch = "wasm32")]
use saddle_sdk::{Definition, Output, Tool, configure_limits};
#[cfg(target_arch = "wasm32")]
use serde_json::json;

pub struct Ask;

fn yes() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    pub question: String,
    pub options: Vec<Choice>,
    #[serde(default = "yes")]
    pub allow_other: bool,
    #[serde(default = "yes")]
    pub allow_note: bool,
}

/// One choice as the model writes it: a label, an optional line of subtext under it, an optional
/// preview shown while it is selected, and whether it is the one being recommended.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub recommended: bool,
}

impl Arguments {
    /// Refuse a question the interface could not draw before anything reaches the person.
    pub fn validate(&self) -> Result<(), String> {
        if self.question.trim().is_empty() {
            return Err("question must not be empty".into());
        }
        if self.question.len() > budgets::ask_bytes() {
            return Err("question exceeds the configured size limit".into());
        }
        if self.options.is_empty() || self.options.len() > budgets::ask_options() {
            return Err(format!(
                "options must hold between 1 and {} choices",
                budgets::ask_options()
            ));
        }
        if !self.allow_other && self.options.len() == 1 && !self.allow_note {
            return Err(
                "a question with one choice and no note is not a question: offer another choice, \
                 allow an answer of the person's own, or allow a note"
                    .into(),
            );
        }
        if self
            .options
            .iter()
            .filter(|choice| choice.recommended)
            .count()
            > 1
        {
            return Err("at most one choice can be recommended".into());
        }
        for (index, choice) in self.options.iter().enumerate() {
            let number = index + 1;
            if choice.label.trim().is_empty() {
                return Err(format!("choice {number} has an empty label"));
            }
            if choice.label.contains('\n') || choice.description.contains('\n') {
                return Err(format!(
                    "choice {number}: the label and its description are one line each; put detail \
                     in the preview"
                ));
            }
            if choice.label.len().saturating_add(choice.description.len())
                > budgets::ask_option_bytes()
            {
                return Err(format!(
                    "choice {number} exceeds the configured label and description limit"
                ));
            }
            if choice.preview.len() > budgets::ask_preview_bytes() {
                return Err(format!(
                    "choice {number} exceeds the configured preview limit"
                ));
            }
        }
        Ok(())
    }

    /// What the model is told, and whether the call succeeded. A dismissal is not an answer: it is
    /// reported as a failure so the model does not read silence as consent.
    pub fn answer(&self, outcome: &Outcome) -> (String, bool) {
        match outcome {
            Outcome::Answered(reply) => match self.render(reply) {
                Ok(text) => (text, true),
                Err(error) => (error, false),
            },
            Outcome::Dismissed => (
                "The person dismissed the question without answering. Do not ask it again: continue \
                 without it, or say in your reply what you would need."
                    .into(),
                false,
            ),
            Outcome::Unavailable(why) => (format!("No person could be asked: {why}"), false),
        }
    }

    fn render(&self, reply: &Reply) -> Result<String, String> {
        let mut text = match &reply.selection {
            Selection::Chosen(index) => {
                let choice = self.options.get(*index as usize).ok_or_else(|| {
                    "the person chose a choice that is not part of this question".to_owned()
                })?;
                format!(
                    "The person chose {}. of {}: {}",
                    index + 1,
                    self.options.len(),
                    choice.label
                )
            }
            Selection::Typed(answer) => {
                format!("The person typed their own answer: {}", answer.trim())
            }
        };
        let note = reply.note.trim();
        if !note.is_empty() {
            text.push_str("\nNote on this choice: ");
            text.push_str(note);
        }
        Ok(text)
    }
}

#[cfg(target_arch = "wasm32")]
impl Tool for Ask {
    fn describe() -> Definition {
        // The schema states the same bounds the host enforces, so read them from the snapshot the
        // host handed this instance; a session that cannot read it keeps the defaults (D145).
        let _ = configure_limits();
        Definition {
            name: "ask".into(),
            description: "ask the person a question when only they can decide — a preference, which \
                          of two acceptable directions to take, or a detail you cannot read from the \
                          repository. Each choice has a label and may carry a line of subtext under \
                          it, a preview shown while that choice is selected (a diff, a sketch or a \
                          paragraph, so it can be judged rather than only a label), and \
                          `recommended: true` on the one you advise. The person may attach a note to \
                          the choice they make, or type an answer of their own. The result is their \
                          answer. This is not a permission prompt: a grant decides what you may do. \
                          A dismissed question is not consent, so do not ask it again."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "the question, as one short paragraph",
                    },
                    "options": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": budgets::ask_options(),
                        "items": {
                            "type": "object",
                            "properties": {
                                "label": {
                                    "type": "string",
                                    "description": "the choice in one line",
                                },
                                "description": {
                                    "type": "string",
                                    "description": "subtext under the label, drawn in a quieter colour; one line, or empty",
                                },
                                "preview": {
                                    "type": "string",
                                    "description": "what the person sees while this choice is selected: a diff, a sketch or a paragraph, or empty",
                                },
                                "recommended": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "mark this as the choice you advise; at most one may be recommended",
                                },
                            },
                            "required": ["label"],
                            "additionalProperties": false,
                        },
                    },
                    "allow_other": {
                        "type": "boolean",
                        "default": true,
                        "description": "offer a choice whose answer the person types themselves",
                    },
                    "allow_note": {
                        "type": "boolean",
                        "default": true,
                        "description": "let the person add a note to the choice they make",
                    },
                },
                "required": ["question", "options"],
                "additionalProperties": false,
            })
            .to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
        if let Err(text) = configure_limits() {
            return Output {
                text,
                is_error: true,
            };
        }
        let result = serde_json::from_str::<Arguments>(&arguments)
            .map_err(|error| format!("invalid arguments: {error}"))
            .and_then(|arguments| {
                arguments.validate()?;
                question::ask(&Question {
                    question: arguments.question.clone(),
                    choices: arguments
                        .options
                        .iter()
                        .map(|choice| WireChoice {
                            label: choice.label.clone(),
                            description: choice.description.clone(),
                            preview: choice.preview.clone(),
                            recommended: choice.recommended,
                        })
                        .collect(),
                    allow_other: arguments.allow_other,
                    allow_note: arguments.allow_note,
                })
                .map(|outcome| arguments.answer(&outcome))
            });
        match result {
            Ok((text, ok)) => Output {
                text,
                is_error: !ok,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(options: &[(&str, &str, &str)]) -> Arguments {
        Arguments {
            question: "which one?".into(),
            options: options
                .iter()
                .map(|(label, description, preview)| Choice {
                    label: (*label).into(),
                    description: (*description).into(),
                    preview: (*preview).into(),
                    recommended: false,
                })
                .collect(),
            allow_other: true,
            allow_note: true,
        }
    }

    #[test]
    fn a_question_needs_words_and_at_least_one_named_choice() {
        assert!(arguments(&[("a", "", "")]).validate().is_ok());
        let mut empty = arguments(&[("a", "", "")]);
        empty.question = "   ".into();
        assert!(empty.validate().unwrap_err().contains("empty"));
        let none = arguments(&[]);
        assert!(none.validate().unwrap_err().contains("between 1"));
        let many = arguments(&vec![("x", "", ""); budgets::ask_options() + 1]);
        assert!(many.validate().unwrap_err().contains("between 1"));
        assert!(arguments(&[("", "", "")]).validate().is_err());
        assert!(arguments(&[("two\nlines", "", "")]).validate().is_err());
        assert!(
            arguments(&[("a", "", &"p".repeat(budgets::ask_preview_bytes() + 1))])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn at_most_one_choice_is_the_one_being_recommended() {
        let mut question = arguments(&[("a", "", ""), ("b", "", "")]);
        assert!(question.validate().is_ok());
        question.options[1].recommended = true;
        assert!(question.validate().is_ok());
        question.options[0].recommended = true;
        assert!(
            question
                .validate()
                .unwrap_err()
                .contains("at most one choice")
        );
    }

    #[test]
    fn a_single_choice_question_needs_a_way_to_say_something_else() {
        let mut question = arguments(&[("yes", "", "")]);
        question.allow_other = false;
        question.allow_note = false;
        assert!(question.validate().is_err());
        question.allow_note = true;
        assert!(question.validate().is_ok());
    }

    #[test]
    fn a_chosen_choice_carries_its_number_and_the_note_about_it() {
        let arguments = arguments(&[("first", "", ""), ("second", "the one with a preview", "")]);
        let (text, ok) = arguments.answer(&Outcome::Answered(Reply {
            selection: Selection::Chosen(1),
            note: "  prefer this one  ".into(),
        }));
        assert!(ok, "{text}");
        assert_eq!(
            text,
            "The person chose 2. of 2: second\nNote on this choice: prefer this one"
        );
        let (text, ok) = arguments.answer(&Outcome::Answered(Reply {
            selection: Selection::Chosen(7),
            note: String::new(),
        }));
        assert!(!ok && text.contains("not part of this question"), "{text}");
    }

    #[test]
    fn a_typed_answer_is_the_person_s_own_words() {
        let arguments = arguments(&[("first", "", "")]);
        let (text, ok) = arguments.answer(&Outcome::Answered(Reply {
            selection: Selection::Typed("  neither; do the small one first  ".into()),
            note: String::new(),
        }));
        assert!(ok);
        assert_eq!(
            text,
            "The person typed their own answer: neither; do the small one first"
        );
    }

    #[test]
    fn a_dismissal_is_a_failure_and_says_so_without_pretending_it_is_consent() {
        let arguments = arguments(&[("first", "", "")]);
        let (text, ok) = arguments.answer(&Outcome::Dismissed);
        assert!(!ok);
        assert!(text.contains("Do not ask it again"), "{text}");
        let (text, ok) = arguments.answer(&Outcome::Unavailable("no terminal".into()));
        assert!(!ok);
        assert!(
            text.contains("No person could be asked: no terminal"),
            "{text}"
        );
    }
}
