use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=winpilot.manifest");

    let msvc = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows"
        && std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default() == "msvc";
    if !msvc {
        return;
    }

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("winpilot.manifest");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.to_string_lossy()
    );
}
