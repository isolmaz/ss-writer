//! Fluent message ids: every id referenced in code must be defined in `assets/i18n/en/*.ftl`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use fluent_syntax::ast::Entry;
use regex::Regex;

use super::Report;
use crate::error::Error;
use crate::fsx;

/// The message ids defined by the English source files and where each one lives.
#[derive(Debug, Default)]
pub struct Defined {
    /// id → file name.
    pub ids: BTreeMap<String, String>,
}

impl Defined {
    /// The first `-`-separated segment of every defined id (`cmd`, `settings`, `sidebar`…):
    /// the "areas" whose ids code literals are checked against.
    fn areas(&self) -> BTreeSet<&str> {
        self.ids
            .keys()
            .filter_map(|id| id.split('-').next())
            .collect()
    }
}

/// Parses `(file name, source)` pairs, collecting message ids. Syntax errors and ids defined
/// twice are problems.
pub fn defined_ids(sources: &[(String, String)], report: &mut Report) -> Defined {
    let mut defined = Defined::default();
    for (file, source) in sources {
        let resource = match fluent_syntax::parser::parse(source.as_str()) {
            Ok(resource) => resource,
            Err((partial, errors)) => {
                for error in errors {
                    report.problem(format!("{file}: Fluent syntax error: {error}"));
                }
                partial
            }
        };
        for entry in resource.body {
            let Entry::Message(message) = entry else {
                continue;
            };
            let id = message.id.name.to_owned();
            if let Some(first) = defined.ids.get(&id) {
                report.problem(format!(
                    "{file}: message `{id}` is already defined in {first}"
                ));
            } else {
                defined.ids.insert(id, file.clone());
            }
        }
    }
    defined
}

/// A whole string literal shaped like a message id: lowercase words joined by `-`.
fn id_literal() -> Regex {
    // The pattern is a constant; every test of this module compiles and uses it.
    Regex::new(r#""([a-z][a-z0-9]*(?:-[a-z0-9]+)+)""#).expect("the id literal pattern is valid")
}

/// Message ids written as string literals in `source`: `(line number, id)`. Only literals
/// whose first segment is one of `areas` count, which keeps strings such as `"ssw-core"` out.
/// Comment lines are skipped.
pub fn referenced_ids<'a>(
    source: &'a str,
    areas: &BTreeSet<&str>,
    literal: &Regex,
) -> Vec<(usize, &'a str)> {
    let mut found = Vec::new();
    for (index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for capture in literal.captures_iter(line) {
            let Some(id) = capture.get(1).map(|m| m.as_str()) else {
                continue;
            };
            if id
                .split('-')
                .next()
                .is_some_and(|area| areas.contains(area))
            {
                found.push((index + 1, id));
            }
        }
    }
    found
}

/// Reports every literal id in `files` (`(display path, source text)`) that `defined` lacks.
pub fn check_references(defined: &Defined, files: &[(String, String)], report: &mut Report) {
    let areas = defined.areas();
    let literal = id_literal();
    for (path, source) in files {
        for (line, id) in referenced_ids(source, &areas, &literal) {
            if !defined.ids.contains_key(id) {
                report.problem(format!(
                    "{path}:{line}: message id `{id}` is not defined in assets/i18n/en"
                ));
            }
        }
    }
}

