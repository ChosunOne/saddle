use crate::config;
use crate::config::ContextConfig;
use crate::config::Overrides;
use crate::config::SkillsConfig;
use crate::config::ToolPath;
use crate::mcp::Exposure;
use crate::mcp::registry::Registry;
use crate::paths;
use crate::provider;
use crate::provider::SearchTarget;
use crate::search_transport;
use crate::search_transport::CachedSearch;
use crate::skills;
use crate::skills::Skill;
use crate::standard::Profiles;
use saddle_cap::policy;
use saddle_cap::roots;
use saddle_limits::Budgets;
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::fs::DirBuilder;
use std::fs::File;
use std::io::Read;
use std::io::Result as IoResult;
use std::io::{Error, ErrorKind};
use std::mem;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use cap_std::fs::Dir;
use saddle_cap::policy::{NetGrant, POLICY_FILE, TOOL_READ_ROLES, TOOL_WRITE_ROLES, ToolReadGrant};
use saddle_cap::sandbox::SandboxExtras;
use saddle_cap::{Location, PluginId, Policy, Principal, RootName, Roots, Scope, names_host};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{AuditLog, Decision, Engine, Kind, Request, net_decision};
use serde_json::{Value, json};

use crate::auth::AuthStore;
use crate::config::UserConfig;
use crate::config::launch_profiles::{
    self, Baselines as LaunchBaselines, Resolved as LaunchProfile,
};
use crate::config::sandbox_overrides::{self, Source as SandboxSource};
use crate::fs_access;
use crate::mcp::Transport;
use crate::paths::{Paths, find_workspace};
use crate::plugins::hooks::Queue as EventQueue;
use crate::plugins::services::State as ExtensionState;
use crate::plugins::{Entry as PluginEntry, HostFailure, Registry as PluginRegistry};
use crate::provider::ToolDef;
use crate::questions::Questioner;
use crate::store::{Snapshot, Store};
use crate::tasks::{self, Tasks};
use crate::team::Manager;

pub fn workspace_header(workspace: &Path) -> String {
    format!(
        "workspace = {}\n{}",
        toml::Value::String(workspace.display().to_string()),
        WORKSPACE_POLICY_HEADER
    )
}

pub const WORKSPACE_POLICY_HEADER: &str = r#"# This workspace's overrides, laid over config.toml (grants and sandbox paths).
#
# The `workspace` line above is the directory this file governs. The name is only a readable
# encoding of that path and is allowed to be approximate; this line is what counts, and a session
# refuses the file if it names a different directory.
#
# Anything not written here is taken from the base, so a change there reaches this workspace. What
# is written here replaces that grant for this scope, and it may grant more or less than the base
# does: this file is the authority for this directory. It lives in the config directory, which no
# root contains, so nothing the agent runs can write it -- only you can. `saddle check` prints the
# result, and --grant/--revoke change grants for one run without touching this file.
# [sandbox] read/write lists inherit when omitted, replace when plain, and modify the global
# list with +path/-path entries. These overrides apply to every session in this workspace.
"#;

pub fn profile_report(setup: &Setup) -> String {
    let mut text = format!(
        "profile: {}",
        setup
            .profile
            .as_deref()
            .unwrap_or("none ([agent] as written)")
    );
    if setup.profiles.is_empty() {
        text.push_str(
            "\nconfig.toml defines no [profiles]; a profile is a table of the grants [agent] takes",
        );
        return text;
    }
    for name in &setup.profiles {
        let mark = if setup.profile.as_deref() == Some(name.as_str()) {
            "   in force"
        } else {
            ""
        };
        text.push_str(&format!("\n  {name}{mark}"));
    }
    text.push_str(
        "\n`/profile <name>` switches this session; `none` is [agent] as written, and a profile is\
         \nlayered on it, with this workspace's own grants on top (D176)",
    );
    text
}

