//! D20: landlock cannot exclude paths beneath a writable root, so a sandboxed command can write
//! the paths D23 reserves. The host snapshots them before each exec and puts them back after.
//!
//! All access goes through a cap-std `Dir` on the workspace, so a command that swaps a reserved
//! directory for a symlink cannot steer the restore outside the workspace.
//!
//! A workspace policy is no longer among these: it lives in the config directory (D42), which no
//! root contains, so the kernel protects it and the restore has nothing to defend.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use cap_std::fs::{Dir, Permissions, PermissionsExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    File { bytes: Vec<u8>, mode: u32 },
    Dir { mode: u32 },
    Symlink(PathBuf),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snapshot(BTreeMap<PathBuf, Entry>);

/// Directories inside a git dir that never hold hooks or config; skipped for speed.
const SKIP: &[&str] = &["objects", "refs", "logs", "lfs"];

/// The reserved paths of the workspace's own repository (D23), and whether each is a directory
/// recorded with everything in it. The engine refuses the same names at any depth; a command's
/// writes are put back here, at the top of the workspace.
const FIXED: &[(&str, bool)] = &[
    (".git/config", false),
    (".git/config.worktree", false),
    (".git/hooks", true),
    (".jj/repo/config.toml", false),
    (".jj/workspace-config.toml", false),
    (".jj/repo/store/git_target", false),
    (".jj/repo/store/git/config", false),
    (".jj/repo/store/git/hooks", true),
];

pub fn take(ws: &Dir) -> io::Result<Snapshot> {
    let mut snap = Snapshot::default();
    for (path, whole) in FIXED {
        record(ws, Path::new(path), &mut snap, *whole)?;
    }
    walk_worktrees(ws, &mut snap)?;
    walk_modules(ws, Path::new(".git/modules"), &mut snap)?;
    Ok(snap)
}

/// A linked worktree's git dir is `.git/worktrees/<name>`, and git reads its `config.worktree`
/// once `extensions.worktreeConfig` is on, which `git sparse-checkout` turns on.
fn walk_worktrees(ws: &Dir, snap: &mut Snapshot) -> io::Result<()> {
    let dir = Path::new(".git/worktrees");
    let entries = match ws.read_dir(dir) {
        Ok(e) => e,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = dir.join(entry?.file_name()).join("config.worktree");
        record(ws, &path, snap, false)?;
    }
    Ok(())
}

/// Submodule git dirs live at any depth under `.git/modules`; each has its own config and hooks.
fn walk_modules(ws: &Dir, dir: &Path, snap: &mut Snapshot) -> io::Result<()> {
    let entries = match ws.read_dir(dir) {
        Ok(e) => e,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let path = dir.join(&name);
        match name.to_str() {
            Some("config" | "config.worktree") => record(ws, &path, snap, false)?,
            Some("hooks") => record(ws, &path, snap, true)?,
            Some(n) if SKIP.contains(&n) => {}
            _ if entry.file_type()?.is_dir() => walk_modules(ws, &path, snap)?,
            _ => {}
        }
    }
    Ok(())
}

