//! guest bindings for a read-only tool (D12, D127), generated from the host's canonical WIT.

wit_bindgen::generate!({
    path: "../saddle-wit/wit",
    world: "read-tool",
    pub_export_macro: true,
});

pub use exports::saddle::plugin::tool::{Definition, Guest as Tool, Output};
pub use saddle::host::fs_read::{Failure, Path as FilePath, read_file};
pub use saddle::host::fs_read_range::{Chunk, read_range};
