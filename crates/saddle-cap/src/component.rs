//! The load-time check (D11): a component's imports and exports, as interface names, against the
//! scopes granted to it. Pure and wasmtime-free, so the v1 thesis test can run on a hand-written
//! list as well as on a compiled component.

use std::collections::BTreeSet;
use std::fmt;

use self::Direction::{Export, Import};
use crate::scope::Scope;

pub const HOST_PACKAGE: &str = "saddle:host";
pub const PLUGIN_PACKAGE: &str = "saddle:plugin";
/// Version line of the saddle WIT packages this build supports (D14).
pub const SADDLE_ABI_LINE: &str = "0.1.";
/// Version line of the WASI interfaces this build supports.
pub const WASI_LINE: &str = "0.2.";

/// A parsed `namespace:package/interface@version` name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceName<'a> {
    pub namespace: &'a str,
    pub package: &'a str,
    pub interface: &'a str,
    pub version: Option<&'a str>,
}

impl<'a> InterfaceName<'a> {
    pub fn parse(name: &'a str) -> Option<Self> {
        let (path, version) = match name.split_once('@') {
            Some((path, version)) => (path, Some(version)),
            None => (name, None),
        };
        let (pkg, interface) = path.split_once('/')?;
        let (namespace, package) = pkg.split_once(':')?;
        if [namespace, package, interface].iter().any(|s| s.is_empty()) {
            return None;
        }
        Some(Self {
            namespace,
            package,
            interface,
            version,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Import,
    Export,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Import => "import",
            Direction::Export => "export",
        })
    }
}

/// What an interface needs before a component may use it.
/// filesystem-service eligibility never substitutes for its per-resource decisions (D139, D140).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    Scope(Scope),
    AnyScope(&'static [Scope]),
    Ambient,
    Refused(String),
}

pub fn classify(direction: Direction, name: &str) -> Requirement {
    let Some(n) = InterfaceName::parse(name) else {
        return Requirement::Refused("not a `namespace:package/interface@version` name".into());
    };
    let line = match n.namespace {
        "saddle" => SADDLE_ABI_LINE,
        "wasi" => WASI_LINE,
        _ => return Requirement::Refused("not in the import table".into()),
    };
    match n.version {
        Some(v) if v.starts_with(line) => {}
        Some(v) => {
            return Requirement::Refused(format!(
                "version {v} is unsupported; this host supports {line}x"
            ));
        }
        None => return Requirement::Refused(format!("unversioned; this host supports {line}x")),
    }

    let scope = |s| Requirement::Scope(s);
    match (direction, n.namespace, n.package, n.interface) {
        (Import, "saddle", "host", i) => match i {
            "exec" => scope(Scope::Exec),
            "http" | "web-search" => scope(Scope::Net),
            "fs-read" | "fs-read-range" | "fs-paths" | "fs-list" | "tool-paths" | "fs-write" => {
                Requirement::AnyScope(&[
                    Scope::FsRead,
                    Scope::ToolRead,
                    Scope::FsWrite,
                    Scope::ToolWrite,
                ])
            }
            "tool-result" => scope(Scope::Tool),
            "transcript-read" => scope(Scope::TranscriptRead),
            "context-contribute" => scope(Scope::ContextContribute),
            "model-invoke" => scope(Scope::ModelInvoke),
            "persist" => scope(Scope::Persist),
            _ => Requirement::Refused("unknown host interface".into()),
        },
        (Export, "saddle", "plugin", i) => match i {
            "tool" => scope(Scope::Tool),
            "observer" => scope(Scope::TranscriptRead),
            "veto" => scope(Scope::Veto),
            "ui" => scope(Scope::Ui),
            _ => Requirement::Refused("unknown plugin hook".into()),
        },
        (Import, "wasi", "http", "outgoing-handler") => scope(Scope::Net),
        (Import, "wasi", "http", "types") => Requirement::Ambient,
        (Import, "wasi", "sockets", _) => Requirement::Refused(
            "raw sockets are not offered; use wasi:http/outgoing-handler with a net grant".into(),
        ),
        (Import, "wasi", "filesystem", "types" | "preopens") => Requirement::Ambient,
        (Import, "wasi", "cli", i)
            if matches!(i, "environment" | "exit" | "stdin" | "stdout" | "stderr")
                || i.starts_with("terminal-") =>
        {
            Requirement::Ambient
        }
        (Import, "wasi", "io", _) => Requirement::Ambient,
        (Import, "wasi", "clocks", "monotonic-clock" | "wall-clock") => Requirement::Ambient,
        (Import, "wasi", "random", _) => Requirement::Ambient,
        _ => Requirement::Refused(format!("not in the {direction} table")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub direction: Direction,
    pub name: String,
    pub reason: String,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} `{}`: {}", self.direction, self.name, self.reason)
    }
}

