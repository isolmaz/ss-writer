//! The slice of `cargo metadata` output the release tooling needs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::Error;
use crate::run;
use crate::workspace::Workspace;

/// The only platform SS Writer ships for.
const TARGET: &str = "x86_64-pc-windows-msvc";

/// `cargo metadata --format-version 1` (only the fields used here).
#[derive(Debug, Deserialize)]
pub struct Metadata {
    /// Every package in the resolved graph.
    pub packages: Vec<Package>,
    /// Package ids of the workspace members.
    pub workspace_members: Vec<String>,
    /// The resolved dependency graph.
    pub resolve: Option<Resolve>,
    /// Where cargo puts build output.
    pub target_directory: PathBuf,
}

/// One package.
#[derive(Debug, Deserialize)]
pub struct Package {
    /// Unique id (`registry+…#name@version`, `path+file:///…#name@version`).
    pub id: String,
    /// Crate name.
    pub name: String,
    /// Crate version.
    pub version: String,
    /// SPDX license expression.
    pub license: Option<String>,
    /// `license-file` from the manifest.
    pub license_file: Option<PathBuf>,
    /// `repository` from the manifest.
    pub repository: Option<String>,
    /// `None` for path dependencies and workspace members, else the registry/git source.
    pub source: Option<String>,
    /// Path of the package's `Cargo.toml`.
    pub manifest_path: PathBuf,
}

/// The dependency graph.
#[derive(Debug, Deserialize)]
pub struct Resolve {
    /// One node per package.
    pub nodes: Vec<Node>,
}

/// A package and its dependencies.
#[derive(Debug, Deserialize)]
pub struct Node {
    /// The package id.
    pub id: String,
    /// Dependencies with their kinds.
    #[serde(default)]
    pub deps: Vec<NodeDep>,
}

/// An edge of the graph.
#[derive(Debug, Deserialize)]
pub struct NodeDep {
    /// The dependency's package id.
    pub pkg: String,
    /// How the dependency is used (normal, build, dev).
    #[serde(default)]
    pub dep_kinds: Vec<DepKind>,
}

/// One use of a dependency.
#[derive(Debug, Deserialize)]
pub struct DepKind {
    /// `None` for a normal dependency, else `"build"` or `"dev"`.
    pub kind: Option<String>,
}

impl Metadata {
    /// Parses `cargo metadata` JSON.
    pub fn parse(json: &[u8]) -> Result<Self, Error> {
        Ok(serde_json::from_slice(json)?)
    }

    /// Runs `cargo metadata` for the Windows target (`--locked`: the lock file must be current).
    pub fn load(workspace: &Workspace) -> Result<Self, Error> {
        let mut command = run::cargo();
        command
            .args([
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--filter-platform",
                TARGET,
            ])
            .current_dir(&workspace.root);
        let output = run::capture(&mut command)?;
        if !output.status.success() {
            return Err(Error::CommandFailed {
                command: "cargo metadata".to_owned(),
                status: output.status,
            });
        }
        Self::parse(&output.stdout)
    }

    /// The packages that end up in the binaries built from the workspace members named
    /// `roots`: everything reachable through normal (not build, not dev) dependencies,
    /// excluding the workspace members themselves. Sorted by name and version.
    pub fn shipped_third_party(&self, roots: &[&str]) -> Vec<&Package> {
        let members: HashSet<&str> = self.workspace_members.iter().map(String::as_str).collect();
        let nodes: HashMap<&str, &Node> = self
            .resolve
            .iter()
            .flat_map(|resolve| resolve.nodes.iter())
            .map(|node| (node.id.as_str(), node))
            .collect();
        let mut pending: Vec<&str> = self
            .packages
            .iter()
            .filter(|p| members.contains(p.id.as_str()) && roots.contains(&p.name.as_str()))
            .map(|p| p.id.as_str())
            .collect();
        let mut seen: HashSet<&str> = pending.iter().copied().collect();
        while let Some(id) = pending.pop() {
            let Some(node) = nodes.get(id) else {
                continue;
            };
            for dep in &node.deps {
                let normal = dep.dep_kinds.iter().any(|kind| kind.kind.is_none());
                if normal && seen.insert(dep.pkg.as_str()) {
                    pending.push(dep.pkg.as_str());
                }
            }
        }
        let mut shipped: Vec<&Package> = self
            .packages
            .iter()
            .filter(|p| seen.contains(p.id.as_str()) && !members.contains(p.id.as_str()))
            .collect();
        shipped.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
        shipped
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn package(name: &str, source: Option<&str>) -> serde_json::Value {
        json!({
            "id": format!("{name} 1.0.0"),
            "name": name,
            "version": "1.0.0",
            "license": "MIT",
            "license_file": null,
            "repository": null,
            "source": source,
            "manifest_path": format!("C:/src/{name}/Cargo.toml"),
        })
    }

    fn node(name: &str, deps: &[(&str, Option<&str>)]) -> serde_json::Value {
        json!({
            "id": format!("{name} 1.0.0"),
            "deps": deps.iter().map(|(dep, kind)| json!({
                "pkg": format!("{dep} 1.0.0"),
                "dep_kinds": [{"kind": kind, "target": null}],
            })).collect::<Vec<_>>(),
        })
    }

    /// app -> normal(a), build(b), dev(c); a -> normal(d); b -> normal(e); lib is a workspace
    /// member reached from app; other-bin is a second root that is not asked for.
    fn sample() -> Metadata {
        let registry = Some("registry+https://github.com/rust-lang/crates.io-index");
        let value = json!({
            "packages": [
                package("app", None), package("lib", None), package("other-bin", None),
                package("a", registry), package("b", registry), package("c", registry),
                package("d", registry), package("e", registry), package("only-other", registry),
            ],
            "workspace_members": ["app 1.0.0", "lib 1.0.0", "other-bin 1.0.0"],
            "resolve": {"nodes": [
                node("app", &[("a", None), ("b", Some("build")), ("c", Some("dev")), ("lib", None)]),
                node("lib", &[("d", None)]),
                node("a", &[("d", None)]),
                node("b", &[("e", None)]),
                node("other-bin", &[("only-other", None)]),
            ]},
            "target_directory": "C:/src/target",
            "version": 1,
        });
        Metadata::parse(value.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn only_normal_dependencies_of_the_requested_roots_ship() {
        let metadata = sample();
        let names: Vec<&str> = metadata
            .shipped_third_party(&["app"])
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "d"]);
    }

    #[test]
    fn several_roots_are_merged_and_unknown_roots_ship_nothing() {
        let metadata = sample();
        let names: Vec<&str> = metadata
            .shipped_third_party(&["other-bin", "app"])
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "d", "only-other"]);
        assert!(metadata.shipped_third_party(&["nothing"]).is_empty());
    }

    #[test]
    fn target_directory_is_read() {
        assert_eq!(sample().target_directory, PathBuf::from("C:/src/target"));
    }

    #[test]
    fn malformed_json_is_a_metadata_error() {
        assert!(matches!(
            Metadata::parse(b"{\"packages\": 3}"),
            Err(Error::Metadata(_))
        ));
    }
}
