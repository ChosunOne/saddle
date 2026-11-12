//! The third lowering (plan §5): a principal's grants to the sandbox a child process runs in (D19).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::policy::Grants;
use crate::roots::{Location, RootName, Roots};

/// Read and execute for every child, whatever the grants.
pub const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/etc",
    "/proc",
    "/dev/urandom",
    "/dev/random",
];
/// Writable device nodes for every child.
pub const SYSTEM_WRITE: &[&str] = &["/dev/null", "/dev/zero", "/dev/full"];
/// Environment variables passed through by default. `LC_*` is matched by prefix.
pub const BASE_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LANGUAGE", "TERM", "TZ",
];

/// Everything the helper needs to confine itself and exec the target. Serialised to the helper's
/// stdin (D18).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// The complete child environment.
    pub env: Vec<(String, String)>,
    /// Paths the child may read and execute beneath.
    pub read: Vec<PathBuf>,
    /// Paths the child may read, write, create and delete beneath.
    pub write: Vec<PathBuf>,
}

/// User-level additions from `config.toml` (`[sandbox] read = [...]`, `write = [...]`), for
/// toolchains that live outside the workspace.
#[derive(Debug, Clone, Default)]
pub struct SandboxExtras {
    pub read: Vec<RootName>,
    pub write: Vec<RootName>,
    /// Directories worked out at startup from an `[[sandbox.include]]` rule, already enumerated
    /// (D62).
    pub include: Vec<PathBuf>,
}

/// One `[[sandbox.include]]` rule: walk `base`, keep the directories that do not match `except`,
/// and hand landlock the list (D62).
#[derive(Debug, Clone)]
pub struct Include {
    pub base: PathBuf,
    /// How deep below `base` to walk. A landlock rule is recursive, so a rule for a directory
    /// grants everything beneath it; this only decides how far saddle looks for exclusions.
    pub depth: usize,
    /// Matched against the path relative to `base`, as slashes.
    pub except: Vec<regex::Regex>,
}

/// What an `Include` rule grants: the directories to add to the child's rules (D62).
///
/// A pattern is only an input. The authority is this list, which is what `check` prints and the
/// audit record names, because a rule that says "everything under home except these" means
/// nothing a person can review until it is resolved.
///
/// The rule that makes it safe: a landlock rule covers everything below its directory, so a
/// directory that **contains** an exclusion must not be granted — granting `~/.config` would grant
/// `~/.config/saddle` however the pattern reads. Such a directory is therefore expanded instead:
/// its children are considered in its place. `protected` is excluded whatever the pattern says,
/// because the state and config directories may never be in a root (D16).
pub fn enumerate(include: &Include, protected: &[PathBuf]) -> Vec<PathBuf> {
    let base = include.base.canonicalize().unwrap_or(include.base.clone());
    let protected: Vec<PathBuf> = protected
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    // Every entry worth deciding about, parents before children.
    let mut found: Vec<Entry> = Vec::new();
    walk(&base, &base, include.depth, &mut found);
    // Two passes, because a directory must not be granted when *anything* below it is withheld,
    // and that is not known until the whole walk has been read.
    //
    // First: which directories are withheld, and with them everything beneath them.
    let mut dead: Vec<String> = Vec::new();
    for entry in &found {
        let withheld = entry.rel.is_empty()
            || dead.iter().any(|d| is_below(&entry.rel, d))
            || entry.is_excluded(&include.except, &protected);
        if withheld && entry.is_dir && !entry.rel.is_empty() {
            dead.push(entry.rel.clone());
        }
    }
    // Second: the rules. A directory above a withheld one is expanded rather than granted, and a
    // directory already covered by a kept rule adds nothing, because landlock rules are recursive.
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut kept_rel: Vec<String> = Vec::new();
    for entry in &found {
        if entry.rel.is_empty()
            || dead
                .iter()
                .any(|d| d == &entry.rel || is_below(&entry.rel, d))
        {
            continue;
        }
        if entry.is_excluded(&include.except, &protected) {
            continue;
        }
        if entry.is_dir && dead.iter().any(|d| is_below(d, &entry.rel)) {
            continue;
        }
        if kept_rel.iter().any(|k| is_below(&entry.rel, k)) {
            continue;
        }
        kept.push(entry.path.clone());
        if entry.is_dir {
            kept_rel.push(entry.rel.clone());
        }
    }
    kept
}

/// A directory or file found while walking, with its path relative to the base.
struct Entry {
    path: PathBuf,
    rel: String,
    is_dir: bool,
}

impl Entry {
    fn is_excluded(&self, except: &[regex::Regex], protected: &[PathBuf]) -> bool {
        protected
            .iter()
            .any(|p| self.path == *p || self.path.starts_with(p))
            || except.iter().any(|re| re.is_match(&self.rel))
    }
}

/// Whether `child` is strictly below `parent`, comparing path strings as slashes.
fn is_below(child: &str, parent: &str) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && child.as_bytes()[parent.len()] == b'/'
}

