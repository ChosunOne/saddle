//! complete extension SDK fixture; every effect still requires its own runtime grant (D156).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::ui::{self as ui_bindings, Panel, Progress, Span, Style, Ui, Widget, export_ui};
use saddle_sdk::veto::{self as veto_bindings, Decision, PendingToolCall, Veto, export_veto};
use saddle_sdk::{self as bindings, Definition, Output, Tool, export};
use saddle_sdk::{context, model, persist, ui};
use serde_json::{Value, json};
use std::hint::spin_loop;

struct Features;
impl Tool for Features {
    fn describe() -> Definition {
        Definition { name:"probe".into(), description:"exercise separately granted extension services".into(), input_schema:json!({"type":"object","properties":{"action":{"type":"string"},"key":{"type":"string"},"value":{"type":"string"},"corrects":{"type":"string"},"expected":{"type":"string"}},"required":["action"],"additionalProperties":false}).to_string() }
    }
    fn invoke(arguments: String) -> Output {
        let result = run(&arguments);
        match result {
            Ok(text) => Output {
                text,
                is_error: false,
            },
            Err(text) => Output {
                text,
                is_error: true,
            },
        }
    }
}
fn run(arguments: &str) -> Result<String, String> {
    let args: Value = serde_json::from_str(arguments).map_err(|error| error.to_string())?;
    let key = args["key"].as_str().unwrap_or("count");
    let value = args["value"].as_str().unwrap_or("1");
    match args["action"].as_str().ok_or("missing action")? {
        "put" => {
            persist::put(key, value.as_bytes())?;
            Ok("stored".into())
        }
        "get" => Ok(persist::get(key)?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or("missing".into())),
        "remove" => {
            persist::remove(key)?;
            Ok("removed".into())
        }
        "cas" => Ok(persist::compare_exchange(
            key,
            args["expected"].as_str().map(str::as_bytes),
            Some(value.as_bytes()),
        )?
        .to_string()),
        "note" => context::append(&context::Contribution {
            body: context::Body::Note(value.into()),
            corrects: args["corrects"].as_str().map(Into::into),
        }),
        "diagnostic" => context::diagnostic(context::Diagnostic {
            root: "workspace".into(),
            path: value.into(),
            start_line: 1,
            end_line: 1,
            severity: context::Severity::Warning,
            message: "fixture diagnostic".into(),
        }),
        "model" => model::invoke(&model::Request {
            instructions: "you are an isolated plugin helper".into(),
            messages: vec![model::Message {
                role: model::Role::User,
                text: value.into(),
            }],
            max_output_tokens: 128,
        })
        .map(|reply| format!("{}|{}|{}", reply.text, reply.reasoning, reply.usage.output)),
        "panel" => {
            ui::publish(&Panel {
                title: value.into(),
                widgets: vec![
                    Widget::Text(vec![Span {
                        text: "working".into(),
                        style: Style::Emphasis,
                    }]),
                    Widget::Progress(Progress {
                        completed: 1,
                        total: 2,
                        label: "index".into(),
                    }),
                ],
            })?;
            Ok("published".into())
        }
        "clear" => {
            ui::clear()?;
            Ok("cleared".into())
        }
        _ => Err("unknown action".into()),
    }
}
impl Veto for Features {
    fn before_tool(call: PendingToolCall) -> Decision {
        let args: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
        if call.name == "write" && args["path"] == "blocked" {
            Decision::Deny("fixture veto: blocked path".into())
        } else if call.name == "write" && args["path"] == "veto-spin" {
            loop {
                spin_loop();
            }
        } else {
            Decision::Continue
        }
    }
}
impl Ui for Features {
    fn render() -> Result<Panel, String> {
        let count = persist::get("count")?
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or("0".into());
        let label = if persist::get("render-mutate")?.is_some() {
            format!("{:?}", context::note("illegal render contribution"))
        } else {
            count
        };
        Ok(Panel {
            title: "extension status".into(),
            widgets: vec![Widget::Text(vec![Span {
                text: label,
                style: Style::Muted,
            }])],
        })
    }
}
export!(Features with_types_in bindings);
export_veto!(Features with_types_in veto_bindings);
export_ui!(Features with_types_in ui_bindings);
