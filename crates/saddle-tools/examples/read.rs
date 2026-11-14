//! the standard text reader as a component, with no native filesystem access (D131).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::Read;

export!(Read with_types_in bindings);
