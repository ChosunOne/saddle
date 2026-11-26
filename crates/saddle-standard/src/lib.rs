pub struct Artifact {
    pub name: &'static str,
    pub id: &'static str,
    pub wasm: &'static [u8],
    pub native: &'static [u8],
    pub manifest: &'static str,
    pub sha256: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/artifacts.rs"));

pub fn artifact(name: &str) -> Option<&'static Artifact> {
    ARTIFACTS.iter().find(|artifact| artifact.name == name)
}

pub const DEFAULT_PROFILES: &str = r#"
[standard-tools]
read = ["fs-read", "tool-read"]
ls = ["fs-read", "tool-read"]
find = ["fs-read", "tool-read"]
grep = ["fs-read", "tool-read"]
write = ["fs-read", "fs-write", "tool-read", "tool-write"]
edit = ["fs-read", "fs-write", "tool-read", "tool-write"]
patch = ["fs-read", "fs-write", "tool-read", "tool-write"]
bash = ["exec", "fs-read", "fs-write", "net"]
run_binary = ["run-binary", "net"]
tasks = ["exec"]
fetch = ["net"]
websearch = ["net"]
agent = ["transcript-read", "model-invoke", "fs-read", "fs-write"]
swarm = ["transcript-read", "model-invoke"]
session_query = ["transcript-read"]
config_help = []
ask = []
"#;

#[doc(hidden)]
pub const SDK_READER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sdk-reader.wasm"));

#[doc(hidden)]
pub const SDK_HOOKS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sdk-hooks.wasm"));

#[doc(hidden)]
pub const SDK_EVENT_LISTENER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/sdk-event-listener.wasm"));

#[doc(hidden)]
pub const SDK_FEATURES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sdk-features.wasm"));
