use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::{Arc, Mutex};

use crate::audit::sha256_hex;
use saddle_cap::policy::{FsGrant, NetGrant, Sessions, net_allows};
use saddle_cap::{Location, Policy, Principal, RuleId, Scope};
use serde_json::{Value, json};

#[derive(Debug, Clone)]
pub enum Request<'a> {
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
    RunBinary {
        profile: &'a str,
    },
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
        session: &'a str,
        others: bool,
        sql: &'a str,
    },
    PluginService {
        scope: Scope,
        operation: &'a str,
        arguments: &'a Value,
    },
    Hook {
        scope: Scope,
        operation: &'a str,
        channel: &'a str,
        publisher: &'a str,
        name: &'a str,
        data: &'a str,
    },
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
            Request::RunBinary { .. } => Scope::RunBinary,
            Request::Net { .. } => Scope::Net,
            Request::ModelInvoke { .. } | Request::ModelReserve { .. } => Scope::ModelInvoke,
            Request::TranscriptQuery { .. } => Scope::TranscriptRead,
            Request::Plain(s)
            | Request::Hook { scope: s, .. }
            | Request::PluginService { scope: s, .. } => *s,
        }
    }

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
            Request::RunBinary { profile } => json!({"profile":profile}),
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

#[derive(Clone)]
pub struct Engine {
    policy: Policy,
    spent: Arc<Mutex<BTreeMap<Principal, u64>>>,
    money: Arc<Mutex<BTreeMap<Principal, f64>>>,
}

pub fn net_decision(reach: &[NetGrant], req: &Request<'_>, who: &str) -> Decision {
    match req {
        Request::Net {
            scheme, host, port, ..
        } => decide_net(reach, who, scheme, host, *port),
        _ => Decision::deny(Scope::Net, format!("{req:?} is not a network request")),
    }
}

fn decide_net(
    reach: &[NetGrant],
    who: impl Display,
    scheme: &str,
    host: &str,
    port: u16,
) -> Decision {
    match net_allows(reach, scheme, host, port) {
        Some(grant) => Decision::allow(Scope::Net, &grant.rule, &grant.why),
        None => Decision::deny(
            Scope::Net,
            format!("{who} has no net grant covering {scheme}://{host}:{port}"),
        ),
    }
}

impl Engine {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            spent: Default::default(),
            money: Default::default(),
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
            if let Some(limit) = grant.tokens_per_session
                && spent
                    .get(&owner)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(tokens)
                    > limit
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

    pub fn replace_policy(&mut self, policy: Policy) {
        self.policy = policy;
    }

