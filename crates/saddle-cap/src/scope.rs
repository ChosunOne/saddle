//! The closed scope enumeration (plan §4).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Scope {
    Exec,
    FsRead,
    FsWrite,
    Net,
    TranscriptRead,
    ContextContribute,
    Tool,
    Veto,
    ModelInvoke,
    Ui,
    Persist,
}

impl Scope {
    pub const ALL: [Scope; 11] = [
        Scope::Exec,
        Scope::FsRead,
        Scope::FsWrite,
        Scope::Net,
        Scope::TranscriptRead,
        Scope::ContextContribute,
        Scope::Tool,
        Scope::Veto,
        Scope::ModelInvoke,
        Scope::Ui,
        Scope::Persist,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Exec => "exec",
            Scope::FsRead => "fs-read",
            Scope::FsWrite => "fs-write",
            Scope::Net => "net",
            Scope::TranscriptRead => "transcript-read",
            Scope::ContextContribute => "context-contribute",
            Scope::Tool => "tool",
            Scope::Veto => "veto",
            Scope::ModelInvoke => "model-invoke",
            Scope::Ui => "ui",
            Scope::Persist => "persist",
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown scope `{0}`")]
pub struct UnknownScope(pub String);

impl FromStr for Scope {
    type Err = UnknownScope;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scope::ALL
            .into_iter()
            .find(|scope| scope.as_str() == s)
            .ok_or_else(|| UnknownScope(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for scope in Scope::ALL {
            assert_eq!(scope.as_str().parse::<Scope>(), Ok(scope));
        }
        assert!("intercept".parse::<Scope>().is_err());
    }
}
