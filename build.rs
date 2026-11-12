//! Embeds `assets/lantern.ico` as icon resource 1 in the Windows exe.
//!
//! Works on a native Windows build (MSVC `rc.exe` or MinGW `windres`) and when
//! cross-compiling with MinGW. If no resource compiler is found the exe still
//! builds; it just uses the default icon.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=assets/lantern.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if !std::env::var("TARGET").unwrap_or_default().contains("windows") {
        return;
    }
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default());
    let icon = manifest.join("assets").join("lantern.ico").display().to_string().replace('\\', "/");
    let rc = out_dir.join("lantern.rc");
    if std::fs::write(&rc, format!("1 ICON \"{icon}\"\n")).is_err() {
        println!("cargo:warning=Could not write the icon resource script; the exe will use the default icon.");
        return;
    }
    if let Err(err) = embed_resource::compile_for(&rc, ["lantern"], embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=Icon not embedded ({err}); the exe will use the default icon.");
    }
}
