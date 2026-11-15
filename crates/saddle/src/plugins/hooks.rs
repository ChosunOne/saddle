//! disjoint host and plugin event channels; bounded, non-reentrant delivery (D155).

use std::collections::VecDeque;
use std::mem;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use saddle_cap::{Principal, Scope};
use saddle_limits::Budgets;
use saddle_policy::{Kind, Request};
use saddle_wit::events::exports::saddle::plugin::plugin_events::Event as PluginEvent;
use saddle_wit::harness::exports::saddle::plugin::observer::{
    Event as HarnessEvent, Kind as HarnessKind,
};
use serde_json::{Value, json};

use super::host::{Job, Reply, execute};
use super::{HostFailure, Registry};
use crate::session::Session;
use crate::tools::timeout;

/// session-owned pending custom events; reload never carries these across authority changes.
#[derive(Default)]
pub struct Queue {
    pending: VecDeque<PluginEvent>,
}

pub(super) fn valid_name(name: &str, budgets: &Budgets) -> bool {
    !name.is_empty()
        && name.len() <= budgets.event_name_bytes
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// publication never accepts a channel or emitter from guest arguments.
pub(super) fn publish(
    session: &mut Session,
    who: &Principal,
    call: &str,
    name: &str,
    data: String,
    invoking: bool,
    deadline: Instant,
) -> Result<Result<(), String>> {
    let Principal::Plugin(id) = who else {
        return Ok(Err("only plugins can publish custom events".into()));
    };
    let request = Request::Hook {
        scope: Scope::EventPublish,
        operation: "publish",
        channel: "plugin",
        publisher: id.as_str(),
        name,
        data: &data,
    };
    let mut decision = session.engine.decide(who, &request);
    let limits = &session.setup.tool_limits;
    let invalid = if Instant::now() >= deadline || timeout::expired() {
        Some("plugin event publication timed out")
    } else if !invoking {
        Some("event publication is refused during registration")
    } else if !valid_name(name, limits)
        || data
            .len()
            .saturating_add(name.len())
            .saturating_add(id.as_str().len())
            > limits.event_bytes
    {
        Some("plugin event exceeds configured size or has an invalid name")
    } else if session.plugin_events.pending.len() >= limits.event_queue {
        Some("plugin event queue is full")
    } else {
        None
    };
    if let Some(why) = invalid {
        decision.allow = false;
        decision.explanation = why.into();
    }
    session
        .audit
        .decision_for_call(who, &request, &decision, call)?;
    if !decision.allow {
        return Ok(Err(decision.explanation));
    }
    session.plugin_events.pending.push_back(PluginEvent {
        publisher: id.to_string(),
        name: name.into(),
        data,
    });
    Ok(Ok(()))
}

/// emit a host-only lifecycle event, then drain custom events queued by its handlers.
/// errors from the host/audit remain fatal; a broken observer cannot veto an operation.
pub fn harness(session: &mut Session, kind: HarnessKind, data: Value) -> Result<()> {
    let mut registry = mem::take(&mut session.plugins);
    let result = registry.dispatch_hooks(session, kind, data.to_string());
    session.plugins = registry;
    result.map_err(|error| {
        if error.is::<HostFailure>() {
            error
        } else {
            HostFailure(error).into()
        }
    })
}

impl Registry {
    fn dispatch_hooks(
        &mut self,
        session: &mut Session,
        kind: HarnessKind,
        data: String,
    ) -> Result<()> {
        let limit = session.setup.tool_limits.event_deliveries;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(
                session.setup.tool_limits.event_seconds as u64,
            ))
            .context("event deadline overflow")?;
        let deadline = timeout::deadline().map_or(deadline, |current| current.min(deadline));
        timeout::using(Some(deadline), || {
            let mut deliveries = 0;
            let targets: Vec<_> = self
                .loaded
                .iter()
                .filter(|(_, loaded)| loaded.harness.contains(&kind))
                .map(|(id, _)| id.clone())
                .collect();
            for id in targets {
                if deliveries >= limit || Instant::now() >= deadline {
                    session.plugin_events.pending.clear();
                    session.audit.event(
                        Kind::Decision,
                        None,
                        None,
                        Some(false),
                        json!({"operation":"event-drain","deliveries":deliveries}),
                        "event dispatch budget exhausted; remaining deliveries dropped",
                    )?;
                    return Ok(());
                }
                let who = Principal::Plugin(id.clone());
                let name = match kind {
                    HarnessKind::TurnStart => "turn-start",
                    HarnessKind::TurnEnd => "turn-end",
                    HarnessKind::ToolStart => "tool-start",
                    HarnessKind::ToolEnd => "tool-end",
                };
                let request = Request::Hook {
                    scope: Scope::TranscriptRead,
                    operation: "observe",
                    channel: "harness",
                    publisher: "saddle",
                    name,
                    data: &data,
                };
                let decision = session.engine.decide(&who, &request);
                let call = self.next_call();
                session
                    .audit
                    .decision_for_call(&who, &request, &decision, &call)?;
                if !decision.allow {
                    continue;
                }
                if data.len() > session.setup.tool_limits.event_bytes {
                    session.audit.event(
                        Kind::Decision,
                        Some(&who),
                        Some(Scope::TranscriptRead),
                        Some(false),
                        request.args(),
                        "harness event exceeds configured event size",
                    )?;
                    continue;
                }
                let prepared = self
                    .loaded
                    .get(&id)
                    .context("hook plugin disappeared")?
                    .prepared
                    .clone();
                deliveries += 1;
                let result = execute(
                    prepared,
                    session,
                    Job::Observe(HarnessEvent {
                        kind,
                        data: data.clone(),
                    }),
                    &call,
                )?;
                record_result(session, &who, Scope::TranscriptRead, &call, result)?;
            }
            while let Some(event) = session.plugin_events.pending.pop_front() {
                let targets: Vec<_> =
                    self.loaded
                        .iter()
                        .filter(|(_, loaded)| {
                            loaded.subscriptions.iter().any(|sub| {
                                sub.publisher == event.publisher && sub.name == event.name
                            })
                        })
                        .map(|(id, _)| id.clone())
                        .collect();
                for id in targets {
                    if deliveries >= limit || Instant::now() >= deadline {
                        session.plugin_events.pending.clear();
                        session.audit.event(
                            Kind::Decision,
                            None,
                            None,
                            Some(false),
                            json!({"operation":"event-drain","deliveries":deliveries}),
                            "event dispatch budget exhausted; remaining deliveries dropped",
                        )?;
                        return Ok(());
                    }
                    let who = Principal::Plugin(id.clone());
                    let request = Request::Hook {
                        scope: Scope::EventSubscribe,
                        operation: "receive",
                        channel: "plugin",
                        publisher: &event.publisher,
                        name: &event.name,
                        data: &event.data,
                    };
                    let decision = session.engine.decide(&who, &request);
                    let call = self.next_call();
                    session
                        .audit
                        .decision_for_call(&who, &request, &decision, &call)?;
                    if !decision.allow {
                        continue;
                    }
                    let prepared = self
                        .loaded
                        .get(&id)
                        .context("event plugin disappeared")?
                        .prepared
                        .clone();
                    deliveries += 1;
                    let result = execute(prepared, session, Job::Receive(event.clone()), &call)?;
                    record_result(session, &who, Scope::EventSubscribe, &call, result)?;
                }
            }
            Ok(())
        })
    }
}

fn record_result(
    session: &mut Session,
    who: &Principal,
    scope: Scope,
    call: &str,
    result: Result<Reply, String>,
) -> Result<()> {
    let (ok, reason) = match result {
        Ok(Reply::Handled(Ok(()))) => (true, "event handler completed".to_owned()),
        Ok(Reply::Handled(Err(why))) | Err(why) => (
            false,
            format!("event handler failed: {why}; committed effects are not rolled back"),
        ),
        Ok(_) => (false, "event handler returned the wrong reply".into()),
    };
    session.audit.event(
        Kind::Decision,
        Some(who),
        Some(scope),
        Some(ok),
        json!({"operation":"event-handler-complete","call_id":call}),
        &reason,
    )?;
    Ok(())
}
