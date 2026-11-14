//! Syntax highlighting for code blocks (D73, D80).
//!
//! The parsing is not this file's job. syntect reads the grammar for the language a fence names and
//! hands back the scope stack of each stretch of the line — `source.rust`, `keyword.control.rust`,
//! `string.quoted.double.rust` — and this module does one thing with it: maps a scope to one of the
//! nine roles below.
//!
//! That division is what keeps the theme in charge. syntect can colour text itself, and a
//! highlighter that took its colours would be a second place a colour comes from, which is exactly
//! what D71 and D72 exist to prevent. The scope is a fact about the code; the colour is the theme's
//! decision, and [`SyntaxColours`] is the only place it is made.

use std::collections::HashMap;
use std::sync::OnceLock;
use two_face::syntax;

use ratatui::style::{Color, Style};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use super::palette::SyntaxColours;

/// What a run of characters is, for colouring. The names are pi's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Comment,
    Keyword,
    Function,
    Variable,
    String,
    Number,
    Type,
    Operator,
    Punctuation,
}

impl Kind {
    pub fn style(self, syntax: &SyntaxColours) -> Style {
        let colour: Color = match self {
            Self::Plain => return Style::new(),
            Self::Comment => syntax.comment,
            Self::Keyword => syntax.keyword,
            Self::Function => syntax.function,
            Self::Variable => syntax.variable,
            Self::String => syntax.string,
            Self::Number => syntax.number,
            Self::Type => syntax.kind,
            Self::Operator => syntax.operator,
            Self::Punctuation => syntax.punctuation,
        };
        Style::new().fg(colour)
    }
}

/// The language a fence named.
///
/// It is the tag rather than an enumeration of the languages this file knows, because syntect knows
/// about a hundred of them and a list here would be a smaller list that has to be maintained. A tag
/// this build has no grammar for is [`Language::Plain`], drawn in the block's own colour.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Language {
    /// A fence that named no language, which pi draws with no highlighting.
    #[default]
    Plain,
    /// A fence that named one: the tag, as written, so a grammar syntect has is used whatever this
    /// file has heard of.
    Tagged(String),
}

impl Language {
    /// The language a fence's tag names, by the spellings that appear in practice.
    pub fn parse(tag: &str) -> Self {
        // A fence may carry more than the name (`rust,ignore`, `python title="x"`).
        let first = tag
            .trim()
            .split([',', ' ', '{', '\t'])
            .next()
            .unwrap_or_default()
            .trim_end_matches(',');
        if first.is_empty() {
            return Self::Plain;
        }
        Self::Tagged(alias(first))
    }
}

/// Fence tags that are not the extension or name syntect matches on.
///
/// A short list of spellings, not a list of languages: every entry is another way of writing
/// something syntect already has, so a tag missing from here is still looked up rather than refused.
fn alias(tag: &str) -> String {
    let lower = tag.to_ascii_lowercase();
    let shorter = match lower.as_str() {
        "console" | "shell-session" | "shell" | "terminal" => "sh",
        "python3" | "py3" => "py",
        "golang" => "go",
        "c++" | "cc" | "cxx" => "cpp",
        "jsonc" | "json5" => "json",
        "yml" => "yaml",
        "csharp" | "c#" => "cs",
        // `sh` is the extension; the name syntect matches is the longer one.
        "markdown" | "md" => "md",
        _ => return lower,
    };
    shorter.to_owned()
}

/// The grammars, loaded once: decoding the set takes tens of milliseconds and a session draws many
/// blocks.
///
/// syntect ships only Sublime's default packages, which are 75 grammars and have neither TypeScript
/// nor TOML — the format this project writes its own configuration in. `two-face` is bat's asset
/// set: the same grammars, plus everything else bat highlights, as one embedded dump.
fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(syntax::extra_newlines)
}

/// The grammar a language names, if this build has one.
fn syntax_for(language: &Language) -> Option<&'static SyntaxReference> {
    let Language::Tagged(tag) = language else {
        return None;
    };
    syntaxes().find_syntax_by_token(tag)
}

