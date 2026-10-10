//! Locating the workspace and reading its version.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;
use crate::fsx;

/// The repository checkout xtask operates on.
#[derive(Debug)]
pub struct Workspace {
    /// The workspace root (the directory holding the root `Cargo.toml`).
    pub root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RootManifest {
    workspace: WorkspaceTable,
}

#[derive(Debug, Deserialize)]
struct WorkspaceTable {
    package: WorkspacePackage,
}

#[derive(Debug, Deserialize)]
struct WorkspacePackage {
    version: String,
}

impl Workspace {
    /// The workspace xtask was compiled in (`xtask/` sits directly under the root).
    pub fn locate() -> Result<Self, Error> {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        match manifest_dir.parent() {
            Some(root) => Ok(Self {
                root: root.to_path_buf(),
            }),
            None => Err(Error::Invalid(format!(
                "cannot find the workspace root above {}",
                manifest_dir.display()
            ))),
        }
    }

    /// `root` joined with a `/`-separated relative path.
    pub fn path(&self, relative: &str) -> PathBuf {
        relative
            .split('/')
            .fold(self.root.clone(), |path, part| path.join(part))
    }

    /// `[workspace.package] version` of the root manifest.
    pub fn version(&self) -> Result<String, Error> {
        let path = self.path("Cargo.toml");
        parse_workspace_version(&fsx::read_text(&path)?, &path)
    }
}

/// Extracts `[workspace.package] version` from root manifest text (`path` is for messages).
pub fn parse_workspace_version(manifest: &str, path: &Path) -> Result<String, Error> {
    toml::from_str::<RootManifest>(manifest)
        .map(|m| m.workspace.package.version)
        .map_err(|source| Error::Toml {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comes_from_the_workspace_package_table() {
        let manifest = "[workspace]\nmembers = [\"a\"]\n\n[workspace.package]\nversion = \"1.4.2\"\nedition = \"2024\"\n";
        assert_eq!(
            parse_workspace_version(manifest, Path::new("Cargo.toml")).unwrap(),
            "1.4.2"
        );
    }

    #[test]
    fn a_manifest_without_a_version_is_an_error_naming_the_file() {
        let err =
            parse_workspace_version("[workspace]\n", Path::new("root/Cargo.toml")).unwrap_err();
        assert!(err.to_string().contains("Cargo.toml"), "{err}");
    }

    #[test]
    fn the_real_workspace_is_found_and_has_a_version() {
        let workspace = Workspace::locate().unwrap();
        assert!(!workspace.version().unwrap().is_empty());
        assert!(workspace.path("docs/spec.md").is_file());
    }
}
