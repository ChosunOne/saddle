//! `/tree`: the conversation as a tree, and where to continue from (D54).
//!
//! Opened by `/tree` or by pressing escape twice on an empty editor, which is what pi does. The
//! rows are every message on the branch, not just the prompts, because the interesting points are
//! often a reply you want to answer differently.
//!
//! Choosing a **prompt** puts its text back in the editor and moves the conversation to just before
//! it: sending it again — edited or not — makes another branch. Choosing anything else continues
//! after it with an empty editor. Nothing is deleted either way; a branch left behind is reachable
//! from the row that says another one leaves there.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::store::{Point, PointKind};

pub const ROWS: usize = 14;

pub struct Tree {
    pub points: Vec<Point>,
    pub selected: usize,
    /// Show only the prompts, which is what a long conversation needs (ctrl+u in pi).
    pub user_only: bool,
}

/// What a keypress asks the controller to do.
#[derive(Debug, PartialEq, Eq)]
pub enum TreeOutcome {
    Stay,
    Close,
    /// Continue from this message. `rewind_to` is the node the conversation moves to, and `text`
    /// is what to put back in the editor.
    Select {
        rewind_to: Option<i64>,
        text: Option<String>,
    },
    /// The whole message, for the clipboard.
    Copy(String),
}

impl Tree {
    pub fn new(points: Vec<Point>) -> Self {
        // The keyboard starts where the conversation is, which is the common answer: keep going.
        let selected = points
            .iter()
            .position(|p| p.here)
            .unwrap_or_else(|| points.len().saturating_sub(1));
        Self {
            points,
            selected,
            user_only: false,
        }
    }

    /// The points the filter lets through.
    pub fn visible(&self) -> Vec<&Point> {
        self.points
            .iter()
            .filter(|p| !self.user_only || p.kind == PointKind::User)
            .collect()
    }

    /// The point the keyboard is on.
    fn current(&self) -> Option<&Point> {
        self.visible().get(self.selected).copied()
    }

    pub fn key(&mut self, key: KeyEvent) -> TreeOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return TreeOutcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shown = self.visible().len();
        match key.code {
            KeyCode::Esc => return TreeOutcome::Close,
            KeyCode::Char('c') if ctrl => return TreeOutcome::Close,
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(shown.saturating_sub(1)),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(ROWS),
            KeyCode::PageDown => {
                self.selected = (self.selected + ROWS).min(shown.saturating_sub(1));
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = shown.saturating_sub(1),
            // ctrl+u / ctrl+a: only the prompts, or everything (pi's userOnly and all filters).
            KeyCode::Char('u') if ctrl => {
                self.user_only = !self.user_only;
                self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            }
            KeyCode::Char('a') if ctrl => {
                self.user_only = false;
                self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            }
            // Jump between prompts, which is how a long conversation is navigated (pi binds this
            // to ctrl+left and alt+left).
            KeyCode::Left if ctrl || alt => self.jump(false),
            KeyCode::Right if ctrl || alt => self.jump(true),
            KeyCode::Char('x') if ctrl => {
                if let Some(p) = self.current() {
                    let text = if p.text.is_empty() {
                        p.preview.clone()
                    } else {
                        p.text.clone()
                    };
                    return TreeOutcome::Copy(text);
                }
            }
            KeyCode::Enter => {
                return match self.current() {
                    // A prompt goes back to the editor, and the conversation moves to just before
                    // it so that sending it again makes another branch rather than appending.
                    Some(p) if p.kind == PointKind::User => TreeOutcome::Select {
                        rewind_to: p.parent,
                        text: Some(p.text.clone()),
                    },
                    Some(p) => TreeOutcome::Select {
                        rewind_to: Some(p.node),
                        text: None,
                    },
                    None => TreeOutcome::Stay,
                };
            }
            _ => {}
        }
        TreeOutcome::Stay
    }

    /// Moves the keyboard to the previous or next prompt.
    fn jump(&mut self, forward: bool) {
        let visible = self.visible();
        let is_prompt = |p: &Point| p.kind == PointKind::User;
        let found = if forward {
            (self.selected + 1..visible.len()).find(|i| is_prompt(visible[*i]))
        } else {
            (0..self.selected).rev().find(|i| is_prompt(visible[*i]))
        };
        if let Some(i) = found {
            self.selected = i;
        }
    }

    /// The first row to draw, so the selection stays on screen.
    pub fn offset(&self) -> usize {
        (self.selected + 1).saturating_sub(ROWS)
    }

    /// What a point reads. A message that is not on the active branch gets a mark of its own:
    /// it is still there, and choosing it goes back onto that branch (D54).
    pub fn label(&self, point: &Point) -> String {
        let mut out = format!("{} {}", point.kind.mark(), point.preview);
        if point.here {
            out.push_str("   ← here");
        } else if !point.active {
            out.push_str("   (other branch)");
        }
        out
    }

    /// Whether this is the message the conversation is at, for drawing it differently.
    pub fn is_here(&self, point: &Point) -> bool {
        point.here
    }

