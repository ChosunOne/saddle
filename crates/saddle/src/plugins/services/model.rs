//! isolated provider requests, shared parent quotas and reported-usage accounting (D156).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::provider::reasoning_text;
use anyhow::{Result, anyhow};
use saddle_cap::{Principal, Scope};
use saddle_policy::audit::sha256_hex;
use saddle_policy::{Kind, Request as PolicyRequest};
use saddle_wit::saddle::host::model_invoke::{Reply, Request, Role, Usage as GuestUsage};
use serde_json::json;

use crate::cancel;
use crate::provider::{Conversation, Item, Part, Sink, Usage};
use crate::session::Session;
use crate::tasks;
use crate::tools::timeout;

pub struct Used {
    pub principal: String,
    pub call_id: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub cost: Option<f64>,
    pub reasoning_tokens: u64,
}

struct Capture {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    bytes: usize,
    limit: usize,
}
impl Sink for Capture {
    fn text(&mut self, text: &str) {
        self.bytes = self.bytes.saturating_add(text.len());
    }
    fn reasoning(&mut self, text: &str) {
        self.bytes = self.bytes.saturating_add(text.len());
    }
    fn note(&mut self, _: &str) {}
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
            || Instant::now() >= self.deadline
            || self.bytes > self.limit
    }
}

pub(super) fn invoke(
    session: &mut Session,
    who: &Principal,
    call: &str,
    request: Request,
    deadline: Instant,
) -> Result<Result<Reply, String>> {
    let limits = &session.setup.tool_limits;
    let input_bytes = request.messages.iter().fold(
        request.instructions.len().saturating_add(32),
        |sum, message| sum.saturating_add(message.text.len()).saturating_add(32),
    );
    if input_bytes > limits.model_input_bytes
        || request.max_output_tokens == 0
        || request.max_output_tokens as usize > limits.model_output_tokens
    {
        return Ok(Err(
            "model request exceeds configured input/output limit".into()
        ));
    }
    let Some(mut provider) = session
        .extensions
        .provider
        .as_ref()
        .map(|provider| provider.twin())
    else {
        return Ok(Err(
            "model invocation requires an active session provider".into()
        ));
    };
    provider.config.max_tokens = Some(request.max_output_tokens);
    provider.response_bytes = Some(limits.model_reply_bytes);
    provider.retry_requests = false;
    let price = provider.price().filter(|price| {
        [
            price.input,
            price.output,
            price.cache_read.unwrap_or(price.input),
            price.cache_write.unwrap_or(price.input),
        ]
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    });
    let reserve = input_bytes as u64 + u64::from(request.max_output_tokens);
    let reserved_cost = price.map(|price| {
        input_bytes as f64
            * price
                .input
                .max(price.cache_read.unwrap_or(price.input))
                .max(price.cache_write.unwrap_or(price.input))
            + f64::from(request.max_output_tokens) * price.output
    });
    let admission = PolicyRequest::ModelReserve {
        tokens: reserve,
        cost: reserved_cost,
    };
    let decision = session.engine.decide(who, &admission);
    session
        .audit
        .decision_for_call(who, &admission, &decision, call)?;
    if !decision.allow {
        return Ok(Err(decision.explanation));
    }
    let items: Vec<Item> = request
        .messages
        .into_iter()
        .map(|message| match message.role {
            Role::User => Item::User(message.text),
            Role::Assistant => Item::Assistant(vec![Part::Text(message.text)]),
        })
        .collect();
    let provider_name = provider.name.clone();
    let max_bytes = limits.model_reply_bytes;
    session.audit.event(Kind::Decision, Some(who), Some(Scope::ModelInvoke), Some(true),json!({"operation":"model-invoke","call_id":call,"provider":provider_name,"model":provider.config.model,"input_bytes":input_bytes,"instructions_sha256":sha256_hex(request.instructions.as_bytes()),"messages_sha256":sha256_hex(&serde_json::to_vec(&items)?),"max_output_tokens":request.max_output_tokens}), "isolated model request admitted")?;
    let (send, receive) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    let worker = thread::spawn(move || {
        let conversation = Conversation {
            system: &request.instructions,
            tools: &[],
            items: &items,
            session: "",
        };
        let mut sink = Capture {
            cancelled: flag,
            deadline,
            bytes: 0,
            limit: max_bytes,
        };
        if sink.cancelled() {
            let _ = send.send(Err(anyhow!("model invocation cancelled before send")));
            return;
        }
        let result = timeout::using(Some(deadline), || {
            provider.complete(&conversation, &mut sink)
        });
        let _ = send.send(result);
    });
    let result = loop {
        if let Err(error) = tasks::service(session) {
            cancelled.store(true, Ordering::Relaxed);
            let _ = worker.join();
            return Err(error);
        }
        if Instant::now() >= deadline || cancel::requested() {
            cancelled.store(true, Ordering::Relaxed);
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => break result,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                break Err(anyhow!("model request worker stopped"));
            }
        }
    };
    let _ = worker.join();
    let reply = match result {
        Ok(reply) => reply,
        Err(error) => {
            return Ok(Err(format!(
                "model request failed; reservation retained: {error:#}"
            )));
        }
    };
    let total = reply
        .usage
        .input
        .saturating_add(reply.usage.output)
        .saturating_add(reply.usage.cache_read)
        .saturating_add(reply.usage.cache_write);
    let cost = if total == 0 {
        None
    } else {
        price
            .map(|price| price.cost(&reply.usage))
            .filter(|value| value.is_finite())
    };
    if total > 0 {
        session
            .engine
            .settle_model(who, reserve, reserved_cost, total, cost);
    }
    session.audit.event(Kind::Decision, Some(who), Some(Scope::ModelInvoke), Some(true),json!({"operation":"model-complete","call_id":call,"usage":reply.usage,"cost":cost,"reserved_tokens":reserve}), "model usage accounted; remote work cannot be recalled")?;
    session.extensions.usage.push(Used {
        principal: who.to_string(),
        call_id: call.into(),
        provider: provider_name,
        model: reply.model.clone(),
        usage: reply.usage,
        cost,
        reasoning_tokens: reply.reasoning_tokens,
    });
    if reply.calls().next().is_some() {
        return Ok(Err(
            "isolated model reply requested tools; none were executed".into(),
        ));
    }
    let text: String = reply
        .parts
        .iter()
        .filter_map(|part| {
            if let Part::Text(text) = part {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    let reasoning = reply
        .parts
        .iter()
        .filter_map(reasoning_text)
        .collect::<Vec<_>>()
        .join("\n\n");
    if text.len().saturating_add(reasoning.len()) > max_bytes {
        return Ok(Err("model reply exceeds configured byte limit".into()));
    }
    Ok(Ok(Reply {
        text,
        reasoning,
        model: reply.model,
        usage: GuestUsage {
            input: reply.usage.input,
            output: reply.usage.output,
            cache_read: reply.usage.cache_read,
            cache_write: reply.usage.cache_write,
            cost,
        },
    }))
}
