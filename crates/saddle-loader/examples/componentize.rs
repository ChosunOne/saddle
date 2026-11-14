//! converts an SDK core module into a validated component for local development (D127).

use std::env;
use std::fs;

use anyhow::{Context, Result, bail};
use wit_component::ComponentEncoder;

fn main() -> Result<()> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 2 {
        bail!("usage: componentize <core.wasm> <component.wasm>");
    }
    let module = fs::read(&args[0]).context("reading SDK module")?;
    let component = ComponentEncoder::default()
        .module(&module)?
        .validate(true)
        .encode()
        .context("encoding SDK component")?;
    fs::write(&args[1], component).context("writing component")?;
    Ok(())
}
