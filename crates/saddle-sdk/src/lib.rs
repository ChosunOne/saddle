//! guest bindings for a read-only tool (D12, D127), generated from the host's canonical WIT.

wit_bindgen::generate!({
    path: "../saddle-wit/wit",
    world: "read-tool",
    pub_export_macro: true,
});

pub use exports::saddle::plugin::tool::{Definition, Guest as Tool, Output};
#[cfg(not(feature = "tool-paths"))]
pub use saddle::host::fs_list::read_directory;
pub use saddle::host::fs_list::{Entry as DirectoryEntry, EntryKind, Page as DirectoryPage};
#[cfg(not(feature = "tool-paths"))]
pub use saddle::host::fs_paths::resolve_path;
#[cfg(not(feature = "tool-paths"))]
pub use saddle::host::fs_read::read_file;
pub use saddle::host::fs_read::{Failure, Path as FilePath};
pub use saddle::host::fs_read_range::Chunk;
#[cfg(not(feature = "tool-paths"))]
pub use saddle::host::fs_read_range::read_range;
pub use saddle::host::tool_paths as managed_paths;
pub use saddle::host::tool_result::{ImageInfo, emit_image};
pub use saddle::host::transcript_read::query as query_transcript;

#[cfg(feature = "tool-paths")]
mod paths;
#[cfg(feature = "tool-paths")]
pub use paths::{read_directory, read_file, read_range, resolve_path};
