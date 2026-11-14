//! the standard editor over runtime-gated mutation primitives (D140).
#![cfg(target_arch = "wasm32")]
use saddle_sdk::{self as bindings, export};
use saddle_tools::mutation::Edit;
export!(Edit with_types_in bindings);