pub fn roots_for_validation() -> Result<Roots> {
    let paths = Paths::from_env()?;
    let user = UserConfig::load(&paths.user_config())?;
    let cwd = env::current_dir().context("reading the working directory")?;
    let workspace = find_workspace(&cwd);
    let mut entries = vec![
        (RootName::workspace(), workspace),
        (RootName::tmp(), env::temp_dir()),
    ];
    entries.extend(user.root_entries()?);
    Ok(Roots::resolve(entries, &paths.protected())?)
}

pub struct Setup {
    pub sandbox_override: Option<SandboxSource>,
    pub launch_profiles: BTreeMap<String, LaunchProfile>,
    pub granted_launch_profiles: BTreeSet<String>,
    pub standard_tools: Profiles,
    pub tool_limits: Budgets,
    pub plugins: BTreeMap<PluginId, PluginEntry>,
    pub paths: Paths,
    pub workspace: PathBuf,
    pub start: PathBuf,
    pub roots: Roots,
    pub extras: SandboxExtras,
    pub policy_path: PathBuf,
    pub policy_text: String,
    pub workspace_policy_path: PathBuf,
    pub policy_sha256: String,
    pub merged: Vec<String>,
    pub global_sha256: String,
    pub global_text: String,
    pub profile: Option<String>,
    pub profiles: Vec<String>,
    pub notes: Vec<String>,
    pub overrides: Overrides,
    pub tool_paths: Vec<ToolPath>,
    pub skills: SkillsConfig,
    pub context: ContextConfig,
    pub exposed: Vec<String>,
    pub mcp: Vec<String>,
}

