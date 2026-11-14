//! temporary native adapter to the guest-owned search workflow and protected transport (D143).

use std::time::{Duration, Instant};

use anyhow::{Error, Result};
use saddle_cap::Principal;
use saddle_sdk::SearchSnapshot;
use saddle_tools::websearch::{Transport, search_with};
use saddle_wit::saddle::host::web_search::Snapshot;
use serde_json::Value;

use super::ToolOutput;
use crate::search_transport;
use crate::session::Session;

struct NativeTransport<'a> {
    session: &'a mut Session,
    fatal: Option<Error>,
}

fn snapshot(value: Snapshot) -> SearchSnapshot {
    SearchSnapshot {
        query: value.query,
        count: value.count,
        via: value.via,
        provider: value.provider,
        status: value.status,
        body: value.body,
    }
}

impl Transport for NativeTransport<'_> {
    fn search(&mut self, query: &str, count: u32) -> Result<SearchSnapshot, String> {
        match search_transport::search(
            self.session,
            &Principal::Agent,
            "native-websearch",
            query.into(),
            count,
            Instant::now() + Duration::from_secs(30),
        ) {
            Ok(result) => result.map(snapshot),
            Err(error) => {
                self.fatal = Some(error);
                Err("search host failure".into())
            }
        }
    }
    fn last(&mut self) -> Result<Option<SearchSnapshot>, String> {
        match search_transport::last(self.session, &Principal::Agent, "native-websearch") {
            Ok(result) => result.map(|value| value.map(snapshot)),
            Err(error) => {
                self.fatal = Some(error);
                Err("search host failure".into())
            }
        }
    }
}

pub fn search(session: &mut Session, input: &Value) -> Result<ToolOutput> {
    let mut transport = NativeTransport {
        session,
        fatal: None,
    };
    let result = search_with(&input.to_string(), &mut transport);
    if let Some(error) = transport.fatal {
        return Err(error);
    }
    Ok(match result {
        Ok(text) => ToolOutput::ok(text),
        Err(why) => ToolOutput::err(why),
    })
}