fn record(ws: &Dir, path: &Path, snap: &mut Snapshot, recurse: bool) -> io::Result<()> {
    let meta = match ws.symlink_metadata(path) {
        Ok(m) => m,
        Err(e)
            if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::NotADirectory =>
        {
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let mode = meta.permissions().mode();
    let ft = meta.file_type();
    if ft.is_symlink() {
        snap.0.insert(
            path.to_path_buf(),
            Entry::Symlink(ws.read_link_contents(path)?),
        );
    } else if ft.is_dir() {
        snap.0.insert(path.to_path_buf(), Entry::Dir { mode });
        if recurse {
            for entry in ws.read_dir(path)? {
                record(ws, &path.join(entry?.file_name()), snap, true)?;
            }
        }
    } else if ft.is_file() {
        snap.0.insert(
            path.to_path_buf(),
            Entry::File {
                bytes: ws.read(path)?,
                mode,
            },
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Created,
    Modified,
    Removed,
}

/// Compares the workspace against `before` and undoes every difference. Returns what changed.
pub fn restore(ws: &Dir, before: &Snapshot) -> io::Result<Vec<(PathBuf, Change)>> {
    let after = take(ws)?;
    let mut changes = Vec::new();

    // New entries, deepest first so a new directory is removed after its contents.
    for (path, entry) in after.0.iter().rev() {
        if before.0.contains_key(path) {
            continue;
        }
        remove(ws, path, entry)?;
        changes.push((path.clone(), Change::Created));
    }
    // Changed or missing entries, parents first.
    for (path, old) in &before.0 {
        let change = match after.0.get(path) {
            Some(now) if now == old => continue,
            Some(now) => {
                remove(ws, path, now)?;
                Change::Modified
            }
            None => Change::Removed,
        };
        put(ws, path, old)?;
        changes.push((path.clone(), change));
    }
    changes.sort();
    Ok(changes)
}

fn remove(ws: &Dir, path: &Path, entry: &Entry) -> io::Result<()> {
    let result = match entry {
        Entry::Dir { .. } => ws.remove_dir_all(path),
        Entry::File { .. } | Entry::Symlink(_) => ws.remove_file(path),
    };
    match result {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn put(ws: &Dir, path: &Path, entry: &Entry) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        ws.create_dir_all(parent)?;
    }
    match entry {
        Entry::File { bytes, mode } => {
            ws.write(path, bytes)?;
            ws.set_permissions(path, Permissions::from_mode(*mode))
        }
        Entry::Dir { mode } => {
            match ws.create_dir(path) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                _ => {}
            }
            ws.set_permissions(path, Permissions::from_mode(*mode))
        }
        Entry::Symlink(target) => ws.symlink(target, path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use saddle_cap::{Location, RelPath, RootName};
    use saddle_policy::Reserved;
    use std::fs;
    use std::os::unix::fs as unix_fs;

    #[test]
    fn hooks_and_config_are_put_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
        fs::create_dir_all(root.join(".git/modules/vendor/lib/hooks")).unwrap();
        fs::write(root.join(".git/config"), "[core]\n").unwrap();
        fs::write(root.join(".git/hooks/pre-commit.sample"), "sample").unwrap();
        fs::write(root.join(".git/modules/vendor/lib/config"), "[core]\n").unwrap();
        let ws = Dir::open_ambient_dir(root, cap_std::ambient_authority()).unwrap();
        let before = take(&ws).unwrap();

        fs::write(root.join(".git/config"), "[core]\nfsmonitor = evil\n").unwrap();
        fs::write(root.join(".git/hooks/pre-commit"), "#!/bin/sh\nevil").unwrap();
        fs::remove_file(root.join(".git/hooks/pre-commit.sample")).unwrap();
        fs::write(
            root.join(".git/modules/vendor/lib/hooks/post-checkout"),
            "evil",
        )
        .unwrap();
        // An ordinary file is not restored: only the reserved paths are (D42 no longer reserves a
        // workspace policy, because it is not read from the tree).
        fs::remove_file(root.join(".git/modules/vendor/lib/config")).unwrap();
        fs::write(root.join("src.rs"), "ordinary edit").unwrap();

        let changes = restore(&ws, &before).unwrap();
        let paths: Vec<String> = changes
            .iter()
            .map(|(p, c)| format!("{c:?} {}", p.display()))
            .collect();
        assert_eq!(
            paths,
            [
                "Modified .git/config",
                "Created .git/hooks/pre-commit",
                "Removed .git/hooks/pre-commit.sample",
                "Removed .git/modules/vendor/lib/config",
                "Created .git/modules/vendor/lib/hooks/post-checkout",
            ]
        );
        assert_eq!(take(&ws).unwrap(), before);
        assert_eq!(
            fs::read_to_string(root.join("src.rs")).unwrap(),
            "ordinary edit"
        );
    }

    #[test]
    fn hooks_dir_swapped_for_a_symlink_is_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".git/hooks")).unwrap();
        let ws = Dir::open_ambient_dir(root, cap_std::ambient_authority()).unwrap();
        let before = take(&ws).unwrap();

        fs::remove_dir(root.join(".git/hooks")).unwrap();
        unix_fs::symlink(outside.path(), root.join(".git/hooks")).unwrap();
        restore(&ws, &before).unwrap();
        assert!(root.join(".git/hooks").symlink_metadata().unwrap().is_dir());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn jj_and_worktree_config_are_put_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join(".jj/repo/store/git/hooks")).unwrap();
        fs::create_dir_all(root.join(".git/worktrees/side")).unwrap();
        fs::write(root.join(".jj/repo/store/git_target"), "git").unwrap();
        fs::write(root.join(".jj/repo/store/git/config"), "[core]\n").unwrap();
        let ws = Dir::open_ambient_dir(root, cap_std::ambient_authority()).unwrap();
        let before = take(&ws).unwrap();

        // jj adopts a repo or workspace config it finds in the tree as the user's own, and
        // `git_target` names the repository whose config git reads on fetch and push.
        fs::write(root.join(".jj/repo/config.toml"), "ui.pager = \"evil\"\n").unwrap();
        fs::write(
            root.join(".jj/workspace-config.toml"),
            "ui.pager = \"evil\"\n",
        )
        .unwrap();
        fs::write(root.join(".jj/repo/store/git_target"), "../../../evil").unwrap();
        fs::write(
            root.join(".jj/repo/store/git/config"),
            "[core]\nsshCommand = evil\n",
        )
        .unwrap();
        fs::write(root.join(".jj/repo/store/git/hooks/pre-push"), "evil").unwrap();
        fs::write(
            root.join(".git/config.worktree"),
            "[core]\nfsmonitor = evil\n",
        )
        .unwrap();
        fs::write(
            root.join(".git/worktrees/side/config.worktree"),
            "[core]\nfsmonitor = evil\n",
        )
        .unwrap();

        let changes = restore(&ws, &before).unwrap();
        let paths: Vec<String> = changes
            .iter()
            .map(|(p, c)| format!("{c:?} {}", p.display()))
            .collect();
        assert_eq!(
            paths,
            [
                "Created .git/config.worktree",
                "Created .git/worktrees/side/config.worktree",
                "Created .jj/repo/config.toml",
                "Modified .jj/repo/store/git/config",
                "Created .jj/repo/store/git/hooks/pre-push",
                "Modified .jj/repo/store/git_target",
                "Created .jj/workspace-config.toml",
            ]
        );
        assert_eq!(take(&ws).unwrap(), before);
    }

    #[test]
    fn every_path_put_back_is_one_the_engine_refuses() {
        let reserved = Reserved::new();
        let paths = FIXED.iter().map(|(path, _)| *path).chain([
            ".git/worktrees/side/config.worktree",
            ".git/modules/vendor/config.worktree",
        ]);
        for path in paths {
            let loc = Location::new(RootName::workspace(), RelPath::new(path).unwrap());
            assert!(reserved.contains(&loc), "{path}");
        }
    }
}
