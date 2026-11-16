//! Per-call decisions (D10, D22, D23). Every decision is a lookup over grants: no ordering, no
//! fallthrough, and a one-sentence explanation either way.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::audit::sha256_hex;
use globset::GlobSet;
use saddle_cap::policy::{FsGrant, NetGrant, POLICY_FILE, Sessions, exclude_set, net_allows};
use saddle_cap::{Location, Policy, Principal, RelPath, RootName, RuleId, Scope};
use serde_json::{Value, json};

/// A request with its arguments already resolved.
#[derive(Debug, Clone)]
pub enum Request<'a> {
    /// the host supplies the current D66 entry's roles, never the guest.
    ToolRead {
        path: &'a str,
        available_tools: &'a [String],
    },
    ToolWrite {
        path: &'a str,
        available_tools: &'a [String],
    },
    FsRead {
        loc: &'a Location,
    },
    FsWrite {
        loc: &'a Location,
    },
    Exec {
        argv: &'a [String],
        cwd: &'a Location,
    },
    /// session-owned process observation/control, still bounded by exec (D153).
    ExecTask {
        operation: &'a str,
        id: Option<&'a str>,
    },
    Net {
        method: &'a str,
        scheme: &'a str,
        host: &'a str,
        port: u16,
        path: &'a str,
        /// Header *names* only, never values: a value is where a key lives, so it is not part of
        /// what a decision or an audit record carries (D48).
        headers: &'a [String],
    },
    ModelInvoke {
        tokens: u64,
    },
    ModelReserve {
        tokens: u64,
        cost: Option<f64>,
    },
    TranscriptQuery {
        /// The conversation the query is against: this one, or another of this workspace (D163).
        session: &'a str,
        /// Whether that is a conversation other than the one this session is holding.
        others: bool,
        sql: &'a str,
    },
    PluginService {
        scope: Scope,
        operation: &'a str,
        arguments: &'a Value,
    },
    /// host-attributed hook delivery or custom-event publication (D155).
    Hook {
        scope: Scope,
        operation: &'a str,
        channel: &'a str,
        publisher: &'a str,
        name: &'a str,
        data: &'a str,
    },
    /// Scopes without arguments: transcript-read, context-contribute, tool, veto, ui, persist.
    Plain(Scope),
}

impl Request<'_> {
    pub fn scope(&self) -> Scope {
        match self {
            Request::ToolRead { .. } => Scope::ToolRead,
            Request::ToolWrite { .. } => Scope::ToolWrite,
            Request::FsRead { .. } => Scope::FsRead,
            Request::FsWrite { .. } => Scope::FsWrite,
            Request::Exec { .. } | Request::ExecTask { .. } => Scope::Exec,
            Request::Net { .. } => Scope::Net,
            Request::ModelInvoke { .. } | Request::ModelReserve { .. } => Scope::ModelInvoke,
            Request::TranscriptQuery { .. } => Scope::TranscriptRead,
            Request::Plain(s)
            | Request::Hook { scope: s, .. }
            | Request::PluginService { scope: s, .. } => *s,
        }
    }

    /// Arguments as they go into the audit log. Query values are redacted (D24).
    pub fn args(&self) -> Value {
        match self {
            Request::ToolRead {
                path,
                available_tools,
            }
            | Request::ToolWrite {
                path,
                available_tools,
            } => json!({"path":path,"available_tools":available_tools}),
            Request::FsRead { loc } | Request::FsWrite { loc } => {
                json!({ "path": loc.to_string() })
            }
            Request::Exec { argv, cwd } => json!({ "argv": argv, "cwd": cwd.to_string() }),
            Request::ExecTask { operation, id } => json!({"operation":operation,"task":id}),
            Request::Net {
                method,
                scheme,
                host,
                port,
                path,
                headers,
            } => {
                let mut args = json!({
                    "method": method,
                    "url": format!("{scheme}://{host}:{port}{}", redact_query(path)),
                });
                if !headers.is_empty() {
                    // The names say what the request carried without saying what was in it.
                    args["headers"] = json!(headers);
                }
                args
            }
            Request::ModelInvoke { tokens } => json!({ "tokens": tokens }),
            Request::ModelReserve { tokens, cost } => {
                json!({"operation":"model-reserve","tokens":tokens,"cost":cost})
            }
            Request::TranscriptQuery {
                session,
                others,
                sql,
            } => json!({ "session": session, "others": others, "sql": sql }),
            Request::Hook {
                operation,
                channel,
                publisher,
                name,
                data,
                ..
            } => {
                json!({"operation":operation,"channel":channel,"publisher":publisher,"name":name,"bytes":data.len(),"sha256":sha256_hex(data.as_bytes())})
            }
            Request::PluginService {
                operation,
                arguments,
                ..
            } => json!({"operation":operation,"arguments":arguments}),
            Request::Plain(_) => json!({}),
        }
    }
}

