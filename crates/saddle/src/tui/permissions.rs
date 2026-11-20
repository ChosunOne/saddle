//! The permissions editor: the grants this session is running with, as a list a person can read
//! and change (D45).
//!
//! It edits only what the user may edit. There is no row for turning on `exec`, `net` or a root:
//! those are written in the policy file, because a menu keypress that hands over a capability is
//! indistinguishable from an accidental one, and because the file is the thing a person reviews.
//! What the menu does is the routine work — an exclusion on a path, and whether a grant has a
//! scope at all where the file grants it — plus a typed line that adds one grant (D46), which the
//! prompt checks as it is written so a mistake is a message rather than a failed write (D173).

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use saddle_cap::Roots;

use super::grant;

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

/// Where the grants come from, and what the files say now: the add prompt checks a line against
/// this project's file as it reads, and against the base it is laid over (D173).
pub struct Source {
    /// This project's grants file, home-relative, named at the foot of the dialog.
    pub policy_path: String,
    /// The base grants in `config.toml`, named beside it.
    pub global_path: String,
    /// This project's file as it reads now.
    pub text: String,
    /// `[agent]` in `config.toml`, so the prompt can say when the project's own list replaces
    /// something the base grants.
    pub base: String,
}

/// What pressing enter would write, kept up to date as the add prompt is typed (D173).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Draft {
    /// Nothing typed yet.
    Empty,
    /// The line parses: this is the grant, and the entry it becomes.
    Ready(grant::Preview),
    /// The line does not parse, and this is what to change.
    Invalid(String),
}

/// The forms the add prompt accepts, shown while it is open.
pub const ADD_HELP: [&str; 2] = [
    "net <host>[:port] or https://host/path · read|write|root <name> · exec · tokens <n>",
    "tab completes a form and a root name · `# why` explains the grant · enter writes it",
];

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
    /// What `enter` would make of that text (D173).
    pub draft: Draft,
    pub error: Option<String>,
    pub source: Source,
    roots: Roots,
}

impl Permissions {
    pub fn new(rows: Vec<Row>, source: Source, roots: Roots) -> Self {
        let selected = rows.iter().position(|r| r.editable).unwrap_or(0);
        Self {
            rows,
            selected,
            editing: None,
            adding: None,
            draft: Draft::Empty,
            error: None,
            source,
            roots,
        }
    }

    pub fn current(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Opens the add prompt, empty (D46).
    pub fn begin_add(&mut self) {
        self.adding = Some(String::new());
        self.draft = Draft::Empty;
        self.error = None;
    }

    /// Puts a line back in the prompt after the write refused it, so a typo is fixed rather than
    /// retyped (D173).
    pub fn retry(&mut self, typed: String) {
        self.adding = Some(typed);
        self.refresh();
    }

    /// Rows and file text after a change has landed, keeping the place and the line being typed.
    pub fn reload(&mut self, rows: Vec<Row>, text: String) {
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.source.text = text;
        self.refresh();
    }

    /// Re-checks the typed line against the file as it now reads (D173).
    fn refresh(&mut self) {
        self.draft = match self.adding.as_deref() {
            None => Draft::Empty,
            Some(text) if text.trim().is_empty() => Draft::Empty,
            Some(text) => {
                match grant::draft(&self.source.text, &self.source.base, text, &self.roots) {
                    Ok(preview) => Draft::Ready(preview),
                    Err(e) => Draft::Invalid(format!("{e:#}")),
                }
            }
        };
    }

    /// What enter does to the selected row, shown under the list.
    pub fn help(&self) -> String {
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

    /// What else the prompt can say: the roots a `root`, `read` or `write` line may name (D173).
    pub fn hint(&self) -> Option<String> {
        let text = self.adding.as_deref()?;
        let form = text.split_whitespace().next()?;
        if !matches!(
            form.to_ascii_lowercase().as_str(),
            "root" | "read" | "write"
        ) {
            return None;
        }
        let names: Vec<&str> = self.roots.iter().map(|(name, _)| name.as_str()).collect();
        if names.is_empty() {
            return Some("this session has no roots; [roots] in config.toml adds one".into());
        }
        Some(format!("roots: {}", names.join(", ")))
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Outcome::Stay;
        }
        if self.adding.is_some() {
            return self.adding_key(key);
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
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => text.clear(),
                KeyCode::Char(c) if !c.is_control() => text.push(c),
                _ => {}
            }
            return Outcome::Stay;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Outcome::Close;
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Enter | KeyCode::Char(' ') => return self.activate(),
            // Adding is a key of its own, so a row that cannot be changed is no obstacle.
            KeyCode::Char('a') => self.begin_add(),
            _ => {}
        }
        Outcome::Stay
    }

