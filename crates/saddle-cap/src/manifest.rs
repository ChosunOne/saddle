use std::collections::BTreeSet;
use toml::de::Error;

use serde::Deserialize;

use crate::policy::{Grants, PluginId};
use crate::scope::Scope;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: PluginId,
    pub version: String,
    pub component: String,
    pub scopes: BTreeSet<Scope>,
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Self, Error> {
        toml::from_str(text)
    }
}

pub fn above_ceiling(manifest: &Manifest, grants: &Grants) -> Vec<Scope> {
    grants
        .scopes()
        .difference(&manifest.scopes)
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{Policy, Principal};
    use crate::roots::{RootName, Roots};

    #[test]
    fn grants_above_the_ceiling_are_reported() {
        let manifest = Manifest::parse(
            r#"
id = "web-search"
version = "0.1.0"
component = "web_search.wasm"
scopes = ["net"]
"#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let roots = Roots::resolve([(RootName::workspace(), dir.path().into())], &[]).unwrap();
        let policy = Policy::parse(
            r#"
version = 1
[plugin.web-search]
net = [{ host = "api.search.example", why = "backend" }]
persist = { why = "cache" }
"#,
            &roots,
        )
        .unwrap();
        let grants = policy
            .grants(&Principal::Plugin(manifest.id.clone()))
            .unwrap();
        assert_eq!(above_ceiling(&manifest, grants), [Scope::Persist]);
    }
}
