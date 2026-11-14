//! guest tool bindings, generated from the host's canonical WIT (D12, D127, D140).

wit_bindgen::generate!({
    path: "../saddle-wit/wit",
    world: "read-tool",
    pub_export_macro: true,
});

pub use exports::saddle::plugin::tool::{Definition, Guest as Tool, Output};
pub use saddle::host::exec::{Command, Reply as ExecReply, run as execute};
pub use saddle::host::fs_list::read_directory;
pub use saddle::host::fs_list::{Entry as DirectoryEntry, EntryKind, Page as DirectoryPage};
pub use saddle::host::fs_paths::label_root;
pub use saddle::host::fs_paths::resolve_path;
pub use saddle::host::fs_read::read_file;
pub use saddle::host::fs_read::{Failure, Path as FilePath};
pub use saddle::host::fs_read_range::Chunk;
pub use saddle::host::fs_read_range::read_range;
pub use saddle::host::fs_write::{Change, Expectation, apply as apply_changes};
pub use saddle::host::http::{
    Header, Request as HttpRequest, Response as HttpResponse, get as http_get,
};
pub use saddle::host::tool_result::emit_diff;
pub use saddle::host::tool_result::{ImageInfo, emit_image};
pub use saddle::host::transcript_read::query as query_transcript;
pub use saddle::host::web_search::{
    Snapshot as SearchSnapshot, last as last_search, search as search_web,
};