    /// Every key the add prompt takes (D46, D173).
    fn adding_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.adding = None;
                self.draft = Draft::Empty;
                self.error = None;
            }
            KeyCode::Tab => self.complete(),
            KeyCode::Enter => {
                let typed = self.adding.take().unwrap_or_default().trim().to_owned();
                self.draft = Draft::Empty;
                if typed.is_empty() {
                    return Outcome::Stay;
                }
                self.error = None;
                return Outcome::Apply(Change::Add(typed));
            }
            KeyCode::Backspace => self.edit(|text| {
                text.pop();
            }),
            KeyCode::Char('u') if ctrl => self.edit(String::clear),
            KeyCode::Char(c) if !c.is_control() => self.edit(|text| text.push(c)),
            _ => {}
        }
        Outcome::Stay
    }

    /// Applies an edit to the prompt's line and checks it again, so what enter would write is on
    /// screen before it is typed (D173).
    fn edit(&mut self, change: impl FnOnce(&mut String)) {
        if let Some(text) = &mut self.adding {
            change(text);
        }
        self.refresh();
        self.error = None;
    }

    /// Tab: the form whose name starts with what is typed, then the session's root names once the
    /// form takes one (D173). Completing a line grants nothing — it is still a line that has to be
    /// finished and entered.
    fn complete(&mut self) {
        let Some(text) = self.adding.clone() else {
            return;
        };
        let typed = text.trim_end();
        let completed = match typed.split_once(char::is_whitespace) {
            // Still typing the form: the first one whose name starts with it.
            None => complete_form(typed).map(|form| format!("{form} ")),
            Some((form, rest)) => match form.to_ascii_lowercase().as_str() {
                "root" | "read" | "write" => complete_root(rest.trim_start(), &self.roots)
                    .map(|name| format!("{form} {name}")),
                _ => None,
            },
        };
        if let Some(completed) = completed {
            self.adding = Some(completed);
            self.refresh();
        }
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

/// The forms the add prompt knows, in the order tab walks them.
const FORMS: [&str; 6] = ["net", "read", "write", "root", "exec", "tokens"];

/// The form whose name starts with `word`: nothing typed gives the first, so tab walks the list.
fn complete_form(word: &str) -> Option<&'static str> {
    let prefix = word.to_ascii_lowercase();
    FORMS.into_iter().find(|form| form.starts_with(&prefix))
}