impl Setup {
    pub fn load(start: &Path, tmp: &Path, overrides: &Overrides) -> Result<Self> {
        let paths = Paths::from_env()?;
        let start = start
            .canonicalize()
            .with_context(|| format!("{}", start.display()))?;
        let workspace = find_workspace(&start);
        let mut user = UserConfig::load(&paths.user_config())?;
        let profile = user.profile_in_force(overrides.profile.as_deref())?;
        let profiles: Vec<String> = user.profiles.keys().cloned().collect();
        let exposed = paths::exposed(&workspace, &paths::sensitive_dirs());

        let mut entries = vec![
            (RootName::workspace(), workspace.clone()),
            (RootName::tmp(), tmp.to_path_buf()),
        ];
        entries.extend(user.root_entries()?);
        let roots = Roots::resolve(entries, &paths.protected())?;
        let config_path = paths.user_config();
        let mut notes = Vec::new();
        let skills_cfg = user.skills.clone();
        let context_cfg = user.context.clone();
        let own_path = paths.workspace_policy(&workspace);
        let own_text = match sandbox_overrides::read(&paths, &own_path)
            .with_context(|| format!("reading {}", own_path.display()))?
        {
            Some(text) => text,
            None => {
                if let Some(dir) = own_path.parent() {
                    fs::create_dir_all(dir)
                        .with_context(|| format!("creating {}", dir.display()))?;
                }
                fs::write(&own_path, workspace_header(&workspace))
                    .with_context(|| format!("creating {}", own_path.display()))?;
                notes.push(format!(
                    "created {} for this workspace's own grants; it adds to {}",
                    own_path.display(),
                    config_path.display()
                ));
                workspace_header(&workspace)
            }
        };
        let sandbox_override;
        let launch_profiles;
        let mut own: Option<toml::Value> = {
            let doc: toml::Table = toml::from_str(&own_text)
                .with_context(|| format!("reading {}", own_path.display()))?;
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
            sandbox_override = Some(sandbox_overrides::apply(
                &mut user, &paths, &roots, &workspace, &own_path, &own_text, &doc,
            )?);
            if (doc.contains_key("launch-profiles")
                || doc
                    .get("agent")
                    .and_then(|agent| agent.get("run-binary"))
                    .is_some()
                || doc
                    .get("plugin")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|plugins| {
                        plugins
                            .values()
                            .any(|plugin| plugin.get("run-binary").is_some())
                    }))
                && !doc
                    .get("workspace")
                    .and_then(toml::Value::as_str)
                    .is_some_and(|claimed| Some(claimed) == workspace.to_str())
            {
                bail!("launch profile overrides require a matching canonical workspace line");
            }
            let launch_overlay = match doc.get("launch-profiles") {
                Some(value) => value.clone().try_into().with_context(|| {
                    format!("invalid launch profiles in {}", own_path.display())
                })?,
                None => BTreeMap::new(),
            };
            launch_profiles = launch_profiles::resolve(
                mem::take(&mut user.launch_profiles),
                launch_overlay,
                &paths.protected(),
            )
            .with_context(|| format!("resolving launch profiles for {}", workspace.display()))?;
            (doc.contains_key("agent") || doc.contains_key("plugin")).then(|| {
                let mut out = toml::Table::new();
                out.insert("version".into(), toml::Value::Integer(1));
                for key in ["agent", "plugin"] {
                    if let Some(value) = doc.get(key) {
                        out.insert(key.into(), value.clone());
                    }
                }
                toml::Value::Table(out)
            })
        };
        let (extras, tool_paths) = user.sandbox_extras(&roots, &paths.protected())?;
        let global = user.global_policy(profile.as_deref())?;
        if !overrides.is_empty() {
            let merged_doc = policy::merge(
                global.clone(),
                own.unwrap_or_else(|| config::policy_document_pub(None)),
            )
            .with_context(|| format!("reading the grants in {}", own_path.display()))?;
            let mut agent = match merged_doc.get("agent") {
                Some(toml::Value::Table(t)) => t.clone(),
                _ => toml::Table::new(),
            };
            overrides.apply(&mut agent)?;
            let mut doc = merged_doc;
            if let toml::Value::Table(table) = &mut doc {
                table.insert("agent".into(), toml::Value::Table(agent));
            }
            own = Some(doc);
            notes.push(format!(
                "grants changed for this session only: {}",
                overrides.describe()
            ));
        }
        let global_text = toml::to_string(&global).context("reading the grants")?;
        let policy_text = match &own {
            Some(doc) => toml::to_string(doc).context("reading this workspace's grants")?,
            None => String::new(),
        };
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
        let global_text = if overrides.is_empty() {
            global_text
        } else {
            String::new()
        };
        let merged = Policy::layer(&global_text, &policy_text, &roots)
            .with_context(|| format!("reading the grants in {}", policy_path.display()))?;
        let merged_lines = merged.describe();
        let agent_grants = merged.grants(&Principal::Agent);
        let mut granted_launch_profiles = BTreeSet::new();
        if let Some(grants) = agent_grants {
            for grant in &grants.run_binary {
                if !launch_profiles.contains_key(&grant.profile) {
                    bail!(
                        "{} names undefined launch profile {:?}",
                        grant.rule,
                        grant.profile
                    );
                }
                granted_launch_profiles.insert(grant.profile.clone());
            }
        }
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
            let per_tool = if entry.tool_exposure.is_empty() {
                String::new()
            } else {
                format!(" · {} tool override(s)", entry.tool_exposure.len())
            };
            let exposure = match entry.exposure() {
                Exposure::Deferred => "searched for",
                Exposure::Direct => "in the prompt",
                Exposure::Hidden => "off",
            };
            mcp.push(format!(
                "{name} · {what} · {access} · {exposure}{per_tool} · {deadline}s per call"
            ));
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
        let policy_sha256 = sha256_hex(
            format!(
                "{global_text}\n---\n{policy_text}\nstandard-tools\n{}",
                toml::to_string(&user.standard_tools)?
            )
            .as_bytes(),
        );
        Ok(Self {
            sandbox_override,
            launch_profiles,
            granted_launch_profiles,
            standard_tools: user.standard_tools,
            tool_limits: user.tool_limits,
            plugins: user.plugins,
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
            profile,
            profiles,
            notes,
            exposed,
            overrides: overrides.clone(),
            tool_paths,
            mcp,
            skills: skills_cfg,
            context: context_cfg,
        })
    }

    pub fn sandbox_audit(&self) -> Value {
        json!({"read":self.extras.read,"write":self.extras.write,"tool_paths":self.tool_paths.iter().map(|entry|json!({"path":entry.dir,"tools":entry.tools})).collect::<Vec<_>>(),"override":self.sandbox_override.as_ref().map(|source|json!({"path":source.path,"sha256":source.sha256}))})
    }

    pub fn parse_policy(&self) -> Result<Policy> {
        let mut policy = Policy::layer(&self.global_text, &self.policy_text, &self.roots)
            .with_context(|| format!("{}", self.policy_path.display()))?;
        let agent = policy.principals.entry(Principal::Agent).or_default();
        for (scope, allowed, output) in [
            (Scope::ToolRead, TOOL_READ_ROLES, &mut agent.tool_read),
            (Scope::ToolWrite, TOOL_WRITE_ROLES, &mut agent.tool_write),
        ] {
            let mut roles: Vec<String> = self
                .tool_paths
                .iter()
                .flat_map(|p| p.tools.iter())
                .filter(|role| allowed.contains(&role.as_str()))
                .cloned()
                .collect();
            roles.sort();
            roles.dedup();
            if self.overrides.revoke.iter().any(|s| s == scope.as_str()) {
                roles.clear();
            }
            if let Some(ceiling) = output {
                roles.retain(|role| ceiling.tools.contains(role));
            }
            *output = (!roles.is_empty()).then(|| {
                if scope == Scope::ToolRead {
                    ToolReadGrant::for_agent(roles)
                } else {
                    ToolReadGrant::for_agent_write(roles)
                }
            });
        }
        Ok(policy)
    }
}

