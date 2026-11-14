//! Per-call decisions (D10, D22, D23). Every decision is a lookup over grants: no ordering, no
//! fallthrough, and a one-sentence explanation either way.

use std::collections::BTreeMap;
use std::sync::Mutex;

use globset::GlobSet;
use saddle_cap::policy::{FsGrant, NetGrant, POLICY_FILE, exclude_set, net_allows};
use saddle_cap::{Location, Policy, Principal, RelPath, RootName, RuleId, Scope};
use serde_json::{Value, json};

/// A request with its arguments already resolved.
#[derive(Debug, Clone)]
pub enum Request<'a> {
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
    TranscriptQuery {
        session: &'a str,
        sql: &'a str,
    },
    /// Scopes without arguments: transcript-read, context-contribute, tool, veto, ui, persist.
    Plain(Scope),
}

impl Request<'_> {
    pub fn scope(&self) -> Scope {
        match self {
            Request::FsRead { .. } => Scope::FsRead,
            Request::FsWrite { .. } => Scope::FsWrite,
            Request::Exec { .. } => Scope::Exec,
            Request::Net { .. } => Scope::Net,
            Request::ModelInvoke { .. } => Scope::ModelInvoke,
            Request::TranscriptQuery { .. } => Scope::TranscriptRead,
            Request::Plain(s) => *s,
        }
    }

    /// Arguments as they go into the audit log. Query values are redacted (D24).
    pub fn args(&self) -> Value {
        match self {
            Request::FsRead { loc } | Request::FsWrite { loc } => {
                json!({ "path": loc.to_string() })
            }
            Request::Exec { argv, cwd } => json!({ "argv": argv, "cwd": cwd.to_string() }),
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
            Request::TranscriptQuery { session, sql } => json!({ "session": session, "sql": sql }),
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

/// Git paths that run code as the user later: hooks and config, in the main repository and in
/// submodules.
pub const RESERVED_GIT: &[&str] = &[
    ".git/hooks",
    ".git/config",
    ".git/modules/**/hooks",
    ".git/modules/**/config",
];

impl Reserved {
    pub fn new() -> Self {
        let patterns: Vec<String> = RESERVED_GIT.iter().map(|p| format!("**/{p}")).collect();
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
        }
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn reserved(&self) -> &Reserved {
        &self.reserved
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
            Request::Exec { .. } => match &grants.exec {
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
            Request::TranscriptQuery { .. } => {
                self.decide(who, &Request::Plain(Scope::TranscriptRead))
            }
            Request::Plain(s) => match grants.plain.get(s) {
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
        ] {
            let d = e.decide(&a, &Request::FsWrite { loc: &ws(path) });
            assert!(!d.allow, "{path}");
            assert!(
                d.explanation.contains("reserved"),
                "{path}: {}",
                d.explanation
            );
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
