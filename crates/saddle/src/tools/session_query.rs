//! temporary native adapter for the core transcript service (D92, D137).

use anyhow::Result;
use saddle_cap::Principal;
use serde_json::Value;

use super::{ToolOutput, str_field};
use crate::session::Session;
use crate::transcript;

pub fn run(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let sql = match str_field(input, "sql") {
        Ok(sql) => sql,
        Err(out) => return Ok(out),
    };
    Ok(
        match transcript::execute(session, &Principal::Agent, sql, None, None)? {
            Ok(value) => ToolOutput::ok(value.to_string()),
            Err(why) => ToolOutput::err(why),
        },
    )
}
