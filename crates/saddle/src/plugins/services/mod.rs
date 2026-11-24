//! explicit extension effects, each decided by the session's one engine (D156).

mod model;
pub use model::Used as ModelUsage;
pub mod panel;
mod persist;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use anyhow::Result;
use saddle_cap::{Principal, RelPath, RootName, Scope};
use saddle_loader::{Service, ServiceReply};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request};
use saddle_wit::saddle::host::context_contribute::{Body, Contribution as Draft, Severity};
use saddle_wit::saddle::host::persist::Expectation;
use serde_json::{Value, json};

use self::panel::PanelView;
use crate::cancel;
use crate::provider::{Contribution, Diagnostic, Item, Provenance, Provider};
use crate::session::{Session, session_id};
use crate::team;
use crate::tools::timeout;

#[derive(Default)]
pub struct State {
    pub active: bool,
    pub known: BTreeMap<String, String>,
    pub pending: VecDeque<Contribution>,
    pub panels: BTreeMap<String, PanelView>,
    pub rendered: BTreeSet<String>,
    pub provider: Option<Provider>,
    pub usage: Vec<model::Used>,
}

impl State {
    pub fn begin(&mut self, items: &[Item], provider: Option<Provider>) {
        self.active = true;
        self.provider = provider;
        self.known = items
            .iter()
            .filter_map(|item| match item {
                Item::Contribution(value) => {
                    Some((value.id.clone(), value.provenance.principal.clone()))
                }
                _ => None,
            })
            .collect();
    }
}

pub fn scope(service: &Service) -> Scope {
    match service {
        Service::Contribute(_) => Scope::ContextContribute,
        Service::PersistGet(_) | Service::PersistApply(_) => Scope::Persist,
        Service::Model(_) | Service::Worker(_) => Scope::ModelInvoke,
        Service::PublishPanel(_) | Service::ClearPanel => Scope::Ui,
    }
}

pub fn execute(
    session: &mut Session,
    who: &Principal,
    mut origin: Provenance,
    service: Service,
    invoking: bool,
    deadline: Instant,
) -> Result<Result<ServiceReply, String>> {
    origin.scope = scope(&service);
    let (operation, arguments) = match &service {
        Service::Contribute(value) => ("append", contribution_args(value)),
        Service::PersistGet(key) => ("get", json!({"key_sha256":sha256_hex(key.as_bytes())})),
        Service::PersistApply(changes) => (
            "apply",
            json!({"changes": changes.iter().map(|change| json!({"key_sha256":sha256_hex(change.key.as_bytes()),"bytes":change.value.as_ref().map(Vec::len),"sha256":change.value.as_ref().map(|v|sha256_hex(v)),"expected":match &change.expected { Expectation::Any => "any", Expectation::Missing => "missing", Expectation::Value(_) => "value" }})).collect::<Vec<_>>()}),
        ),
        Service::Worker(op) => ("worker", team::arguments(op)),
        Service::Model(_) => ("invoke", json!({})),
        Service::PublishPanel(_) => ("publish", json!({})),
        Service::ClearPanel => ("clear", json!({})),
    };
    let request = Request::PluginService {
        scope: origin.scope,
        operation,
        arguments: &arguments,
    };
    // Model admission additionally reserves parent/child quotas before a provider request.
    let mut decision = if matches!(service, Service::Model(_) | Service::Worker(_)) {
        session
            .engine
            .decide(who, &Request::ModelInvoke { tokens: 0 })
    } else {
        session.engine.decide(who, &request)
    };
    if !invoking || cancel::requested() || Instant::now() >= deadline || timeout::expired() {
        decision.allow = false;
        decision.explanation =
            "extension effects are refused during registration, cancellation or expiry".into();
    }
    session
        .audit
        .decision_for_call(who, &request, &decision, &origin.call_id)?;
    if !decision.allow {
        return Ok(Err(decision.explanation));
    }
    let scope = origin.scope;
    let call_id = origin.call_id.clone();
    let result = match service {
        Service::Contribute(value) => contribute(session, origin, value),
        Service::PersistGet(key) => {
            persist::get(session, who, &key, deadline).map(|result| result.map(ServiceReply::Value))
        }
        Service::PersistApply(changes) => persist::apply(session, who, changes, deadline)
            .map(|result| result.map(ServiceReply::Applied)),
        Service::Worker(op) => {
            team::manage(session, who, op, &origin).map(|r| r.map(ServiceReply::Worker))
        }
        Service::Model(request) => model::invoke(session, who, &origin.call_id, request, deadline)
            .map(|result| result.map(ServiceReply::Model)),
        Service::PublishPanel(value) => {
            match panel::validate(&origin.principal, value, &session.setup.tool_limits) {
                Ok(panel) => {
                    session.extensions.panels.insert(origin.principal, panel);
                    Ok(Ok(ServiceReply::Unit))
                }
                Err(why) => Ok(Err(why)),
            }
        }
        Service::ClearPanel => {
            session.extensions.panels.remove(&origin.principal);
            Ok(Ok(ServiceReply::Unit))
        }
    };
    if let Ok(Err(reason)) = &result {
        session.audit.event(
            Kind::Decision,
            Some(who),
            Some(scope),
            Some(false),
            json!({"operation":"extension-failed","service":operation,"call_id":call_id}),
            reason,
        )?;
    }
    result
}

