use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::policy::Grants;
use crate::roots::{Location, RootName, Roots};

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
pub const SYSTEM_WRITE: &[&str] = &["/dev/null", "/dev/zero", "/dev/full"];
pub const BASE_ENV: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LANGUAGE", "TERM", "TZ",
];

pub const PROXY_PORT: u16 = 3128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetBridge {
    pub socket: PathBuf,
    pub proxy_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    #[serde(default)]
    pub net: Option<NetBridge>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_launch: Option<NamedLaunch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedLaunch {
    pub protected: Vec<PathBuf>,
    pub agent_write: Vec<PathBuf>,
    #[serde(default)]
    pub executable_sha256: Option<[u8; 32]>,
    #[serde(default)]
    pub helpers: Vec<(PathBuf, Option<[u8; 32]>)>,
}

#[derive(Debug, Clone, Default)]
pub struct SandboxExtras {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub include: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Include {
    pub base: PathBuf,
    pub depth: usize,
    pub except: Vec<regex::Regex>,
}

pub fn enumerate(include: &Include, protected: &[PathBuf]) -> Vec<PathBuf> {
    let base = include.base.canonicalize().unwrap_or(include.base.clone());
    let protected: Vec<PathBuf> = protected
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    let mut found: Vec<Entry> = Vec::new();
    walk(&base, &base, include.depth, &mut found);
    let mut withheld: Vec<String> = Vec::new();
    let mut withheld_dirs: Vec<String> = Vec::new();
    for entry in &found {
        if entry.rel.is_empty() || withheld_dirs.iter().any(|d| is_below(&entry.rel, d)) {
            continue;
        }
        if entry.is_excluded(&include.except, &protected) {
            withheld.push(entry.rel.clone());
            if entry.is_dir {
                withheld_dirs.push(entry.rel.clone());
            }
        }
    }
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut kept_rel: Vec<String> = Vec::new();
    for entry in &found {
        if entry.rel.is_empty() || withheld_dirs.iter().any(|d| is_below(&entry.rel, d)) {
            continue;
        }
        if entry.is_excluded(&include.except, &protected) {
            continue;
        }
        if entry.is_dir && withheld.iter().any(|w| is_below(w, &entry.rel)) {
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

fn is_below(child: &str, parent: &str) -> bool {
    child.len() > parent.len()
        && child.starts_with(parent)
        && child.as_bytes()[parent.len()] == b'/'
}

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
    let Ok(entries) = fs::read_dir(dir) else {
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
    for name in grants.fs_read.iter().map(|g| &g.root) {
        read.push(path(name)?);
    }
    read.extend(extras.read.iter().cloned());
    read.extend(extras.include.iter().cloned());
    let mut write: Vec<PathBuf> = SYSTEM_WRITE.iter().map(PathBuf::from).collect();
    for name in grants.fs_write.iter().map(|g| &g.root) {
        write.push(path(name)?);
    }
    write.extend(extras.write.iter().cloned());
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
    let proxy = format!("http://127.0.0.1:{PROXY_PORT}");
    for name in [
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
    ] {
        env.push((name.into(), proxy.clone()));
    }
    for name in ["no_proxy", "NO_PROXY"] {
        env.push((name.into(), "localhost,127.0.0.1,::1".into()));
    }
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
            net: None,
            named_launch: None,
        },
        timeout: exec.timeout,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Policy, Principal};
    use crate::roots::RelPath;
    use std::fs;
    use std::slice;

    fn tree(dirs: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for d in dirs {
            fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        fs::write(dir.path().join("README"), "x").unwrap();
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
        let dir = tree(&["cargo/bin", "rustup/toolchains", "jj"]);
        let got = rel(dir.path(), &enumerate(&rule(dir.path(), 2, &[]), &[]));
        assert_eq!(got, ["README", "cargo", "jj", "rustup"], "{got:?}");
    }

    #[test]
    fn a_directory_containing_an_exclusion_is_expanded_not_granted() {
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
        assert_eq!(got, [".config/git", ".config/other", "README"], "{got:?}");
    }

    #[test]
    fn a_withheld_file_expands_its_directory_too() {
        let dir = tree(&["cargo/registry"]);
        fs::write(dir.path().join("cargo/credentials.toml"), "token\n").unwrap();
        fs::write(dir.path().join("cargo/config.toml"), "[net]\n").unwrap();
        let got = rel(
            dir.path(),
            &enumerate(&rule(dir.path(), 2, &[r"^cargo/credentials\.toml$"]), &[]),
        );
        assert!(!got.iter().any(|p| p == "cargo"), "{got:?}");
        assert!(!got.iter().any(|p| p.contains("credentials")), "{got:?}");
        assert!(got.iter().any(|p| p == "cargo/config.toml"), "{got:?}");
        assert!(got.iter().any(|p| p == "cargo/registry"), "{got:?}");
    }

    #[test]
    fn the_state_and_config_directories_are_excluded_whatever_the_pattern_says() {
        let dir = tree(&["state/saddle", "config", "cargo"]);
        let state = dir.path().join("state/saddle");
        let got = rel(
            dir.path(),
            &enumerate(&rule(dir.path(), 2, &[]), slice::from_ref(&state)),
        );
        assert_eq!(got, ["README", "cargo", "config"], "{got:?}");
    }

    #[test]
    fn a_depth_limit_decides_how_far_saddle_looks_for_exclusions() {
        let dir = tree(&["a/b/c"]);
        assert_eq!(
            rel(dir.path(), &enumerate(&rule(dir.path(), 1, &[]), &[])),
            ["README", "a"]
        );
        assert_eq!(
            rel(dir.path(), &enumerate(&rule(dir.path(), 3, &[]), &[])),
            ["README", "a"]
        );
        let dir = tree(&["a/b/secret", "a/b/keep"]);
        let got = rel(
            dir.path(),
            &enumerate(&rule(dir.path(), 3, &[r"^a/b/secret$"]), &[]),
        );
        assert_eq!(got, ["README", "a/b/keep"], "{got:?}");
    }

    #[test]
    fn a_pattern_that_matches_a_directory_by_prefix_does_not_leak_its_siblings() {
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
        assert_eq!(
            names,
            [
                "ALL_PROXY",
                "CARGO_TERM_COLOR",
                "HTTPS_PROXY",
                "HTTP_PROXY",
                "LC_ALL",
                "NO_PROXY",
                "PATH",
                "TMPDIR",
                "all_proxy",
                "http_proxy",
                "https_proxy",
                "no_proxy",
            ]
        );
        let proxy = format!("http://127.0.0.1:{PROXY_PORT}");
        for name in ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"] {
            assert!(
                spec.env.contains(&(name.to_owned(), proxy.clone())),
                "{name} does not reach the proxy"
            );
        }
        assert!(
            spec.env
                .contains(&("no_proxy".to_owned(), "localhost,127.0.0.1,::1".to_owned()))
        );
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
