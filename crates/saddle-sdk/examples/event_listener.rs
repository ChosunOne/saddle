//! custom-only listener fixture: no tool, harness subscription or publishing authority (D155).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::events::{self as bindings, Event, Handler, Subscription, export_plugin_events};
use saddle_sdk::{Change, Expectation, apply_changes, configure_limits};

struct Listener;

impl Handler for Listener {
    fn subscriptions() -> Vec<Subscription> {
        vec![Subscription {
            publisher: "hooks".into(),
            name: "turn-start".into(),
        }]
    }

    fn receive(event: Event) -> Result<(), String> {
        configure_limits()?;
        apply_changes(&[Change {
            path: "listener-output".into(),
            content: Some(
                format!("{}:{}:{}", event.publisher, event.name, event.data).into_bytes(),
            ),
            expected: Expectation::Any,
        }])
    }
}

export_plugin_events!(Listener with_types_in bindings);