/// Fills `out` with every entry below `dir`, down to `depth` (0 means none), parents first.
///
/// Files are collected as well as directories. A directory that contains an exclusion is expanded
/// rather than granted (D62), and without file entries its regular files would become unreadable
/// too — `~/.gitconfig` would stop working the moment anything else in home is excluded. Landlock
/// accepts a rule on a regular file, so the expansion can be exact.
fn walk(dir: &Path, base: &Path, depth: usize, out: &mut Vec<Entry>) {
    let rel = dir
        .strip_prefix(base)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    out.push(Entry {
        path: dir.to_path_buf(),
        rel,
        is_dir: true,
    });
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        } else {
            files.push(path);
        }
    }
    dirs.sort();
    files.sort();
    // Files first, so a file's own rule is never shadowed by a directory rule decided later.
    for path in files {
        let rel = path
            .strip_prefix(base)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        out.push(Entry {
            path,
            rel,
            is_dir: false,
        });
    }
    if depth == 0 {
        return;
    }
    for child in dirs {
        walk(&child, base, depth - 1, out);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LowerError {
    #[error("no exec grant")]
    NoExecGrant,
    #[error("root `{0}` is not defined in this session")]
    UnknownRoot(RootName),
    #[error("empty command")]
    EmptyArgv,
}

pub struct ExecRequest<'a> {
    pub argv: Vec<String>,
    pub cwd: &'a Location,
    /// The host's environment, filtered here through the allowlist.
    pub host_env: &'a [(String, String)],
}

pub struct Lowered {
    pub spec: SandboxSpec,
    pub timeout: Duration,
}

