//! `cargo xtask coverage`: the 100 % coverage gate (architecture.md "Testing").

use crate::error::Error;
use crate::fsx;
use crate::run;
use crate::workspace::Workspace;

/// Files excluded from the report: test sources, xtask itself, vendored third-party code and
/// `ssw-win/src/system_ui.rs`, the documented exception that only holds the calls putting
/// system UI on the screen (popup menu, file, task and print dialogs), which no test may make.
const IGNORE_FILENAME_REGEX: &str =
    r"(^|[\\/])(tests|xtask|third_party)[\\/]|[\\/]ssw-win[\\/]src[\\/]system_ui\.rs$";

/// Runs every test under instrumentation with all features (the test-only `fault-injection`
/// features of the platform crates included), writes `lcov.info` (workspace root) and
/// `target/coverage-summary.txt`, and fails unless lines, regions and functions are all at 100 %.
pub fn run(workspace: &Workspace) -> Result<(), Error> {
    let mut collect = run::cargo();
    collect
        .args([
            "llvm-cov",
            "nextest",
            "--workspace",
            "--all-features",
            "--no-report",
        ])
        .current_dir(&workspace.root);
    run::run(&mut collect)?;

    let lcov = workspace.path("lcov.info");
    let mut export = run::cargo();
    export
        .args(["llvm-cov", "report", "--lcov", "--ignore-filename-regex"])
        .arg(IGNORE_FILENAME_REGEX)
        .arg("--output-path")
        .arg(&lcov)
        .current_dir(&workspace.root);
    run::run(&mut export)?;
    println!("coverage: wrote {}", lcov.display());

    // Every region no test reached, by file and position: the summary only counts them.
    let json = workspace.path("target/coverage.json");
    let mut export_json = run::cargo();
    export_json
        .args(["llvm-cov", "report", "--json", "--ignore-filename-regex"])
        .arg(IGNORE_FILENAME_REGEX)
        .arg("--output-path")
        .arg(&json)
        .current_dir(&workspace.root);
    run::run(&mut export_json)?;
    let export: serde_json::Value = serde_json::from_slice(&fsx::read_bytes(&json)?)
        .map_err(|error| Error::Failed(format!("{}: {error}", json.display())))?;
    let ignored = regex::Regex::new(IGNORE_FILENAME_REGEX).expect("a valid pattern");
    let uncovered = uncovered_regions(&export, &ignored);
    let uncovered_path = workspace.path("target/coverage-uncovered.txt");
    fsx::write_file(&uncovered_path, uncovered.join("\n").as_bytes())?;
    println!(
        "coverage: {} uncovered regions in {}",
        uncovered.len(),
        uncovered_path.display()
    );

    let mut gate = run::cargo();
    gate.args([
        "llvm-cov",
        "report",
        "--summary-only",
        // The run that fails has to say which lines are missing: the log is all a developer
        // gets, because this gate runs on CI.
        "--show-missing-lines",
        "--ignore-filename-regex",
    ])
    .arg(IGNORE_FILENAME_REGEX)
    .args([
        "--fail-under-lines",
        "100",
        "--fail-under-regions",
        "100",
        "--fail-under-functions",
        "100",
    ])
    .current_dir(&workspace.root);
    let output = run::capture(&mut gate)?;
    let summary = String::from_utf8_lossy(&output.stdout);
    print!("{summary}");
    let summary_path = workspace.path("target/coverage-summary.txt");
    fsx::write_file(&summary_path, summary.as_bytes())?;
    println!("coverage: wrote {}", summary_path.display());

    if output.status.success() {
        println!("coverage: 100 % lines, regions and functions");
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "coverage gate failed ({}): lines, regions and functions must all be 100 %; \
             see {}",
            output.status,
            summary_path.display()
        )))
    }
}