/// What the highlighter carries between lines: a block comment or a string can open on one line and
/// close on a later one, so the grammar's own state is kept for the block.
#[derive(Default)]
pub struct State {
    inner: Inner,
}

#[derive(Default)]
enum Inner {
    /// No line has been read yet, so the grammar has not been chosen.
    #[default]
    Unset,
    /// Nothing to highlight: no language, or one this build has no grammar for.
    Plain,
    Parsing {
        language: Language,
        parse: Box<ParseState>,
        stack: ScopeStack,
        /// A scope is a bit-packed pair, cheap to hash; the role it means is not, because reading
        /// it builds a string. A block asks the same few dozen questions on every line.
        roles: HashMap<Scope, Kind>,
    },
}

/// One line of code, as runs of characters with the kind they are.
///
/// The runs cover the whole line, in order, so a caller can render them without recomputing anything.
/// `line` is a line as the caller has it, without its newline: the grammar wants the newline to end
/// a line comment, so one is added and the runs are cut back to the line.
pub fn highlight<'a>(
    language: &Language,
    line: &'a str,
    state: &mut State,
) -> Vec<(Kind, &'a str)> {
    if language == &Language::Plain {
        return plain(line);
    }
    // A block has one language, so a different one means a different block.
    let unchanged =
        matches!(&state.inner, Inner::Parsing { language: open, .. } if open == language);
    if !unchanged {
        state.inner = match syntax_for(language) {
            Some(syntax) => Inner::Parsing {
                language: language.clone(),
                parse: Box::new(ParseState::new(syntax)),
                stack: ScopeStack::new(),
                roles: HashMap::new(),
            },
            // A tag this build has no grammar for is drawn plain rather than guessed at: a wrong
            // colour is worse than none, and the fence said something this cannot deliver.
            None => Inner::Plain,
        };
    }
    let Inner::Parsing {
        parse,
        stack,
        roles,
        ..
    } = &mut state.inner
    else {
        return plain(line);
    };

    // The grammars want a newline to end a line comment; the caller has no newline to give, so one
    // is added and every offset is then read back against the line itself.
    let mut owned = String::with_capacity(line.len() + 1);
    owned.push_str(line);
    owned.push('\n');
    let Ok(ops) = parse.parse_line(&owned, syntaxes()) else {
        return plain(line);
    };

    let mut spans: Vec<(Kind, usize, usize)> = Vec::new();
    let mut at = 0;
    for (offset, op) in ops {
        let end = snapped(line, offset);
        if end > at {
            add(&mut spans, role_of(stack, roles), at, end);
        }
        let _ = stack.apply(&op);
        at = at.max(end);
    }
    add(&mut spans, role_of(stack, roles), at, line.len());

    spans
        .into_iter()
        .map(|(kind, from, to)| (kind, &line[from..to]))
        .collect()
}

