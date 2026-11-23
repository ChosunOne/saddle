//! guest tool bindings, generated from the host's canonical WIT (D12, D127, D140).

use saddle::host::budgets::read as read_budgets;
use saddle_limits::{self as limits, Budgets};
use serde_json::{Value, from_str, from_value, to_value};

wit_bindgen::generate!({
    path: "../saddle-wit/wit",
    world: "read-tool",
    with: { "saddle:widgets/types@0.1.0": generate },
    pub_export_macro: true,
});

pub use exports::saddle::plugin::tool::{Definition, Guest as Tool, Output};
pub use saddle::host::exec::{Command, Reply as ExecReply, run as execute};
pub use saddle::host::exec_tasks::{
    Task, cancel as cancel_task, inspect as inspect_task, list as list_tasks,
    set_wake as wake_task, start as start_task,
};
pub use saddle::host::fs_list::read_directory;
pub use saddle::host::fs_list::{Entry as DirectoryEntry, EntryKind, Page as DirectoryPage};
pub use saddle::host::fs_paths::label_root;
pub use saddle::host::fs_paths::resolve_path;
pub use saddle::host::fs_preimage::read as read_preimage;
pub use saddle::host::fs_read::read_file;
pub use saddle::host::fs_read::{Failure, Path as FilePath};
pub use saddle::host::fs_read_range::Chunk;
pub use saddle::host::fs_read_range::read_range;
pub use saddle::host::fs_write::{Change, Expectation, apply as apply_changes};
pub use saddle::host::http::{
    Header, Request as HttpRequest, Response as HttpResponse, get as http_get,
};
pub use saddle::host::run_binary::{Launch, run as run_binary};
pub use saddle::host::tool_result::emit_diff;
pub use saddle::host::tool_result::{ImageInfo, emit_image};
pub use saddle::host::transcript_read::query as query_transcript;
pub use saddle::host::web_search::{
    Snapshot as SearchSnapshot, last as last_search, search as search_web,
};

/// register host-originated lifecycle hooks; this API cannot publish harness events (D155).
pub mod harness {
    wit_bindgen::generate!({
        path: "../saddle-wit/wit",
        world: "harness-hook",
        pub_export_macro: true,
        export_macro_name: "export_harness_hooks",
    });
    pub use exports::saddle::plugin::observer::{Event, Guest as Hooks, Kind};
}

/// register custom-event subscriptions independently of harness hooks (D155).
pub mod events {
    wit_bindgen::generate!({
        path: "../saddle-wit/wit",
        world: "plugin-event-handler",
        pub_export_macro: true,
        export_macro_name: "export_plugin_events",
    });
    pub use crate::saddle::host::plugin_events::emit;
    pub use exports::saddle::plugin::plugin_events::{Event, Guest as Handler, Subscription};
}

/// append attributed facts, never system instructions or history rewrites (D156).
pub mod context {
    pub use crate::saddle::host::context_contribute::{
        Body, Contribution, Diagnostic, Severity, append,
    };
    pub fn note(text: &str) -> Result<String, String> {
        append(&Contribution {
            body: Body::Note(text.into()),
            corrects: None,
        })
    }
    pub fn diagnostic(value: Diagnostic) -> Result<String, String> {
        append(&Contribution {
            body: Body::Diagnostic(value),
            corrects: None,
        })
    }
}

/// bounded atomic private storage, not filesystem access (D156).
pub mod persist {
    pub use crate::saddle::host::persist::{Change, Expectation, apply, get};
    pub fn put(key: &str, value: &[u8]) -> Result<(), String> {
        apply(&[Change {
            key: key.into(),
            value: Some(value.into()),
            expected: Expectation::Any,
        }])
        .map(|_| ())
    }
    pub fn remove(key: &str) -> Result<(), String> {
        apply(&[Change {
            key: key.into(),
            value: None,
            expected: Expectation::Any,
        }])
        .map(|_| ())
    }
    pub fn compare_exchange(
        key: &str,
        expected: Option<&[u8]>,
        value: Option<&[u8]>,
    ) -> Result<bool, String> {
        apply(&[Change {
            key: key.into(),
            value: value.map(Into::into),
            expected: expected.map_or(Expectation::Missing, |bytes| {
                Expectation::Value(bytes.into())
            }),
        }])
    }
}

/// isolated host-provider requests; replies never execute tools (D156).
pub mod model {
    pub use crate::saddle::host::model_invoke::{Message, Reply, Request, Role, Usage, invoke};
}

/// pre-tool checks can only abstain or deny (D156).
pub mod veto {
    wit_bindgen::generate!({
        path: "../saddle-wit/wit", world: "veto-hook",
        pub_export_macro: true, export_macro_name: "export_veto",
    });
    pub use exports::saddle::plugin::veto::{Decision, Guest as Veto, PendingToolCall};
}

/// host-framed widgets, with no input or arbitrary terminal access (D156).
pub mod ui {
    wit_bindgen::generate!({
        path: "../saddle-wit/wit", world: "ui-hook",
        pub_export_macro: true, export_macro_name: "export_ui",
        with: { "saddle:widgets/types@0.1.0": crate::saddle::widgets::types },
    });
    pub use crate::saddle::host::ui::{clear, publish};
    pub use crate::saddle::widgets::types::{Pair, Panel, Progress, Span, Style, Widget};
    pub use exports::saddle::plugin::ui::Guest as Ui;
}

/// read resource budgets from the host before a standard guest workflow starts (D145).
/// fresh instances prevent cached settings from surviving a config reload.
pub fn configure_limits() -> Result<(), String> {
    limits::initialize(parse_budgets(&read_budgets())?);
    Ok(())
}

fn parse_budgets(text: &str) -> Result<Budgets, String> {
    let mut value: Value = from_str(text).map_err(|e| format!("invalid host budgets: {e}"))?;
    // Ignore future metadata keys so a host upgrade need not rebuild a pinned guest.
    // Protected config parsing remains strict; only this public snapshot is extensible.
    let known = to_value(Budgets::default()).map_err(|e| e.to_string())?;
    if let (Some(values), Some(known)) = (value.as_object_mut(), known.as_object()) {
        values.retain(|name, _| known.contains_key(name));
    }
    let budgets: Budgets = from_value(value).map_err(|e| format!("invalid host budgets: {e}"))?;
    budgets.validate()?;
    Ok(budgets)
}

/// preserve the ordinary denial shape across standard filesystem workflows (D147).
pub fn failure_message(error: Failure) -> String {
    match error {
        Failure::Denied(why) => format!("denied: {why}"),
        Failure::InvalidPath(why) | Failure::Io(why) => why,
        Failure::TooLarge => "configured resource limit exceeded".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_is_forward_compatible_but_invalid_values_are_not_accepted() {
        let profile =
            parse_budgets(r#"{"plugin-input-bytes":1048576,"future-budget":42}"#).unwrap();
        assert_eq!(profile.plugin_input_bytes, 1048576);
        assert!(parse_budgets(r#"{"range-bytes":0}"#).is_err());
        assert!(parse_budgets("not JSON").is_err());
    }
}
