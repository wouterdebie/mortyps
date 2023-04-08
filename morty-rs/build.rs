// Necessary because of this issue: https://github.com/rust-lang/cargo/issues/9641
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    prost_build::compile_protos(
        &[format!("{project_dir}/src/morty.proto")],
        &[format!("{project_dir}/src/")],
    )?;
    Ok(())
}
