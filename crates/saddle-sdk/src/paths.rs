//! optional routing to D66's separately gated tool-path services (D138).

use crate::managed_paths::{
    self, EntryKind as ManagedKind, Failure as ManagedFailure, Path as ManagedPath,
};
use crate::saddle::host::{fs_list, fs_paths, fs_read, fs_read_range};
use crate::{Chunk, DirectoryEntry, DirectoryPage, EntryKind, Failure, FilePath};

const PREFIX: &str = "@tool:";

fn failure(error: ManagedFailure) -> Failure {
    match error {
        ManagedFailure::Denied(why) => Failure::Denied(why),
        ManagedFailure::InvalidPath(why) => Failure::InvalidPath(why),
        ManagedFailure::Io(why) => Failure::Io(why),
        ManagedFailure::TooLarge => Failure::TooLarge,
    }
}

fn managed(path: &FilePath) -> Option<ManagedPath> {
    path.root.strip_prefix(PREFIX).map(|root| ManagedPath {
        root: root.into(),
        relative: path.relative.clone(),
    })
}

pub fn resolve_path(value: &str) -> Result<FilePath, Failure> {
    match fs_paths::resolve_path(value) {
        Ok(path) => Ok(path),
        Err(_) => managed_paths::resolve_path(value)
            .map(|path| FilePath {
                root: format!("{PREFIX}{}", path.root),
                relative: path.relative,
            })
            .map_err(failure),
    }
}

pub fn read_range(path: &FilePath, offset: u64, limit: u32) -> Result<Chunk, Failure> {
    if let Some(path) = managed(path) {
        managed_paths::read_range(&path, offset, limit)
            .map(|chunk| Chunk {
                bytes: chunk.bytes,
                eof: chunk.eof,
            })
            .map_err(failure)
    } else {
        fs_read_range::read_range(path, offset, limit)
    }
}

pub fn read_file(path: &FilePath) -> Result<Vec<u8>, Failure> {
    if managed(path).is_none() {
        return fs_read::read_file(path);
    }
    let chunk = read_range(path, 0, 65536)?;
    if !chunk.eof {
        return Err(Failure::TooLarge);
    }
    Ok(chunk.bytes)
}

pub fn read_directory(path: &FilePath, after: &str, limit: u32) -> Result<DirectoryPage, Failure> {
    if let Some(path) = managed(path) {
        managed_paths::read_directory(&path, after, limit)
            .map(|page| DirectoryPage {
                entries: page
                    .entries
                    .into_iter()
                    .map(|entry| DirectoryEntry {
                        name: entry.name,
                        kind: match entry.kind {
                            ManagedKind::File => EntryKind::File,
                            ManagedKind::Directory => EntryKind::Directory,
                            ManagedKind::Symlink => EntryKind::Symlink,
                            ManagedKind::Other => EntryKind::Other,
                        },
                    })
                    .collect(),
                total: page.total,
                skipped: page.skipped,
                more: page.more,
            })
            .map_err(failure)
    } else {
        fs_list::read_directory(path, after, limit)
    }
}
