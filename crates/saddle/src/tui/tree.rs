use std::collections::HashSet;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::store::{Point, PointKind};
use crate::tui::keys::Keys;

pub const ROWS: usize = 14;

pub struct Tree {
    pub points: Vec<Point>,
    pub selected: usize,
    pub rows: usize,
    pub keys: Keys,
    pub user_only: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TreeOutcome {
    Stay,
    Close,
    Select {
        rewind_to: Option<i64>,
        text: Option<String>,
        left: usize,
    },
    Copy(String),
}

impl Tree {
    pub fn new(points: Vec<Point>) -> Self {
        let selected = points
            .iter()
            .position(|p| p.here)
            .unwrap_or_else(|| points.len().saturating_sub(1));
        Self {
            points,
            selected,
            rows: ROWS,
            keys: Keys::default(),
            user_only: false,
        }
    }

    pub fn visible(&self) -> Vec<&Point> {
        self.points
            .iter()
            .filter(|p| !self.user_only || p.kind == PointKind::User)
            .collect()
    }

    fn current(&self) -> Option<&Point> {
        self.visible().get(self.selected).copied()
    }

    pub fn key(&mut self, key: KeyEvent) -> TreeOutcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return TreeOutcome::Stay;
        }
        if self.keys.tree_next_end.as_ref().is_some_and(|k| k.is(&key)) {
            self.jump_leaf(true);
            return TreeOutcome::Stay;
        }
        if self
            .keys
            .tree_previous_end
            .as_ref()
            .is_some_and(|k| k.is(&key))
        {
            self.jump_leaf(false);
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
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(self.rows.max(1)),
            KeyCode::PageDown => {
                self.selected = (self.selected + self.rows.max(1)).min(shown.saturating_sub(1));
            }

            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = shown.saturating_sub(1),
            KeyCode::Char('u') if ctrl => {
                self.user_only = !self.user_only;
                self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            }
            KeyCode::Char('a') if ctrl => {
                self.user_only = false;
                self.selected = self.selected.min(self.visible().len().saturating_sub(1));
            }
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
                    Some(p) if p.kind == PointKind::User => TreeOutcome::Select {
                        rewind_to: p.parent,
                        text: Some(p.text.clone()),
                        left: self.left_behind(p.parent),
                    },
                    Some(p) => TreeOutcome::Select {
                        rewind_to: Some(p.node),
                        text: None,
                        left: self.left_behind(Some(p.node)),
                    },
                    None => TreeOutcome::Stay,
                };
            }
            _ => {}
        }
        TreeOutcome::Stay
    }

    fn left_behind(&self, target: Option<i64>) -> usize {
        self.points
            .iter()
            .filter(|p| p.active && target.is_none_or(|t| p.node > t))
            .filter(|p| matches!(p.kind, PointKind::Assistant | PointKind::Tool))
            .count()
    }

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

    pub fn leaves(&self) -> Vec<usize> {
        let parents: HashSet<i64> = self.points.iter().filter_map(|p| p.parent).collect();
        self.visible()
            .iter()
            .enumerate()
            .filter(|(_, p)| !parents.contains(&p.node))
            .map(|(i, _)| i)
            .collect()
    }

    fn jump_leaf(&mut self, forward: bool) {
        let leaves = self.leaves();
        let found = if forward {
            leaves.into_iter().find(|i| *i > self.selected)
        } else {
            leaves.into_iter().rev().find(|i| *i < self.selected)
        };
        if let Some(i) = found {
            self.selected = i;
        }
    }

    pub fn offset(&self) -> usize {
        (self.selected + 1).saturating_sub(self.rows.max(1))
    }

    pub fn label(&self, point: &Point) -> String {
        let mut out = format!("{} {}", point.kind.mark(), point.preview);
        if point.here {
            out.push_str("   ← here");
        } else if !point.active {
            out.push_str("   (other branch)");
        }
        out
    }

    pub fn is_here(&self, point: &Point) -> bool {
        point.here
    }

    pub fn is_other(&self, point: &Point) -> bool {
        !point.active
    }

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
    use crate::tui::keys::Stroke;

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

    fn stroke(tree: &mut Tree, text: &str) {
        let stroke = Stroke::parse(text).expect("a key");
        tree.key(stroke.keystroke());
    }

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
        let mut tree = conversation();
        tree.selected = 3;
        assert_eq!(
            press(&mut tree, KeyCode::Enter),
            TreeOutcome::Select {
                rewind_to: Some(3),
                text: Some("second task (full)".into()),
                left: 1,
            }
        );
        tree.selected = 0;
        assert_eq!(
            press(&mut tree, KeyCode::Enter),
            TreeOutcome::Select {
                rewind_to: None,
                text: Some("first task (full)".into()),
                left: 3,
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
                left: 2,
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
        assert!(tree.selected < tree.visible().len());
        tree.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert!(!tree.user_only);
        assert_eq!(tree.visible().len(), 5);
    }

    #[test]
    fn ctrl_arrows_jump_between_prompts() {
        let mut tree = conversation();
        tree.selected = 0;
        tree.key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(tree.selected, 3);
        tree.key(KeyEvent::new(KeyCode::Left, KeyModifiers::ALT));
        assert_eq!(tree.selected, 0);
        tree.key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn a_message_on_another_branch_can_be_chosen_back() {
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
                left: 1,
            }
        );
    }

    #[test]
    fn what_a_move_leaves_behind_is_counted_the_way_a_summary_would_use_it() {
        let mut tree = conversation();
        tree.selected = 3;
        let TreeOutcome::Select { left, .. } = press(&mut tree, KeyCode::Enter) else {
            panic!("a prompt continues the conversation")
        };
        assert_eq!(left, 1, "the reply that followed in the branch");
        tree.selected = 4;
        let TreeOutcome::Select { left, .. } = press(&mut tree, KeyCode::Enter) else {
            panic!("a reply continues the conversation")
        };
        assert_eq!(left, 0);
        tree.points.push(point(6, PointKind::User, "third task"));
        tree.selected = 5;
        let TreeOutcome::Select { left, .. } = press(&mut tree, KeyCode::Enter) else {
            panic!("a prompt continues the conversation")
        };
        assert_eq!(left, 0, "a prompt that produced nothing is not work");
    }

    fn branched() -> Tree {
        let mut points = vec![
            point(1, PointKind::User, "first task"),
            point(2, PointKind::Assistant, "the reply"),
            point(3, PointKind::User, "try A"),
            point(4, PointKind::Assistant, "A done"),
            point(5, PointKind::User, "try B"),
            point(6, PointKind::Assistant, "B done"),
        ];
        points[4].parent = Some(2);
        Tree::new(points)
    }

    #[test]
    fn the_ends_of_the_branches_are_one_bound_key_apart() {
        let mut tree = branched();
        assert_eq!(
            tree.leaves(),
            vec![3, 5],
            "the two replies end the two branches"
        );
        tree.selected = 0;
        press(&mut tree, KeyCode::Tab);
        assert_eq!(tree.selected, 0, "no key bound, so the keyboard stays");
        tree.keys.tree_next_end = Some(Stroke::parse("ctrl+n").unwrap());
        tree.keys.tree_previous_end = Some(Stroke::parse("ctrl+p").unwrap());
        stroke(&mut tree, "ctrl+n");
        assert_eq!(tree.selected, 3);
        stroke(&mut tree, "ctrl+n");
        assert_eq!(tree.selected, 5);
        stroke(&mut tree, "ctrl+n");
        assert_eq!(tree.selected, 5, "at the last end it stays put");
        stroke(&mut tree, "ctrl+p");
        assert_eq!(tree.selected, 3);
        stroke(&mut tree, "ctrl+p");
        assert_eq!(tree.selected, 3, "and at the first");
        let mut line = conversation();
        line.selected = 0;
        line.keys = tree.keys.clone();
        assert_eq!(line.leaves().len(), 1);
        stroke(&mut line, "ctrl+n");
        assert_eq!(line.selected, 4);
        tree.user_only = true;
        tree.selected = 0;
        assert!(tree.leaves().is_empty(), "{:?}", tree.leaves());
        stroke(&mut tree, "ctrl+n");
        assert_eq!(tree.selected, 0);
        let filtered = tree.user_only;
        stroke(&mut tree, "ctrl+u");
        assert_ne!(tree.user_only, filtered, "the filter key is not swallowed");
    }

    #[test]
    fn a_page_is_the_height_the_view_has() {
        let mut tree = conversation();
        tree.rows = 2;
        tree.selected = 0;
        press(&mut tree, KeyCode::PageDown);
        assert_eq!(tree.selected, 2);
        tree.rows = 40;
        press(&mut tree, KeyCode::PageDown);
        assert_eq!(tree.selected, 4, "clamped to the last row");
        press(&mut tree, KeyCode::PageUp);
        assert_eq!(tree.selected, 0);
    }

    #[test]
    fn escape_closes_and_ctrl_x_copies_the_whole_message() {
        let mut tree = conversation();
        tree.selected = 3;
        assert_eq!(
            tree.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            TreeOutcome::Copy("second task (full)".into())
        );
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