fn resolve_missing_tool_path(path: &Path, depth: usize) -> IoResult<PathBuf> {
    if depth > 128 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "tool path resolution exceeds depth limit",
        ));
    }
    match path.canonicalize() {
        Ok(real) => Ok(real),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "tool path has no parent"))?;
            if let Ok(target) = fs::read_link(path) {
                return resolve_missing_tool_path(
                    &if target.is_absolute() {
                        target
                    } else {
                        parent.join(target)
                    },
                    depth + 1,
                );
            }
            let real_parent = resolve_missing_tool_path(parent, depth + 1)?;
            let name = path
                .file_name()
                .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "tool path has no file name"))?;
            Ok(real_parent.join(name))
        }
        Err(error) => Err(error),
    }
}

fn configured_search(setup: &Setup) -> Option<SearchTarget> {
    let user = UserConfig::load(&setup.paths.user_config()).ok()?;
    let name = user.provider?;
    let config = provider::endpoint(&name, &user.providers).ok()?;
    let url = config.search_url?;
    Some(SearchTarget {
        provider: name,
        url,
        key_env: config.api_key_env,
    })
}

pub struct PrivateTmp(PathBuf);

impl PrivateTmp {
    pub fn create(id: &str) -> Result<Self> {
        let path = env::temp_dir().join(format!("saddle-{id}"));
        DirBuilder::new()
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
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub struct ToolRoot {
    pub name: RootName,
    pub dir: PathBuf,
    pub tools: Vec<String>,
    pub written: String,
    pub handle: Option<Dir>,
    pub protected: bool,
}

fn configured_tool_roots(setup: &Setup) -> Vec<ToolRoot> {
    let mut used: Vec<RootName> = setup.roots.iter().map(|(name, _)| name.clone()).collect();
    let protected: Vec<PathBuf> = setup
        .paths
        .protected()
        .iter()
        .flat_map(|path| {
            let lexical = roots::normalize(path);
            [lexical.clone(), lexical.canonicalize().unwrap_or(lexical)]
        })
        .collect();
    let mut roots = Vec::new();
    for path in &setup.tool_paths {
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
        let base = roots::normalize(&path.dir);
        let actual = base.canonicalize().unwrap_or_else(|_| base.clone());
        roots.push(ToolRoot {
            name,
            dir: path.dir.clone(),
            tools: path.tools.clone(),
            written: path.written.clone(),
            handle: fs_access::root(&path.dir).ok(),
            protected: protected.iter().any(|secret| {
                [&base, &actual]
                    .iter()
                    .any(|base| base.starts_with(secret) || secret.starts_with(base))
            }),
        });
    }
    roots
}

pub fn provider_for(paths: &Paths, name: &str, model: Option<&str>) -> Result<provider::Provider> {
    let user = UserConfig::load(&paths.user_config())?;
    let auth = AuthStore::load(&paths.auth_store())?;
    let reported = provider::Reported::stored(&paths.reported_windows());
    let config = provider::resolve(name, &user.providers, model)?;
    Ok(
        provider::Provider::new(name, config, auth.key(name), &reported)
            .with_stored_oauth(&paths.auth_store()),
    )
}

pub struct Session {
    pub extensions: ExtensionState,
    pub plugin_events: EventQueue,
    pub tasks: Tasks,
    pub file_roots: BTreeMap<RootName, Dir>,
    pub plugins: PluginRegistry,
    pub setup: Setup,
    pub launch_baselines: LaunchBaselines,
    pub engine: Engine,
    pub audit: AuditLog,
    pub sandbox: Result<i32, String>,
    pub host_env: Vec<(String, String)>,
    pub skills: Vec<Skill>,
    pub skill_notes: Vec<String>,
    pub tool_roots: Vec<ToolRoot>,
    pub search: Option<SearchTarget>,
    pub search_snapshots: BTreeMap<Principal, CachedSearch>,
    pub mcp: Registry,
    pub query_snapshot: Option<Snapshot>,
    pub query_conversation: Option<String>,
    transcripts: Option<Store>,
    _tmp: PrivateTmp,
    pub team: Manager,
    pub team_label: String,
    pub cancel: Arc<AtomicBool>,
    pub questions: Option<Box<dyn Questioner>>,
}

impl Session {
    pub fn open_with(start: &Path, id: Option<&str>, overrides: &Overrides) -> Result<Self> {
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
                "sandbox-config": setup.sandbox_audit(),
                "sandbox": match &sandbox { Ok(abi) => json!({ "landlock-abi": abi }), Err(e) => json!({ "unavailable": e }) },
            }),
            "session opened",
        )?;

