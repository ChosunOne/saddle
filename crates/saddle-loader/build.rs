//! record the executable target so bundled and runtime compilers use the same baseline ISA (D147).
use std::env;
fn main() -> Result<(), env::VarError> {
    println!(
        "cargo:rustc-env=SADDLE_BUILD_TARGET={}",
        env::var("TARGET")?
    );
    Ok(())
}