/// The next root name after what is typed, so tab both completes and walks the session's roots.
fn complete_root<'a>(typed: &str, roots: &'a Roots) -> Option<&'a str> {
    let names: Vec<&'a str> = roots.iter().map(|(name, _)| name.as_str()).collect();
    let prefix = typed.to_ascii_lowercase();
    let next = match names.iter().position(|name| *name == prefix) {
        Some(at) => names.get((at + 1) % names.len()),
        None => names
            .iter()
            .find(|name| name.starts_with(&prefix))
            .or_else(|| names.first()),
    };
    next.copied()
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
    use saddle_cap::RootName;

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

    /// A session with the two built-in roots and one of its own, so tab has something to walk.
    fn roots() -> Roots {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        Roots::resolve(
            [
                (RootName::workspace(), path.clone()),
                (RootName::tmp(), path.clone()),
                (RootName::new("scratch").unwrap(), path),
            ],
            &[],
        )
        .unwrap()
    }

    fn source(text: &str, base: &str) -> Source {
        Source {
            policy_path: "/c/policies/w.toml".into(),
            global_path: "/c/config.toml".into(),
            text: text.into(),
            base: base.into(),
        }
    }

    fn editor() -> Permissions {
        Permissions::new(rows(), source("version = 1\n", ""), roots())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_line(dialog: &mut Permissions, line: &str) {
        for c in line.chars() {
            dialog.key(key(KeyCode::Char(c)));
        }
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
        assert!(ADD_HELP[0].contains("net <host>"));
        type_line(&mut p, "net api.example.com:8443 # reads the catalogue");
        assert_eq!(
            p.key(key(KeyCode::Enter)),
            Outcome::Apply(Change::Add(
                "net api.example.com:8443 # reads the catalogue".into()
            ))
        );
        assert!(p.adding.is_none(), "the prompt closes on enter");
    }

    #[test]
    fn the_prompt_says_what_enter_would_write_before_it_is_typed() {
        let mut p = editor();
        p.begin_add();
        // Nothing typed is nothing to say.
        assert_eq!(p.draft, Draft::Empty);

        type_line(&mut p, "net https://api.example.com/v2");
        let Draft::Ready(preview) = &p.draft else {
            panic!("{:?}", p.draft);
        };
        assert_eq!(preview.summary, "net grant for https://api.example.com:443");
        assert!(
            preview.line.contains(r#"host = "api.example.com""#),
            "{:?}",
            preview.line
        );

        // A line that cannot be a grant is a reason while it is still being typed.
        p.adding = Some("net".into());
        p.key(key(KeyCode::Char(' ')));
        assert!(
            matches!(&p.draft, Draft::Invalid(e) if e.contains("needs a host")),
            "{:?}",
            p.draft
        );
    }

    #[test]
    fn a_refused_line_goes_back_into_the_prompt_to_be_fixed() {
        let mut p = editor();
        p.begin_add();
        p.retry("net api.example.com:4443".into());
        assert_eq!(p.adding.as_deref(), Some("net api.example.com:4443"));
        assert!(matches!(p.draft, Draft::Ready(_)), "{:?}", p.draft);
    }

    #[test]
    fn tab_completes_a_form_and_then_a_root_name() {
        let mut p = editor();
        p.begin_add();
        // Nothing typed walks the forms from the first.
        p.key(key(KeyCode::Tab));
        assert_eq!(p.adding.as_deref(), Some("net "));
        // A prefix picks the form it starts.
        p.adding = Some("re".into());
        p.key(key(KeyCode::Tab));
        assert_eq!(p.adding.as_deref(), Some("read "));
        // Then the session's own root names, which are the ones a grant may name (D30).
        p.adding = Some("read work".into());
        p.key(key(KeyCode::Tab));
        assert_eq!(p.adding.as_deref(), Some("read workspace"));
        // Tab again walks to the next root rather than sitting on this one.
        p.key(key(KeyCode::Tab));
        assert_eq!(p.adding.as_deref(), Some("read scratch"));
        // A form that takes no root has nothing to complete.
        p.adding = Some("net api".into());
        p.key(key(KeyCode::Tab));
        assert_eq!(p.adding.as_deref(), Some("net api"));
    }

    #[test]
    fn the_prompt_lists_the_roots_a_row_may_name() {
        let mut p = editor();
        p.begin_add();
        assert!(p.hint().is_none(), "nothing typed says nothing");
        type_line(&mut p, "read ");
        let hint = p.hint().unwrap();
        assert!(hint.contains("workspace"), "{hint}");
        assert!(hint.contains("scratch"), "{hint}");

        p.adding = Some("net api.example.com".into());
        assert!(p.hint().is_none(), "a host is not a root");
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

    #[test]
    fn a_reload_keeps_the_place_and_rechecks_the_typed_line() {
        let mut p = editor();
        p.selected = 2;
        p.begin_add();
        type_line(&mut p, "net api.example.com");
        // The file now grants the same host, and the project's rows are rebuilt around it.
        let text = "version = 1\n[agent]\nnet = [{ host = \"api.example.com\", scheme = \"https\", port = 443, why = \"x\" }]\n";
        p.reload(rows(), text.into());
        assert_eq!(p.selected, 2, "the place is kept");
        assert!(matches!(p.draft, Draft::Ready(_)), "{:?}", p.draft);
        // The line is checked against the file the reload brought in, not the one the prompt was
        // opened with.
        assert!(p.source.text.contains("api.example.com"));
    }

    #[test]
    fn a_source_says_where_the_grants_come_from_and_what_the_base_grants() {
        let p = Permissions::new(
            rows(),
            source("version = 1\n", "[agent]\nexec = {}\n"),
            Roots::default(),
        );
        assert_eq!(p.source.policy_path, "/c/policies/w.toml");
        assert_eq!(p.source.global_path, "/c/config.toml");
        assert!(p.source.base.contains("exec"));
    }
}
