//! pinned, no-follow descriptors for plugin artifacts and authorized file reads (D129).

use std::fs::{File, OpenOptions as StdOpenOptions};
use std::io::{self, ErrorKind};
use std::os::unix::fs::OpenOptionsExt as StdOpenOptionsExt;
use std::path::{Component, Path};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};
#[cfg(target_os = "linux")]
use libc::O_PATH;
use libc::{O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use saddle_cap::RelPath;

#[cfg(target_os = "linux")]
const PATH_HANDLE: i32 = O_PATH;
#[cfg(not(target_os = "linux"))]
const PATH_HANDLE: i32 = 0;

fn options(directory: bool) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No).custom_flags(
        O_NOFOLLOW
            | if directory {
                O_DIRECTORY | PATH_HANDLE
            } else {
                O_NONBLOCK
            },
    );
    options
}

/// pins a canonical absolute root without allowing a replaced ancestor to steer the open.
pub fn root(path: &Path) -> io::Result<Dir> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "root is not absolute",
        ));
    }
    // Ancestors are locations, not readable grants. O_PATH pins them without requiring read-dir
    // on `/` or widening landlock. Directory enumeration and final file opens still need their
    // normal permissions, exactly as with cap-std's O_PATH directory handles.
    let anchor = StdOpenOptions::new()
        .read(true)
        .custom_flags(PATH_HANDLE | O_DIRECTORY | O_NOFOLLOW)
        .open("/")?;
    let mut dir = Dir::from_std_file(anchor);
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(part) => {
                dir = Dir::from_std_file(dir.open_with(part, &options(true))?.into_std());
            }
            _ => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "root is not canonical",
                ));
            }
        }
    }
    Ok(dir)
}

/// opens a previously resolved relative path; any subsequent symlink substitution fails closed.
pub fn file(root: &Dir, rel: &RelPath) -> io::Result<File> {
    let path = rel.to_path();
    let mut parts = path.components().peekable();
    let mut dir = root.try_clone()?;
    while let Some(Component::Normal(part)) = parts.next() {
        let directory = parts.peek().is_some();
        let file = dir.open_with(part, &options(directory))?;
        if !directory {
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "only regular files may be read",
                ));
            }
            return Ok(file.into_std());
        }
        dir = Dir::from_std_file(file.into_std());
    }
    Err(io::Error::new(
        ErrorKind::InvalidInput,
        "not a regular file path",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Read;
    use std::os::unix::fs::symlink;

    #[test]
    fn pinning_rejects_a_replaced_final_or_intermediate_link() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("dir")).unwrap();
        fs::write(temp.path().join("dir/file"), "hello").unwrap();
        symlink("dir/file", temp.path().join("link")).unwrap();
        symlink("dir", temp.path().join("alias")).unwrap();
        let dir = root(temp.path()).unwrap();
        assert!(file(&dir, &RelPath::new("link").unwrap()).is_err());
        assert!(file(&dir, &RelPath::new("alias/file").unwrap()).is_err());
        assert!(file(&dir, &RelPath::new("dir/file").unwrap()).is_ok());
    }

    #[test]
    fn a_root_descriptor_cannot_be_retargeted_by_renaming_its_path() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("root")).unwrap();
        fs::create_dir(temp.path().join("other")).unwrap();
        fs::write(temp.path().join("root/file"), "original").unwrap();
        fs::write(temp.path().join("other/file"), "outside").unwrap();
        let pinned = root(&temp.path().join("root")).unwrap();
        fs::rename(temp.path().join("root"), temp.path().join("old")).unwrap();
        symlink("other", temp.path().join("root")).unwrap();
        assert!(root(&temp.path().join("root")).is_err());
        let mut text = String::new();
        file(&pinned, &RelPath::new("file").unwrap())
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "original");
    }
}