/// Refuses the component unless every import and export is ambient or covered by a granted scope.
/// Returns every refusal, not only the first, so the log names the whole problem.
pub fn check_component<'a>(
    granted: &BTreeSet<Scope>,
    imports: impl IntoIterator<Item = &'a str>,
    exports: impl IntoIterator<Item = &'a str>,
) -> Result<(), Vec<Refusal>> {
    let items = imports
        .into_iter()
        .map(|n| (Direction::Import, n))
        .chain(exports.into_iter().map(|n| (Direction::Export, n)));
    let refusals: Vec<Refusal> = items
        .filter_map(|(direction, name)| {
            let reason = match classify(direction, name) {
                Requirement::Ambient => return None,
                Requirement::Scope(s) if granted.contains(&s) => return None,
                Requirement::Scope(s) => format!("needs scope `{s}`, which is not granted"),
                Requirement::AnyScope(scopes) if scopes.iter().any(|s| granted.contains(s)) => {
                    return None;
                }
                Requirement::AnyScope(scopes) => format!(
                    "needs scope {}, which are not granted",
                    scopes
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(" or ")
                ),
                Requirement::Refused(reason) => reason,
            };
            Some(Refusal {
                direction,
                name: name.to_owned(),
                reason,
            })
        })
        .collect();
    if refusals.is_empty() {
        Ok(())
    } else {
        Err(refusals)
    }
}

