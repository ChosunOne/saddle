//! The permissions editor: the grants this session is running with, as a list a person can read
//! and change (D45).
//!
//! It edits only what the user may edit. There is no row for turning on `exec`, `net` or a root:
//! those are written in the policy file, because a menu keypress that hands over a capability is
//! indistinguishable from an accidental one, and because the file is the thing a person reviews.
//! What the menu does is the routine work — an exclusion on a path, and whether the agent has a
//! scope at all where the file already grants it.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// One editable grant, as the editor sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// The rule id, as `saddle check` prints it: `agent.fs-write[0]`.
    pub rule: String,
    pub kind: Kind,
    /// The root the grant is for, for a filesystem row.
    pub root: String,
    /// Exclusions, joined by commas while being edited.
    pub exclude: String,
    /// Whether a scope row is on. Only scopes the policy grants have a row.
    pub on: bool,
    /// Whether `enter` can change this row at all.
    pub editable: bool,
    /// Why it cannot, when it cannot.
    pub locked: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    FsRead,
    FsWrite,
    Exec,
    Net,
    ModelInvoke,
    Plain,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::FsRead => "read",
            Kind::FsWrite => "write",
            Kind::Exec => "run commands",
            Kind::Net => "network",
            Kind::ModelInvoke => "spend tokens",
            Kind::Plain => "scope",
        }
    }
}

/// The forms the add prompt accepts, shown when it is open.
pub const ADD_HELP: &str = "net <host>[:port] · root <name> · read <name> · write <name> · \
                            exec · tokens <n>   (# why, optional)";

/// What the editor asks the controller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A grant typed into the add prompt (D46).
    Add(String),
    /// The exclusion list of one filesystem grant, as typed. Empty clears it.
    Exclude { rule: String, patterns: Vec<String> },
    /// Turn a scope off or on, where the form supports it. Only exclusions and scopes are
    /// editable from here.
    Toggle { rule: String, on: bool },
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Close,
    Apply(Change),
}

pub struct Permissions {
    pub rows: Vec<Row>,
    pub selected: usize,
    /// The text being typed into an exclusion row.
    pub editing: Option<String>,
    /// The text being typed into the add prompt, when it is open.
    pub adding: Option<String>,
    pub error: Option<String>,
    /// Where the file is, and where the session's grants came from.
    pub policy_path: String,
    pub global_path: String,
}

impl Permissions {
    pub fn new(rows: Vec<Row>, policy_path: String, global_path: String) -> Self {
        let selected = rows.iter().position(|r| r.editable).unwrap_or(0);
        Self {
            rows,
            selected,
            editing: None,
            adding: None,
            error: None,
            policy_path,
            global_path,
        }
    }

