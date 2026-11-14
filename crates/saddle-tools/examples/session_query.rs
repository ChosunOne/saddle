//! the standard transcript-query tool, without database or filesystem access (D137).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::transcript::SessionQuery;

export!(SessionQuery with_types_in bindings);