fn redact_query(path: &str) -> String {
    let Some((head, query)) = path.split_once('?') else {
        return path.to_owned();
    };
    let redacted: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((k, _)) => format!("{k}=…"),
            None => pair.to_owned(),
        })
        .collect();
    format!("{head}?{}", redacted.join("&"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub scope: Scope,
    pub rule: Option<RuleId>,
    pub explanation: String,
}

impl Decision {
    fn allow(scope: Scope, rule: &RuleId, why: &str) -> Self {
        Self {
            allow: true,
            scope,
            rule: Some(rule.clone()),
            // A grant with no `why` (D41) is explained by its rule alone, rather than by a
            // dangling colon.
            explanation: match why.is_empty() {
                true => rule.to_string(),
                false => format!("{rule}: {why}"),
            },
        }
    }

    fn deny(scope: Scope, explanation: String) -> Self {
        Self {
            allow: false,
            scope,
            rule: None,
            explanation,
        }
    }
}

/// Paths no grant can write (D23).
#[derive(Debug)]
pub struct Reserved {
    any_root: GlobSet,
    policy_file: Location,
}

/// Version-control paths that run code as the user later, outside any sandbox: hooks and config,
/// matched at any depth (D23).
pub const RESERVED_VCS: &[&str] = &[
    // git: the repository, a linked worktree's own config, submodules, and a bare repository
    // (`name.git`), which has no `.git` directory to match.
    ".git/hooks",
    ".git/config",
    ".git/config.worktree",
    ".git/worktrees/*/config.worktree",
    ".git/modules/**/hooks",
    ".git/modules/**/config",
    ".git/modules/**/config.worktree",
    "*.git/hooks",
    "*.git/config",
    // jj: a repo or workspace config found in the tree is adopted as the user's own, `git_target`
    // names the git repository jj drives, and a jj repository that is not colocated keeps that
    // repository under `.jj`, where git reads its config on fetch and push.
    ".jj/repo/config.toml",
    ".jj/workspace-config.toml",
    ".jj/repo/store/git_target",
    ".jj/repo/store/git/hooks",
    ".jj/repo/store/git/config",
];

impl Reserved {
    pub fn new() -> Self {
        let patterns: Vec<String> = RESERVED_VCS.iter().map(|p| format!("**/{p}")).collect();
        Self {
            any_root: exclude_set(&patterns).expect("reserved patterns compile"),
            policy_file: Location::new(
                RootName::workspace(),
                RelPath::new(POLICY_FILE).expect("policy file name is a relative path"),
            ),
        }
    }

    pub fn contains(&self, loc: &Location) -> bool {
        *loc == self.policy_file
            || (!loc.rel.is_root() && self.any_root.is_match(loc.rel.as_slash()))
    }

    pub fn policy_file(&self) -> &Location {
        &self.policy_file
    }
}

impl Default for Reserved {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Engine {
    policy: Policy,
    reserved: Reserved,
    spent: Mutex<BTreeMap<Principal, u64>>,
    money: Mutex<BTreeMap<Principal, f64>>,
}

/// One narrowed list of net grants against one request (D110). A component or a server has a list
/// of its own rather than a principal the policy resolves, so the question is asked here — with the
/// same constructors and the same explanation an ordinary decision gets — and the caller names what
/// it was answering for.
pub fn net_decision(reach: &[NetGrant], req: &Request<'_>, who: &str) -> Decision {
    match req {
        Request::Net {
            scheme, host, port, ..
        } => match net_allows(reach, scheme, host, *port) {
            Some(grant) => Decision::allow(Scope::Net, &grant.rule, &grant.why),
            None => Decision::deny(
                Scope::Net,
                format!("{who} has no net grant covering {scheme}://{host}:{port}"),
            ),
        },
        _ => Decision::deny(Scope::Net, format!("{req:?} is not a network request")),
    }
}

impl Engine {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            reserved: Reserved::new(),
            spent: Mutex::new(BTreeMap::new()),
            money: Mutex::new(BTreeMap::new()),
        }
    }

    fn model_owners(who: &Principal) -> Vec<Principal> {
        if matches!(who, Principal::Plugin(_)) {
            vec![Principal::Agent, who.clone()]
        } else {
            vec![who.clone()]
        }
    }
    fn reserve_model(&self, who: &Principal, tokens: u64, cost: Option<f64>) -> Decision {
        if cost.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return Decision::deny(
                Scope::ModelInvoke,
                "model price estimate must be finite and nonnegative".into(),
            );
        }
        let mut spent = self.spent.lock().unwrap_or_else(|error| error.into_inner());
        let mut money = self.money.lock().unwrap_or_else(|error| error.into_inner());
        for owner in Self::model_owners(who) {
            let Some(grant) = self
                .policy
                .grants(&owner)
                .and_then(|grants| grants.model_invoke.as_ref())
            else {
                return Decision::deny(
                    Scope::ModelInvoke,
                    format!("{owner} has no model-invoke grant"),
                );
            };
            if spent
                .get(&owner)
                .copied()
                .unwrap_or(0)
                .saturating_add(tokens)
                > grant.tokens_per_session
            {
                return Decision::deny(
                    Scope::ModelInvoke,
                    format!("{owner} model token budget exceeded"),
                );
            }
            if grant.cost_per_session.is_some_and(|limit| {
                cost.is_none_or(|cost| {
                    !cost.is_finite()
                        || cost < 0.0
                        || money.get(&owner).copied().unwrap_or(0.0) + cost > limit
                })
            }) {
                return Decision::deny(
                    Scope::ModelInvoke,
                    format!("{owner} model cost budget exceeded or price is unknown"),
                );
            }
        }
        for owner in Self::model_owners(who) {
            *spent.entry(owner.clone()).or_default() += tokens;
            *money.entry(owner).or_default() += cost.unwrap_or(0.0);
        }
        match self
            .policy
            .grants(who)
            .and_then(|grants| grants.model_invoke.as_ref())
        {
            Some(grant) => Decision::allow(Scope::ModelInvoke, &grant.rule, &grant.why),
            None => Decision::deny(Scope::ModelInvoke, "model-invoke is not granted".into()),
        }
    }
    /// reconcile reported usage; unknown/failed usage retains its reservation (D156).
    pub fn settle_model(
        &self,
        who: &Principal,
        reserved: u64,
        reserved_cost: Option<f64>,
        actual: u64,
        actual_cost: Option<f64>,
    ) {
        let mut spent = self.spent.lock().unwrap_or_else(|error| error.into_inner());
        let mut money = self.money.lock().unwrap_or_else(|error| error.into_inner());
        for owner in Self::model_owners(who) {
            let used = spent.entry(owner.clone()).or_default();
            *used = used.saturating_sub(reserved).saturating_add(actual);
            let used = money.entry(owner).or_default();
            *used = (*used - reserved_cost.unwrap_or(0.0)).max(0.0)
                + actual_cost.or(reserved_cost).unwrap_or(0.0);
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// replaces effective grants without refunding any principal's session spending (D129).
    pub fn replace_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }

    pub fn reserved(&self) -> &Reserved {
        &self.reserved
    }

    /// Which conversations the principal's transcript grant covers (D163). `None` when it holds
    /// none, which is what the host uses to decide whether another session may even be resolved;
    /// the decision itself is still [`Engine::decide`].
    pub fn transcript_sessions(&self, who: &Principal) -> Option<Sessions> {
        self.policy
            .grants(who)
            .and_then(|g| g.plain.get(&Scope::TranscriptRead))
            .map(|g| g.sessions)
    }

    /// Whether a net grant names this host exactly (D28). The fetch tool asks before dialling a
    /// literal address: a wildcard grant allows public addresses but does not open loopback, which
    /// only a grant that spells the address out may do (D40).
    pub fn net_names_host(&self, who: &Principal, host: &str) -> bool {
        self.policy.grants(who).is_some_and(|g| g.names_host(host))
    }

    pub fn decide(&self, who: &Principal, req: &Request<'_>) -> Decision {
        let scope = req.scope();
        let Some(grants) = self.policy.grants(who) else {
            return Decision::deny(scope, format!("{who} has no grants in the policy"));
        };
        match req {
            Request::ToolWrite {
                path,
                available_tools,
            } => {
                let relative = RelPath::new(path.trim_start_matches('/'));
                if relative.as_ref().is_ok_and(|rel| {
                    self.reserved
                        .contains(&Location::new(RootName::workspace(), rel.clone()))
                }) {
                    return Decision::deny(
                        scope,
                        "path is reserved by the host and no grant can write it".into(),
                    );
                }
                match &grants.tool_write {
                    Some(g) if g.tools.iter().any(|tool| available_tools.contains(tool)) => {
                        Decision::allow(scope, &g.rule, &g.why)
                    }
                    _ => Decision::deny(
                        scope,
                        format!("{who} has no tool-write role covering this configured directory"),
                    ),
                }
            }
            Request::ToolRead {
                available_tools, ..
            } => match &grants.tool_read {
                Some(g) if g.tools.iter().any(|tool| available_tools.contains(tool)) => {
                    Decision::allow(scope, &g.rule, &g.why)
                }
                _ => Decision::deny(
                    scope,
                    format!("{who} has no tool-read role covering this configured directory"),
                ),
            },
            Request::FsRead { loc } => fs_decision(who, scope, &grants.fs_read, loc),
            Request::FsWrite { loc } => {
                if self.reserved.contains(loc) {
                    return Decision::deny(
                        scope,
                        format!("{loc} is reserved by the host and no grant can write it"),
                    );
                }
                fs_decision(who, scope, &grants.fs_write, loc)
            }
            Request::Exec { .. } | Request::ExecTask { .. } => match &grants.exec {
                Some(g) => Decision::allow(scope, &g.rule, &g.why),
                None => Decision::deny(scope, format!("{who} has no exec grant")),
            },
            Request::Net {
                scheme, host, port, ..
            } => match grants.net.iter().find(|g| g.allows(scheme, host, *port)) {
                Some(g) => Decision::allow(scope, &g.rule, &g.why),
                None => Decision::deny(
                    scope,
                    format!("{who} has no net grant covering {scheme}://{host}:{port}"),
                ),
            },
            Request::ModelReserve { tokens, cost } => self.reserve_model(who, *tokens, *cost),
            Request::ModelInvoke { tokens } => {
                let Some(g) = &grants.model_invoke else {
                    return Decision::deny(scope, format!("{who} has no model-invoke grant"));
                };
                let mut spent = self.spent.lock().unwrap_or_else(|e| e.into_inner());
                let used = spent.entry(who.clone()).or_insert(0);
                if used.saturating_add(*tokens) > g.tokens_per_session {
                    return Decision::deny(
                        scope,
                        format!(
                            "{who} has used {used} of {} tokens this session; {tokens} more would exceed {}",
                            g.tokens_per_session, g.rule
                        ),
                    );
                }
                *used += tokens;
                Decision::allow(scope, &g.rule, &g.why)
            }
            // D163: the grant names which conversations it covers. A query against this one needs
            // no more than the grant itself; one against another needs the workspace's.
            Request::TranscriptQuery { others, .. } => {
                let Some(grant) = grants.plain.get(&Scope::TranscriptRead) else {
                    return Decision::deny(scope, format!("{who} has no {scope} grant"));
                };
                if *others && grant.sessions != Sessions::Workspace {
                    return Decision::deny(
                        scope,
                        format!(
                            "{who} may read only this conversation, not another one ({})",
                            grant.rule
                        ),
                    );
                }
                Decision::allow(scope, &grant.rule, &grant.why)
            }
            Request::Plain(s)
            | Request::Hook { scope: s, .. }
            | Request::PluginService { scope: s, .. } => match grants.plain.get(s) {
                Some(g) => Decision::allow(scope, &g.rule, &g.why),
                None => Decision::deny(scope, format!("{who} has no {s} grant")),
            },
        }
    }
}

