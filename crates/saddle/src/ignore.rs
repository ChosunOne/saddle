//! Ignore files, shared by the search tools and by skill discovery (D68).
//!
//! One parser, two callers, because "what is ignored" has to mean the same thing wherever it is
//! asked: a file `grep` skips and skill discovery walks is a disagreement the person cannot see.
//! The file names are the ones ripgrep and pi honour; which of them a caller reads is its own
//! choice, since the search tools keep to `.gitignore` and skills take all three.
//!
//! The syntax is the part of `.gitignore` that matters here: `#` comments, `!` negation, a
//! trailing `/` for directories only, a pattern with a slash anchored to the file's directory and
//! one without matching at any depth beneath it. The last matching rule decides, so a later `!`
//! can re-include what an earlier line excluded.

use globset::{GlobBuilder, GlobMatcher};

/// The ignore files honoured, in the order they are read. A later file's rules are appended, so
/// they win on a tie, which is what `.gitignore` then `.ignore` then `.fdignore` means.
pub const FILES: &[&str] = &[".gitignore", ".ignore", ".fdignore"];

struct Rule {
    glob: GlobMatcher,
    negate: bool,
    dir_only: bool,
}

#[derive(Default)]
pub struct Rules {
    rules: Vec<Rule>,
}

impl Rules {
    /// Adds the rules in `text`, which came from a file in `base` (`""` for a tree's root).
    /// Patterns are prefixed with that directory, so `sub/.gitignore`'s `foo` means `sub/foo`.
    pub fn add(&mut self, text: &str, base: &str) {
        for line in text.lines() {
            let line = line.trim();
            // `\#` and `\!` are literals; an unescaped `#` starts a comment.
            let escaped = line.starts_with("\\#") || line.starts_with("\\!");
            if line.is_empty() || (!escaped && line.starts_with('#')) {
                continue;
            }
            // The escape protects the first character from being read as syntax, so it is
            // dropped once and the character after it is taken literally.
            let (negate, line) = if escaped {
                (false, &line[1..])
            } else {
                match line.strip_prefix('!') {
                    Some(rest) => (true, rest),
                    None => (false, line),
                }
            };
            let (dir_only, line) = match line.strip_suffix('/') {
                Some(rest) => (true, rest),
                None => (false, line),
            };
            // A pattern with a slash is anchored to the file it came from; one without matches at
            // any depth below it.
            let anchored = line.contains('/');
            let line = line.trim_start_matches('/');
            if line.is_empty() {
                continue;
            }
            let prefix = if base.is_empty() {
                String::new()
            } else {
                format!("{base}/")
            };
            let pattern = if anchored {
                format!("{prefix}{line}")
            } else {
                format!("{prefix}**/{line}")
            };
            if let Ok(glob) = GlobBuilder::new(&pattern).literal_separator(true).build() {
                self.rules.push(Rule {
                    glob: glob.compile_matcher(),
                    negate,
                    dir_only,
                });
            }
        }
    }

    /// The last matching rule decides.
    pub fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        let mut ignored = false;
        for r in &self.rules {
            if (!r.dir_only || is_dir) && r.glob.is_match(rel) {
                ignored = !r.negate;
            }
        }
        ignored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchoring_negation_depth_and_directory_only() {
        let mut r = Rules::default();
        r.add("*.log\n/root.txt\n!keep.log\nbuild/\nsub/*.txt\n", "");

        // A pattern with no slash matches at any depth.
        assert!(r.ignored("src/deep/x.log", false));
        assert!(!r.ignored("keep.log", false), "a later ! wins");
        // A leading slash anchors to the file's own directory.
        assert!(r.ignored("root.txt", false));
        assert!(!r.ignored("src/root.txt", false));
        // A trailing slash is directories only.
        assert!(r.ignored("a/build", true));
        assert!(!r.ignored("a/build", false));
        // An inner slash anchors to that directory.
        assert!(r.ignored("sub/x.txt", false));
        assert!(!r.ignored("other/sub/x.txt", false));
    }

    #[test]
    fn a_rule_from_a_subdirectory_is_prefixed() {
        let mut r = Rules::default();
        r.add("*.tmp\n", "");
        r.add("*.tmp\n", "crates");
        assert!(r.ignored("crates/x.tmp", false));
        assert!(r.ignored("x.tmp", false));
        // The prefix narrows nothing on its own: the same rule twice is still one rule.
        assert!(!r.ignored("other/x.rs", false));
    }

    #[test]
    fn comments_blank_lines_and_escapes() {
        let mut r = Rules::default();
        r.add("# a comment\n\n   \n", "");
        assert!(r.rules.is_empty(), "no rules from comments or blanks");
        let mut r = Rules::default();
        r.add("\\#literal\n\\!bang\n", "");
        assert!(r.ignored("#literal", false));
        assert!(
            r.ignored("!bang", false),
            "an escaped ! is a literal pattern"
        );
    }
}
