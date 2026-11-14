//! the standard patcher over runtime-gated mutation primitives (D140).
#![cfg(target_arch = "wasm32")]
use saddle_sdk::{self as bindings, export};
use saddle_tools::mutation::Patch;
export!(Patch with_types_in bindings);
