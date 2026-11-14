//! listing is guest presentation over bounded, policy-filtered metadata (D132).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::listing::Ls;

export!(Ls with_types_in bindings);
