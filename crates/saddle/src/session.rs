//! A session: resolved roots, the policy for this workspace, the engine, and an open audit log.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use saddle_cap::policy::{NetGrant, POLICY_FILE};
use saddle_cap::sandbox::SandboxExtras;
use saddle_cap::{Location, Policy, Principal, RootName, Roots, Scope, names_host};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{AuditLog, Decision, Engine, Kind, Request, net_decision};
use serde_json::json;

use crate::config::UserConfig;
use crate::mcp::Transport;
use crate::paths::{Paths, find_workspace};
use crate::store::Snapshot;

/// The header a workspace's own grant file starts with. Empty of grants on purpose: its presence
/// and an empty file mean the same thing, and an empty file is an invitation to write a key in it.
pub fn workspace_header(workspace: &Path) -> String {
    format!(
        "workspace = {}\n{}",
        toml::Value::String(workspace.display().to_string()),
        WORKSPACE_POLICY_HEADER
    )
}

pub const WORKSPACE_POLICY_HEADER: &str = r#"# This workspace's own grants, laid over the [agent] table in config.toml.
#
# The `workspace` line above is the directory this file governs. The name is only a readable
# encoding of that path and is allowed to be approximate; this line is what counts, and a session
# refuses the file if it names a different directory.
#
# Anything not written here is taken from the base, so a change there reaches this workspace. What
# is written here replaces that grant for this scope, and it may grant more or less than the base
# does: this file is the authority for this directory. It lives in the config directory, which no
# root contains, so nothing the agent runs can write it -- only you can. `saddle check` prints the
# result, and --grant/--revoke change it for one run without touching this file.
"#;

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
    /// This workspace's own grant file, which the TUI editor writes (D45, D63).
    pub workspace_policy_path: PathBuf,
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
    /// Directories the file tools may reach outside every root, and the tools allowed on each
    /// (D66). From `[sandbox]` in the user's config only, so no workspace policy can add one.
    pub tool_paths: Vec<crate::config::ToolPath>,
    /// Whether and how many skills to look for (D64), read with the rest of the config.
    pub skills: crate::config::SkillsConfig,
    /// The instruction-file settings, kept so `/reload` re-reads them with the ones that were used
    /// at open (D70).
    pub context: crate::config::ContextConfig,
    /// The sensitive directories this workspace puts in reach, which is what the person is asked
    /// about before the session opens (D60). Empty for an ordinary project.
    pub exposed: Vec<String>,
    /// The MCP servers this config declares, and which of them a grant enables (D104). `check`
    /// prints these lines, so a server that was declared but never granted is visible rather than
    /// silently idle.
    pub mcp: Vec<String>,
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
        let (extras, tool_paths) = user.sandbox_extras(&roots, &paths.protected())?;

        // `[agent]` in config.toml is the base; a separate file per workspace overrides only the
        // scopes it names (D63). Both live outside every root, protected by the kernel (D42).
        let config_path = paths.user_config();
        let mut notes = Vec::new();
        let user = UserConfig::load(&config_path)?;
        let skills_cfg = user.skills.clone();
        let context_cfg = user.context.clone();
        // This workspace's own grants are their own file (D63), so a directory's authority is
        // named after the directory and the global file does not grow a key per workspace.
        let own_path = paths.workspace_policy(&workspace);
        let own_text = match std::fs::read_to_string(&own_path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(dir) = own_path.parent() {
                    std::fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                }
                std::fs::write(&own_path, workspace_header(&workspace))
                    .with_context(|| format!("creating {}", own_path.display()))?;
                notes.push(format!(
                    "created {} for this workspace's own grants; it adds to {}",
                    own_path.display(),
                    config_path.display()
                ));
                workspace_header(&workspace)
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", own_path.display())),
        };
        // A header with no grants is the same as no file: nothing is overridden.
        let mut own: Option<toml::Value> = {
            let doc: toml::Table = toml::from_str(&own_text)
                .with_context(|| format!("reading {}", own_path.display()))?;
            // The file says which directory it governs. The name is a readable encoding of the
            // path, and it is lossy (`a-b` and `a/b` collide), so the name may not be trusted for
            // this: applying one directory's grants to another is how a `net *` written for one
            // project would silently cover a second (D63).
            if let Some(claimed) = doc.get("workspace").and_then(|w| w.as_str())
                && Path::new(claimed) != workspace
            {
                anyhow::bail!(
                    "{} is for {}, not {}: two directories cannot share one grants file, so move \
                     or delete it and start again",
                    own_path.display(),
                    claimed,
                    workspace.display()
                );
            }
            doc.get("agent").cloned().map(|agent| {
                let mut out = toml::Table::new();
                out.insert("version".into(), toml::Value::Integer(1));
                out.insert("agent".into(), agent);
                toml::Value::Table(out)
            })
        };
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
                if own.is_some() {
                    format!(" and {}", own_path.display())
                } else {
                    String::new()
                }
            ));
        }
        let policy_path = config_path;
        let workspace_policy_path = own_path;
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
        // D104: what config declares, against what the grants enable. A granted server that no
        // table declares is the mistake that would otherwise show up as a server that never
        // starts, so it is named here.
        let agent_grants = merged.grants(&Principal::Agent);
        let mut mcp = Vec::new();
        for (name, entry) in &user.mcp {
            let what = match entry.transport() {
                Ok(Transport::Stdio { command, .. }) => format!("stdio {command}"),
                Ok(Transport::Http { url }) => format!("http {url}"),
                Err(_) => "no transport".to_owned(),
            };
            let access = agent_grants
                .and_then(|g| g.mcp_server(name))
                .map(|g| format!("granted [{}]", g.tools.join(", ")))
                .unwrap_or_else(|| "not granted".to_owned());
            let deadline = entry.timeout().as_secs();
            mcp.push(format!("{name} · {what} · {access} · {deadline}s per call"));
        }
        if let Some(grants) = agent_grants {
            for grant in &grants.mcp {
                if !user.mcp.contains_key(&grant.server) {
                    mcp.push(format!(
                        "{} · granted, but no [mcp.{}] table in config.toml",
                        grant.server, grant.server
                    ));
                }
            }
        }
        // The hash names the grants, not the file: a provider change is not a policy change.
        let policy_sha256 = sha256_hex(format!("{global_text}\n---\n{policy_text}").as_bytes());
        Ok(Self {
            paths,
            start: start.clone(),
            workspace,
            roots,
            extras,
            policy_path,
            workspace_policy_path,
            policy_text,
            policy_sha256,
            merged: merged_lines,
            global_sha256: sha256_hex(global_text.as_bytes()),
            global_text,
            notes,
            exposed,
            overrides: overrides.clone(),
            tool_paths,
            mcp,
            skills: skills_cfg,
            context: context_cfg,
        })
    }

    pub fn parse_policy(&self) -> Result<Policy> {
        Policy::layer(&self.global_text, &self.policy_text, &self.roots)
            .with_context(|| format!("{}", self.policy_path.display()))
    }
}

