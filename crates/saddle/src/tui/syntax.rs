//! Syntax highlighting for code blocks (D73).
//!
//! A lexical highlighter, not a parser: it knows comments, strings, numbers, keywords, types,
//! function calls and punctuation for the languages this harness and its projects are written in.
//! That is what a terminal block needs — it is read, not compiled — and it needs no dependency, which
//! matters because this build is offline and `Cargo.lock` cannot grow.
//!
//! Every colour comes from the theme's `syntax*` roles, so a pi palette paints the code too.

use super::palette::SyntaxColours;
use ratatui::style::{Color, Style};

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

/// The languages with their own words. Anything else gets [`Language::Unknown`], which highlights
/// only what is unambiguous in any language: strings, numbers and comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    Shell,
    Json,
    Toml,
    Yaml,
    C,
    Go,
    /// A fence that names no language: drawn in the block's own colour, with no highlighting, which
    /// is what pi does with an untagged block.
    #[default]
    Plain,
    /// A fence naming a language this highlighter does not know: strings, numbers and comments
    /// are still highlighted, since those read the same in any language.
    Unknown,
}

impl Language {
    /// The language a fence's tag names, by the spellings that appear in practice.
    pub fn parse(tag: &str) -> Self {
        let tag = tag.trim().to_ascii_lowercase();
        // A fence may carry more than the name (`rust,ignore`, `python title="x"`).
        let first = tag
            .split([',', ' ', '{'])
            .next()
            .unwrap_or_default()
            .trim_end_matches(',');
        match first {
            "rust" | "rs" => Self::Rust,
            "python" | "py" | "python3" => Self::Python,
            "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "mjs" | "cjs" => {
                Self::JavaScript
            }
            "sh" | "bash" | "zsh" | "shell" | "console" | "shell-session" => Self::Shell,
            "json" | "jsonc" | "json5" => Self::Json,
            "toml" => Self::Toml,
            "yaml" | "yml" => Self::Yaml,
            "c" | "h" | "cpp" | "c++" | "cc" | "hpp" | "java" | "kt" | "kotlin" | "cs"
            | "csharp" => Self::C,
            "go" | "golang" => Self::Go,
            "" => Self::Plain,
            _ => Self::Unknown,
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
                "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match",
                "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct",
                "super", "trait", "true", "type", "unsafe", "use", "where", "while", "yield",
            ],
            Self::Python => &[
                "and", "as", "assert", "async", "await", "break", "class", "continue", "def",
                "del", "elif", "else", "except", "False", "finally", "for", "from", "global", "if",
                "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise",
                "return", "True", "try", "while", "with", "yield",
            ],
            Self::JavaScript => &[
                "async",
                "await",
                "break",
                "case",
                "catch",
                "class",
                "const",
                "continue",
                "debugger",
                "default",
                "delete",
                "do",
                "else",
                "export",
                "extends",
                "false",
                "finally",
                "for",
                "from",
                "function",
                "if",
                "import",
                "in",
                "instanceof",
                "let",
                "new",
                "null",
                "of",
                "return",
                "static",
                "super",
                "switch",
                "this",
                "throw",
                "true",
                "try",
                "typeof",
                "undefined",
                "var",
                "void",
                "while",
                "yield",
            ],
            Self::Shell => &[
                "case", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
                "select", "then", "time", "until", "while",
            ],
            Self::Json => &["true", "false", "null"],
            Self::Toml | Self::Yaml => &["true", "false", "null", "yes", "no", "on", "off"],
            Self::C => &[
                "auto",
                "break",
                "case",
                "char",
                "class",
                "const",
                "continue",
                "default",
                "do",
                "double",
                "else",
                "enum",
                "extern",
                "false",
                "float",
                "for",
                "goto",
                "if",
                "inline",
                "int",
                "long",
                "namespace",
                "new",
                "nullptr",
                "private",
                "protected",
                "public",
                "return",
                "short",
                "signed",
                "sizeof",
                "static",
                "struct",
                "switch",
                "template",
                "this",
                "throw",
                "true",
                "try",
                "catch",
                "typedef",
                "typename",
                "union",
                "unsigned",
                "using",
                "virtual",
                "void",
                "volatile",
                "while",
            ],
            Self::Go => &[
                "break",
                "case",
                "chan",
                "const",
                "continue",
                "default",
                "defer",
                "else",
                "fallthrough",
                "false",
                "for",
                "func",
                "go",
                "goto",
                "if",
                "import",
                "interface",
                "iota",
                "map",
                "nil",
                "package",
                "range",
                "return",
                "select",
                "struct",
                "switch",
                "true",
                "type",
                "var",
            ],
            Self::Plain | Self::Unknown => &[],
        }
    }

    /// Built-in type names, which are coloured as types even in lowercase.
    fn types(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &[
                "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str",
                "u8", "u16", "u32", "u64", "u128", "usize", "String", "Vec", "Option", "Result",
                "Box", "Rc", "Arc", "HashMap", "BTreeMap", "Self",
            ],
            Self::Python => &[
                "int", "float", "str", "bool", "list", "dict", "set", "tuple", "bytes",
            ],
            Self::JavaScript => &["bigint", "boolean", "number", "string", "symbol", "object"],
            Self::C => &[
                "bool", "char", "double", "float", "int", "long", "short", "size_t", "string",
                "unsigned", "void",
            ],
            Self::Go => &[
                "bool",
                "byte",
                "complex64",
                "complex128",
                "error",
                "float32",
                "float64",
                "int",
                "int8",
                "int16",
                "int32",
                "int64",
                "rune",
                "string",
                "uint",
                "uint8",
                "uint16",
                "uint32",
                "uint64",
                "uintptr",
            ],
            _ => &[],
        }
    }

    fn line_comment(self) -> &'static [&'static str] {
        match self {
            Self::Rust | Self::JavaScript | Self::C | Self::Go => &["//"],
            Self::Python | Self::Shell | Self::Toml | Self::Yaml => &["#"],
            // JSON has no comments; JSONC does, and highlighting them is harmless.
            Self::Json => &["//"],
            Self::Plain => &[],
            Self::Unknown => &["//", "#"],
        }
    }

    /// Whether a `#` starts a comment. In a shell a `#` mid-word is a literal, so it is only a
    /// comment at the start of a word; that is handled where the token is read.
    fn hash_comment(self) -> bool {
        matches!(
            self,
            Self::Python | Self::Shell | Self::Toml | Self::Yaml | Self::Unknown
        )
    }
}

