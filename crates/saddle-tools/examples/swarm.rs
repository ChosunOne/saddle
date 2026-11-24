//! the standard swarm tool (D194).

#![cfg(target_arch = "wasm32")]

use saddle_sdk::{self as bindings, export};
use saddle_tools::team::Swarm;

export!(Swarm with_types_in bindings);
