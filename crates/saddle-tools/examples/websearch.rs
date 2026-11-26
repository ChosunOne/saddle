#![cfg(target_arch = "wasm32")]
use saddle_sdk::{self as bindings, export};
use saddle_tools::websearch::Websearch;
export!(Websearch with_types_in bindings);