    pub fn current(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// What enter does to the selected row, shown under the list.
    pub fn help(&self) -> String {
        if self.adding.is_some() {
            return ADD_HELP.into();
        }
        let Some(row) = self.current() else {
            return "nothing is granted yet; press `a` to add a grant".into();
        };
        if let Some(why) = &row.locked {
            return why.clone();
        }
        match row.kind {
            Kind::FsRead | Kind::FsWrite => format!(
                "exclusions for {}: comma separated, a leading / anchors at the root's top",
                row.root
            ),
            _ => "enter turns this scope off or on for this workspace".into(),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Outcome::Stay;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(text) = &mut self.adding {
            match key.code {
                KeyCode::Esc => {
                    self.adding = None;
                    self.error = None;
                }
                KeyCode::Enter => {
                    let typed = self.adding.take().unwrap_or_default();
                    let typed = typed.trim().to_owned();
                    if typed.is_empty() {
                        return Outcome::Stay;
                    }
                    self.error = None;
                    return Outcome::Apply(Change::Add(typed));
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if !c.is_control() => text.push(c),
                _ => {}
            }
            return Outcome::Stay;
        }
        if let Some(text) = &mut self.editing {
            match key.code {
                KeyCode::Esc => {
                    self.editing = None;
                    self.error = None;
                }
                KeyCode::Enter => return self.finish_edit(),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if !c.is_control() => text.push(c),
                _ => {}
            }
            return Outcome::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Char('c') if ctrl => return Outcome::Close,
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
            // Adding is a key of its own, so a row that cannot be changed is no obstacle.
            KeyCode::Char('a') => {
                self.adding = Some(String::new());
                self.error = None;
            }
            _ => {}
        }
        Outcome::Stay
    }

    /// Moves to the next row in `step`'s direction, skipping rows that cannot be changed.
    fn step(&mut self, step: isize) {
        if self.rows.is_empty() {
            return;
        }
        let len = self.rows.len() as isize;
        let mut at = self.selected as isize;
        for _ in 0..len {
            at = (at + step).rem_euclid(len);
            if self.rows[at as usize].editable {
                self.selected = at as usize;
                return;
            }
        }
    }

    fn activate(&mut self) -> Outcome {
        self.error = None;
        let Some(row) = self.current().cloned() else {
            return Outcome::Stay;
        };
        if row.locked.is_some() {
            return Outcome::Stay;
        }
        match row.kind {
            Kind::FsRead | Kind::FsWrite => {
                self.editing = Some(row.exclude.clone());
                Outcome::Stay
            }
            _ => Outcome::Apply(Change::Toggle {
                rule: row.rule,
                on: !row.on,
            }),
        }
    }

    fn finish_edit(&mut self) -> Outcome {
        let text = self.editing.take().unwrap_or_default();
        let Some(row) = self.current().cloned() else {
            return Outcome::Stay;
        };
        let patterns: Vec<String> = text
            .split(',')
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .collect();
        // Checked here, so a bad pattern is a message rather than a policy that will not parse.
        for pattern in &patterns {
            if let Err(e) = checks(pattern) {
                self.error = Some(e);
                self.editing = Some(text);
                return Outcome::Stay;
            }
        }
        Outcome::Apply(Change::Exclude {
            rule: row.rule,
            patterns,
        })
    }
}

/// The mistakes worth catching before the file is written: a pattern glintset would reject, or a
/// path outside the root, which an exclusion cannot express.
fn checks(pattern: &str) -> Result<(), String> {
    if pattern.contains("..") {
        return Err(format!(
            "`{pattern}`: an exclusion is inside its root; `..` is not"
        ));
    }
    if pattern.starts_with('/') && pattern.trim_matches('/').is_empty() {
        return Err(format!(
            "`{pattern}`: a bare `/` would exclude the whole root"
        ));
    }
    // A `[` that does not close is the common typo in a glob.
    if pattern.matches('[').count() != pattern.matches(']').count() {
        return Err(format!("`{pattern}`: unbalanced `[` in the pattern"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<Row> {
        vec![
            Row {
                rule: "agent.fs-read[0]".into(),
                kind: Kind::FsRead,
                root: "workspace".into(),
                exclude: ".env*".into(),
                on: true,
                editable: true,
                locked: None,
            },
            Row {
                rule: "agent.exec".into(),
                kind: Kind::Exec,
                root: String::new(),
                exclude: String::new(),
                on: true,
                editable: false,
                locked: Some("exec is granted in the policy file, not from here".into()),
            },
            Row {
                rule: "agent.fs-write[0]".into(),
                kind: Kind::FsWrite,
                root: "workspace".into(),
                exclude: String::new(),
                on: true,
                editable: true,
                locked: None,
            },
        ]
    }

    fn editor() -> Permissions {
        Permissions::new(rows(), "/c/policy.toml".into(), "/c/policies/w.toml".into())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn a_row_that_cannot_change_is_skipped_and_says_why() {
        let mut p = editor();
        assert_eq!(p.current().unwrap().rule, "agent.fs-read[0]");
        // Down skips the exec row, which is the whole point of `editable`.
        p.key(key(KeyCode::Down));
        assert_eq!(p.current().unwrap().rule, "agent.fs-write[0]");
        p.key(key(KeyCode::Down));
        assert_eq!(p.current().unwrap().rule, "agent.fs-read[0]", "wraps");

        p.selected = 1;
        assert!(p.help().contains("policy file"));
        assert_eq!(p.key(key(KeyCode::Enter)), Outcome::Stay, "nothing happens");
        assert!(p.editing.is_none());
    }

    #[test]
    fn a_filesystem_row_edits_its_exclusions() {
        let mut p = editor();
        assert_eq!(p.key(key(KeyCode::Enter)), Outcome::Stay);
        assert_eq!(p.editing.as_deref(), Some(".env*"));
        p.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(p.editing.as_deref() == Some(""));
        for c in ".env*, /secrets ,".chars() {
            p.key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            p.key(key(KeyCode::Enter)),
            Outcome::Apply(Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: vec![".env*".into(), "/secrets".into()],
            }),
            "trimmed, split, empties dropped"
        );
    }

    #[test]
    fn a_bad_pattern_is_a_message_not_a_broken_policy() {
        let mut p = editor();
        p.key(key(KeyCode::Enter));
        p.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for c in "../etc".chars() {
            p.key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.key(key(KeyCode::Enter)), Outcome::Stay);
        assert!(p.error.as_deref().unwrap().contains("`..` is not"));
        assert!(p.editing.is_some(), "the text is still there to fix");

        p.editing = Some("[abc".into());
        assert_eq!(p.key(key(KeyCode::Enter)), Outcome::Stay);
        assert!(p.error.as_deref().unwrap().contains("unbalanced"));

        // Esc leaves without applying.
        assert_eq!(p.key(key(KeyCode::Esc)), Outcome::Stay);
        assert!(p.editing.is_none());
    }

    #[test]
    fn a_grant_is_added_by_typing_it() {
        let mut p = editor();
        assert_eq!(p.key(key(KeyCode::Char('a'))), Outcome::Stay);
        assert_eq!(p.adding.as_deref(), Some(""));
        // The forms are on screen while typing.
        assert!(p.help().contains("net <host>"));
        for c in "net api.example.com:8443 # reads the catalogue".chars() {
            p.key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            p.key(key(KeyCode::Enter)),
            Outcome::Apply(Change::Add(
                "net api.example.com:8443 # reads the catalogue".into()
            ))
        );
        assert!(p.adding.is_none(), "the prompt closes on enter");
    }

    #[test]
    fn an_empty_add_prompt_adds_nothing_and_esc_leaves() {
        let mut p = editor();
        p.key(key(KeyCode::Char('a')));
        assert_eq!(p.key(key(KeyCode::Enter)), Outcome::Stay);
        assert!(p.adding.is_none(), "nothing typed, nothing added");

        p.key(key(KeyCode::Char('a')));
        for c in "exec".chars() {
            p.key(key(KeyCode::Char(c)));
        }
        assert_eq!(p.key(key(KeyCode::Esc)), Outcome::Stay);
        assert!(p.adding.is_none(), "esc discards");
    }

    #[test]
    fn empty_exclusions_clear_the_row() {
        let mut p = editor();
        p.key(key(KeyCode::Enter));
        p.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(
            p.key(key(KeyCode::Enter)),
            Outcome::Apply(Change::Exclude {
                rule: "agent.fs-read[0]".into(),
                patterns: Vec::new(),
            })
        );
    }
}
