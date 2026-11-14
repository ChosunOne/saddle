//! the standard shell over delegated execution (D142).
#![cfg(target_arch = "wasm32")]
use saddle_sdk::{self as bindings, export};
use saddle_tools::bash::Bash;
export!(Bash with_types_in bindings);
