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

    let mut gate = run::cargo();
    gate.args([
        "llvm-cov",
        "report",
        "--summary-only",
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

#[cfg(test)]
mod tests {
    use super::IGNORE_FILENAME_REGEX;
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
}