/// The nearest character boundary at or after `at`: a run may only be sliced where a character
/// starts, and a grammar splitting a multi-byte character would otherwise panic.
fn snapped(line: &str, at: usize) -> usize {
    let mut at = at.min(line.len());
    while at < line.len() && !line.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Adds `[from..to)` as `kind`, joining it to the run before when that run is the same kind and
/// touches it.
///
/// The joining is not cosmetic: a grammar pushes and pops a scope inside one string or comment —
/// for its delimiters, its escapes — and one run per scope would draw the same text in pieces and
/// make every caller deal with a string that is not one string.
fn add(spans: &mut Vec<(Kind, usize, usize)>, kind: Kind, from: usize, to: usize) {
    if to <= from {
        return;
    }
    if let Some((last, _, end)) = spans.last_mut()
        && *last == kind
        && *end == from
    {
        *end = to;
        return;
    }
    spans.push((kind, from, to));
}

fn plain(line: &str) -> Vec<(Kind, &str)> {
    if line.is_empty() {
        Vec::new()
    } else {
        vec![(Kind::Plain, line)]
    }
}

/// The role a scope stack means: the most specific scope that names one.
///
/// A scope that encloses a token wins over the punctuation inside it, so the `//` of a line comment
/// is comment-coloured rather than punctuation-coloured. Punctuation and operators are therefore
/// only used when nothing else on the stack names a role.
fn role_of(stack: &ScopeStack, roles: &mut HashMap<Scope, Kind>) -> Kind {
    let scopes = stack.as_slice();
    let mut weak = None;
    for scope in scopes.iter().rev() {
        let kind = match roles.get(scope) {
            Some(kind) => *kind,
            None => {
                let kind = role(scope.build_string().as_str());
                roles.insert(*scope, kind);
                kind
            }
        };
        if matches!(kind, Kind::Operator | Kind::Punctuation) {
            weak = weak.or(Some(kind));
        } else if kind != Kind::Plain {
            return kind;
        }
    }
    weak.unwrap_or(Kind::Plain)
}

/// The role one scope names, by the conventions the grammars are written to.
///
/// The scopes are Sublime Text's, which syntect's grammars all follow: `comment`, `string`,
/// `constant.numeric`, `keyword`, `storage`, `entity.name`, `support`, `variable`, `punctuation`.
fn role(scope: &str) -> Kind {
    // A scope is `a.b.c`, and every test here is on a prefix, so `keyword.operator` has to be
    // asked about before `keyword`.
    const RULES: &[(&str, Kind)] = &[
        ("comment", Kind::Comment),
        ("string", Kind::String),
        ("constant.character.escape", Kind::String),
        ("constant.numeric", Kind::Number),
        ("constant.language", Kind::Keyword),
        ("constant", Kind::Number),
        ("keyword.operator", Kind::Operator),
        ("keyword", Kind::Keyword),
        // `r#"..."` is a string whose opening `r` carries a storage scope of its own.
        ("storage.type.string", Kind::String),
        // `let`, `fn`, `struct`, `int`, `u32` and `str` all carry this in the grammars, which is
        // how a Sublime theme paints them: a keyword, not a type. A type *name* is `entity.name`.
        ("storage", Kind::Keyword),
        ("entity.name.function", Kind::Function),
        ("entity.name.lifetime", Kind::Variable),
        ("entity.name.type", Kind::Type),
        ("entity.name.class", Kind::Type),
        ("entity.name.struct", Kind::Type),
        ("entity.name.enum", Kind::Type),
        ("entity.name.trait", Kind::Type),
        ("entity.name.tag", Kind::Type),
        ("support.function", Kind::Function),
        ("support.type", Kind::Type),
        ("support.class", Kind::Type),
        ("support.constant", Kind::Number),
        ("variable.function", Kind::Function),
        ("variable.annotation", Kind::Function),
        ("variable", Kind::Variable),
        ("punctuation", Kind::Punctuation),
    ];
    for (prefix, kind) in RULES {
        if scope.starts_with(prefix) {
            return *kind;
        }
    }
    Kind::Plain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(language: Language, line: &str) -> Vec<(Kind, String)> {
        let mut state = State::default();
        highlight(&language, line, &mut state)
            .into_iter()
            .map(|(k, s)| (k, s.to_owned()))
            .collect()
    }

    fn kinds(language: Language, line: &str) -> Vec<Kind> {
        runs(language, line).into_iter().map(|(k, _)| k).collect()
    }

    fn text_of(runs: &[(Kind, String)], kind: Kind) -> Vec<String> {
        runs.iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, s)| s.clone())
            .collect()
    }

    #[test]
    fn a_run_of_code_is_coloured_the_way_it_reads() {
        let r = runs(Language::parse("rust"), "let x: u32 = foo(1); // done");
        assert!(r.contains(&(Kind::Keyword, "let".into())), "{r:?}");
        // The grammar scopes a built-in type and a keyword the same way, and a Sublime theme
        // paints both as keywords, so `u32` reads as one too.
        assert!(
            text_of(&r, Kind::Keyword).contains(&"u32".to_owned()),
            "{r:?}"
        );
        assert!(
            text_of(&r, Kind::Function).contains(&"foo".to_owned()),
            "{r:?}"
        );
        assert!(text_of(&r, Kind::Number).contains(&"1".to_owned()), "{r:?}");
        assert!(
            text_of(&r, Kind::Comment)
                .iter()
                .any(|s| s.contains("done")),
            "{r:?}"
        );
        // A type this project defined is the one that gets the type role.
        let r = runs(Language::parse("rust"), "struct Point { x: f64 }");
        assert!(
            text_of(&r, Kind::Type).contains(&"Point".to_owned()),
            "{r:?}"
        );
    }

    #[test]
    fn the_runs_cover_the_line_in_order() {
        // A caller renders them straight through, so nothing may be dropped or reordered.
        for (language, line) in [
            ("rust", "let x = foo(1, \"two\"); // done"),
            ("python", "  # just a comment"),
            ("rust", ""),
            ("rust", "   "),
            ("python", "a = 'unterminated"),
            ("json", "{\"a\": [1, 2]}"),
            ("toml", "key = \"value\""),
            ("bash", "echo $HOME | grep -v x"),
        ] {
            let runs = runs(Language::parse(language), line);
            let joined: String = runs.iter().map(|(_, s)| s.as_str()).collect();
            assert_eq!(joined, line, "for {language} {line:?}");
        }
    }

    #[test]
    fn a_lifetime_is_not_a_character_literal() {
        // The bug a hand-written scanner has and a grammar does not: `'a` is a lifetime, and only
        // a quote with a closing quote is a character literal.
        let r = runs(Language::parse("rust"), "fn f<'a>(x: &'a str) -> &'a str {");
        assert!(
            text_of(&r, Kind::String).is_empty(),
            "a lifetime was read as a string: {r:?}"
        );
        let r = runs(Language::parse("rust"), "let c = 'x';");
        assert!(
            text_of(&r, Kind::String).iter().any(|s| s == "'x'"),
            "a character literal is one: {r:?}"
        );
        let r = runs(Language::parse("rust"), "let n = '\\n';");
        assert!(
            text_of(&r, Kind::String).iter().any(|s| s == "'\\n'"),
            "an escape is one too: {r:?}"
        );
    }

    #[test]
    fn a_raw_string_is_one_string_however_many_quotes_it_holds() {
        let r = runs(Language::parse("rust"), "let s = r#\"a \"quoted\" b\"#;");
        assert!(
            text_of(&r, Kind::String)
                .iter()
                .any(|s| s == "r#\"a \"quoted\" b\"#"),
            "a raw string was cut at the first quote: {r:?}"
        );
        // Two hashes, so the string may hold one: it closes at `"##` and not before.
        let r = runs(Language::parse("rust"), "let b = r##\"# \"quoted\" #\"##;");
        assert!(
            text_of(&r, Kind::String)
                .iter()
                .any(|s| s == "r##\"# \"quoted\" #\"##"),
            "{r:?}"
        );
        // A byte string keeps its prefix in the string.
        let r = runs(Language::parse("rust"), "let c = b\"raw bytes\";");
        assert!(
            text_of(&r, Kind::String)
                .iter()
                .any(|s| s == "b\"raw bytes\""),
            "{r:?}"
        );
    }

    #[test]
    fn nested_generics_are_not_a_shift() {
        // `>>` closes two brackets, which a scanner that reads `>>` as one token gets wrong.
        let r = runs(Language::parse("rust"), "let m: HashMap<K, Vec<V>> = x;");
        assert!(
            !text_of(&r, Kind::Operator).iter().any(|s| s == ">>"),
            "{r:?}"
        );
        assert!(r.iter().any(|(k, _)| *k != Kind::Plain), "{r:?}");
    }

    #[test]
    fn a_block_comment_and_a_string_carry_over_lines() {
        let language = Language::parse("rust");
        let mut state = State::default();
        // A block comment opened on one line is a comment on the next.
        let first = highlight(&language, "/* start", &mut state);
        assert_eq!(first.first().map(|(k, _)| *k), Some(Kind::Comment));
        let second = highlight(&language, " still", &mut state);
        assert_eq!(second, [(Kind::Comment, " still")]);
        let third = highlight(&language, " end */ x", &mut state);
        assert!(
            third
                .iter()
                .any(|(k, s)| *k == Kind::Comment && s.contains("end */")),
            "the comment closed: {third:?}"
        );
        // What follows the close is no longer part of the comment.
        assert!(
            third
                .iter()
                .any(|(k, s)| *k == Kind::Plain && s.trim() == "x"),
            "the rest of the line is not swallowed by the comment: {third:?}"
        );
        assert!(
            highlight(&language, "let y = 1;", &mut state)
                .iter()
                .any(|(k, s)| *k == Kind::Keyword && *s == "let"),
            "the comment did not reopen"
        );
    }

    #[test]
    fn a_python_triple_quoted_string_carries_over_lines() {
        let language = Language::parse("python");
        let mut state = State::default();
        let first = highlight(&language, "s = '''a", &mut state);
        assert!(first.iter().any(|(k, _)| *k == Kind::String), "{first:?}");
        let second = highlight(&language, "b''' + c", &mut state);
        assert!(
            second
                .iter()
                .any(|(k, s)| *k == Kind::String && s.contains("b'''")),
            "the string closed: {second:?}"
        );
        // `c` after the close is code again, not swallowed by the string.
        assert!(
            second.iter().any(|(k, s)| *k == Kind::Plain && *s == " c"),
            "{second:?}"
        );
    }

    #[test]
    fn a_hash_is_a_comment_in_shell_but_not_inside_a_string() {
        let r = runs(Language::parse("toml"), "key = \"#not a comment\"");
        assert!(text_of(&r, Kind::Comment).is_empty(), "{r:?}");
        let r = runs(Language::parse("bash"), "echo hi  # a note");
        assert!(
            text_of(&r, Kind::Comment)
                .iter()
                .any(|s| s.contains("a note")),
            "{r:?}"
        );
        let r = runs(Language::parse("python"), "x = 1  # note");
        assert!(!text_of(&r, Kind::Comment).is_empty(), "{r:?}");
    }

    #[test]
    fn escapes_do_not_end_a_string_early() {
        let r = runs(Language::parse("rust"), r#"let s = "a\"b";"#);
        assert!(
            text_of(&r, Kind::String).iter().any(|s| s == r#""a\"b""#),
            "{r:?}"
        );
        // The semicolon after it is punctuation, not swallowed by the string.
        assert!(
            text_of(&r, Kind::Punctuation).iter().any(|s| s == ";"),
            "{r:?}"
        );
    }

    #[test]
    fn each_language_is_read_with_its_own_grammar() {
        for (language, line) in [
            ("python", "def f():"),
            ("go", "func main()"),
            ("c", "int main(void)"),
            ("javascript", "if (x) {}"),
            ("java", "public class A {}"),
            ("ruby", "def f; end"),
            ("sql", "SELECT 1 FROM t"),
        ] {
            assert!(
                !kinds(Language::parse(language), line).is_empty(),
                "{language} produced no runs"
            );
            assert!(
                kinds(Language::parse(language), line).contains(&Kind::Keyword),
                "{language} did not find a keyword in {line:?}"
            );
        }
        // A type named in a Rust signature gets the type role.
        let r = runs(Language::parse("rust"), "let m: HashMap<String, u8> = x;");
        assert!(
            text_of(&r, Kind::Type).iter().any(|s| s == "String"),
            "{r:?}"
        );
    }

    #[test]
    fn every_tag_this_project_writes_finds_a_grammar() {
        for tag in [
            "rust",
            "rs",
            "python",
            "py",
            "js",
            "javascript",
            "ts",
            "typescript",
            "tsx",
            "sh",
            "bash",
            "zsh",
            "console",
            "json",
            "toml",
            "yaml",
            "yml",
            "c",
            "h",
            "cpp",
            "go",
            "java",
            "ruby",
            "html",
            "css",
            "sql",
            "markdown",
            "diff",
            "dockerfile",
        ] {
            let language = Language::parse(tag);
            assert!(
                syntax_for(&language).is_some(),
                "`{tag}` has no grammar: {language:?}"
            );
        }
    }

    #[test]
    fn a_fence_tag_is_read_by_its_first_word() {
        assert_eq!(
            Language::parse(""),
            Language::Plain,
            "no tag, no highlighting"
        );
        assert_eq!(Language::parse("  "), Language::Plain);
        assert_eq!(
            Language::parse("rust,ignore"),
            Language::Tagged("rust".into())
        );
        assert_eq!(Language::parse("TS"), Language::Tagged("ts".into()));
        assert_eq!(Language::parse("bash"), Language::Tagged("bash".into()));
        assert_eq!(Language::parse("console"), Language::Tagged("sh".into()));
        assert_eq!(Language::parse("python3"), Language::Tagged("py".into()));
        // A tag this build has no grammar for names nothing to highlight, which is drawn plain.
        let r = runs(Language::parse("brainfuck"), "+++[->++<]");
        assert_eq!(r, [(Kind::Plain, "+++[->++<]".to_owned())]);
    }

    #[test]
    fn an_untagged_block_is_drawn_plain() {
        let r = runs(Language::Plain, "let x = 1; // looks like rust");
        assert_eq!(
            r,
            [(Kind::Plain, "let x = 1; // looks like rust".to_owned())]
        );
        assert!(runs(Language::Plain, "").is_empty());
    }

    #[test]
    fn a_block_that_changes_language_starts_again() {
        let mut state = State::default();
        highlight(&Language::parse("rust"), "/* open", &mut state);
        // A block that names another language must not inherit the open comment.
        let runs = highlight(&Language::parse("python"), "x = 1", &mut state);
        assert!(!runs.iter().any(|(k, _)| *k == Kind::Comment), "{runs:?}");
    }

    #[test]
    fn a_role_with_no_colour_leaves_the_text_plain() {
        let syntax = SyntaxColours::default();
        // `Plain` is the unstyled one, which is what an all-default palette relies on.
        assert_eq!(Kind::Plain.style(&syntax), Style::new());
    }

    #[test]
    fn a_scope_says_which_role_it_means() {
        assert_eq!(role("comment.line.double-slash.rust"), Kind::Comment);
        assert_eq!(role("string.quoted.double.rust"), Kind::String);
        assert_eq!(role("constant.numeric.integer.rust"), Kind::Number);
        assert_eq!(role("constant.language.boolean.json"), Kind::Keyword);
        assert_eq!(role("keyword.control.rust"), Kind::Keyword);
        // The one that has to be asked first: an operator is not a keyword.
        assert_eq!(role("keyword.operator.assignment.rust"), Kind::Operator);
        // `let`, `fn` and a built-in type all carry a storage scope, which a theme paints as a
        // keyword; a type this project defined is an entity name.
        assert_eq!(role("storage.type.rust"), Kind::Keyword);
        assert_eq!(role("storage.modifier.rust"), Kind::Keyword);
        assert_eq!(role("entity.name.struct.rust"), Kind::Type);
        // The prefix of a raw string is a string, not a keyword.
        assert_eq!(role("storage.type.string.rust"), Kind::String);
        assert_eq!(role("entity.name.function.rust"), Kind::Function);
        assert_eq!(role("variable.other.rust"), Kind::Variable);
        assert_eq!(role("punctuation.section.parens.rust"), Kind::Punctuation);
        // A scope that means nothing to colour.
        assert_eq!(role("source.rust"), Kind::Plain);
        assert_eq!(role("meta.function-call.rust"), Kind::Plain);
    }
}
