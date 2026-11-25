//! the standard question tool: the schema and the answer's shape live in the guest, the question
//! surface and the wait in the host (D197).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::ask::Ask;

export!(Ask with_types_in bindings);
