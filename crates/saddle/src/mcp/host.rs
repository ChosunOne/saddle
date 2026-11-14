//! D114: what saddle answers when a server asks it something. A server may send requests of its
//! own once the handshake is done, and the three the spec defines are `roots/list`, `sampling` and
//! `elicitation`. The first is answerable from what the session already knows — the roots a
//! sandboxed command may reach — and the other two are refused by name: `-32601`, not silence, so a
//! server that tries one learns why rather than waiting for an answer that will never come.
//!
//! `initialize` declares no capability for them, so a conforming server will not ask. This is what
//! happens when one asks anyway, which is the case worth having a policy for.

use serde_json::{Value, json};

use crate::session::Session;

/// One answer to a server's request, or the reason there is none. The roots are handed in rather
/// than looked up, because a stdio client holds a pipe and not a session (D114).
pub fn answer(roots: &[Value], method: &str) -> Result<Value, String> {
    match method {
        "roots/list" => Ok(json!({ "roots": roots })),
        "ping" => Ok(json!({})),
        // Named rather than ignored: these would need the model or the person in the loop, and a
        // server that asks for either is asking for something this client does not offer.
        other => Err(format!(
            "`{other}` is not supported by this client: saddle declares no sampling, elicitation \
             or resource capabilities"
        )),
    }
}

/// The roots a sandboxed command may reach, which is the honest answer to `roots/list`: it is the
/// list the session would lower into a spec, not a wish list.
pub fn roots(session: &Session) -> Vec<Value> {
    let mut out = Vec::new();
    for (name, path) in session.roots().iter() {
        out.push(json!({
            "uri": format!("file://{}", path.display()),
            "name": name.as_str(),
        }));
    }
    out
}
