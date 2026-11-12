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