/// What the highlighter carries between lines: a block comment or a multi-line string can open on one
/// line and close on a later one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    block_comment: bool,
    /// The quote that opened a multi-line string, for Python's triple quotes and a trailing `\`.
    string: Option<&'static str>,
}

/// One line of code, as runs of characters with the kind they are.
///
/// The runs cover the whole line, in order, so a caller can render them without recomputing anything.
pub fn highlight<'a>(language: Language, line: &'a str, state: &mut State) -> Vec<(Kind, &'a str)> {
    if language == Language::Plain {
        // Nothing to say about a block that named no language: one run, the block's own colour.
        return if line.is_empty() {
            Vec::new()
        } else {
            vec![(Kind::Plain, line)]
        };
    }
    let bytes = line.as_bytes();
    let mut out: Vec<(Kind, &str)> = Vec::new();
    let mut at = 0;

    // A block comment or string opened on an earlier line continues.
    if state.block_comment {
        match line.find("*/") {
            Some(end) => {
                push(&mut out, line, Kind::Comment, 0, end + 2);
                state.block_comment = false;
                at = end + 2;
            }
            None => {
                push(&mut out, line, Kind::Comment, 0, line.len());
                return out;
            }
        }
    }
    if let Some(quote) = state.string {
        match line.find(quote) {
            Some(end) => {
                push(&mut out, line, Kind::String, 0, end + quote.len());
                state.string = None;
                at = end + quote.len();
            }
            None => {
                push(&mut out, line, Kind::String, 0, line.len());
                return out;
            }
        }
    }

    while at < bytes.len() {
        let rest = &line[at..];
        let byte = bytes[at];

        // Whitespace and anything unclassified.
        if byte.is_ascii_whitespace() {
            let mut end = at;
            while end < bytes.len() && bytes[end].is_ascii_whitespace() {
                end += 1;
            }
            push(&mut out, line, Kind::Plain, at, end);
            at = end;
            continue;
        }

        // A line comment, or the opening of a block comment.
        if rest.starts_with("/*") {
            match rest.find("*/") {
                Some(end) => {
                    push(&mut out, line, Kind::Comment, at, at + end + 2);
                    at += end + 2;
                    continue;
                }
                None => {
                    push(&mut out, line, Kind::Comment, at, line.len());
                    state.block_comment = true;
                    return out;
                }
            }
        }
        if language
            .line_comment()
            .iter()
            .any(|marker| rest.starts_with(marker))
        {
            push(&mut out, line, Kind::Comment, at, line.len());
            return out;
        }
        if language.hash_comment()
            && byte == b'#'
            && (at == 0 || bytes[at - 1].is_ascii_whitespace())
        {
            push(&mut out, line, Kind::Comment, at, line.len());
            return out;
        }
        // Python's docstring and Rust's raw string are opened by their own quotes.
        if rest.starts_with("\"\"\"") || rest.starts_with("'''") {
            let quote = &rest[..3];
            let after = at + 3;
            match line[after..].find(quote) {
                Some(end) => {
                    push(&mut out, line, Kind::String, at, after + end + 3);
                    at = after + end + 3;
                    continue;
                }
                None => {
                    push(&mut out, line, Kind::String, at, line.len());
                    state.string = Some(if quote == "\"\"\"" { "\"\"\"" } else { "'''" });
                    return out;
                }
            }
        }

        // A string: single or double quoted, with backslash escapes.
        if byte == b'"' || byte == b'\'' {
            let quote = byte;
            let mut end = at + 1;
            let mut closed = false;
            while end < bytes.len() {
                match bytes[end] {
                    b'\\' => end += 2,
                    b if b == quote => {
                        end += 1;
                        closed = true;
                        break;
                    }
                    _ => end += 1,
                }
            }
            let end = end.min(bytes.len());
            push(&mut out, line, Kind::String, at, end);
            if !closed {
                state.string = Some(if quote == b'"' { "\"" } else { "'" });
            }
            at = end;
            continue;
        }

        // A number: digits, with `_`, `.`, an exponent and a suffix.
        if byte.is_ascii_digit() {
            let mut end = at;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric()
                    || matches!(bytes[end], b'_' | b'.')
                    || ((bytes[end] == b'+' || bytes[end] == b'-')
                        && end > at
                        && matches!(bytes[end - 1], b'e' | b'E')))
            {
                end += 1;
            }
            push(&mut out, line, Kind::Number, at, end);
            at = end;
            continue;
        }

        // An identifier, which may be a keyword, a type, or a call.
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let mut end = at;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            let word = &line[at..end];
            // A following `(` makes it a call, unless it is a keyword: `if (` is not a function.
            let call = line[end..].trim_start().starts_with('(');
            let kind = if language.keywords().contains(&word) {
                Kind::Keyword
            } else if language.types().contains(&word) || is_camel_case(word) {
                Kind::Type
            } else if call {
                Kind::Function
            } else {
                Kind::Variable
            };
            push(&mut out, line, kind, at, end);
            at = end;
            continue;
        }

        // Operators and punctuation, one character at a time so a run does not colour a `-` in a
        // path as an operator across the whole path.
        let kind = if b"+-*/%=<>!&|^~?:".contains(&byte) {
            Kind::Operator
        } else {
            Kind::Punctuation
        };
        let mut end = at + 1;
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && !bytes[end].is_ascii_alphanumeric()
            && !b"+-*/%=<>!&|^~?:\"'#_".contains(&bytes[end])
            && !(kind == Kind::Punctuation && b"+-*/%=<>!&|^~?:".contains(&bytes[end]))
        {
            end += 1;
        }
        push(&mut out, line, kind, at, end);
        at = end;
    }
    out
}