    /// Whether this message is on a branch the next request would not carry.
    pub fn is_other(&self, point: &Point) -> bool {
        !point.active
    }

    /// How many of the points are prompts, for the count in the footer.
    pub fn prompts(&self) -> usize {
        self.points
            .iter()
            .filter(|p| p.kind == PointKind::User)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(node: i64, kind: PointKind, preview: &str) -> Point {
        Point {
            node,
            parent: (node > 1).then(|| node - 1),
            kind,
            preview: preview.into(),
            text: if kind == PointKind::User {
                format!("{preview} (full)")
            } else {
                String::new()
            },
            here: false,
            active: true,
        }
    }

    fn press(tree: &mut Tree, code: KeyCode) -> TreeOutcome {
        tree.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// prompt, reply, tool result, prompt, reply — the ordinary shape of a conversation.
    fn conversation() -> Tree {
        let mut t = Tree::new(vec![
            point(1, PointKind::User, "first task"),
            point(2, PointKind::Assistant, "done"),
            point(3, PointKind::Tool, "tool results"),
            point(4, PointKind::User, "second task"),
            point(5, PointKind::Assistant, "also done"),
        ]);
        t.points[4].here = true;
        t.selected = t.points.len() - 1;
        t
    }

    #[test]
    fn the_keyboard_starts_at_the_message_the_conversation_is_at() {
        let tree = conversation();
        assert_eq!(tree.selected, 4);
        assert!(tree.is_here(&tree.points[4]));
        assert_eq!(tree.prompts(), 2);
    }

    #[test]
    fn a_prompt_goes_back_to_the_editor_and_moves_the_conversation_before_it() {
        // Choosing the second prompt: the conversation moves to the message before it, so sending
        // the text again makes a sibling branch instead of appending (D54).
        let mut tree = conversation();
        tree.selected = 3;
        assert_eq!(
            press(&mut tree, KeyCode::Enter),
            TreeOutcome::Select {
                rewind_to: Some(3),
                text: Some("second task (full)".into()),
            }
        );
        tree.selected = 0;
        assert_eq!(
            press(&mut tree, KeyCode::Enter),
            TreeOutcome::Select {
                rewind_to: None,
                text: Some("first task (full)".into()),
            }
        );
    }

    #[test]
    fn a_reply_continues_after_itself_with_an_empty_editor() {
        let mut tree = conversation();
        tree.selected = 1;
        assert_eq!(
            press(&mut tree, KeyCode::Enter),
            TreeOutcome::Select {
                rewind_to: Some(2),
                text: None,
            }
        );
    }

    #[test]
    fn ctrl_u_shows_only_prompts_and_ctrl_a_brings_the_rest_back() {
        let mut tree = conversation();
        tree.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(tree.user_only);
        let shown: Vec<i64> = tree.visible().iter().map(|p| p.node).collect();
        assert_eq!(shown, [1, 4]);
        // The selection stays inside the shorter list.
        assert!(tree.selected < tree.visible().len());
        tree.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!tree.user_only);
        assert_eq!(tree.visible().len(), 5);
    }

    #[test]
    fn ctrl_arrows_jump_between_prompts() {
        let mut tree = conversation();
        tree.selected = 0;
        // Forward: the second prompt, skipping the reply and the tool result between them.
        tree.key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(tree.selected, 3);
        tree.key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(tree.selected, 0);
        // At either end it stays put rather than wrapping.
        tree.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn a_message_on_another_branch_can_be_chosen_back() {
        // Rewinding leaves the old branch in the list, marked as not active; choosing it goes
        // back (D54).
        let mut tree = conversation();
        tree.points[3].active = false;
        assert!(tree.is_other(&tree.points[3]));
        assert!(tree.label(&tree.points[3]).contains("(other branch)"));
        tree.selected = 3;
        assert_eq!(
            press(&mut tree, KeyCode::Enter),
            TreeOutcome::Select {
                rewind_to: Some(3),
                text: Some("second task (full)".into()),
            }
        );
    }

    #[test]
    fn escape_closes_and_ctrl_x_copies_the_whole_message() {
        let mut tree = conversation();
        tree.selected = 3;
        assert_eq!(
            tree.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            TreeOutcome::Copy("second task (full)".into())
        );
        // A reply has no separate body, so its preview is what is copied.
        tree.selected = 1;
        assert_eq!(
            tree.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            TreeOutcome::Copy("done".into())
        );
        assert_eq!(press(&mut tree, KeyCode::Esc), TreeOutcome::Close);
        assert_eq!(
            tree.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            TreeOutcome::Close
        );
    }

    #[test]
    fn each_kind_has_a_mark_and_here_is_marked() {
        let tree = conversation();
        assert_eq!(tree.label(&tree.points[0]), "❯ first task");
        assert_eq!(tree.label(&tree.points[1]), "◆ done");
        assert_eq!(tree.label(&tree.points[2]), "⚙ tool results");
        assert_eq!(tree.label(&tree.points[4]), "◆ also done   ← here");
    }
}
