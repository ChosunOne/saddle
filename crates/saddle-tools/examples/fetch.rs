#![cfg(target_arch = "wasm32")]
use saddle_sdk::{self as bindings, export};
use saddle_tools::fetch::Fetch;
export!(Fetch with_types_in bindings);