/// All `.ftl` files directly inside `dir`, as `(file name, source)`.
pub fn read_ftl_sources(dir: &Path) -> Result<Vec<(String, String)>, Error> {
    let mut sources = Vec::new();
    for (path, is_dir) in fsx::read_dir_sorted(dir)? {
        if is_dir || path.extension().is_none_or(|e| e != "ftl") {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        sources.push((name, fsx::read_text(&path)?));
    }
    Ok(sources)
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    for (path, is_dir) in fsx::read_dir_sorted(dir)? {
        if is_dir {
            collect_rust_files(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Every `.rs` file under `crates/*/` (sources and integration tests), as
/// `(path relative to root with `/`, text)`.
pub fn read_rust_sources(root: &Path) -> Result<Vec<(String, String)>, Error> {
    let mut paths = Vec::new();
    for (krate, is_dir) in fsx::read_dir_sorted(&root.join("crates"))? {
        if is_dir {
            collect_rust_files(&krate, &mut paths)?;
        }
    }
    paths
        .into_iter()
        .map(|path| -> Result<(String, String), Error> {
            let shown = path
                .strip_prefix(root)
                .unwrap_or(path.as_path())
                .to_string_lossy()
                .replace('\\', "/");
            Ok((shown, fsx::read_text(&path)?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, s)| ((*n).to_owned(), (*s).to_owned()))
            .collect()
    }

    const MAIN: &str = "app-name = SS Writer\ncmd-file-new = New file\n# a comment\n-term = T\n";

    #[test]
    fn message_ids_are_collected_from_every_file_and_terms_are_ignored() {
        let mut report = Report::default();
        let defined = defined_ids(
            &sources(&[
                ("main.ftl", MAIN),
                ("sidebar.ftl", "sidebar-title = Files\n"),
            ]),
            &mut report,
        );
        assert!(report.problems.is_empty(), "{:?}", report.problems);
        let ids: Vec<&str> = defined.ids.keys().map(String::as_str).collect();
        assert_eq!(ids, vec!["app-name", "cmd-file-new", "sidebar-title"]);
        assert_eq!(defined.ids["sidebar-title"], "sidebar.ftl");
        let areas: Vec<&str> = defined.areas().into_iter().collect();
        assert_eq!(areas, vec!["app", "cmd", "sidebar"]);
    }

    #[test]
    fn duplicate_ids_and_syntax_errors_are_problems() {
        let mut report = Report::default();
        let defined = defined_ids(
            &sources(&[
                ("a.ftl", "dup-id = One\n"),
                ("b.ftl", "dup-id = Two\nbroken = { \n"),
            ]),
            &mut report,
        );
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("b.ftl: message `dup-id` is already defined in a.ftl")),
            "{:?}",
            report.problems
        );
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("Fluent syntax error")),
            "{:?}",
            report.problems
        );
        assert_eq!(defined.ids["dup-id"], "a.ftl");
    }

    #[test]
    fn undefined_ids_in_area_prefixed_literals_are_reported_with_file_and_line() {
        let mut report = Report::default();
        let defined = defined_ids(&sources(&[("main.ftl", MAIN)]), &mut report);
        let code = "\
fn labels() {
    // \"cmd-in-a-comment\" is ignored
    let ok = loc.text(\"cmd-file-new\");
    let missing = loc.text(\"cmd-file-nope\");
    let unrelated = \"ssw-core\";
    let other_area = \"zzz-not-an-area\";
}
";
        check_references(
            &defined,
            &[("crates/x/src/lib.rs".to_owned(), code.to_owned())],
            &mut report,
        );
        assert_eq!(
            report.problems,
            vec![
                "crates/x/src/lib.rs:4: message id `cmd-file-nope` is not defined in assets/i18n/en"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn ftl_and_rust_sources_are_read_from_disk() {
        let root = tempfile::tempdir().unwrap();
        fsx::write_file(&root.path().join("i18n/main.ftl"), MAIN).unwrap();
        fsx::write_file(&root.path().join("i18n/notes.txt"), "x").unwrap();
        fsx::write_file(&root.path().join("crates/a/src/lib.rs"), "fn a() {}").unwrap();
        fsx::write_file(&root.path().join("crates/a/tests/t.rs"), "fn t() {}").unwrap();
        fsx::write_file(&root.path().join("crates/a/README.md"), "x").unwrap();
        fsx::write_file(&root.path().join("crates/stray.rs"), "fn s() {}").unwrap();

        let ftl = read_ftl_sources(&root.path().join("i18n")).unwrap();
        assert_eq!(ftl, sources(&[("main.ftl", MAIN)]));
        let rust = read_rust_sources(root.path()).unwrap();
        let names: Vec<&str> = rust.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, vec!["crates/a/src/lib.rs", "crates/a/tests/t.rs"]);
    }

    #[test]
    fn the_real_main_ftl_parses_cleanly_and_the_real_code_is_found() {
        let workspace = crate::workspace::Workspace::locate().unwrap();
        let mut report = Report::default();
        let ftl = read_ftl_sources(&workspace.path("assets/i18n/en")).unwrap();
        let defined = defined_ids(&ftl, &mut report);
        assert_eq!(report.problems, Vec::<String>::new());
        assert!(defined.ids.contains_key("cmd-file-new"));
        let code = read_rust_sources(&workspace.root).unwrap();
        assert!(
            code.iter()
                .any(|(path, _)| path == "crates/ssw-core/src/command.rs")
        );
    }
}