pub fn lower_exec(
    grants: &Grants,
    roots: &Roots,
    extras: &SandboxExtras,
    req: ExecRequest<'_>,
) -> Result<Lowered, LowerError> {
    let exec = grants.exec.as_ref().ok_or(LowerError::NoExecGrant)?;
    if req.argv.is_empty() {
        return Err(LowerError::EmptyArgv);
    }
    let path = |name: &RootName| -> Result<PathBuf, LowerError> {
        roots
            .get(name)
            .map(Path::to_path_buf)
            .ok_or_else(|| LowerError::UnknownRoot(name.clone()))
    };

    let tmp = path(&RootName::tmp())?;
    let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
    for name in grants.fs_read.iter().map(|g| &g.root).chain(&extras.read) {
        read.push(path(name)?);
    }
    // Enumerated by an `[[sandbox.include]]` rule at startup; already a list of directories, so
    // nothing here has to know what a toolchain is (D62).
    read.extend(extras.include.iter().cloned());
    let mut write: Vec<PathBuf> = SYSTEM_WRITE.iter().map(PathBuf::from).collect();
    for name in grants.fs_write.iter().map(|g| &g.root).chain(&extras.write) {
        write.push(path(name)?);
    }
    write.push(tmp.clone());
    read.sort();
    read.dedup();
    write.sort();
    write.dedup();

    let mut env: Vec<(String, String)> = req
        .host_env
        .iter()
        .filter(|(k, _)| {
            BASE_ENV.contains(&k.as_str()) || k.starts_with("LC_") || exec.env.contains(k)
        })
        .cloned()
        .collect();
    env.retain(|(k, _)| k != "TMPDIR");
    env.push(("TMPDIR".into(), tmp.to_string_lossy().into_owned()));
    env.sort();

    let cwd = roots
        .path_of(req.cwd)
        .ok_or_else(|| LowerError::UnknownRoot(req.cwd.root.clone()))?;
    Ok(Lowered {
        spec: SandboxSpec {
            argv: req.argv,
            cwd,
            env,
            read,
            write,
        },
        timeout: exec.timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Policy, Principal};
    use crate::roots::RelPath;

    fn tree(dirs: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for d in dirs {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        std::fs::write(dir.path().join("README"), "x").unwrap();
        dir
    }

    fn rule(base: &Path, depth: usize, except: &[&str]) -> Include {
        Include {
            base: base.to_path_buf(),
            depth,
            except: except
                .iter()
                .map(|p| regex::Regex::new(p).unwrap())
                .collect(),
        }
    }

    fn rel(base: &Path, out: &[PathBuf]) -> Vec<String> {
        let mut v: Vec<String> = out
            .iter()
            .map(|p| p.strip_prefix(base).unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_include_grants_the_directories_that_are_not_excluded() {
        // D62: the pattern is an input; this list is the authority.
        let dir = tree(&["cargo/bin", "rustup/toolchains", "jj"]);
        let got = rel(dir.path(), &enumerate(&rule(dir.path(), 2, &[]), &[]));
        // The base is never a rule, and a directory with nothing excluded beneath it is one
        // recursive rule rather than one per child. Files get a rule of their own, because they
        // are not covered by any directory rule the base does not have.
        assert_eq!(got, ["README", "cargo", "jj", "rustup"], "{got:?}");
    }

    #[test]
    fn a_directory_containing_an_exclusion_is_expanded_not_granted() {
        // The rule that makes this safe: a landlock rule covers everything below its directory, so
        // granting ~/.config would grant ~/.config/saddle however the pattern reads (D16, D20).
        let dir = tree(&[".config/saddle", ".config/git", ".config/other", ".ssh"]);
        let got = rel(
            dir.path(),
            &enumerate(
                &rule(dir.path(), 2, &[r"^\.config/saddle$", r"^\.ssh$"]),
                &[],
            ),
        );
        assert!(
            !got.iter().any(|p| p == ".config"),
            "the parent must not be granted: {got:?}"
        );
        assert!(
            !got.iter().any(|p| p.starts_with(".config/saddle")),
            "{got:?}"
        );
        assert!(!got.iter().any(|p| p == ".ssh"), "{got:?}");
        // Its siblings still are, so the tool configs that live there keep working.
        assert_eq!(got, [".config/git", ".config/other", "README"], "{got:?}");
    }

    #[test]
    fn the_state_and_config_directories_are_excluded_whatever_the_pattern_says() {
        // A pattern cannot open what D16 keeps out of every root.
        let dir = tree(&["state/saddle", "config", "cargo"]);
        let state = dir.path().join("state/saddle");
        let got = rel(
            dir.path(),
            &enumerate(&rule(dir.path(), 2, &[]), std::slice::from_ref(&state)),
        );
        assert_eq!(got, ["README", "cargo", "config"], "{got:?}");
    }

    #[test]
    fn a_depth_limit_decides_how_far_saddle_looks_for_exclusions() {
        // Depth bounds only the search: a granted directory covers its children anyway.
        let dir = tree(&["a/b/c"]);
        assert_eq!(
            rel(dir.path(), &enumerate(&rule(dir.path(), 1, &[]), &[])),
            ["README", "a"]
        );
        assert_eq!(
            rel(dir.path(), &enumerate(&rule(dir.path(), 3, &[]), &[])),
            ["README", "a"]
        );
        // An exclusion two levels down forces the expansion down to it.
        let dir = tree(&["a/b/secret", "a/b/keep"]);
        let got = rel(
            dir.path(),
            &enumerate(&rule(dir.path(), 3, &[r"^a/b/secret$"]), &[]),
        );
        assert_eq!(got, ["README", "a/b/keep"], "{got:?}");
    }

    #[test]
    fn a_pattern_that_matches_a_directory_by_prefix_does_not_leak_its_siblings() {
        // Unanchored patterns are allowed, which is how a person writes ".ssh" rather than the
        // full relative path; the point of the test is that matching is still per directory.
        let dir = tree(&["cache/ssh-agent", "cache/other"]);
        let got = rel(
            dir.path(),
            &enumerate(&rule(dir.path(), 2, &["ssh-agent"]), &[]),
        );
        assert_eq!(got, ["README", "cache/other"], "{got:?}");
    }

    #[test]
    fn an_empty_or_missing_base_grants_nothing_rather_than_everything() {
        let out = enumerate(&rule(Path::new("/nonexistent-saddle-test"), 2, &[]), &[]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn child_gets_grant_roots_and_an_allowlisted_env() {
        let ws = tempfile::tempdir().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let roots = Roots::resolve(
            [
                (RootName::workspace(), ws.path().into()),
                (RootName::tmp(), tmp.path().into()),
            ],
            &[],
        )
        .unwrap();
        let policy = Policy::parse(
            r#"
version = 1
[agent]
fs-read = [{ root = "workspace", why = "r" }]
exec = { why = "x", env = ["CARGO_TERM_COLOR"] }
"#,
            &roots,
        )
        .unwrap();
        let grants = policy.grants(&Principal::Agent).unwrap();
        let host_env: Vec<(String, String)> = [
            ("PATH", "/usr/bin"),
            ("LC_ALL", "C"),
            ("CARGO_TERM_COLOR", "always"),
            ("ANTHROPIC_API_KEY", "secret"),
            ("SSH_AUTH_SOCK", "/run/user/1000/ssh"),
            ("TMPDIR", "/tmp"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cwd = Location::new(RootName::workspace(), RelPath::default());
        let lowered = lower_exec(
            grants,
            &roots,
            &SandboxExtras::default(),
            ExecRequest {
                argv: vec!["true".into()],
                cwd: &cwd,
                host_env: &host_env,
            },
        )
        .unwrap();
        let spec = lowered.spec;
        let names: Vec<&str> = spec.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["CARGO_TERM_COLOR", "LC_ALL", "PATH", "TMPDIR"]);
        let tmp_canon = tmp.path().canonicalize().unwrap();
        assert!(
            spec.env
                .contains(&("TMPDIR".into(), tmp_canon.to_string_lossy().into()))
        );
        let ws_canon = ws.path().canonicalize().unwrap();
        assert!(spec.read.contains(&ws_canon));
        assert!(
            !spec.write.contains(&ws_canon),
            "fs-read alone must not make the root writable"
        );
        assert!(spec.write.contains(&tmp_canon));
        assert_eq!(spec.cwd, ws_canon);
    }
}
