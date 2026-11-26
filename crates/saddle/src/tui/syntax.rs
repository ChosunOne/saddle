use std::collections::HashMap;
use std::sync::OnceLock;
use two_face::syntax;

use ratatui::style::{Color, Style};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use super::palette::SyntaxColours;

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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    Plain,
    Tagged(String),
}

impl Language {
    pub fn parse(tag: &str) -> Self {
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
        "markdown" | "md" => "md",
        _ => return lower,
    };
    shorter.to_owned()
}

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(syntax::extra_newlines)
}

fn syntax_for(language: &Language) -> Option<&'static SyntaxReference> {
    let Language::Tagged(tag) = language else {
        return None;
    };
    syntaxes().find_syntax_by_token(tag)
}

#[derive(Default)]
pub struct State {
    inner: Inner,
}

#[derive(Default)]
enum Inner {
    #[default]
    Unset,
    Plain,
    Parsing {
        language: Language,
        parse: Box<ParseState>,
        stack: ScopeStack,
        roles: HashMap<Scope, Kind>,
    },
}

pub fn highlight<'a>(
    language: &Language,
    line: &'a str,
    state: &mut State,
) -> Vec<(Kind, &'a str)> {
    if language == &Language::Plain {
        return plain(line);
    }
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

fn snapped(line: &str, at: usize) -> usize {
    let mut at = at.min(line.len());
    while at < line.len() && !line.is_char_boundary(at) {
        at += 1;
    }
    at
}

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

fn role(scope: &str) -> Kind {
    const RULES: &[(&str, Kind)] = &[
        ("comment", Kind::Comment),
        ("string", Kind::String),
        ("constant.character.escape", Kind::String),
        ("constant.numeric", Kind::Number),
        ("constant.language", Kind::Keyword),
        ("constant", Kind::Number),
        ("keyword.operator", Kind::Operator),
        ("keyword", Kind::Keyword),
        ("storage.type.string", Kind::String),
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
        let r = runs(Language::parse("rust"), "struct Point { x: f64 }");
        assert!(
            text_of(&r, Kind::Type).contains(&"Point".to_owned()),
            "{r:?}"
        );
    }

    #[test]
    fn the_runs_cover_the_line_in_order() {
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
        let r = runs(Language::parse("rust"), "let b = r##\"# \"quoted\" #\"##;");
        assert!(
            text_of(&r, Kind::String)
                .iter()
                .any(|s| s == "r##\"# \"quoted\" #\"##"),
            "{r:?}"
        );
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
        let runs = highlight(&Language::parse("python"), "x = 1", &mut state);
        assert!(!runs.iter().any(|(k, _)| *k == Kind::Comment), "{runs:?}");
    }

    #[test]
    fn a_role_with_no_colour_leaves_the_text_plain() {
        let syntax = SyntaxColours::default();
        assert_eq!(Kind::Plain.style(&syntax), Style::new());
    }

    #[test]
    fn a_scope_says_which_role_it_means() {
        assert_eq!(role("comment.line.double-slash.rust"), Kind::Comment);
        assert_eq!(role("string.quoted.double.rust"), Kind::String);
        assert_eq!(role("constant.numeric.integer.rust"), Kind::Number);
        assert_eq!(role("constant.language.boolean.json"), Kind::Keyword);
        assert_eq!(role("keyword.control.rust"), Kind::Keyword);
        assert_eq!(role("keyword.operator.assignment.rust"), Kind::Operator);
        assert_eq!(role("storage.type.rust"), Kind::Keyword);
        assert_eq!(role("storage.modifier.rust"), Kind::Keyword);
        assert_eq!(role("entity.name.struct.rust"), Kind::Type);
        assert_eq!(role("storage.type.string.rust"), Kind::String);
        assert_eq!(role("entity.name.function.rust"), Kind::Function);
        assert_eq!(role("variable.other.rust"), Kind::Variable);
        assert_eq!(role("punctuation.section.parens.rust"), Kind::Punctuation);
        assert_eq!(role("source.rust"), Kind::Plain);
        assert_eq!(role("meta.function-call.rust"), Kind::Plain);
    }
}