/// Where the provider named in the config answers a web search, if it serves one (D82).
///
/// A provider that cannot be resolved simply has no search, because a broken provider table is
/// reported by `check` and by the model loop, not by a tool that was asked a different question.
fn configured_search(setup: &Setup) -> Option<crate::provider::SearchTarget> {
    let user = UserConfig::load(&setup.paths.user_config()).ok()?;
    let name = user.provider?;
    let config = crate::provider::endpoint(&name, &user.providers).ok()?;
    let url = config.search_url?;
    Some(crate::provider::SearchTarget {
        provider: name,
        url,
        key_env: config.api_key_env,
    })
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

/// A `[sandbox]` directory the file tools may reach, with an internal name so a `Location` works
/// unchanged (D66). The name is never shown: `check`, the prompt and the audit use the real path.
pub struct ToolRoot {
    pub name: RootName,
    pub dir: PathBuf,
    pub tools: Vec<String>,
    pub written: String,
}

pub struct Session {
    pub setup: Setup,
    pub engine: Engine,
    pub audit: AuditLog,
    /// `Ok(landlock ABI)` or the reason exec is unavailable.
    pub sandbox: Result<i32, String>,
    pub host_env: Vec<(String, String)>,
    /// Skills advertised to the model and loaded on demand (D64), read once at open like the
    /// instruction files.
    pub skills: Vec<crate::skills::Skill>,
    /// Notes for the person about a skill that was refused, shadowed or ignored.
    pub skill_notes: Vec<String>,
    /// The `[sandbox]` directories the file tools may reach (D66).
    pub tool_roots: Vec<ToolRoot>,
    /// Where the configured provider answers a web search, when it serves one (D82). Set again
    /// whenever the provider changes, so the tool follows the model.
    pub search: Option<crate::provider::SearchTarget>,
    /// The results of the last search, so `{"result": n}` can show one in full without asking
    /// again. Held here rather than in the tool because a result's number only means the page the
    /// model was shown, and that is a property of the session (D82).
    pub last_search: Option<crate::tools::websearch::LastSearch>,
    /// The MCP servers this policy grants, and the tools of theirs the model has declared (D108).
    pub mcp: crate::mcp::registry::Registry,
    /// A host-built, branch-only snapshot, supplied by the agent for one query (D92, D94).
    pub query_snapshot: Option<Snapshot>,
    pub query_conversation: Option<String>,
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

        let search = configured_search(&setup);
        let host_env: Vec<(String, String)> = std::env::vars().collect();
        // D108: the servers this policy grants, and the tables that declare how to start them. A
        // granted server with no table is not an error here: `saddle check` says so, and the first
        // search reports it once.
        let mcp = {
            let user = UserConfig::load(&setup.paths.user_config()).unwrap_or_default();
            let grants = policy
                .grants(&Principal::Agent)
                .map(|g| g.mcp.clone())
                .unwrap_or_default();
            let tmp = setup.roots.get(&RootName::tmp()).map(Path::to_path_buf);
            crate::mcp::registry::Registry::new(grants, user.mcp, host_env.clone(), tmp)
        };
        let mut session = Self {
            setup,
            engine: Engine::new(policy),
            audit,
            sandbox,
            host_env,
            skills: Vec::new(),
            skill_notes: Vec::new(),
            tool_roots: Vec::new(),
            // The configured default provider, which is what `saddle tool` has. A run that resolved
            // a provider (`-p`, or one picked with /login) replaces this with that one's (D82).
            search,
            last_search: None,
            mcp,
            query_snapshot: None,
            query_conversation: None,
            _tmp: tmp,
        };
        // An internal name per tool-visible directory, skipping any name a root already uses, so a
        // `Location` addresses it without the name meaning anything to a policy (D66).
        let mut used: Vec<RootName> = session.setup.roots.iter().map(|(n, _)| n.clone()).collect();
        for path in session.setup.tool_paths.clone() {
            let mut n = 1;
            let name = loop {
                let candidate = match RootName::new(&format!("sandbox-{n}")) {
                    Ok(name) => name,
                    Err(_) => break None,
                };
                if !used.contains(&candidate) {
                    break Some(candidate);
                }
                n += 1;
            };
            let Some(name) = name else {
                continue;
            };
            used.push(name.clone());
            session.tool_roots.push(ToolRoot {
                name,
                dir: path.dir,
                tools: path.tools,
                written: path.written,
            });
        }
        // Discovered before the first request so the prompt can advertise them, and once only: a
        // skill is read at startup as an instruction file is, so a grants reload does not change
        // what the conversation was built from.
        let cfg = session.setup.skills.clone();
        let (found, notes) = crate::skills::discover(&mut session, &cfg)?;
        session.skills = found;
        session.skill_notes = notes;
        Ok(session)
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

    /// Decides one request an MCP server's HTTP transport makes (D110). It is the server's own
    /// narrowed reach that answers, not the agent's whole list, so a server whose grant names one
    /// host cannot reach another the agent may — and the record says which server it was for,
    /// because an HTTP transport has no child process to point at.
    pub fn decide_mcp_net(
        &mut self,
        server: &str,
        reach: &[NetGrant],
        req: &Request<'_>,
    ) -> Result<Decision> {
        let decision = net_decision(reach, req, &format!("`{server}`"));
        let mut args = req.args();
        args["via"] = serde_json::json!("mcp");
        args["server"] = serde_json::json!(server);
        self.audit
            .event(
                Kind::Decision,
                Some(&Principal::Agent),
                Some(Scope::Net),
                Some(decision.allow),
                args,
                decision.explanation.clone(),
            )
            .context("writing the audit log")?;
        Ok(decision)
    }

    /// Whether a server's reach spells this host out, which is what reaches loopback (D40, D110).
    pub fn mcp_names_host(reach: &[NetGrant], host: &str) -> bool {
        names_host(reach, host)
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
        tool: &str,
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
        // D66: a `[sandbox]` directory that names tools. No grant covers it, so the config entry is
        // the authority; the tool name is what it lists, which is why the name is passed in.
        if let Some(root) = self.tool_root_for(&abs) {
            if root.tools.iter().any(|t| t == tool) {
                let rel = match saddle_cap::RelPath::new(
                    abs.strip_prefix(&root.dir).unwrap_or(Path::new("")),
                ) {
                    Ok(rel) => rel,
                    Err(e) => return Ok(Err(format!("{raw}: {e}"))),
                };
                let loc = Location::new(root.name.clone(), rel);
                let explanation = format!(
                    "`{tool}` on {} is allowed by config.toml [sandbox]",
                    root.written
                );
                self.audit
                    .event(
                        Kind::Decision,
                        Some(who),
                        Some(scope),
                        Some(true),
                        json!({ "path": abs.display().to_string() }),
                        &explanation,
                    )
                    .context("writing the audit log")?;
                return Ok(Ok(loc));
            }
            let explanation = format!(
                "`{tool}` is not one of the tools config.toml allows on {} ({})",
                root.written,
                root.tools.join(", ")
            );
            self.audit
                .event(
                    Kind::Decision,
                    Some(who),
                    Some(scope),
                    Some(false),
                    json!({ "path": abs.display().to_string() }),
                    &explanation,
                )
                .context("writing the audit log")?;
            return Ok(Err(explanation));
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

    /// The tool-visible directory containing `abs`, innermost first (D66).
    fn tool_root_for(&self, abs: &Path) -> Option<&ToolRoot> {
        let abs = saddle_cap::roots::normalize(abs);
        self.tool_roots
            .iter()
            .filter(|r| abs.starts_with(&r.dir))
            .max_by_key(|r| r.dir.components().count())
    }

    /// A tool-visible location with its symlinks followed (D66).
    ///
    /// A skill is installed by linking it in from wherever it lives, so a link that leaves the
    /// directory is the normal arrangement. The target must be inside *some* declared tool path,
    /// which keeps the rule honest: a link may point at another directory the user listed, and
    /// never at something undeclared — otherwise `~/.agents/x -> ~/.config/saddle/config.toml`
    /// would hand the tools the policy that governs them (D16).
    pub fn resolve_tool(&self, loc: &Location) -> std::io::Result<Location> {
        use std::io::{Error, ErrorKind};
        let base = self
            .base_of(&loc.root)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no root `{}`", loc.root)))?;
        let base = base.canonicalize()?;
        let abs = base.join(loc.rel.to_path());
        let real = match abs.canonicalize() {
            Ok(real) => real,
            // A file that does not exist yet is resolved through its parent, so a write beneath a
            // linked directory works too.
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let Some(parent) = abs.parent() else {
                    return Err(e);
                };
                match (parent.canonicalize(), abs.file_name()) {
                    (Ok(parent), Some(name)) => parent.join(name),
                    _ => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        if let Some(root) = self.tool_root_for(&real) {
            let rel = real.strip_prefix(&root.dir).unwrap_or(Path::new(""));
            let rel = saddle_cap::RelPath::new(rel)
                .map_err(|e| Error::new(ErrorKind::InvalidInput, e))?;
            return Ok(Location::new(root.name.clone(), rel));
        }
        Err(Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "resolves to {}, which is not inside any directory a [sandbox] entry names for \
                 the tools; add that directory as an entry to allow it",
                real.display()
            ),
        ))
    }

    /// The directory a `Location`'s root stands for: a root, or a `[sandbox]` directory (D66).
    pub fn base_of(&self, root: &RootName) -> Option<PathBuf> {
        if let Some(path) = self.roots().get(root) {
            return Some(path.to_path_buf());
        }
        self.tool_roots
            .iter()
            .find(|r| &r.name == root)
            .map(|r| r.dir.clone())
    }

    /// Whether a location is a `[sandbox]` directory rather than a root, for the gate (D66).
    pub fn is_tool_root(&self, root: &RootName) -> bool {
        self.tool_roots.iter().any(|r| &r.name == root)
    }

    /// Runs discovery again, so a skill added or edited during the session is advertised (D70).
    ///
    /// The same call as at open, so the rules do not change: each candidate is read through the gate
    /// and every read is recorded. What it does not do is touch the conversation — a body already
    /// loaded stays where it was put, which is why the note says so.
    pub fn reload_skills(&mut self) -> Result<()> {
        let cfg = self.setup.skills.clone();
        let (found, notes) = crate::skills::discover(self, &cfg)?;
        self.skills = found;
        self.skill_notes = notes;
        Ok(())
    }

    /// Records that a skill's instructions entered the conversation (D69). Discovery already
    /// recorded the read and its hash; this is the other half — that the body was put in front of
    /// the model, and by which name.
    pub fn record_skill_load(&mut self, name: &str) -> Result<()> {
        let skill = self.skills.iter().find(|s| s.name == name);
        let args = match skill {
            Some(skill) => json!({
                "loaded": {
                    "name": skill.name,
                    "file": skill.label,
                    "bytes": skill.text.len(),
                    "sha256": sha256_hex(skill.text.as_bytes()),
                }
            }),
            // A prompt that says it came from a skill the session does not have is worth a record
            // of its own rather than a silent pass.
            None => json!({ "loaded": { "name": name, "missing": true } }),
        };
        self.audit
            .event(
                Kind::Skill,
                Some(&Principal::Agent),
                None,
                None,
                args,
                format!("skill `{name}` loaded into the conversation"),
            )
            .context("writing the audit log")
    }

    /// Closes the audit log. An allowance is *not* dropped here: it outlives the session so it can
    /// be picked again later, and is removed with `saddle session revoke` (D61).
    pub fn close(mut self) -> Result<String> {
        self.mcp.close();
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
