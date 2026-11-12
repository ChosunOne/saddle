//! A session: resolved roots, the policy for this workspace, the engine, and an open audit log.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use saddle_cap::policy::POLICY_FILE;
use saddle_cap::sandbox::SandboxExtras;
use saddle_cap::{Location, Policy, Principal, RootName, Roots, Scope};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{AuditLog, Decision, Engine, Kind, Request};
use serde_json::json;

use crate::config::UserConfig;
use crate::paths::{Paths, find_workspace};

/// What a workspace's own policy file starts as (D43): a comment saying what it is for, and nothing
/// else. An empty file and no file mean the same thing — the global policy alone.
pub const WORKSPACE_POLICY_HEADER: &str = r#"# This workspace's own policy, layered on top of the global policy in policy.toml.
#
# Anything not written here is taken from the global policy, so a change there reaches this
# workspace. What is written here replaces that grant for this scope, and it may grant more or
# less than the global policy does: this file is the authority for this directory. It is safe to
# let it widen because it lives in the config directory, which no root contains, so nothing the
# agent runs can write it -- only you can.
"#;

/// Whether a workspace's own file says anything beyond the header, which is only used to word a
/// note: the policy itself is layered either way (D43).
fn workspace_policy_has_grants(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .any(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        })
        .unwrap_or(false)
}

/// The roots a policy edit is validated against, without opening a session: the same set
/// `Setup::load` resolves, so a change that would not load is caught before it is written (D45).
pub fn roots_for_validation() -> Result<Roots> {
    let paths = Paths::from_env()?;
    let user = UserConfig::load(&paths.user_config())?;
    let cwd = std::env::current_dir().context("reading the working directory")?;
    let workspace = find_workspace(&cwd);
    let mut entries = vec![
        (RootName::workspace(), workspace),
        (RootName::tmp(), std::env::temp_dir()),
    ];
    entries.extend(user.root_entries()?);
    Ok(Roots::resolve(entries, &paths.protected())?)
}

/// Everything needed to parse the policy. `check` stops here; a session goes on to open the audit
/// log.
pub struct Setup {
    pub paths: Paths,
    pub workspace: PathBuf,
    /// The directory saddle was started in, inside the workspace.
    pub start: PathBuf,
    pub roots: Roots,
    pub extras: SandboxExtras,
    pub policy_path: PathBuf,
    pub policy_text: String,
    pub policy_sha256: String,
    /// The effective grants of the layered policy, as `Policy::describe` prints them, which is what
    /// `check` shows and the audit log's session record names.
    pub merged: Vec<String>,
    /// The global policy this workspace's file was layered on, for the same reason.
    pub global_sha256: String,
    /// Notes for the session intro, such as a workspace `saddle.toml` that is no longer read.
    pub notes: Vec<String>,
    /// The sensitive directories this workspace puts in reach, which is what the person is asked
    /// about before the session opens (D60). Empty for an ordinary project.
    pub exposed: Vec<String>,
}

impl Setup {
    /// `tmp` is the directory to use as the `tmp` root.
    pub fn load(start: &Path, tmp: &Path) -> Result<Self> {
        let paths = Paths::from_env()?;
        let start = start
            .canonicalize()
            .with_context(|| format!("{}", start.display()))?;
        let workspace = find_workspace(&start);
        let user = UserConfig::load(&paths.user_config())?;
        // Reported before a root is resolved: this is a warning, not a rule about workspaces (D60).
        let exposed = crate::paths::exposed(&workspace, &crate::paths::sensitive_dirs());

        let mut entries = vec![
            (RootName::workspace(), workspace.clone()),
            (RootName::tmp(), tmp.to_path_buf()),
        ];
        entries.extend(user.root_entries()?);
        let roots = Roots::resolve(entries, &paths.protected())?;
        let extras = user.sandbox_extras(&roots)?;

        // The global policy is the base, and a workspace's own file is layered on top of it rather
        // than copied from it (D43), so a change to the global policy reaches every workspace that
        // does not override the key. Both live in the config directory, which no root contains, so
        // the kernel protects them (D42).
        let own = paths.workspace_policy(&workspace);
        let mut notes = Vec::new();
        let global = match std::fs::read_to_string(paths.default_policy()) {
            Ok(text) => text,
            Err(e) => {
                notes.push(format!(
                    "{} is unreadable ({e}); using the built-in default policy",
                    paths.default_policy().display()
                ));
                crate::defaults::default_policy_text()
            }
        };
        // A workspace override is created empty rather than as a copy: its absence and an empty
        // file mean the same thing, and an empty file is an invitation to write a key in it.
        if own.symlink_metadata().is_err() {
            if let Some(dir) = own.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            std::fs::write(&own, WORKSPACE_POLICY_HEADER)
                .with_context(|| format!("creating {}", own.display()))?;
            notes.push(format!(
                "created {} for this workspace's own grants; it adds to {}",
                own.display(),
                paths.default_policy().display()
            ));
        }
        // A tree-level policy is not authority, and silently ignoring one would be worse than
        // saying so.
        let in_tree = workspace.join(POLICY_FILE);
        if in_tree.symlink_metadata().is_ok() {
            notes.push(format!(
                "{} is not read: this workspace's policy is {}{}",
                in_tree.display(),
                paths.default_policy().display(),
                if workspace_policy_has_grants(&own) {
                    format!(" with {}", own.display())
                } else {
                    String::new()
                }
            ));
        }
        let policy_path = own;
        let policy_text = std::fs::read_to_string(&policy_path)
            .with_context(|| format!("reading {}", policy_path.display()))?;
        // Layered here rather than in `parse_policy`, so a project file that would widen a global
        // grant is refused before a session opens rather than on the first tool call.
        let merged = Policy::layer(&global, &policy_text, &roots)
            .with_context(|| format!("reading {}", policy_path.display()))?;
        let merged_lines = merged.describe();
        let policy_sha256 = sha256_hex(policy_text.as_bytes());
        Ok(Self {
            paths,
            start: start.clone(),
            workspace,
            roots,
            extras,
            policy_path,
            policy_text,
            policy_sha256,
            merged: merged_lines,
            global_sha256: sha256_hex(global.as_bytes()),
            notes,
            exposed,
        })
    }