fn severity(value: Severity) -> &'static str {
    match value {
        Severity::Information => "information",
        Severity::Warning => "warning",
        Severity::Error => "error",
    }
}

fn contribution_args(value: &Draft) -> Value {
    match &value.body {
        Body::Note(text) => {
            json!({"kind":"note","bytes":text.len(),"sha256":sha256_hex(text.as_bytes()),"corrects":value.corrects})
        }
        Body::Diagnostic(diagnostic) => {
            json!({"kind":"diagnostic","root":diagnostic.root,"path":diagnostic.path,"start":diagnostic.start_line,"end":diagnostic.end_line,"bytes":diagnostic.message.len(),"sha256":sha256_hex(diagnostic.message.as_bytes()),"corrects":value.corrects})
        }
    }
}

fn contribute(
    session: &mut Session,
    origin: Provenance,
    draft: Draft,
) -> Result<Result<ServiceReply, String>> {
    if !session.extensions.active {
        return Ok(Err("no active conversation for a contribution".into()));
    }
    if session.extensions.pending.len() >= session.setup.tool_limits.contribution_count {
        return Ok(Err("contribution queue is full".into()));
    }
    if draft
        .corrects
        .as_ref()
        .is_some_and(|id| session.extensions.known.get(id) != Some(&origin.principal))
    {
        return Ok(Err(
            "correction must reference this plugin's contribution on the active branch".into(),
        ));
    }
    let diagnostic = match &draft.body {
        Body::Diagnostic(value) => Some(value),
        _ => None,
    };
    if let Some(value) = diagnostic {
        let valid = RootName::new(&value.root)
            .ok()
            .is_some_and(|root| session.setup.roots.get(&root).is_some())
            && RelPath::new(&value.path).is_ok()
            && value.start_line > 0
            && value.end_line >= value.start_line;
        if !valid {
            return Ok(Err(
                "diagnostic requires a known root, relative path and valid line range".into(),
            ));
        }
    }
    let diagnostic = diagnostic.map(|value| Diagnostic {
        root: value.root.clone(),
        path: value.path.clone(),
        start_line: value.start_line,
        end_line: value.end_line,
        severity: severity(value.severity).into(),
        message: value.message.clone(),
    });
    let text = match draft.body {
        Body::Note(text) => text,
        Body::Diagnostic(value) => format!(
            "{} {}:{}:{}-{}: {}",
            severity(value.severity),
            value.root,
            value.path,
            value.start_line,
            value.end_line,
            value.message
        ),
    };
    if text.len() > session.setup.tool_limits.contribution_bytes {
        return Ok(Err("contribution exceeds configured byte limit".into()));
    }
    let id = session_id();
    let content = format!(
        "contribution {id}{}\n{text}",
        draft
            .corrects
            .as_ref()
            .map_or(String::new(), |old| format!(" corrects {old}"))
    );
    session
        .extensions
        .known
        .insert(id.clone(), origin.principal.clone());
    session.extensions.pending.push_back(Contribution {
        id: id.clone(),
        corrects: draft.corrects,
        diagnostic,
        provenance: origin,
        content,
    });
    Ok(Ok(ServiceReply::Contribution(id)))
}
