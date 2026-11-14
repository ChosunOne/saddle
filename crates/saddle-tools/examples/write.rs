//! the standard writer over runtime-gated mutation primitives (D140).
#![cfg(target_arch = "wasm32")]
use saddle_sdk::{self as bindings, export};
use saddle_tools::mutation::Write;
export!(Write with_types_in bindings);
