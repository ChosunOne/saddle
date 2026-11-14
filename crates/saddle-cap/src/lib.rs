//! The one capability representation (plan §5). Everything else lowers from here: the component
//! import check, per-call policy decisions, and the exec sandbox spec.

pub mod component;
pub mod delegate;
pub mod manifest;
pub mod narrow;
pub mod policy;
pub mod roots;
pub mod sandbox;
pub mod scope;

pub use policy::{
    Grants, McpGrant, McpReach, PluginId, Policy, PolicyError, Principal, RuleId, names_host,
    net_allows,
};
pub use roots::{Location, RelPath, RootName, Roots};
pub use scope::Scope;