fn fs_decision(who: &Principal, scope: Scope, grants: &[FsGrant], loc: &Location) -> Decision {
    let mut excluded_by = None;
    for g in grants.iter().filter(|g| g.root == loc.root) {
        if g.excludes(&loc.rel) {
            excluded_by.get_or_insert(&g.rule);
        } else {
            return Decision::allow(scope, &g.rule, &g.why);
        }
    }
    match excluded_by {
        Some(rule) => Decision::deny(scope, format!("{loc} is excluded by {rule}")),
        None => Decision::deny(scope, format!("{who} has no {scope} grant covering {loc}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saddle_cap::{PluginId, Roots};

    #[test]
    fn tool_paths_intersect_host_supplied_entry_roles_and_the_calling_principal() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse("version=1\n[plugin.reader]\ntool-read={tools=['read']}\n[plugin.sibling]\ntool-read={tools=['ls']}\n", &roots).unwrap();
        let engine = Engine::new(policy);
        let who = Principal::Plugin(PluginId::new("reader").unwrap());
        for (roles, allow) in [
            (vec!["read".into()], true),
            (vec!["ls".into()], false),
            (Vec::new(), false),
        ] {
            let request = Request::ToolRead {
                path: "/shared/file",
                available_tools: &roles,
            };
            assert_eq!(engine.decide(&who, &request).allow, allow);
            assert_eq!(request.args()["path"], "/shared/file");
        }
        let missing = Principal::Plugin(PluginId::new("missing").unwrap());
        assert!(
            !engine
                .decide(
                    &missing,
                    &Request::ToolRead {
                        path: "/shared/file",
                        available_tools: &["read".into()]
                    }
                )
                .allow
        );
        assert!(
            !engine
                .decide(
                    &who,
                    &Request::FsRead {
                        loc: &Location::new(RootName::workspace(), RelPath::new("file").unwrap())
                    }
                )
                .allow
        );
    }

    #[test]
    fn model_reservations_share_parent_and_preserve_spending_on_reload() {
        let ws = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        let source = "version=1\n[agent]\nmodel-invoke={tokens-per-session=100,cost-per-session=1.0}\n[plugin.first]\nmodel-invoke={tokens-per-session=100,cost-per-session=1.0}\n[plugin.second]\nmodel-invoke={tokens-per-session=100,cost-per-session=1.0}\n";
        let mut engine = Engine::new(Policy::parse(source, &roots).unwrap());
        let first = Principal::Plugin(PluginId::new("first").unwrap());
        let second = Principal::Plugin(PluginId::new("second").unwrap());
        let request = Request::ModelReserve {
            tokens: 60,
            cost: Some(0.6),
        };
        assert!(engine.decide(&first, &request).allow);
        assert!(!engine.decide(&second, &request).allow);
        engine.replace_policy(Policy::parse(source, &roots).unwrap());
        assert!(!engine.decide(&second, &request).allow);
        engine.settle_model(&first, 60, Some(0.6), 20, Some(0.2));
        assert!(engine.decide(&second, &request).allow);
        assert!(
            !engine
                .decide(
                    &first,
                    &Request::ModelReserve {
                        tokens: 1,
                        cost: Some(0.3)
                    }
                )
                .allow
        );
        assert!(
            !engine
                .decide(
                    &first,
                    &Request::ModelReserve {
                        tokens: 1,
                        cost: None
                    }
                )
                .allow
        );
        // Rejected admissions did not charge either owner.
        assert!(
            engine
                .decide(
                    &first,
                    &Request::ModelReserve {
                        tokens: 20,
                        cost: Some(0.1)
                    }
                )
                .allow
        );
    }

    #[test]
    fn model_reservation_requires_parent_authority_even_with_a_child_grant() {
        let (_ws, engine) = engine();
        let who = Principal::Plugin(PluginId::new("web-search").unwrap());
        assert!(
            !engine
                .decide(
                    &who,
                    &Request::ModelReserve {
                        tokens: 1,
                        cost: Some(0.0)
                    }
                )
                .allow
        );
    }

    fn engine() -> (tempfile::TempDir, Engine) {
        let ws = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        let policy = Policy::parse(
            r#"
version = 1
[agent]
fs-read  = [{ root = "workspace", why = "reads the repo" }]
fs-write = [{ root = "workspace", exclude = [".env*"], why = "edits the repo" }]
exec     = { why = "runs tests" }

[plugin.web-search]
net = [{ host = "*.search.example", why = "backend" }]
model-invoke = { tokens-per-session = 100, why = "summaries" }
"#,
            &roots,
        )
        .unwrap();
        (ws, Engine::new(policy))
    }

    #[test]
    fn a_transcript_query_against_another_conversation_needs_a_workspace_grant() {
        // D163: `transcript-read` names the conversations it covers. Reading the one in hand is
        // what it always meant; reading another is the workspace's, and a bare grant is that.
        let (dir, _) = engine();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let case = |text: &str, others: bool| {
            let policy = Policy::parse(text, &roots).unwrap();
            let engine = Engine::new(policy);
            let req = Request::TranscriptQuery {
                session: "other",
                others,
                sql: "SELECT 1",
            };
            engine.decide(&Principal::Agent, &req)
        };
        let bare = "version = 1\n[agent]\ntranscript-read = {}\n";
        assert!(case(bare, false).allow);
        assert!(case(bare, true).allow, "a bare grant covers the workspace");
        let only = "version = 1\n[agent]\ntranscript-read = { sessions = \"self\" }\n";
        assert!(
            case(only, false).allow,
            "this conversation is still readable"
        );
        let refused = case(only, true);
        assert!(!refused.allow);
        assert!(
            refused.explanation.contains("only this conversation"),
            "{}",
            refused.explanation
        );
        let none = case("version = 1\n[agent]\nexec = {}\n", false);
        assert!(!none.allow && none.explanation.contains("transcript-read"));
    }

    #[test]
    fn a_net_request_records_its_header_names_and_no_values() {
        // D48: the record says what the request carried, not what was in it.
        let headers = vec!["authorization".to_owned(), "x-api-key".to_owned()];
        let req = Request::Net {
            method: "GET",
            scheme: "https",
            host: "api.example.com",
            port: 443,
            path: "/v2/quotas?token=secret",
            headers: &headers,
        };
        let args = req.args();
        assert_eq!(args["headers"], serde_json::json!(headers));
        assert_eq!(args["url"], "https://api.example.com:443/v2/quotas?token=…");
        assert!(!args.to_string().contains("secret"), "{args}");
        // With no headers the field is left out entirely.
        let req = Request::Net {
            method: "GET",
            scheme: "https",
            host: "api.example.com",
            port: 443,
            path: "/x",
            headers: &[],
        };
        assert!(req.args().get("headers").is_none());
    }

    #[test]
    fn task_control_uses_the_same_exec_grant_and_records_its_identity() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let request = Request::ExecTask {
            operation: "task-inspect",
            id: Some("task-123"),
        };
        assert_eq!(request.scope(), Scope::Exec);
        assert_eq!(
            request.args(),
            json!({"operation":"task-inspect","task":"task-123"})
        );
        for (source, allow) in [
            ("version=1\n[agent]\nexec={}\n", true),
            ("version=1\n[agent]\n", false),
        ] {
            let engine = Engine::new(Policy::parse(source, &roots).unwrap());
            assert_eq!(engine.decide(&Principal::Agent, &request).allow, allow);
        }
    }

    #[test]
    fn an_allow_without_a_why_is_explained_by_its_rule() {
        // D41: a grant may omit its reason, and an allow then names the rule without a colon.
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse("version = 1\n[agent]\nexec = {}\n", &roots).unwrap();
        let engine = Engine::new(policy);
        let d = engine.decide(
            &Principal::Agent,
            &Request::Exec {
                argv: &["sh".to_owned()],
                cwd: &ws("."),
            },
        );
        assert!(d.allow);
        assert_eq!(d.explanation, "agent.exec");
    }

    fn ws(rel: &str) -> Location {
        Location::new(RootName::workspace(), RelPath::new(rel).unwrap())
    }

    #[test]
    fn fs_allow_exclude_and_reserved() {
        let (_d, e) = engine();
        let a = Principal::Agent;
        let d = e.decide(
            &a,
            &Request::FsWrite {
                loc: &ws("src/main.rs"),
            },
        );
        assert!(d.allow);
        assert_eq!(d.explanation, "agent.fs-write[0]: edits the repo");

        let d = e.decide(
            &a,
            &Request::FsWrite {
                loc: &ws("conf/.env.local"),
            },
        );
        assert!(!d.allow);
        assert_eq!(
            d.explanation,
            "workspace:conf/.env.local is excluded by agent.fs-write[0]"
        );

        for path in [
            "saddle.toml",
            ".git/hooks/pre-commit",
            ".git/config",
            "sub/.git/hooks",
            ".git/modules/x/config",
            ".git/config.worktree",
            ".git/worktrees/side/config.worktree",
            ".git/modules/x/config.worktree",
            "vendor/lib.git/config",
            "vendor/lib.git/hooks/pre-receive",
            ".jj/repo/config.toml",
            ".jj/workspace-config.toml",
            ".jj/repo/store/git_target",
            ".jj/repo/store/git/config",
            ".jj/repo/store/git/hooks/pre-push",
        ] {
            let d = e.decide(&a, &Request::FsWrite { loc: &ws(path) });
            assert!(!d.allow, "{path}");
            assert!(
                d.explanation.contains("reserved"),
                "{path}: {}",
                d.explanation
            );
        }
        // The rest of a repository stays writable: jj and git rewrite it on every command.
        for path in [
            ".git/config.lock",
            ".jj/repo/op_heads/heads/x",
            ".jj/repo/store/git/objects/ab/cd",
            ".jj/working_copy/tree_state",
        ] {
            let d = e.decide(&a, &Request::FsWrite { loc: &ws(path) });
            assert!(d.allow, "{path}: {}", d.explanation);
        }
        // Reading reserved paths is fine; only writes are refused.
        assert!(
            e.decide(
                &a,
                &Request::FsRead {
                    loc: &ws(".git/config")
                }
            )
            .allow
        );
        assert!(
            e.decide(
                &a,
                &Request::FsWrite {
                    loc: &ws(".git/objects/ab/cd")
                }
            )
            .allow
        );
    }

    #[test]
    fn unknown_principal_and_missing_scope() {
        let (_d, e) = engine();
        let ghost = Principal::Plugin(PluginId::new("ghost").unwrap());
        let d = e.decide(&ghost, &Request::Plain(Scope::Ui));
        assert_eq!(d.explanation, "plugin.ghost has no grants in the policy");
        let d = e.decide(&Principal::Agent, &Request::Plain(Scope::Persist));
        assert_eq!(d.explanation, "agent has no persist grant");
    }

    #[test]
    fn replacing_policy_does_not_refund_spending_even_after_revocation() {
        let (_dir, mut engine) = engine();
        let who = Principal::Plugin(PluginId::new("web-search").unwrap());
        let original = engine.policy().clone();
        assert!(
            engine
                .decide(&who, &Request::ModelInvoke { tokens: 60 })
                .allow
        );
        engine.replace_policy(original.clone());
        assert!(
            !engine
                .decide(&who, &Request::ModelInvoke { tokens: 60 })
                .allow
        );
        engine.replace_policy(Policy::default());
        assert!(
            !engine
                .decide(&who, &Request::ModelInvoke { tokens: 1 })
                .allow
        );
        engine.replace_policy(original);
        assert!(
            engine
                .decide(&who, &Request::ModelInvoke { tokens: 40 })
                .allow
        );
        assert!(
            !engine
                .decide(&who, &Request::ModelInvoke { tokens: 1 })
                .allow
        );
    }

    #[test]
    fn net_and_budget() {
        let (_d, e) = engine();
        let web = Principal::Plugin(PluginId::new("web-search").unwrap());
        let headers: Vec<String> = Vec::new();
        let req = |host| Request::Net {
            method: "GET",
            scheme: "https",
            host,
            port: 443,
            path: "/q?key=secret",
            headers: &headers,
        };
        assert!(e.decide(&web, &req("api.search.example")).allow);
        let d = e.decide(&web, &req("evil.example"));
        assert_eq!(
            d.explanation,
            "plugin.web-search has no net grant covering https://evil.example:443"
        );
        assert_eq!(req("x").args()["url"], "https://x:443/q?key=…");

        assert!(e.decide(&web, &Request::ModelInvoke { tokens: 60 }).allow);
        let d = e.decide(&web, &Request::ModelInvoke { tokens: 60 });
        assert!(!d.allow);
        assert!(
            d.explanation.contains("used 60 of 100"),
            "{}",
            d.explanation
        );
    }
}
