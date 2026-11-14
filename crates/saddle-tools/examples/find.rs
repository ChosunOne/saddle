//! file discovery uses only scoped metadata and reads (D133).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::search::Find;

export!(Find with_types_in bindings);