    pub fn parse_policy(&self) -> Result<Policy> {
        let global = std::fs::read_to_string(self.paths.default_policy())
            .unwrap_or_else(|_| crate::defaults::default_policy_text());
        Policy::layer(&global, &self.policy_text, &self.roots)
            .with_context(|| format!("{}", self.policy_path.display()))
    }
}

/// The per-session `tmp` root: a 0700 directory under the system temp dir, removed on drop.
pub struct PrivateTmp(PathBuf);

impl PrivateTmp {
    pub fn create(id: &str) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::env::temp_dir().join(format!("saddle-{id}"));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self(path))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateTmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Session {
    pub setup: Setup,
    pub engine: Engine,
    pub audit: AuditLog,
    /// `Ok(landlock ABI)` or the reason exec is unavailable.
    pub sandbox: Result<i32, String>,
    pub host_env: Vec<(String, String)>,
    /// This session's id (D53, D60).
    pub id: String,
    _tmp: PrivateTmp,
}

impl Session {
    /// Opens the session named `id`, or one generated here. The id is what an allowance is written
    /// against (D60) and what the audit log is named for.
    pub fn open(start: &Path, id: Option<&str>) -> Result<Self> {
        let id = id.map(str::to_owned).unwrap_or_else(session_id);
        let tmp = PrivateTmp::create(&id)?;
        let setup = Setup::load(start, tmp.path())?;
        let policy = setup.parse_policy()?;
        let sandbox = saddle_sandbox::availability();

        let mut audit = AuditLog::create(&setup.paths.audit_dir(), &id)?;
        let roots: serde_json::Map<String, serde_json::Value> = setup
            .roots
            .iter()
            .map(|(n, p)| (n.to_string(), json!(p)))
            .collect();
        audit.event(
            Kind::SessionStart,
            None,
            None,
            None,
            json!({
                "workspace": setup.workspace,
                "roots": roots,
                "policy": {
                    "path": setup.policy_path,
                    "sha256": setup.policy_sha256,
                },
                "sandbox": match &sandbox { Ok(abi) => json!({ "landlock-abi": abi }), Err(e) => json!({ "unavailable": e }) },
            }),
            "session opened",
        )?;

        Ok(Self {
            setup,
            engine: Engine::new(policy),
            audit,
            sandbox,
            host_env: std::env::vars().collect(),
            id,
            _tmp: tmp,
        })
    }

    pub fn roots(&self) -> &Roots {
        &self.setup.roots
    }

    /// Decides and logs. Logging failure is fatal: an unlogged decision breaks D10.
    /// Reads this workspace's policy again and rebuilds the engine, for a grant changed mid-session
    /// (D47). The new file is parsed and layered exactly as at open, so a change that would not load
    /// stops the reload and leaves the session on the grants it had.
    pub fn reload_policy(&mut self) -> Result<()> {
        let fresh = Setup::load(&self.setup.start, self._tmp.path())?;
        let policy = fresh.parse_policy()?;
        let before = self.setup.policy_sha256.clone();
        self.setup = fresh;
        self.engine = Engine::new(policy);
        self.audit.event(
            Kind::Policy,
            None,
            None,
            None,
            json!({
                "policy": {
                    "path": self.setup.policy_path,
                    "sha256": self.setup.policy_sha256,
                    "previous_sha256": before,
                },
            }),
            "policy reloaded mid-session",
        )?;
        Ok(())
    }

    /// Whether the policy names this host in a net grant, asked before a literal address is
    /// dialled (D28, D40). Not a decision, so it is not logged: the request that follows is.
    pub fn net_names_host(&self, who: &Principal, host: &str) -> bool {
        self.engine.net_names_host(who, host)
    }

    pub fn decide(&mut self, who: &Principal, req: &Request<'_>) -> Result<Decision> {
        let d = self.engine.decide(who, req);
        self.audit
            .decision(who, req, &d)
            .context("writing the audit log")?;
        Ok(d)
    }

    /// Maps a path from the model to a location: relative paths are taken from the workspace
    /// root. A path outside every root is denied and logged here.
    pub fn locate(
        &mut self,
        who: &Principal,
        scope: Scope,
        raw: &str,
    ) -> Result<Result<Location, String>> {
        let path = Path::new(raw);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.setup.workspace.join(path)
        };
        if let Some(loc) = self.roots().locate(&abs) {
            return Ok(Ok(loc));
        }
        let explanation = format!("{raw} is outside every root");
        self.audit.event(
            Kind::Decision,
            Some(who),
            Some(scope),
            Some(false),
            json!({ "path": raw }),
            &explanation,
        )?;
        Ok(Err(explanation))
    }

    /// Closes the audit log and drops this session's allowance, so a permission to use a sensitive
    /// workspace cannot outlive the session it was granted for (D60).
    pub fn close(self) -> Result<String> {
        let closed = self.audit.close(&self.setup.paths.sessions_index())?;
        crate::allow::Allowance::new(&self.setup.paths.state).revoke(&self.id);
        Ok(closed)
    }
}

pub fn session_id() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{ms:x}-{:x}", std::process::id())
}
