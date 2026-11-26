#![cfg(target_arch = "wasm32")]

use saddle_sdk::events::{
    self as event_bindings, Event as PluginEvent, Handler, Subscription, emit, export_plugin_events,
};
use saddle_sdk::harness::{
    self as harness_bindings, Event as HarnessEvent, Hooks, Kind, export_harness_hooks,
};
use saddle_sdk::{Change, Expectation, apply_changes, read_file, resolve_path};
use serde_json::Value;

struct Example;

impl Hooks for Example {
    fn subscriptions() -> Vec<Kind> {
        vec![Kind::TurnStart]
    }

    fn observe(event: HarnessEvent) -> Result<(), String> {
        let data: Value = serde_json::from_str(&event.data).map_err(|error| error.to_string())?;
        let path = data["prompt"].as_str().ok_or("missing prompt")?;
        if path == "overflow" {
            emit("one", "first")?;
            return emit("two", "second");
        }
        let text = if path == "cycle" {
            "cycle".into()
        } else {
            let path = resolve_path(path).map_err(|error| format!("{error:?}"))?;
            String::from_utf8(read_file(&path).map_err(|error| format!("{error:?}"))?)
                .map_err(|error| error.to_string())?
        };
        emit("turn-start", &text)
    }
}

impl Handler for Example {
    fn subscriptions() -> Vec<Subscription> {
        vec![Subscription {
            publisher: "hooks".into(),
            name: "turn-start".into(),
        }]
    }

    fn receive(event: PluginEvent) -> Result<(), String> {
        if event.data == "cycle" {
            return emit("turn-start", &event.data);
        }
        apply_changes(&[Change {
            path: "hook-output".into(),
            content: Some(
                format!("{}:{}:{}", event.publisher, event.name, event.data).into_bytes(),
            ),
            expected: Expectation::Any,
        }])
    }
}

export_harness_hooks!(Example with_types_in harness_bindings);
export_plugin_events!(Example with_types_in event_bindings);