    pub fn transcript_sessions(&self, who: &Principal) -> Option<Sessions> {
        self.policy
            .grants(who)
            .and_then(|g| g.plain.get(&Scope::TranscriptRead))
            .map(|g| g.sessions)
    }

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
                available_tools, ..
            } => match &grants.tool_write {
                Some(g) if g.tools.iter().any(|tool| available_tools.contains(tool)) => {
                    Decision::allow(scope, &g.rule, &g.why)
                }
                _ => Decision::deny(
                    scope,
                    format!("{who} has no tool-write role covering this configured directory"),
                ),
            },
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
            Request::FsWrite { loc } => fs_decision(who, scope, &grants.fs_write, loc),
            Request::Exec { .. } | Request::ExecTask { .. } => match &grants.exec {
                Some(g) => Decision::allow(scope, &g.rule, &g.why),
                None => Decision::deny(scope, format!("{who} has no exec grant")),
            },
            Request::RunBinary { profile } => match grants.launch_profile(profile) {
                Some(grant) => Decision::allow(scope, &grant.rule, &grant.why),
                None => Decision::deny(
                    scope,
                    format!("{who} has no run-binary grant for profile {profile:?}"),
                ),
            },
            Request::Net {
                scheme, host, port, ..
            } => decide_net(&grants.net, who, scheme, host, *port),
            Request::ModelReserve { tokens, cost } => self.reserve_model(who, *tokens, *cost),
            Request::ModelInvoke { tokens } => {
                let Some(g) = &grants.model_invoke else {
                    return Decision::deny(scope, format!("{who} has no model-invoke grant"));
                };
                let mut spent = self.spent.lock().unwrap_or_else(|e| e.into_inner());
                let used = spent.entry(who.clone()).or_insert(0);
                if let Some(limit) = g.tokens_per_session
                    && used.saturating_add(*tokens) > limit
                {
                    return Decision::deny(
                        scope,
                        format!(
                            "{who} has used {used} of {limit} tokens this session; {tokens} more would exceed {}",
                            g.rule
                        ),
                    );
                }
                *used += tokens;
                Decision::allow(scope, &g.rule, &g.why)
            }
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
    use saddle_cap::{PluginId, RelPath, RootName, Roots};

    fn net_request<'a>(scheme: &'a str, host: &'a str, port: u16) -> Request<'a> {
        Request::Net {
            method: "GET",
            scheme,
            host,
            port,
            path: "/",
            headers: &[],
        }
    }

    #[test]
    fn principal_and_mcp_net_decisions_match_for_the_same_reach() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse(
            r#"
version = 1
[agent]
net = [
    { host = "api.example.com", why = "exact" },
    { host = "*.example.com", why = "suffix" },
    { host = "api.example.com", scheme = "http", port = 8080 },
]
"#,
            &roots,
        )
        .unwrap();
        let engine = Engine::new(policy);
        let reach = &engine.policy().grants(&Principal::Agent).unwrap().net;
        for (scheme, host, port, expected) in [
            ("https", "api.example.com", 443, Some((0, ": exact"))),
            ("https", "other.example.com", 443, Some((1, ": suffix"))),
            ("http", "api.example.com", 8080, Some((2, ""))),
            ("https", "example.com", 443, None),
            ("https", "api.example.com", 8080, None),
            ("http", "api.example.com", 80, None),
        ] {
            let req = net_request(scheme, host, port);
            let principal = engine.decide(&Principal::Agent, &req);
            assert_eq!(principal, net_decision(reach, &req, "agent"));
            assert_eq!(principal.scope, Scope::Net);
            if let Some((index, reason)) = expected {
                assert!(principal.allow);
                assert_eq!(principal.rule.as_ref(), Some(&reach[index].rule));
                assert_eq!(principal.explanation, format!("agent.net[{index}]{reason}"));
            } else {
                assert!(!principal.allow);
                assert_eq!(principal.rule, None);
                assert_eq!(
                    principal.explanation,
                    format!("agent has no net grant covering {scheme}://{host}:{port}")
                );
            }
        }
    }

    #[test]
    fn mcp_net_decisions_use_only_the_supplied_reach() {
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse(
            r#"
version = 1
[agent]
net = [{ host = "api.search.example" }, { host = "other.search.example" }]
"#,
            &roots,
        )
        .unwrap();
        let engine = Engine::new(policy);
        let agent = Principal::Agent;
        let reach = &engine.policy().grants(&agent).unwrap().net[..1];
        let req = net_request("https", "api.search.example", 443);
        assert!(engine.decide(&agent, &req).allow);
        assert!(net_decision(reach, &req, "`search`").allow);
        assert!(!net_decision(&[], &req, "`search`").allow);
        let unknown = Principal::Plugin(PluginId::new("unknown").unwrap());
        assert_eq!(
            engine.decide(&unknown, &req),
            Decision::deny(
                Scope::Net,
                "plugin.unknown has no grants in the policy".into()
            )
        );
        let other = net_request("https", "other.search.example", 443);
        assert!(engine.decide(&agent, &other).allow);
        assert_eq!(
            net_decision(reach, &other, "`search`"),
            Decision::deny(
                Scope::Net,
                "`search` has no net grant covering https://other.search.example:443".into()
            )
        );
        assert_eq!(
            net_decision(reach, &Request::Plain(Scope::Tool), "`search`"),
            Decision::deny(Scope::Net, "Plain(Tool) is not a network request".into())
        );
    }

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
    fn launch_permission_is_exact_and_independent_of_exec_and_other_principals() {
        let (ws, _) = engine();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        let agent = Principal::Agent;
        let plugin = Principal::Plugin(PluginId::new("shell").unwrap());
        let launch = Request::RunBinary { profile: "fixture" };
        let cwd = Location::new(RootName::workspace(), RelPath::default());
        let ordinary = Request::Exec {
            argv: &[],
            cwd: &cwd,
        };
        let base = "version = 1\n[agent]\nrun-binary = [{ profile = 'fixture' }]\n[plugin.shell]\nexec = {}\n";
        let granted = Engine::new(Policy::parse(base, &roots).unwrap());
        assert_eq!(launch.args(), json!({"profile":"fixture"}));
        let decision = granted.decide(&agent, &launch);
        assert!(decision.allow);
        assert_eq!(decision.explanation, "agent.run-binary[0]");
        assert!(
            !granted
                .decide(&agent, &Request::RunBinary { profile: "Fixture" })
                .allow
        );
        assert!(!granted.decide(&agent, &ordinary).allow);
        assert!(granted.decide(&plugin, &ordinary).allow);
        assert!(!granted.decide(&plugin, &launch).allow);

        let revoked = Engine::new(
            Policy::layer(
                base,
                "version = 1\n[agent]\nrun-binary = ['-fixture']\n",
                &roots,
            )
            .unwrap(),
        );
        assert!(!revoked.decide(&agent, &launch).allow);
        assert!(
            Policy::parse(
                "version = 1\n[agent]\nrun-binary = [{profile = '*'}]\n",
                &roots
            )
            .is_err()
        );
    }

    #[test]
    fn a_transcript_query_against_another_conversation_needs_a_workspace_grant() {
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
    fn fs_allow_and_exclude() {
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
            ".git/config",
            ".git/hooks/pre-commit",
            "sub/.git/hooks/pre-commit",
            ".git/modules/x/config",
            ".git/config.worktree",
            ".jj/repo/config.toml",
            ".jj/repo/store/git_target",
            ".jj/repo/store/git/config",
            ".jj/repo/store/git/hooks/pre-push",
            ".git/config.lock",
            ".git/objects/ab/cd",
        ] {
            let d = e.decide(&a, &Request::FsWrite { loc: &ws(path) });
            assert!(d.allow, "{path}: {}", d.explanation);
        }
        let d = e.decide(
            &a,
            &Request::FsWrite {
                loc: &ws(".git/config"),
            },
        );
        assert_eq!(
            d.rule.as_ref().map(|r| r.as_str()),
            Some("agent.fs-write[0]")
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
    fn a_grant_without_a_token_budget_does_not_cap_spending() {
        let ws = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), ws.path().into())], &[]).unwrap();
        let policy = Policy::parse(
            "version = 1\n[agent]\nmodel-invoke = { why = \"workers\" }\n",
            &roots,
        )
        .unwrap();
        assert_eq!(
            policy
                .grants(&Principal::Agent)
                .unwrap()
                .model_invoke
                .as_ref()
                .unwrap()
                .tokens_per_session,
            None
        );
        let engine = Engine::new(policy);
        for tokens in [1_000, 500_000_000] {
            let d = engine.decide(&Principal::Agent, &Request::ModelInvoke { tokens });
            assert!(d.allow, "{}", d.explanation);
        }
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