/// The code regions the summary counts as missed, from an `llvm-cov export` (`report --json`),
/// as sorted `file:line:column-line:column` lines; files matching `ignored` are left out.
///
/// The summary groups the compiled copies of a function by where it starts (the instantiations
/// of a generic function, and the same function built into the library and into each test
/// binary) and credits the group with its best single copy. So a region counts as missed when
/// that copy did not run it, even if another copy did: those are the regions listed here.
fn uncovered_regions(export: &serde_json::Value, ignored: &regex::Regex) -> Vec<String> {
    /// `kind` of a code region (the others are expansions, skipped and gap regions).
    const CODE_REGION: u64 = 0;
    type Region = (String, [u64; 4], bool);
    let mut groups: std::collections::BTreeMap<(String, u64, u64), Vec<Vec<Region>>> =
        std::collections::BTreeMap::new();
    let functions = export["data"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|data| data["functions"].as_array().into_iter().flatten());
    for function in functions {
        let filenames = function["filenames"].as_array();
        let file_of = |id: Option<u64>| {
            id.and_then(|id| filenames?.get(usize::try_from(id).ok()?)?.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let regions: Vec<&Vec<serde_json::Value>> = function["regions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_array)
            .collect();
        let Some(first) = regions.first() else {
            continue;
        };
        let number = |region: &[serde_json::Value], index: usize| {
            region.get(index).and_then(serde_json::Value::as_u64)
        };
        let start = (
            file_of(number(first, 5)),
            number(first, 0).unwrap_or_default(),
            number(first, 1).unwrap_or_default(),
        );
        let code: Vec<Region> = regions
            .iter()
            .filter(|region| number(region, 7) == Some(CODE_REGION))
            .map(|region| {
                (
                    file_of(number(region, 5)),
                    [0, 1, 2, 3].map(|index| number(region, index).unwrap_or_default()),
                    number(region, 4).unwrap_or_default() > 0,
                )
            })
            .collect();
        groups.entry(start).or_default().push(code);
    }
    let mut missed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for copies in groups.values() {
        let best = copies
            .iter()
            .max_by_key(|code| code.iter().filter(|(_, _, ran)| *ran).count());
        for (file, [l1, c1, l2, c2], ran) in best.into_iter().flatten() {
            if !ran && !file.is_empty() && !ignored.is_match(file) {
                missed.insert(format!("{file}:{l1}:{c1}-{l2}:{c2}"));
            }
        }
    }
    missed.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{IGNORE_FILENAME_REGEX, uncovered_regions};
    use regex::Regex;

    #[test]
    fn only_tests_tools_vendored_code_and_the_system_ui_calls_are_left_out() {
        let ignored = Regex::new(IGNORE_FILENAME_REGEX).unwrap();
        for path in [
            r"C:\repo\crates\ssw-ui\tests\editor_spell.rs",
            r"C:\repo\xtask\src\coverage.rs",
            r"C:\repo\third_party\mermaid\src\lib.rs",
            r"C:\repo\crates\ssw-win\src\system_ui.rs",
            "/repo/crates/ssw-win/src/system_ui.rs",
        ] {
            assert!(ignored.is_match(path), "{path}");
        }
        for path in [
            r"C:\repo\crates\ssw-win\src\print.rs",
            r"C:\repo\crates\ssw-win\src\menus.rs",
            r"C:\repo\crates\ssw-app\src\ui\system_ui_flows.rs",
            r"C:\repo\crates\ssw-win\src\system_ui.rs.bak",
            r"C:\repo\crates\ssw-core\src\markdown\parse\tests.rs",
        ] {
            assert!(!ignored.is_match(path), "{path}");
        }
    }

    #[test]
    fn a_region_is_missed_when_the_best_copy_of_its_function_did_not_run_it() {
        let export = serde_json::json!({ "data": [{ "functions": [
            // Two copies of one function (a generic, or the library in two test binaries):
            // the second ran more, so its missed region counts even though the first ran it.
            { "filenames": ["src/a.rs"], "regions": [
                [1, 1, 1, 9, 1, 0, 0, 0], [2, 5, 2, 9, 1, 0, 0, 0], [3, 5, 3, 9, 0, 0, 0, 0]
            ]},
            { "filenames": ["src/a.rs"], "regions": [
                [1, 1, 1, 9, 3, 0, 0, 0], [2, 5, 2, 9, 0, 0, 0, 0], [3, 5, 3, 9, 2, 0, 0, 0],
                [4, 5, 4, 9, 2, 0, 0, 0],
                // A gap region and an expansion never count.
                [5, 1, 5, 2, 0, 0, 0, 3], [6, 1, 6, 2, 0, 0, 0, 1]
            ]},
            // Test files are not part of the report.
            { "filenames": ["crates/x/tests/t.rs"], "regions": [[5, 1, 5, 2, 0, 0, 0, 0]] },
            // A region of a file the function expands into (an `include!`, a macro).
            { "filenames": ["src/b.rs", "src/c.rs"], "regions": [
                [7, 1, 9, 1, 1, 0, 0, 0], [7, 3, 7, 4, 0, 1, 0, 0]
            ]},
            { "filenames": ["src/d.rs"], "regions": [] }
        ]}]});
        let ignored = Regex::new(IGNORE_FILENAME_REGEX).unwrap();
        assert_eq!(
            uncovered_regions(&export, &ignored),
            ["src/a.rs:2:5-2:9", "src/c.rs:7:3-7:4"]
        );
        assert!(uncovered_regions(&serde_json::json!({}), &ignored).is_empty());
    }
}