/// The interfaces a Linker for this grant set should define (D11: the Linker is built from the
/// grants, so an import the check missed still fails to link).
pub fn host_interfaces(granted: &BTreeSet<Scope>) -> Vec<&'static str> {
    let mut out = Vec::new();
    for scope in granted {
        let interface = match scope {
            Scope::Exec => "saddle:host/exec",
            Scope::FsRead | Scope::ToolRead | Scope::FsWrite | Scope::ToolWrite => {
                if !out.contains(&"saddle:host/fs-read") {
                    out.extend([
                        "saddle:host/fs-read",
                        "saddle:host/fs-read-range",
                        "saddle:host/fs-paths",
                        "saddle:host/fs-list",
                        "saddle:host/tool-paths",
                        "saddle:host/fs-write",
                    ]);
                }
                continue;
            }
            Scope::TranscriptRead => "saddle:host/transcript-read",
            Scope::ContextContribute => "saddle:host/context-contribute",
            Scope::ModelInvoke => "saddle:host/model-invoke",
            Scope::Persist => "saddle:host/persist",
            Scope::Net => {
                out.extend(["saddle:host/http", "saddle:host/web-search"]);
                continue;
            }
            // An MCP grant declares no component import: the client is host-side and reached
            // through built-in tools (D103). A plugin calling one needs `saddle:host/mcp`.
            Scope::Tool | Scope::Veto | Scope::Ui | Scope::Mcp => continue,
        };
        out.push(interface);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a minimal Rust std component imports before it touches any scope.
    const STD_AMBIENT: &[&str] = &[
        "wasi:cli/environment@0.2.3",
        "wasi:cli/exit@0.2.3",
        "wasi:io/error@0.2.3",
        "wasi:io/streams@0.2.3",
        "wasi:cli/stdout@0.2.3",
        "wasi:cli/stderr@0.2.3",
        "wasi:clocks/wall-clock@0.2.3",
        "wasi:filesystem/types@0.2.3",
        "wasi:filesystem/preopens@0.2.3",
        "wasi:random/random@0.2.3",
    ];

    fn imports(extra: &[&'static str]) -> Vec<&'static str> {
        STD_AMBIENT.iter().chain(extra).copied().collect()
    }

    /// The v1 thesis at the pure level: importing fs-read without the grant is refused.
    #[test]
    fn hostile_fs_read_import_is_refused() {
        let granted = BTreeSet::new();
        let err =
            check_component(&granted, imports(&["saddle:host/fs-read@0.1.0"]), []).unwrap_err();
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].name, "saddle:host/fs-read@0.1.0");
        assert!(err[0].reason.contains("fs-read"));

        let granted = BTreeSet::from([Scope::FsRead]);
        check_component(&granted, imports(&["saddle:host/fs-read@0.1.0"]), []).unwrap();
    }

    #[test]
    fn std_ambient_imports_need_nothing() {
        check_component(&BTreeSet::new(), imports(&[]), []).unwrap();
    }

    #[test]
    fn exports_are_checked() {
        let err = check_component(&BTreeSet::new(), [], ["saddle:plugin/tool@0.1.0"]).unwrap_err();
        assert!(err[0].reason.contains("`tool`"));
        let ok = BTreeSet::from([Scope::Tool, Scope::TranscriptRead]);
        check_component(
            &ok,
            [],
            ["saddle:plugin/tool@0.1.0", "saddle:plugin/observer@0.1.0"],
        )
        .unwrap();
        // A command-style export is not a plugin hook.
        assert!(check_component(&ok, [], ["wasi:cli/run@0.2.3"]).is_err());
    }

    #[test]
    fn sockets_versions_and_strangers_are_refused() {
        let all: BTreeSet<Scope> = Scope::ALL.into_iter().collect();
        for name in [
            "wasi:sockets/tcp@0.2.3",
            "saddle:host/fs-read@0.2.0",
            "saddle:host/fs-read",
            "saddle:host/intercept@0.1.0",
            "acme:evil/backdoor@1.0.0",
            "garbage",
        ] {
            assert!(
                check_component(&all, [name], []).is_err(),
                "{name} should be refused"
            );
        }
        check_component(&all, ["wasi:http/outgoing-handler@0.2.3"], []).unwrap();
    }

    #[test]
    fn linker_interfaces_follow_grants() {
        let granted = BTreeSet::from([Scope::FsRead, Scope::Net, Scope::Tool]);
        assert_eq!(
            host_interfaces(&granted),
            [
                "saddle:host/fs-read",
                "saddle:host/fs-read-range",
                "saddle:host/fs-paths",
                "saddle:host/fs-list",
                "saddle:host/tool-paths",
                "saddle:host/fs-write",
                "saddle:host/http",
                "saddle:host/web-search"
            ]
        );
    }

    #[test]
    fn one_read_service_family_accepts_either_read_capability_not_registration_alone() {
        let imports = [
            "saddle:host/fs-read@0.1.0",
            "saddle:host/fs-read-range@0.1.0",
            "saddle:host/fs-paths@0.1.0",
            "saddle:host/fs-list@0.1.0",
            "saddle:host/tool-paths@0.1.0",
        ];
        for scope in [Scope::FsRead, Scope::ToolRead, Scope::FsWrite] {
            check_component(&BTreeSet::from([scope]), imports, []).unwrap();
        }
        assert!(check_component(&BTreeSet::from([Scope::Tool]), imports, []).is_err());
        assert_eq!(
            host_interfaces(&BTreeSet::from([Scope::FsRead])),
            host_interfaces(&BTreeSet::from([Scope::ToolRead]))
        );
        assert_eq!(
            host_interfaces(&BTreeSet::from([Scope::FsRead, Scope::ToolRead])),
            host_interfaces(&BTreeSet::from([Scope::FsRead]))
        );
    }
}
