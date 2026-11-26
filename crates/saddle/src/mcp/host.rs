use serde_json::{Value, json};

use crate::session::Session;

pub fn answer(roots: &[Value], method: &str) -> Result<Value, String> {
    match method {
        "roots/list" => Ok(json!({ "roots": roots })),
        "ping" => Ok(json!({})),
        other => Err(format!(
            "`{other}` is not supported by this client: saddle declares no sampling, elicitation \
             or resource capabilities"
        )),
    }
}

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
