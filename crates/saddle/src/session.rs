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
    /// The base grants as TOML, so a policy reload re-layers without reading the file twice.
    pub global_text: String,
    /// Notes for the session intro, such as a workspace `saddle.toml` that is no longer read.
    pub notes: Vec<String>,
    /// This run's grant overrides, kept so a policy reload does not silently drop them (D63).
    pub overrides: crate::config::Overrides,
    /// The sensitive directories this workspace puts in reach, which is what the person is asked
    /// about before the session opens (D60). Empty for an ordinary project.
    pub exposed: Vec<String>,
}

impl Setup {
    /// `tmp` is the directory to use as the `tmp` root. `overrides` are this run's `--grant` and
    /// `--revoke`, which are not stored (D63).
    pub fn load(start: &Path, tmp: &Path, overrides: &crate::config::Overrides) -> Result<Self> {
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
        // The include rules are enumerated here, where the protected directories are known, so
        // what a child may reach is a list rather than a pattern (D62).
        let extras = user.sandbox_extras(&roots, &paths.protected())?;

        // One file holds everything (D63): `[agent]` is the base every workspace inherits, and
        // `[workspaces.<key>]` is this workspace's own grants, layered on top of it rather than
        // copied from it, so a change to the base reaches every workspace that does not override
        // that key. It lives in the config directory, which no root contains, so the kernel
        // protects it (D42).
        let config_path = paths.user_config();
        let mut notes = Vec::new();
        let user = UserConfig::load(&config_path)?;
        let key = crate::paths::workspace_key(&workspace);
        let mut own = user.workspace_policy(&key);
        let global = user.global_policy();
        if !overrides.is_empty() {
            // Applied to the *merged* grants, not to the workspace's own: `--revoke exec` has to
            // remove a grant that came from the base, and a workspace-level edit could only
            // shadow it (D63).
            let merged_doc = saddle_cap::policy::merge(
                global.clone(),
                own.unwrap_or_else(|| crate::config::policy_document_pub(None)),
            );
            let mut agent = match merged_doc.get("agent") {
                Some(toml::Value::Table(t)) => t.clone(),
                _ => toml::Table::new(),
            };
            overrides.apply(&mut agent)?;
            let mut doc = toml::Table::new();
            doc.insert("version".into(), toml::Value::Integer(1));
            doc.insert("agent".into(), toml::Value::Table(agent));
            own = Some(toml::Value::Table(doc));
            notes.push(format!(
                "grants changed for this session only: {}",
                overrides.describe()
            ));
        }
        let global = user.global_policy();
        let global_text = toml::to_string(&global).context("reading the grants")?;
        let policy_text = match &own {
            Some(doc) => toml::to_string(doc).context("reading this workspace's grants")?,
            None => String::new(),
        };
        // A tree-level policy is not authority, and silently ignoring one would be worse than
        // saying so.
        let in_tree = workspace.join(POLICY_FILE);
        if in_tree.symlink_metadata().is_ok() {
            notes.push(format!(
                "{} is not read: this workspace's grants are in {}{}",
                in_tree.display(),
                config_path.display(),
                if user.workspace_has_grants(&key) {
                    format!(" under [workspaces.\"{key}\"]")
                } else {
                    String::new()
                }
            ));
        }
        let policy_path = config_path;
        // With overrides the workspace document already holds the merged result, so the base is
        // not layered a second time (D63). This is what is stored, because `parse_policy` layers
        // from it: the engine has to be built from the same document `check` describes.
        let global_text = if overrides.is_empty() {
            global_text
        } else {
            String::new()
        };
        let merged = Policy::layer(&global_text, &policy_text, &roots)
            .with_context(|| format!("reading the grants in {}", policy_path.display()))?;
        let merged_lines = merged.describe();
        // The hash names the grants, not the file: a provider change is not a policy change.
        let policy_sha256 = sha256_hex(format!("{global_text}\n---\n{policy_text}").as_bytes());
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
            global_sha256: sha256_hex(global_text.as_bytes()),
            global_text,
            notes,
            exposed,
            overrides: overrides.clone(),
        })
    }

    pub fn parse_policy(&self) -> Result<Policy> {
        Policy::layer(&self.global_text, &self.policy_text, &self.roots)
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
    _tmp: PrivateTmp,
}

impl Session {
    /// Opens the session named `id`, or one generated here.
    ///
    /// The id names the permission (D60), so it can be reused by a later run (D61). The audit log
    /// and the private temporary directory belong to a *run* rather than to the id, so they get a
    /// token of their own: two processes sharing one id must not fight over one file, and a reused
    /// id must not append to a chain that a previous process already closed.
    /// Opens a session with this run's grant overrides applied (D63).
    pub fn open_with(
        start: &Path,
        id: Option<&str>,
        overrides: &crate::config::Overrides,
    ) -> Result<Self> {
        let id = id.map(str::to_owned).unwrap_or_else(session_id);
        let run = format!("{id}-{}", session_id());
        let tmp = PrivateTmp::create(&run)?;
        let setup = Setup::load(start, tmp.path(), overrides)?;
        let policy = setup.parse_policy()?;
        let sandbox = saddle_sandbox::availability();

        let mut audit = AuditLog::create(&setup.paths.audit_dir(), &run)?;
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
        let fresh = Setup::load(&self.setup.start, self._tmp.path(), &self.setup.overrides)?;
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

    /// Closes the audit log. An allowance is *not* dropped here: it outlives the session so it can
    /// be picked again later, and is removed with `saddle session revoke` (D61).
    pub fn close(self) -> Result<String> {
        Ok(self.audit.close(&self.setup.paths.sessions_index())?)
    }
}

/// A session id: 16 hex characters from the kernel's random source (D61).
///
/// It used to be a timestamp and a pid, which was fine while an id only named an audit log. Now an
/// allowance is a permission held by whoever knows the id, so it has to be unguessable: a
/// time-and-pid id can be narrowed to a few thousand candidates from the clock alone.
pub fn session_id() -> String {
    let mut bytes = [0u8; 8];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !ok {
        // No random source: fall back to the clock rather than refusing to start, and say so in
        // the id so an id of this shape is visibly weaker.
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("time{ms:x}");
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
