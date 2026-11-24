//! typed interface catalogue and pre-initialization eligibility checks (D11, D14, D139, D150).
//! pure and wasmtime-free; service bindings never substitute for per-resource authorization.

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

/// service eligibility is not per-resource operational authority (D139, D150).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requirement {
    Scope(Scope),
    AnyScope(&'static [Scope]),
    Ambient,
}
impl Requirement {
    pub fn satisfied_by(self, scopes: &BTreeSet<Scope>) -> bool {
        self == Self::Ambient || scopes.iter().any(|scope| self.mentions(*scope))
    }
    fn mentions(self, scope: Scope) -> bool {
        match self {
            Self::Scope(s) => s == scope,
            Self::AnyScope(scopes) => scopes.contains(&scope),
            Self::Ambient => false,
        }
    }
    fn refusal(self) -> String {
        match self {
            Self::Scope(s) => format!("needs scope `{s}`, which is not granted"),
            Self::AnyScope(scopes) => format!(
                "needs scope {}, which are not granted",
                scopes
                    .iter()
                    .map(|s| format!("`{s}`"))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
            Self::Ambient => "ambient interface".into(),
        }
    }
}
const FILESYSTEM_SCOPES: &[Scope] = &[
    Scope::FsRead,
    Scope::ToolRead,
    Scope::FsWrite,
    Scope::ToolWrite,
];

// One production catalogue generates identities, names, requirements and availability (D150).
macro_rules! catalogue {
    ($kind:ident { $( $variant:ident => ($name:literal, $requirement:expr, $implemented:literal) ),* $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $kind { $( $variant, )* }
        impl $kind {
            pub const ALL: &'static [Self] = &[$( Self::$variant, )*];
            pub fn name(self) -> &'static str { match self { $( Self::$variant => $name, )* } }
            pub fn requirement(self) -> Requirement { match self { $( Self::$variant => $requirement, )* } }
            pub fn implemented(self) -> bool { match self { $( Self::$variant => $implemented, )* } }
        }
    };
}
catalogue!(HostInterface {
    RunBinary => ("saddle:host/run-binary", Requirement::Scope(Scope::RunBinary), true),
    Exec => ("saddle:host/exec", Requirement::Scope(Scope::Exec), true),
    PluginEvents => ("saddle:host/plugin-events", Requirement::Scope(Scope::EventPublish), true),
    ExecTasks => ("saddle:host/exec-tasks", Requirement::Scope(Scope::Exec), true),
    Http => ("saddle:host/http", Requirement::Scope(Scope::Net), true),
    WebSearch => ("saddle:host/web-search", Requirement::Scope(Scope::Net), true),
    FsRead => ("saddle:host/fs-read", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    FsReadRange => ("saddle:host/fs-read-range", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    FsPaths => ("saddle:host/fs-paths", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    FsList => ("saddle:host/fs-list", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    ToolPaths => ("saddle:host/tool-paths", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    FsWrite => ("saddle:host/fs-write", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    FsPreimage => ("saddle:host/fs-preimage", Requirement::AnyScope(FILESYSTEM_SCOPES), true),
    ToolResult => ("saddle:host/tool-result", Requirement::Scope(Scope::Tool), true),
    Budgets => ("saddle:host/budgets", Requirement::Ambient, true),
    TranscriptRead => ("saddle:host/transcript-read", Requirement::Scope(Scope::TranscriptRead), true),
    ContextContribute => ("saddle:host/context-contribute", Requirement::Scope(Scope::ContextContribute), true),
    Workers => ("saddle:host/workers", Requirement::Scope(Scope::Tool), true),
    ModelInvoke => ("saddle:host/model-invoke", Requirement::Scope(Scope::ModelInvoke), true),
    Widgets => ("saddle:widgets/types", Requirement::Ambient, true),
    Ui => ("saddle:host/ui", Requirement::Scope(Scope::Ui), true),
    Persist => ("saddle:host/persist", Requirement::Scope(Scope::Persist), true),
});
catalogue!(PluginExport {
    Tool => ("saddle:plugin/tool", Requirement::Scope(Scope::Tool), true),
    Observer => ("saddle:plugin/observer", Requirement::Scope(Scope::TranscriptRead), true),
    PluginEvents => ("saddle:plugin/plugin-events", Requirement::Scope(Scope::EventSubscribe), true),
    Veto => ("saddle:plugin/veto", Requirement::Scope(Scope::Veto), true),
    Ui => ("saddle:plugin/ui", Requirement::Scope(Scope::Ui), true),
});
catalogue!(WasiInterface {
    Environment => ("wasi:cli/environment", Requirement::Ambient, true),
    Exit => ("wasi:cli/exit", Requirement::Ambient, true),
    Stdin => ("wasi:cli/stdin", Requirement::Ambient, true),
    Stdout => ("wasi:cli/stdout", Requirement::Ambient, true),
    Stderr => ("wasi:cli/stderr", Requirement::Ambient, true),
    TerminalInput => ("wasi:cli/terminal-input", Requirement::Ambient, true),
    TerminalOutput => ("wasi:cli/terminal-output", Requirement::Ambient, true),
    TerminalStdin => ("wasi:cli/terminal-stdin", Requirement::Ambient, true),
    TerminalStdout => ("wasi:cli/terminal-stdout", Requirement::Ambient, true),
    TerminalStderr => ("wasi:cli/terminal-stderr", Requirement::Ambient, true),
    MonotonicClock => ("wasi:clocks/monotonic-clock", Requirement::Ambient, true),
    WallClock => ("wasi:clocks/wall-clock", Requirement::Ambient, true),
    FilesystemTypes => ("wasi:filesystem/types", Requirement::Ambient, true),
    Preopens => ("wasi:filesystem/preopens", Requirement::Ambient, true),
    Random => ("wasi:random/random", Requirement::Ambient, true),
    InsecureRandom => ("wasi:random/insecure", Requirement::Ambient, true),
    InsecureSeed => ("wasi:random/insecure-seed", Requirement::Ambient, true),
    IoError => ("wasi:io/error", Requirement::Ambient, true),
    Streams => ("wasi:io/streams", Requirement::Ambient, true),
    Poll => ("wasi:io/poll", Requirement::Ambient, false),
    HttpOutgoingHandler => ("wasi:http/outgoing-handler", Requirement::Scope(Scope::Net), false),
    HttpTypes => ("wasi:http/types", Requirement::Ambient, false),
});
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interface {
    Host(HostInterface),
    Wasi(WasiInterface),
    Export(PluginExport),
}
impl Interface {
    pub fn all() -> impl Iterator<Item = Self> {
        HostInterface::ALL
            .iter()
            .copied()
            .map(Self::Host)
            .chain(WasiInterface::ALL.iter().copied().map(Self::Wasi))
            .chain(PluginExport::ALL.iter().copied().map(Self::Export))
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Host(i) => i.name(),
            Self::Wasi(i) => i.name(),
            Self::Export(i) => i.name(),
        }
    }
    pub fn direction(self) -> Direction {
        match self {
            Self::Host(_) | Self::Wasi(_) => Import,
            Self::Export(_) => Export,
        }
    }
    pub fn requirement(self) -> Requirement {
        match self {
            Self::Host(i) => i.requirement(),
            Self::Wasi(i) => i.requirement(),
            Self::Export(i) => i.requirement(),
        }
    }
    pub fn implemented(self) -> bool {
        match self {
            Self::Host(i) => i.implemented(),
            Self::Wasi(i) => i.implemented(),
            Self::Export(i) => i.implemented(),
        }
    }
}
/// rejects unsupported grants even when their service is not imported (D150).
pub fn runtime_supports_scope(scope: Scope) -> bool {
    Interface::all().any(|i| i.implemented() && i.requirement().mentions(scope))
}
pub fn classify(direction: Direction, name: &str) -> Result<Interface, String> {
    let n = InterfaceName::parse(name).ok_or("not a `namespace:package/interface@version` name")?;
    let line = match n.namespace {
        "saddle" => SADDLE_ABI_LINE,
        "wasi" => WASI_LINE,
        _ => return Err("not in the import table".into()),
    };
    match n.version {
        Some(v) if v.starts_with(line) => {}
        Some(v) => {
            return Err(format!(
                "version {v} is unsupported; this host supports {line}x"
            ));
        }
        None => return Err(format!("unversioned; this host supports {line}x")),
    }
    // Canonical Saddle bindings remain exact; WASI retains its compatible 0.2.x line.
    if n.namespace == "saddle" && n.version != Some("0.1.0") {
        return Err("only the canonical saddle ABI version 0.1.0 is implemented".into());
    }
    if n.namespace == "wasi" && n.package == "sockets" {
        return Err("raw sockets are not offered; use saddle:host/http with a net grant".into());
    }
    let base = name.split_once('@').map(|(base, _)| base).unwrap_or(name);
    Interface::all()
        .find(|i| i.direction() == direction && i.name() == base)
        .ok_or_else(|| format!("not in the {direction} table"))
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
/// interface identities checked for eligibility and implementation before initialization.
#[derive(Debug)]
pub struct CheckedComponent {
    pub imports: Vec<Interface>,
    pub exports: Vec<Interface>,
}
/// collects all refusals, including unused interfaces, before any guest initialization (D150).
pub fn check_component<'a>(
    granted: &BTreeSet<Scope>,
    imports: impl IntoIterator<Item = &'a str>,
    exports: impl IntoIterator<Item = &'a str>,
) -> Result<CheckedComponent, Vec<Refusal>> {
    let mut checked = CheckedComponent {
        imports: Vec::new(),
        exports: Vec::new(),
    };
    let mut refusals = Vec::new();
    for (direction, name) in imports
        .into_iter()
        .map(|n| (Import, n))
        .chain(exports.into_iter().map(|n| (Export, n)))
    {
        let result = classify(direction, name).and_then(|i| {
            if !i.requirement().satisfied_by(granted) {
                Err(i.requirement().refusal())
            } else if !i.implemented() {
                Err("interface is not implemented by the runtime".into())
            } else {
                Ok(i)
            }
        });
        match result {
            Ok(i) => {
                let plan = match direction {
                    Import => &mut checked.imports,
                    Export => &mut checked.exports,
                };
                // Compatible WASI patch versions can name the same adapter more than once.
                if !plan.contains(&i) {
                    plan.push(i);
                }
            }
            Err(reason) => refusals.push(Refusal {
                direction,
                name: name.to_owned(),
                reason,
            }),
        }
    }
    if refusals.is_empty() {
        Ok(checked)
    } else {
        Err(refusals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use wit_parser::{Resolve, WorldItem};

    #[test]
    fn catalogue_matches_canonical_wit_interfaces_and_world() {
        let mut resolve = Resolve::default();
        let (host, _) = resolve
            .push_dir(Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../saddle-wit/wit"
            )))
            .unwrap();
        let mut host_names: BTreeSet<_> = resolve.packages[host]
            .interfaces
            .keys()
            .map(|name| format!("saddle:host/{name}"))
            .collect();
        let (_, widgets) = resolve
            .packages
            .iter()
            .find(|(_, package)| {
                package.name.namespace == "saddle" && package.name.name == "widgets"
            })
            .unwrap();
        host_names.extend(
            widgets
                .interfaces
                .keys()
                .map(|name| format!("saddle:widgets/{name}")),
        );
        let catalogue_host: BTreeSet<_> = HostInterface::ALL
            .iter()
            .filter(|i| i.implemented())
            .map(|i| i.name().to_owned())
            .collect();
        assert_eq!(host_names, catalogue_host);
        let (_, plugin) = resolve
            .packages
            .iter()
            .find(|(_, package)| {
                package.name.namespace == "saddle" && package.name.name == "plugin"
            })
            .unwrap();
        let export_names: BTreeSet<_> = plugin
            .interfaces
            .keys()
            .map(|name| format!("saddle:plugin/{name}"))
            .collect();
        let catalogue_exports: BTreeSet<_> = PluginExport::ALL
            .iter()
            .filter(|i| i.implemented())
            .map(|i| i.name().to_owned())
            .collect();
        assert_eq!(export_names, catalogue_exports);
        let world = &resolve.worlds[resolve.packages[host].worlds["read-tool"]];
        let world_names = |items: Vec<&WorldItem>| -> BTreeSet<_> {
            items
                .into_iter()
                .map(|item| match item {
                    WorldItem::Interface { id, .. } => resolve.id_of(*id).unwrap(),
                    _ => panic!("only interface instances belong in the tool world"),
                })
                .collect()
        };
        assert_eq!(
            world_names(world.imports.values().collect()),
            catalogue_host
                .iter()
                .map(|name| format!("{name}@0.1.0"))
                .collect()
        );
        assert_eq!(
            [
                "read-tool",
                "harness-hook",
                "plugin-event-handler",
                "veto-hook",
                "ui-hook"
            ]
            .into_iter()
            .flat_map(|name| world_names(
                resolve.worlds[resolve.packages[host].worlds[name]]
                    .exports
                    .values()
                    .collect()
            ))
            .collect::<BTreeSet<_>>(),
            catalogue_exports
                .iter()
                .map(|name| format!("{name}@0.1.0"))
                .collect()
        );
    }

    #[test]
    fn catalogue_names_are_unique_and_every_entry_round_trips() {
        let mut names = BTreeSet::new();
        let all: BTreeSet<_> = Scope::ALL.into_iter().collect();
        for interface in Interface::all() {
            assert!(names.insert(interface.name()));
            let version = if matches!(interface, Interface::Wasi(_)) {
                "0.2.3"
            } else {
                "0.1.0"
            };
            let name = format!("{}@{version}", interface.name());
            assert_eq!(classify(interface.direction(), &name), Ok(interface));
            let result = match interface.direction() {
                Import => check_component(&all, [name.as_str()], []),
                Export => check_component(&all, [], [name.as_str()]),
            };
            assert_eq!(result.is_ok(), interface.implemented(), "{name}");
        }
    }

    #[test]
    fn compatible_wasi_imports_share_one_binding_without_inheriting_other_services() {
        let checked = check_component(
            &BTreeSet::new(),
            ["wasi:io/error@0.2.0", "wasi:io/error@0.2.3"],
            [],
        )
        .unwrap();
        assert_eq!(checked.imports, [Interface::Wasi(WasiInterface::IoError)]);
        assert!(checked.exports.is_empty());
    }

    #[test]
    fn filesystem_service_eligibility_is_shared_but_registration_is_not_read_authority() {
        for interface in HostInterface::ALL
            .iter()
            .filter(|i| i.requirement() == Requirement::AnyScope(FILESYSTEM_SCOPES))
        {
            let name = format!("{}@0.1.0", interface.name());
            for scope in FILESYSTEM_SCOPES {
                check_component(&BTreeSet::from([*scope]), [name.as_str()], []).unwrap();
            }
            assert!(check_component(&BTreeSet::from([Scope::Tool]), [name.as_str()], []).is_err());
        }
    }

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
        check_component(&ok, [], ["saddle:plugin/tool@0.1.0"]).unwrap();
        check_component(&ok, [], ["saddle:plugin/observer@0.1.0"]).unwrap();
        assert!(check_component(&ok, [], ["saddle:plugin/plugin-events@0.1.0"]).is_err());
        let custom = BTreeSet::from([Scope::EventSubscribe]);
        check_component(&custom, [], ["saddle:plugin/plugin-events@0.1.0"]).unwrap();
        assert!(check_component(&custom, [], ["saddle:plugin/observer@0.1.0"]).is_err());
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
        assert!(check_component(&all, ["wasi:http/outgoing-handler@0.2.3"], []).is_err());
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
    }
}
