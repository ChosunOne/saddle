//! the standard configuration-reference tool: text compiled into the guest, no filesystem or
//! network access (D171).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::config_help::ConfigHelp;

export!(ConfigHelp with_types_in bindings);
