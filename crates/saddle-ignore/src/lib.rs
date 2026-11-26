use globset::{GlobBuilder, GlobMatcher};

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
    pub fn add(&mut self, text: &str, base: &str) {
        for line in text.lines() {
            let line = line.trim();
            let escaped = line.starts_with("\\#") || line.starts_with("\\!");
            if line.is_empty() || (!escaped && line.starts_with('#')) {
                continue;
            }
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

        assert!(r.ignored("src/deep/x.log", false));
        assert!(!r.ignored("keep.log", false), "a later ! wins");
        assert!(r.ignored("root.txt", false));
        assert!(!r.ignored("src/root.txt", false));
        assert!(r.ignored("a/build", true));
        assert!(!r.ignored("a/build", false));
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