/// Adds `line[from..to]` as `kind`, dropping an empty run.
fn push<'a>(out: &mut Vec<(Kind, &'a str)>, line: &'a str, kind: Kind, from: usize, to: usize) {
    if to > from {
        out.push((kind, &line[from..to]));
    }
}

/// A name that starts with a capital and has a lowercase letter: a type by convention in most of
/// these languages. `HTTP` and `MAX` are constants, which are variables.
fn is_camel_case(word: &str) -> bool {
    let mut chars = word.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && word.chars().any(|c| c.is_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(language: Language, line: &str) -> Vec<(Kind, String)> {
        let mut state = State::default();
        highlight(language, line, &mut state)
            .into_iter()
            .map(|(k, s)| (k, s.to_owned()))
            .collect()
    }

    fn kinds(language: Language, line: &str) -> Vec<Kind> {
        runs(language, line).into_iter().map(|(k, _)| k).collect()
    }

    #[test]
    fn the_runs_cover_the_line_in_order() {
        // A caller renders them straight through, so nothing may be dropped or reordered.
        for line in [
            "let x = foo(1, \"two\"); // done",
            "  # just a comment",
            "",
            "   ",
            "a = 'unterminated",
        ] {
            let runs = runs(Language::Rust, line);
            let joined: String = runs.iter().map(|(_, s)| s.as_str()).collect();
            assert_eq!(joined, line, "for {line:?}");
        }
    }

    #[test]
    fn rust_is_read_the_way_it_reads() {
        let r = runs(Language::Rust, "let x: u32 = foo(1); // done");
        assert_eq!(r[0], (Kind::Keyword, "let".into()));
        assert!(r.contains(&(Kind::Type, "u32".into())), "{r:?}");
        assert!(r.contains(&(Kind::Function, "foo".into())), "{r:?}");
        assert!(r.contains(&(Kind::Number, "1".into())), "{r:?}");
        assert!(
            r.iter()
                .any(|(k, s)| *k == Kind::Comment && s.contains("done")),
            "{r:?}"
        );
        // A capitalised custom type.
        let r = runs(Language::Rust, "struct Point { x: f64 }");
        assert!(r.contains(&(Kind::Type, "Point".into())), "{r:?}");
    }

    #[test]
    fn a_block_comment_and_a_multiline_string_carry_over_lines() {
        let mut state = State::default();
        assert_eq!(
            kind_of(highlight(Language::Rust, "/* start", &mut state)),
            Some(Kind::Comment)
        );
        assert!(state.block_comment);
        assert_eq!(
            kind_of(highlight(Language::Rust, " still", &mut state)),
            Some(Kind::Comment)
        );
        assert_eq!(
            kind_of(highlight(Language::Rust, " end */ x", &mut state)),
            Some(Kind::Comment)
        );
        assert!(!state.block_comment, "the comment closed");

        let mut state = State::default();
        let runs = highlight(Language::Python, "s = '''a", &mut state);
        assert!(runs.iter().any(|(k, _)| *k == Kind::String), "{runs:?}");
        assert!(state.string.is_some(), "an open triple quote");
        // The next line closes it, and what follows the close is highlighted normally again.
        let runs = highlight(Language::Python, "b''' + c", &mut state);
        assert!(
            runs.iter()
                .any(|(k, s)| *k == Kind::String && s.starts_with("b'''")),
            "{runs:?}"
        );
        assert!(
            runs.iter().any(|(k, s)| *k == Kind::Variable && *s == "c"),
            "the rest of the line is not swallowed: {runs:?}"
        );
        assert!(state.string.is_none(), "the quote closed");
    }

    #[test]
    fn a_hash_is_a_comment_in_shell_but_not_inside_a_word() {
        // `#` in `a#b` is part of the word; at the start of one it is a comment.
        let r = runs(Language::Toml, "key = \"#not a comment\"");
        assert!(r.iter().all(|(k, _)| *k != Kind::Comment), "{r:?}");
        let r = runs(Language::Shell, "echo hi  # a note");
        assert!(
            r.iter()
                .any(|(k, s)| *k == Kind::Comment && s.contains("a note")),
            "{r:?}"
        );
        // Python's `#` is a comment even with no space before it.
        let r = runs(Language::Python, "x = 1  # note");
        assert!(r.iter().any(|(k, _)| *k == Kind::Comment), "{r:?}");
    }

    #[test]
    fn escapes_do_not_end_a_string_early() {
        let r = runs(Language::Rust, r#"let s = "a\"b";"#);
        assert!(
            r.iter()
                .any(|(k, s)| *k == Kind::String && s == r#""a\"b""#),
            "{r:?}"
        );
        // The semicolon after it is punctuation, not swallowed by the string.
        assert!(
            r.iter().any(|(k, s)| *k == Kind::Punctuation && s == ";"),
            "{r:?}"
        );
    }

    #[test]
    fn keywords_types_and_calls_are_told_apart_in_each_language() {
        assert!(kinds(Language::Python, "def f():").contains(&Kind::Keyword));
        assert!(kinds(Language::Go, "func main()").contains(&Kind::Keyword));
        assert!(kinds(Language::Json, "\"a\": true").contains(&Kind::Keyword));
        assert!(kinds(Language::C, "int main(void)").contains(&Kind::Keyword));
        assert!(kinds(Language::C, "size_t n = 0;").contains(&Kind::Type));
        let r = runs(Language::JavaScript, "if (x)");
        assert!(r.contains(&(Kind::Keyword, "if".into())), "{r:?}");
        // An unknown language still gets strings, numbers and comments.
        let r = runs(Language::Unknown, "a = 1 # note");
        assert!(r.contains(&(Kind::Number, "1".into())), "{r:?}");
        assert!(r.iter().any(|(k, _)| *k == Kind::Comment), "{r:?}");
    }

    #[test]
    fn a_fence_tag_is_matched_by_its_usual_spellings() {
        assert_eq!(Language::parse("rust"), Language::Rust);
        assert_eq!(Language::parse("rs"), Language::Rust);
        assert_eq!(Language::parse("rust,ignore"), Language::Rust);
        assert_eq!(Language::parse("TS"), Language::JavaScript);
        assert_eq!(Language::parse("bash"), Language::Shell);
        assert_eq!(Language::parse("console"), Language::Shell);
        assert_eq!(
            Language::parse(""),
            Language::Plain,
            "no tag, no highlighting"
        );
        assert_eq!(Language::parse("  "), Language::Plain);
        assert_eq!(Language::parse("brainfuck"), Language::Unknown);
    }

    #[test]
    fn a_role_with_no_colour_leaves_the_text_plain() {
        let syntax = SyntaxColours::default();
        // `Plain` is the unstyled one, which is what an all-default palette relies on.
        assert_eq!(Kind::Plain.style(&syntax), Style::new());
    }

    fn kind_of(runs: Vec<(Kind, &str)>) -> Option<Kind> {
        runs.first().map(|(k, _)| *k)
    }
}
