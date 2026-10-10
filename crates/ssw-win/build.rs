//! Links the application manifest (`crates/ssw-app/SSWriter.manifest`) into this crate's test
//! executables, so they run with what SS Writer itself runs with: Common-Controls v6
//! (`TaskDialogIndirect`, activation-context lookups), per-monitor DPI v2, UTF-8 and long
//! paths. The library itself links nothing; only linked artifacts of this package (unit and
//! integration test executables) receive the arguments.

fn main() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let manifest = manifest_dir
        .join("..")
        .join("ssw-app")
        .join("SSWriter.manifest");
    println!("cargo:rerun-if-changed={}", manifest.display());
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    }
}
