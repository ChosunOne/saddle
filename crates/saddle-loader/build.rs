use std::env;
fn main() -> Result<(), env::VarError> {
    println!(
        "cargo:rustc-env=SADDLE_BUILD_TARGET={}",
        env::var("TARGET")?
    );
    Ok(())
}