        let search = configured_search(&setup);
        let file_roots = setup
            .roots
            .iter()
            .map(|(name, path)| Ok((name.clone(), fs_access::root(path)?)))
            .collect::<Result<_>>()?;
        let plugins = PluginRegistry::new(setup.plugins.clone());
        let user = UserConfig::load(&setup.paths.user_config()).unwrap_or_default();
        let secrets = provider::secret_env_names(&user.providers);
        let host_env: Vec<(String, String)> = env::vars()
            .filter(|(name, _)| !secrets.contains(name))
            .collect();
        let mcp = {
            let grants = policy
                .grants(&Principal::Agent)
                .map(|g| g.mcp.clone())
                .unwrap_or_default();
            let tmp = setup.roots.get(&RootName::tmp()).map(Path::to_path_buf);
            Registry::new(grants, user.mcp, host_env.clone(), tmp)
        };
        let mut launch_baselines = LaunchBaselines::default();
        launch_baselines.enroll(&setup.launch_profiles);
        let mut session = Self {
            extensions: Default::default(),
            plugin_events: Default::default(),
            tasks: Tasks::default(),
            file_roots,
            plugins,
            setup,
            launch_baselines,
            engine: Engine::new(policy),
            audit,
            sandbox,
            host_env,
            skills: Vec::new(),
            skill_notes: Vec::new(),
            tool_roots: Vec::new(),
            search,
            search_snapshots: BTreeMap::new(),
            mcp,
            query_snapshot: None,
            query_conversation: None,
            transcripts: None,
            _tmp: tmp,
            team: Default::default(),
            team_label: "lead".into(),
            cancel: Arc::new(AtomicBool::new(false)),
            questions: None,
        };
        session.tool_roots = configured_tool_roots(&session.setup);
        let cfg = session.setup.skills.clone();
        let (found, notes) = skills::discover(&mut session, &cfg)?;
        session.skills = found;
        session.skill_notes = notes;
        let mut plugins = mem::take(&mut session.plugins);
        let loaded = plugins.load(&mut session);
        session.plugins = plugins;
        loaded?;
        Ok(session)
    }

    pub(crate) fn tmp_dir(&self) -> &Path {
        self._tmp.path()
    }

    pub(crate) fn derive(&self, id: &str) -> Result<Self> {
        let run = format!("{id}-{}", session_id());
        let tmp = PrivateTmp::create(&run)?;
        let setup = Setup::load(&self.setup.start, tmp.path(), &self.setup.overrides)?;
        let policy = setup.parse_policy()?;
        let mut audit = AuditLog::create(&setup.paths.audit_dir(), &run)?;
        audit.event(
            Kind::SessionStart,
            None,
            None,
            None,
            json!({"workspace": setup.workspace, "worker": id, "parent": self.audit.session()}),
            "worker session opened",
        )?;
        let file_roots = setup
            .roots
            .iter()
            .map(|(name, path)| Ok((name.clone(), fs_access::root(path)?)))
            .collect::<Result<_>>()?;
        let mut session = Self {
            extensions: Default::default(),
            plugin_events: Default::default(),
            tasks: Tasks::default(),
            file_roots,
            plugins: PluginRegistry::new(setup.plugins.clone()),
            launch_baselines: LaunchBaselines::default(),
            setup,
            engine: Engine::new(policy),
            audit,
            sandbox: self.sandbox.clone(),
            host_env: self.host_env.clone(),
            skills: Vec::new(),
            skill_notes: Vec::new(),
            tool_roots: Vec::new(),
            search: self.search.clone(),
            search_snapshots: BTreeMap::new(),
            mcp: Registry::default(),
            query_snapshot: None,
            query_conversation: None,
            transcripts: None,
            _tmp: tmp,
            team: self.team.child(),
            team_label: id.to_owned(),
            cancel: Arc::new(AtomicBool::new(false)),
            questions: None,
        };
        session.tool_roots = configured_tool_roots(&session.setup);
        let mut plugins = mem::take(&mut session.plugins);
        let loaded = plugins.load(&mut session);
        session.plugins = plugins;
        loaded?;
        Ok(session)
    }

    pub fn transcript_store(&mut self) -> Result<&Store> {
        if self.transcripts.is_none() {
            let store = Store::open(&self.setup.paths.session_db(), self.setup.workspace.clone())
                .context("opening the saved conversations")?;
            self.transcripts = Some(store);
        }
        Ok(self.transcripts.as_ref().expect("opened just above"))
    }

    pub fn roots(&self) -> &Roots {
        &self.setup.roots
    }

    pub fn declared_tools(&self) -> Vec<ToolDef> {
        let mut tools = self.mcp.declared();
        tools.extend(self.plugins.definitions());
        tools
    }

    pub fn reload_plugins(&mut self) -> Result<()> {
        let entries = self.setup.plugins.clone();
        let fresh = self.plugins.reconfigured(entries);
        self.plugins = fresh;
        let mut registry = mem::take(&mut self.plugins);
        let loaded = registry.load(self);
        self.plugins = registry;
        loaded
    }

    pub fn switch_profile(&mut self, name: Option<&str>) -> Result<()> {
        let previous = self.setup.overrides.profile.clone();
        self.setup.overrides.profile = name.map(str::to_owned);
        if let Err(error) = self.reload_policy() {
            self.setup.overrides.profile = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn reload_policy(&mut self) -> Result<()> {
        let fresh = Setup::load(&self.setup.start, self._tmp.path(), &self.setup.overrides)?;
        let policy = fresh.parse_policy()?;
        let file_roots = fresh
            .roots
            .iter()
            .map(|(name, path)| Ok((name.clone(), fs_access::root(path)?)))
            .collect::<Result<_>>()?;
        let workers = self.team.stop_all();
        tasks::stop_all(self)?;
        self.tasks = Tasks::default();
        self.plugin_events = Default::default();
        self.extensions = Default::default();
        let before = self.setup.policy_sha256.clone();
        self.launch_baselines.enroll(&fresh.launch_profiles);
        self.setup = fresh;
        search_transport::trim_cache(self);
        self.tool_roots = configured_tool_roots(&self.setup);
        self.file_roots = file_roots;
        self.engine.replace_policy(policy);
        self.audit
            .event(
                Kind::Policy,
                None,
                None,
                None,
                json!({
                    "sandbox-config": self.setup.sandbox_audit(),
                    "policy": {
                        "path": self.setup.policy_path,
                        "sha256": self.setup.policy_sha256,
                        "previous_sha256": before,
                        "workers_stopped": workers,
                    },
                }),
                "policy reloaded mid-session",
            )
            .map_err(|error| HostFailure(error.into()))?;
        self.reload_plugins()
    }

    pub fn net_names_host(&self, who: &Principal, host: &str) -> bool {
        self.engine.net_names_host(who, host)
            && (!matches!(who, Principal::Plugin(_))
                || self.engine.net_names_host(&Principal::Agent, host))
    }

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
                    "`{tool}` on {} is allowed by protected [sandbox] settings",
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
                "`{tool}` is not one of the tools protected [sandbox] settings allow on {} ({})",
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

    pub(crate) fn tool_root_for(&self, abs: &Path) -> Option<&ToolRoot> {
        let abs = roots::normalize(abs);
        self.tool_roots
            .iter()
            .filter(|r| abs.starts_with(&r.dir))
            .max_by_key(|r| r.dir.components().count())
    }

    pub fn resolve_tool(&self, loc: &Location) -> IoResult<Location> {
        let base = self
            .base_of(&loc.root)
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no root `{}`", loc.root)))?;
        let base = base.canonicalize()?;
        let abs = base.join(loc.rel.to_path());
        let real = resolve_missing_tool_path(&abs, 0)?;
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

    pub fn base_of(&self, root: &RootName) -> Option<PathBuf> {
        if let Some(path) = self.roots().get(root) {
            return Some(path.to_path_buf());
        }
        self.tool_roots
            .iter()
            .find(|r| &r.name == root)
            .map(|r| r.dir.clone())
    }

    pub fn is_tool_root(&self, root: &RootName) -> bool {
        self.tool_roots.iter().any(|r| &r.name == root)
    }

    pub(crate) fn tool_root(&self, name: &RootName) -> Option<&ToolRoot> {
        self.tool_roots.iter().find(|root| &root.name == name)
    }

    pub fn reload_skills(&mut self) -> Result<()> {
        let cfg = self.setup.skills.clone();
        let (found, notes) = skills::discover(self, &cfg)?;
        self.skills = found;
        self.skill_notes = notes;
        Ok(())
    }

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

    pub fn close(mut self) -> Result<String> {
        self.team.stop_all();
        tasks::stop_all(&mut self)?;
        self.mcp.close();
        Ok(self.audit.close(&self.setup.paths.sessions_index())?)
    }
}

pub fn session_id() -> String {
    let mut bytes = [0u8; 8];
    let ok = File::open("/dev/urandom")
        .and_then(|mut f| Read::read_exact(&mut f, &mut bytes))
        .is_ok();
    if !ok {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!("time{ms:x}");
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
