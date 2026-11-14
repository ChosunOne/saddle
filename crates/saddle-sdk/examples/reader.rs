//! a read-only guest; filesystem access goes through the SDK, never `std::fs` (D12, D127).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, Definition, FilePath, Output, Tool, export, read_file};
use serde_json::{Value, json};

struct Reader;

impl Tool for Reader {
    fn describe() -> Definition {
        Definition {
            name: "read-file".into(),
            description: "read one UTF-8 file beneath an explicitly granted root".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "root": {"type": "string"},
                    "relative": {"type": "string"},
                },
                "required": ["root", "relative"],
                "additionalProperties": false,
            })
            .to_string(),
        }
    }

    fn invoke(arguments: String) -> Output {
        let result = read(&arguments);
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

fn read(arguments: &str) -> Result<String, String> {
    let args: Value =
        serde_json::from_str(arguments).map_err(|e| format!("invalid arguments: {e}"))?;
    let root = args["root"].as_str().ok_or("missing root")?;
    let relative = args["relative"].as_str().ok_or("missing relative path")?;
    let bytes = read_file(&FilePath {
        root: root.into(),
        relative: relative.into(),
    })
    .map_err(|e| format!("read failed: {e:?}"))?;
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8".into())
}

export!(Reader with_types_in bindings);
