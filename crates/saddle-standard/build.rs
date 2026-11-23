//! build the guest sources once, then compile immutable target-specific delivery artifacts (D147).

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use saddle_cap::manifest::Manifest;
use saddle_loader::Loader;
use sha2::{Digest, Sha256};
use wit_component::ComponentEncoder;

const NAMES: &[&str] = &[
    "read",
    "write",
    "edit",
    "patch",
    "ls",
    "find",
    "grep",
    "bash",
    "run_binary",
    "tasks",
    "fetch",
    "websearch",
    "session_query",
    "config_help",
];

fn main() -> Result<(), Box<dyn Error>> {
    let workspace = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?)
        .join("../..")
        .canonicalize()?;
    for source in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/saddle-sdk",
        "crates/saddle-wit",
        "crates/saddle-tools",
        "crates/saddle-limits",
        "crates/saddle-ignore",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace.join(source).display()
        );
    }
    let out = PathBuf::from(env::var("OUT_DIR")?);
    // A distinct target directory avoids recursively acquiring the parent Cargo build lock.
    // This invocation selects only guests/SDKs, never this package, and uses the same lockfile.
    let guest_target = workspace.join("target/standard-guests");
    let status = Command::new(env::var_os("CARGO").ok_or("CARGO is unavailable")?)
        .current_dir(&workspace)
        .args([
            "build",
            "--locked",
            "-p",
            "saddle-tools",
            "-p",
            "saddle-sdk",
            "--examples",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .env("CARGO_TARGET_DIR", &guest_target)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("RUSTFLAGS")
        .status()?;
    if !status.success() {
        return Err(
            "standard guest build failed; install the wasm32-unknown-unknown Rust target".into(),
        );
    }
    let reader =
        fs::read(guest_target.join("wasm32-unknown-unknown/release/examples/reader.wasm"))?;
    fs::write(
        out.join("sdk-reader.wasm"),
        ComponentEncoder::default()
            .module(&reader)?
            .validate(true)
            .encode()?,
    )?;
    let hooks = fs::read(guest_target.join("wasm32-unknown-unknown/release/examples/hooks.wasm"))?;
    fs::write(
        out.join("sdk-hooks.wasm"),
        ComponentEncoder::default()
            .module(&hooks)?
            .validate(true)
            .encode()?,
    )?;
    let listener =
        fs::read(guest_target.join("wasm32-unknown-unknown/release/examples/event_listener.wasm"))?;
    fs::write(
        out.join("sdk-event-listener.wasm"),
        ComponentEncoder::default()
            .module(&listener)?
            .validate(true)
            .encode()?,
    )?;
    let features =
        fs::read(guest_target.join("wasm32-unknown-unknown/release/examples/features.wasm"))?;
    fs::write(
        out.join("sdk-features.wasm"),
        ComponentEncoder::default()
            .module(&features)?
            .validate(true)
            .encode()?,
    )?;
    let target = env::var("TARGET")?;
    let mut generated = String::from(
        "/// immutable components shipped with this binary.\npub static ARTIFACTS: &[Artifact] = &[\n",
    );
    for name in NAMES {
        let module = fs::read(guest_target.join(format!(
            "wasm32-unknown-unknown/release/examples/{name}.wasm"
        )))?;
        let wasm = ComponentEncoder::default()
            .module(&module)?
            .validate(true)
            .encode()?;
        let native = Loader::precompile_for_target(&wasm, &target)?;
        let hash = format!("{:x}", Sha256::digest(&wasm));
        let manifest = Manifest::parse(&fs::read_to_string(
            workspace.join(format!("crates/saddle-tools/plugins/{name}/plugin.toml")),
        )?)?;
        let id = format!("standard-{}", name.replace('_', "-"));
        let scopes = manifest
            .scopes
            .iter()
            .map(|scope| format!("\"{scope}\""))
            .collect::<Vec<_>>()
            .join(",");
        let manifest = format!(
            "id='{id}'\nversion='{}'\ncomponent='{name}.component.wasm'\nscopes=[{scopes}]\n",
            manifest.version
        );
        fs::write(out.join(format!("{name}.wasm")), &wasm)?;
        fs::write(out.join(format!("{name}.native")), native)?;
        writeln!(
            generated,
            "Artifact {{ name: {name:?}, id: {id:?}, wasm: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}.wasm\")), native: include_bytes!(concat!(env!(\"OUT_DIR\"), \"/{name}.native\")), manifest: {manifest:?}, sha256: {hash:?} }},"
        )?;
    }
    generated.push_str("];\n");
    fs::write(out.join("artifacts.rs"), generated)?;
    Ok(())
}
