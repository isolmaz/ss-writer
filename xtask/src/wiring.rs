//! `cargo xtask wiring`: every user-interface module has to be reachable from the application.
//!
//! A widget can be complete and fully tested and still be dead: nothing in the application ever
//! names it, so no user can reach it. Coverage does not notice (its own tests cover it), which is
//! how a finished tooltip widget sat unused in 0.1.2. The check reads the production code of the
//! workspace (test modules removed) and fails for every module below `ssw-ui/src/widgets` and
//! `ssw-ui/src/shell` whose public names appear in no other production file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::fsx;
use crate::workspace::Workspace;

/// The folders whose modules have to be reachable, below `crates/`.
const WATCHED: [&str; 2] = ["ssw-ui/src/widgets", "ssw-ui/src/shell"];

/// Modules that exist for their own sake; `mod.rs` only re-exports.
const SKIP_FILE_NAMES: [&str; 2] = ["mod.rs", "lib.rs"];

/// A module and the names it offers.
#[derive(Debug, PartialEq, Eq)]
pub struct Module {
    /// Path below the workspace root, for the message.
    pub path: PathBuf,
    /// Public item names declared at the top level of the file.
    pub names: BTreeSet<String>,
}

/// Runs the check and prints what it found.
///
/// # Errors
/// [`Error::Io`] when a source file cannot be read, [`Error::Failed`] when a module is unwired.
pub fn run(workspace: &Workspace) -> Result<(), Error> {
    let sources = sources(&workspace.root.join("crates"))?;
    let mut production: Vec<(PathBuf, String)> = Vec::new();
    for path in &sources {
        production.push((path.clone(), without_tests(&fsx::read_text(path)?)));
    }
    let modules = watched_modules(&workspace.root, &production);
    let unwired = unwired(&modules, &production);
    for module in &unwired {
        eprintln!(
            "error: nothing outside {} uses it: {}",
            module.path.display(),
            module.names.iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    println!(
        "wiring: {} module(s) checked, {} unwired",
        modules.len(),
        unwired.len()
    );
    if unwired.is_empty() {
        Ok(())
    } else {
        Err(Error::Failed(
            "a module no other code names cannot be reached by a user".to_owned(),
        ))
    }
}

/// Every `.rs` file below `root`, without the `tests` folders of the crates.
fn sources(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for (path, is_dir) in fsx::read_dir_sorted(&dir)? {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if is_dir {
                if name != "tests" && name != "target" {
                    stack.push(path);
                }
            } else if name.ends_with(".rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The modules of [`WATCHED`] with the names they declare.
fn watched_modules(root: &Path, production: &[(PathBuf, String)]) -> Vec<Module> {
    production
        .iter()
        .filter(|(path, _)| is_watched(root, path))
        .map(|(path, text)| Module {
            path: path.strip_prefix(root).unwrap_or(path).to_path_buf(),
            names: public_names(text),
        })
        .filter(|module| !module.names.is_empty())
        .collect()
}

fn is_watched(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root.join("crates")) else {
        return false;
    };
    let text = relative.to_string_lossy().replace('\\', "/");
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    !SKIP_FILE_NAMES.contains(&name.as_ref())
        && WATCHED.iter().any(|folder| text.starts_with(folder))
}

/// The modules of `modules` no production file outside themselves names.
fn unwired(modules: &[Module], production: &[(PathBuf, String)]) -> Vec<Module> {
    modules
        .iter()
        .filter(|module| {
            !production.iter().any(|(path, text)| {
                !path.ends_with(&module.path) && module.names.iter().any(|name| names(text, name))
            })
        })
        .map(|module| Module {
            path: module.path.clone(),
            names: module.names.clone(),
        })
        .collect()
}

/// Whether `text` uses `name` as an identifier (not as part of a longer one). A `pub use` only
/// passes a name on: it says nothing about anybody reaching the module.
fn names(text: &str, name: &str) -> bool {
    let text: String = text
        .lines()
        .filter(|line| !line.trim_start().starts_with("pub use "))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.as_str();
    let bytes = text.as_bytes();
    text.match_indices(name).any(|(start, _)| {
        let before = start.checked_sub(1).map(|index| bytes[index]);
        let after = bytes.get(start + name.len()).copied();
        let boundary = |byte: Option<u8>| {
            byte.is_none_or(|byte| !byte.is_ascii_alphanumeric() && byte != b'_')
        };
        boundary(before) && boundary(after)
    })
}

/// The names `pub` items declare at the top level of a file.
pub fn public_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in text.lines() {
        // Top level only: an indented `pub fn` belongs to an `impl` block or an inner module.
        if line.starts_with("pub ")
            && let Some(name) = declared_name(line)
        {
            names.insert(name);
        }
    }
    names
}

/// The name a `pub` declaration introduces, if the line declares one.
fn declared_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("pub ")?;
    let rest = rest
        .strip_prefix("async ")
        .or_else(|| rest.strip_prefix("unsafe "))
        .unwrap_or(rest);
    let (keyword, tail) = rest.split_once(' ')?;
    if !matches!(
        keyword,
        "fn" | "struct" | "enum" | "trait" | "type" | "const" | "static" | "union"
    ) {
        return None;
    }
    let name: String = tail
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// `text` without its `#[cfg(test)]` modules: what ships.
pub fn without_tests(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            // Skip the attribute and the item it belongs to, counting braces.
            let mut depth = 0i32;
            let mut started = false;
            for item in lines.by_ref() {
                depth += i32::try_from(item.matches('{').count()).unwrap_or(0);
                depth -= i32::try_from(item.matches('}').count()).unwrap_or(0);
                started |= item.contains('{');
                if started && depth <= 0 {
                    break;
                }
            }
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_names_are_read_from_top_level_declarations_only() {
        let text = "\
pub struct Tooltip;
pub enum Side { Top }
pub fn show() {}
pub const DELAY: u32 = 1;
pub(crate) fn hidden() {}
    pub fn method_of_an_impl() {}
pub use crate::other::Thing;
";
        let names = public_names(text);
        assert_eq!(
            names.iter().cloned().collect::<Vec<_>>(),
            ["DELAY", "Side", "Tooltip", "show"]
        );
    }

    #[test]
    fn test_modules_are_not_production_code() {
        let text = "\
pub fn kept() {}
#[cfg(test)]
mod tests {
    use super::*;
    fn inner() { if true { } }
}
pub fn also_kept() {}
";
        let production = without_tests(text);
        assert!(production.contains("pub fn kept"));
        assert!(production.contains("pub fn also_kept"));
        assert!(!production.contains("mod tests"));
        assert!(!production.contains("inner"));
    }

    #[test]
    fn a_name_counts_only_as_a_whole_identifier() {
        assert!(names("let t = Tooltip::new();", "Tooltip"));
        assert!(!names("let t = TooltipHost::new();", "Tooltip"));
        assert!(!names("let t = MyTooltip::new();", "Tooltip"));
        assert!(names("use crate::widgets::tooltip::Tooltip;", "Tooltip"));
        // Passing the name on is not using it.
        assert!(!names(
            "pub use crate::widgets::tooltip::Tooltip;",
            "Tooltip"
        ));
    }

    #[test]
    fn a_module_nobody_names_is_reported_and_a_used_one_is_not() {
        let modules = vec![
            Module {
                path: PathBuf::from("crates/ssw-ui/src/widgets/tooltip.rs"),
                names: BTreeSet::from(["Tooltip".to_owned()]),
            },
            Module {
                path: PathBuf::from("crates/ssw-ui/src/widgets/button.rs"),
                names: BTreeSet::from(["Button".to_owned()]),
            },
        ];
        let production = vec![
            (
                PathBuf::from("crates/ssw-ui/src/widgets/tooltip.rs"),
                "pub struct Tooltip;".to_owned(),
            ),
            (
                PathBuf::from("crates/ssw-ui/src/widgets/button.rs"),
                "pub struct Button;".to_owned(),
            ),
            (
                PathBuf::from("crates/ssw-app/src/ui/shell_view.rs"),
                "let b = Button::new();".to_owned(),
            ),
        ];

        let found = unwired(&modules, &production);

        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].path,
            PathBuf::from("crates/ssw-ui/src/widgets/tooltip.rs")
        );
    }

    #[test]
    fn only_the_watched_folders_are_checked() {
        let root = Path::new("C:/repo");
        assert!(is_watched(
            root,
            Path::new("C:/repo/crates/ssw-ui/src/widgets/tooltip.rs")
        ));
        assert!(is_watched(
            root,
            Path::new("C:/repo/crates/ssw-ui/src/shell/banners.rs")
        ));
        assert!(!is_watched(
            root,
            Path::new("C:/repo/crates/ssw-ui/src/widgets/mod.rs")
        ));
        assert!(!is_watched(
            root,
            Path::new("C:/repo/crates/ssw-io/src/net.rs")
        ));
    }
}
