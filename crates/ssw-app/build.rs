//! Lists every `assets/i18n/<locale>/*.ftl` and `assets/welcome/<locale>.md` file and generates
//! the `include_str!` table the app embeds, so a new `.ftl` file is picked up without editing
//! any Rust source.
//!
//! On Windows it also links the application manifest and a `.res` file with the application
//! and document icons (`assets/icons/*.ico`, made by `cargo xtask icons`) and the version
//! information into `SSWriter.exe`.

mod build_resources;

use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use build_resources::{ResourceFile, parse_ico};

/// Resource ids of the icon groups in `SSWriter.exe`: `1` is the application icon (the first
/// group, so it is the one Explorer shows), `2` the document icon that the installer's ProgID
/// `DefaultIcon` entries select with `SSWriter.exe,-2`.
const ICON_GROUPS: [IconGroup; 2] = [
    IconGroup {
        group_id: 1,
        first_icon_id: 1,
        file: "app.ico",
    },
    IconGroup {
        group_id: 2,
        first_icon_id: 101,
        file: "document.ico",
    },
];

/// One icon of the executable: an `.ico` file embedded as `RT_GROUP_ICON` `group_id` with its
/// images as `RT_ICON` resources numbered from `first_icon_id`.
struct IconGroup {
    group_id: u16,
    first_icon_id: u16,
    file: &'static str,
}

/// Writes `SSWriter.res` (icons and `VERSIONINFO`) into `OUT_DIR` and returns its path.
fn write_resource_file(icons: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let mut resources = ResourceFile::default();
    for group in &ICON_GROUPS {
        let path = icons.join(group.file);
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = fs::read(&path).map_err(|error| {
            format!(
                "cannot read {} (run `cargo xtask icons`): {error}",
                path.display()
            )
        })?;
        let images = parse_ico(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
        resources.add_icon(group.group_id, group.first_icon_id, &images)?;
    }

    let major: u16 = std::env::var("CARGO_PKG_VERSION_MAJOR")?.parse()?;
    let minor: u16 = std::env::var("CARGO_PKG_VERSION_MINOR")?.parse()?;
    let patch: u16 = std::env::var("CARGO_PKG_VERSION_PATCH")?.parse()?;
    let prerelease = !std::env::var("CARGO_PKG_VERSION_PRE")?.is_empty();
    let product_version = std::env::var("CARGO_PKG_VERSION")?;
    let file_version = format!("{major}.{minor}.{patch}.0");
    resources.add_version(
        [major, minor, patch, 0],
        prerelease,
        &[
            ("CompanyName", "isolmaz"),
            ("FileDescription", "SS Writer"),
            ("FileVersion", &file_version),
            ("InternalName", "SSWriter"),
            ("LegalCopyright", "\u{a9} isolmaz"),
            ("OriginalFilename", "SSWriter.exe"),
            ("ProductName", "SS Writer"),
            ("ProductVersion", &product_version),
        ],
    );

    let target = PathBuf::from(std::env::var("OUT_DIR")?).join("SSWriter.res");
    fs::write(&target, resources.into_bytes())?;
    Ok(target)
}

/// Sorted entries of `dir`.
fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        entries.push(entry?.path());
    }
    entries.sort();
    Ok(entries)
}

fn utf8_name(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{} has no UTF-8 file name", path.display()).into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let assets = manifest.join("..").join("..").join("assets");
    let i18n = assets.join("i18n");
    let welcome = assets.join("welcome");
    println!("cargo:rerun-if-changed={}", i18n.display());
    println!("cargo:rerun-if-changed={}", welcome.display());

    // The executable needs Common-Controls v6 (TaskDialogIndirect), per-monitor DPI v2, UTF-8
    // and long paths. `embed-manifest` links it into the binary targets; the integration test
    // executables get the same manifest, because they create real windows and task dialogs.
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let manifest_file = manifest.join("SSWriter.manifest");
        println!("cargo:rerun-if-changed={}", manifest_file.display());
        embed_manifest::embed_manifest_file(&manifest_file)
            .map_err(|error| format!("cannot embed the application manifest: {error}"))?;
        println!("cargo:rustc-link-arg-tests=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-tests=/MANIFESTINPUT:{}",
            manifest_file.display()
        );

        // Icons and version information: a `.res` file the MSVC linker merges into the
        // executable's resources. It holds no `RT_MANIFEST` (the manifest above is embedded
        // by the linker itself), so the executable ends up with exactly one of each.
        if std::env::var("CARGO_CFG_TARGET_ENV")? != "msvc" {
            return Err("SS Writer links its resources with the MSVC linker only".into());
        }
        let resource_file = write_resource_file(&assets.join("icons"))?;
        println!("cargo:rustc-link-arg-bins={}", resource_file.display());
    }

    let mut out = String::new();
    out.push_str("/// `(locale, file name, source)` of every embedded Fluent file.\n");
    out.push_str("pub const FTL_FILES: &[(&str, &str, &str)] = &[\n");
    for locale_dir in sorted_entries(&i18n)? {
        if !locale_dir.is_dir() {
            continue;
        }
        let locale = utf8_name(&locale_dir)?.to_owned();
        for file in sorted_entries(&locale_dir)? {
            if file.extension().and_then(|ext| ext.to_str()) != Some("ftl") {
                continue;
            }
            let name = utf8_name(&file)?;
            let canonical = fs::canonicalize(&file)?;
            println!("cargo:rerun-if-changed={}", canonical.display());
            writeln!(
                out,
                "    ({locale:?}, {name:?}, include_str!({:?})),",
                canonical.to_string_lossy().trim_start_matches(r"\\?\")
            )?;
        }
    }
    out.push_str("];\n\n");
    out.push_str("/// `(locale, source)` of every embedded welcome document.\n");
    out.push_str("pub const WELCOME_FILES: &[(&str, &str)] = &[\n");
    if welcome.is_dir() {
        for file in sorted_entries(&welcome)? {
            if file.extension().and_then(|ext| ext.to_str()) != Some("md") {
                continue;
            }
            let stem = file
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or("welcome file without a UTF-8 name")?;
            let canonical = fs::canonicalize(&file)?;
            println!("cargo:rerun-if-changed={}", canonical.display());
            writeln!(
                out,
                "    ({stem:?}, include_str!({:?})),",
                canonical.to_string_lossy().trim_start_matches(r"\\?\")
            )?;
        }
    }
    out.push_str("];\n");

    let target = PathBuf::from(std::env::var("OUT_DIR")?).join("embedded_assets.rs");
    fs::write(target, out)?;
    Ok(())
}
