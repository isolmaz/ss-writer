//! `cargo xtask dist`: the release artifacts under `dist/`.
//!
//! * `SSWriter-<version>-x64.msi` — per-user installer built from `installer/*.wxs` with WiX;
//! * `SSWriter-<version>-x64-portable.zip` — exe + `portable` marker + licenses + notices;
//! * `THIRD-PARTY-NOTICES.txt`, `SHA256SUMS.txt`;
//! * `winget/` — the manifests of `packaging/winget/` with version, URL hash and product code.

use std::path::{Path, PathBuf};

use crate::cli::DistArgs;
use crate::error::Error;
use crate::fsx;
use crate::hash;
use crate::metadata::Metadata;
use crate::notices;
use crate::package;
use crate::run;
use crate::winget;
use crate::wix::{self, BuildInputs, ReleaseVersion};
use crate::workspace::Workspace;

/// The workspace members whose binaries are shipped; their normal dependencies are the
/// third-party components of the notices file.
const SHIPPED_ROOTS: &[&str] = &["ssw-app", "ssw-preview"];

/// The WiX command-line tool: the .NET global tool when installed, else whatever `PATH` finds.
fn wix_program() -> PathBuf {
    let installed = std::env::var_os("USERPROFILE")
        .map(|profile| {
            PathBuf::from(profile)
                .join(".dotnet")
                .join("tools")
                .join("wix.exe")
        })
        .filter(|path| path.is_file());
    installed.unwrap_or_else(|| PathBuf::from("wix"))
}

fn wxs_sources(installer_dir: &Path) -> Result<Vec<PathBuf>, Error> {
    let sources: Vec<PathBuf> = fsx::read_dir_sorted(installer_dir)?
        .into_iter()
        .filter(|(path, is_dir)| !is_dir && path.extension().is_some_and(|e| e == "wxs"))
        .map(|(path, _)| path)
        .collect();
    if sources.is_empty() {
        return Err(Error::Invalid(format!(
            "no .wxs sources in {}",
            installer_dir.display()
        )));
    }
    Ok(sources)
}

/// Builds everything. Steps run in dependency order and stop at the first failure.
pub fn run(workspace: &Workspace, args: &DistArgs) -> Result<(), Error> {
    let version = ReleaseVersion::parse(&workspace.version()?)?;
    if let Some(tag) = &args.expect_tag {
        wix::check_tag(tag, version)?;
    }
    let version_text = version.text();

    let metadata = Metadata::load(workspace)?;

    let mut build = run::cargo();
    build
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "ssw-app",
            "-p",
            "ssw-preview",
        ])
        .current_dir(&workspace.root);
    run::run(&mut build)?;
    let release_dir = metadata.target_directory.join("release");
    let exe = release_dir.join("SSWriter.exe");

    let dist = workspace.path("dist");
    fsx::create_dir(&dist)?;

    // Notices.
    let shipped = metadata.shipped_third_party(SHIPPED_ROOTS);
    let crate_dirs: Vec<PathBuf> = shipped
        .iter()
        .filter_map(|package| package.manifest_path.parent().map(Path::to_path_buf))
        .collect();
    let mut components = notices::crate_components(&shipped)?;
    components.extend(notices::vendored_directories(
        &workspace.path("third_party"),
        &crate_dirs,
    )?);
    let fonts = notices::bundled_fonts(&workspace.path("assets/fonts"))?;
    let notices_path = dist.join("THIRD-PARTY-NOTICES.txt");
    let notices_text = notices::render(&fonts, &components);
    fsx::write_file(&notices_path, &notices_text)?;

    // Portable zip.
    let zip_path = dist.join(format!("SSWriter-{version_text}-x64-portable.zip"));
    let font_licenses = fonts
        .licenses
        .iter()
        .map(|license| (license.file.clone(), license.text.clone().into_bytes()))
        .collect();
    let entries = package::portable_entries(
        fsx::read_bytes(&exe)?,
        fsx::read_bytes(&workspace.path("LICENSE"))?,
        notices_text.into_bytes(),
        font_licenses,
    );
    package::write_zip(&zip_path, &entries)?;

    // MSI.
    let msi_path = dist.join(format!("SSWriter-{version_text}-x64.msi"));
    let sources = wxs_sources(&workspace.path("installer"))?;
    let defines = [
        ("Version", version_text.clone()),
        ("ProductCode", version.product_code()),
        ("BinDir", release_dir.display().to_string()),
        ("AssetsDir", workspace.path("assets").display().to_string()),
        (
            "LicenseFile",
            workspace.path("LICENSE").display().to_string(),
        ),
        ("NoticesFile", notices_path.display().to_string()),
    ];
    let inputs = BuildInputs {
        sources: &sources,
        output: &msi_path,
        defines: &defines,
        accept_eula: args.accept_wix_eula,
    };
    let mut wix_build = run::tool(wix_program());
    wix_build
        .args(wix::build_args(&inputs))
        .current_dir(&workspace.root);
    run::run(&mut wix_build)?;
    let mut wix_validate = run::tool(wix_program());
    wix_validate
        .args(wix::validate_args(&msi_path, args.accept_wix_eula))
        .current_dir(&workspace.root);
    run::run(&mut wix_validate)?;

    // Checksums and winget manifests.
    let msi_sha = hash::sha256_file(&msi_path)?;
    let zip_sha = hash::sha256_file(&zip_path)?;
    let file_name = |path: &Path| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    fsx::write_file(
        &dist.join("SHA256SUMS.txt"),
        hash::checksum_lines(&[
            (file_name(&msi_path), msi_sha.clone()),
            (file_name(&zip_path), zip_sha),
        ]),
    )?;

    let winget_dir = dist.join("winget");
    fsx::remove_dir_if_exists(&winget_dir)?;
    let manifests = winget::generate(
        &workspace.path("packaging/winget"),
        &winget_dir,
        &[
            ("Version", version_text),
            ("Sha256", msi_sha.to_ascii_uppercase()),
            ("ProductCode", version.product_code()),
        ],
    )?;

    println!("dist: {}", msi_path.display());
    println!("dist: {}", zip_path.display());
    println!("dist: {}", notices_path.display());
    println!("dist: {}", dist.join("SHA256SUMS.txt").display());
    for manifest in manifests {
        println!("dist: {}", manifest.display());
    }
    Ok(())
}
