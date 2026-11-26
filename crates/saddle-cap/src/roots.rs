use std::collections::BTreeMap;
use std::fmt;
use std::io::Error;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const WORKSPACE: &str = "workspace";
pub const TMP: &str = "tmp";

pub const BUILTIN: [&str; 4] = [WORKSPACE, TMP, "config", "data"];

pub fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 64
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-'))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RootName(String);

impl RootName {
    pub fn new(name: &str) -> Result<Self, RootError> {
        if is_ident(name) {
            Ok(Self(name.to_owned()))
        } else {
            Err(RootError::BadName(name.to_owned()))
        }
    }

    pub fn workspace() -> Self {
        Self(WORKSPACE.to_owned())
    }

    pub fn tmp() -> Self {
        Self(TMP.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_builtin(&self) -> bool {
        BUILTIN.contains(&self.0.as_str())
    }
}

impl TryFrom<String> for RootName {
    type Error = RootError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(&s)
    }
}

impl From<RootName> for String {
    fn from(name: RootName) -> String {
        name.0
    }
}

impl fmt::Display for RootName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RelPath(Vec<String>);

impl RelPath {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, RootError> {
        let path = path.as_ref();
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => match part.to_str() {
                    Some(part) => parts.push(part.to_owned()),
                    None => return Err(RootError::NotUtf8(path.to_path_buf())),
                },
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(RootError::Escapes(path.to_path_buf()));
                }
            }
        }
        Ok(Self(parts))
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_slash(&self) -> String {
        if self.0.is_empty() {
            ".".to_owned()
        } else {
            self.0.join("/")
        }
    }

    pub fn to_path(&self) -> PathBuf {
        if self.0.is_empty() {
            PathBuf::from(".")
        } else {
            self.0.iter().collect()
        }
    }

    pub fn parent(&self) -> Option<RelPath> {
        if self.0.is_empty() {
            None
        } else {
            Some(Self(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    pub fn join(&self, part: &str) -> Result<RelPath, RootError> {
        let tail = RelPath::new(part)?;
        let mut parts = self.0.clone();
        parts.extend(tail.0);
        Ok(Self(parts))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_slash())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Location {
    pub root: RootName,
    pub rel: RelPath,
}

impl Location {
    pub fn new(root: RootName, rel: RelPath) -> Self {
        Self { root, rel }
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.root, self.rel)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RootError {
    #[error("`{0}` is not a valid name (expected [a-z][a-z0-9-]*)")]
    BadName(String),
    #[error("`{0}` is a built-in root and cannot be redefined")]
    Builtin(RootName),
    #[error("root `{0}` is defined twice")]
    Duplicate(RootName),
    #[error("root `{name}` at {path}: {source}")]
    Unresolvable {
        name: RootName,
        path: PathBuf,
        source: Error,
    },
    #[error("root `{name}` at {path} is not a directory")]
    NotDir { name: RootName, path: PathBuf },
    #[error("root `{name}` at {root} contains {protected}, which no root may contain")]
    ContainsProtected {
        name: RootName,
        root: PathBuf,
        protected: PathBuf,
    },
    #[error("path {0} climbs out of its root")]
    Escapes(PathBuf),
    #[error("path {0} is not valid UTF-8")]
    NotUtf8(PathBuf),
}

#[derive(Debug, Clone, Default)]
pub struct Roots {
    entries: BTreeMap<RootName, PathBuf>,
}

impl Roots {
    pub fn resolve(
        entries: impl IntoIterator<Item = (RootName, PathBuf)>,
        protected: &[PathBuf],
    ) -> Result<Self, RootError> {
        let protected: Vec<PathBuf> = protected
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| normalize(p)))
            .collect();
        let mut out = BTreeMap::new();
        for (name, path) in entries {
            let canonical = path
                .canonicalize()
                .map_err(|source| RootError::Unresolvable {
                    name: name.clone(),
                    path: path.clone(),
                    source,
                })?;
            if !canonical.is_dir() {
                return Err(RootError::NotDir {
                    name,
                    path: canonical,
                });
            }
            if let Some(p) = protected.iter().find(|p| p.starts_with(&canonical)) {
                return Err(RootError::ContainsProtected {
                    name,
                    root: canonical,
                    protected: p.clone(),
                });
            }
            if out.contains_key(&name) {
                return Err(RootError::Duplicate(name));
            }
            out.insert(name, canonical);
        }
        Ok(Self { entries: out })
    }

    pub fn get(&self, name: &RootName) -> Option<&Path> {
        self.entries.get(name).map(PathBuf::as_path)
    }

    pub fn contains(&self, name: &RootName) -> bool {
        self.entries.contains_key(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RootName, &Path)> {
        self.entries.iter().map(|(n, p)| (n, p.as_path()))
    }

    pub fn path_of(&self, loc: &Location) -> Option<PathBuf> {
        let base = self.get(&loc.root)?;
        Some(if loc.rel.is_root() {
            base.to_path_buf()
        } else {
            base.join(loc.rel.to_path())
        })
    }

    pub fn locate(&self, path: &Path) -> Option<Location> {
        if !path.is_absolute() {
            return None;
        }
        let path = normalize(path);
        self.entries
            .iter()
            .filter(|(_, base)| path.starts_with(base))
            .max_by_key(|(_, base)| base.components().count())
            .and_then(|(name, base)| {
                let rel = RelPath::new(path.strip_prefix(base).ok()?).ok()?;
                Some(Location::new(name.clone(), rel))
            })
    }
}

pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rel_paths_cannot_escape() {
        assert!(RelPath::new("src/main.rs").is_ok());
        assert_eq!(RelPath::new("./a/./b").unwrap().as_slash(), "a/b");
        assert!(RelPath::new("../x").is_err());
        assert!(RelPath::new("a/../../x").is_err());
        assert!(RelPath::new("/etc/passwd").is_err());
        assert!(RelPath::new("").unwrap().is_root());
    }

    #[test]
    fn locate_picks_innermost_root() {
        let ws = tempfile::tempdir().unwrap();
        fs::create_dir(ws.path().join("vendor")).unwrap();
        let roots = Roots::resolve(
            [
                (RootName::workspace(), ws.path().to_path_buf()),
                (RootName::new("vendor").unwrap(), ws.path().join("vendor")),
            ],
            &[],
        )
        .unwrap();
        let base = ws.path().canonicalize().unwrap();

        let loc = roots.locate(&base.join("src/lib.rs")).unwrap();
        assert_eq!(loc.to_string(), "workspace:src/lib.rs");
        let loc = roots.locate(&base.join("vendor/x/y")).unwrap();
        assert_eq!(loc.to_string(), "vendor:x/y");
        assert!(roots.locate(&base.join("../outside")).is_none());
        assert!(roots.locate(Path::new("relative")).is_none());
    }

    #[test]
    fn root_containing_protected_dir_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join(".local/state/saddle");
        let err = Roots::resolve(
            [(RootName::new("home").unwrap(), home.path().into())],
            &[state],
        )
        .unwrap_err();
        assert!(matches!(err, RootError::ContainsProtected { .. }), "{err}");
    }
}
