//! `cargo xtask coverage`: the 100 % coverage gate (architecture.md "Testing").

use crate::error::Error;
use crate::fsx;
use crate::run;
use crate::workspace::Workspace;

/// Files excluded from the report: test sources and xtask itself, nothing else.
const IGNORE_FILENAME_REGEX: &str = r"(^|[\\/])(tests|xtask)[\\/]";

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
