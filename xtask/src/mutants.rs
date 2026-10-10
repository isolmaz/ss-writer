//! `cargo xtask mutants`: run cargo-mutants and summarize its result.

use std::path::Path;

use crate::cli::MutantsArgs;
use crate::error::Error;
use crate::fsx;
use crate::run;
use crate::workspace::Workspace;

/// The mutants of one cargo-mutants run, by outcome (one description per mutant, e.g.
/// `src/lib.rs:12:5: replace foo -> bool with true`).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcomes {
    /// Mutants the tests detected.
    pub caught: Vec<String>,
    /// Mutants no test detected.
    pub missed: Vec<String>,
    /// Mutants whose test run did not finish in time.
    pub timeout: Vec<String>,
    /// Mutants that did not compile.
    pub unviable: Vec<String>,
}

fn read_list(out_dir: &Path, file: &str) -> Result<Vec<String>, Error> {
    let path = out_dir.join(file);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    Ok(fsx::read_text(&path)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Reads `caught.txt`, `missed.txt`, `timeout.txt` and `unviable.txt` from a `mutants.out`
/// directory.
pub fn load_outcomes(out_dir: &Path) -> Result<Outcomes, Error> {
    if !out_dir.is_dir() {
        return Err(Error::Invalid(format!(
            "cargo-mutants wrote no results to {}",
            out_dir.display()
        )));
    }
    Ok(Outcomes {
        caught: read_list(out_dir, "caught.txt")?,
        missed: read_list(out_dir, "missed.txt")?,
        timeout: read_list(out_dir, "timeout.txt")?,
        unviable: read_list(out_dir, "unviable.txt")?,
    })
}

fn push_list(out: &mut String, title: &str, items: &[String]) {
    out.push_str(&format!("## {title}\n\n"));
    if items.is_empty() {
        out.push_str("None.\n\n");
        return;
    }
    for item in items {
        out.push_str(&format!("- `{item}`\n"));
    }
    out.push('\n');
}

/// The Markdown report: counts, the score and the lists of missed and timed-out mutants.
pub fn render_report(outcomes: &Outcomes) -> String {
    let (caught, missed, timeout, unviable) = (
        outcomes.caught.len(),
        outcomes.missed.len(),
        outcomes.timeout.len(),
        outcomes.unviable.len(),
    );
    let judged = caught + missed + timeout;
    let score = if let Some(tenths) = (caught * 1000).checked_div(judged) {
        format!("{}.{}%", tenths / 10, tenths % 10)
    } else {
        "n/a".to_owned()
    };
    let mut out = String::from("# Mutation testing report\n\n");
    out.push_str("| Outcome | Count |\n|---|---:|\n");
    out.push_str(&format!("| Caught | {caught} |\n"));
    out.push_str(&format!("| Missed | {missed} |\n"));
    out.push_str(&format!("| Timeout | {timeout} |\n"));
    out.push_str(&format!("| Unviable | {unviable} |\n"));
    out.push_str(&format!(
        "| Total | {} |\n\n",
        caught + missed + timeout + unviable
    ));
    out.push_str(&format!(
        "Mutation score (caught / (caught + missed + timeout)): **{score}**\n\n"
    ));
    push_list(&mut out, "Missed mutants", &outcomes.missed);
    push_list(&mut out, "Timed-out mutants", &outcomes.timeout);
    out
}

/// Runs cargo-mutants over the library crates (xtask excluded) and writes
/// `target/mutants-report.md`. Fails when cargo-mutants fails, finds missed mutants or times out.
pub fn run(workspace: &Workspace, args: &MutantsArgs) -> Result<(), Error> {
    let target = workspace.path("target");
    let mut command = run::cargo();
    command
        .args(["mutants", "--workspace", "--exclude", "xtask/**"])
        .args(["--test-tool", "nextest", "--jobs"])
        .arg(args.jobs.to_string())
        .arg("--output")
        .arg(&target)
        .args(&args.extra)
        .current_dir(&workspace.root);
    let status = run::status(&mut command)?;

    let outcomes = load_outcomes(&target.join("mutants.out"))?;
    let report = target.join("mutants-report.md");
    fsx::write_file(&report, render_report(&outcomes))?;
    println!(
        "mutants: {} caught, {} missed, {} timeout, {} unviable; report: {}",
        outcomes.caught.len(),
        outcomes.missed.len(),
        outcomes.timeout.len(),
        outcomes.unviable.len(),
        report.display()
    );
    if status.success() {
        Ok(())
    } else {
        Err(Error::Failed(format!(
            "cargo-mutants exited with {status}; see {}",
            report.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            fsx::write_file(&dir.path().join(name), text).unwrap();
        }
        dir
    }

    #[test]
    fn outcome_lists_are_read_line_by_line_and_missing_files_are_empty() {
        let dir = out_dir(&[
            (
                "caught.txt",
                "a.rs:1:1: replace f -> u8 with 0\n\nb.rs:2:2: delete !\n",
            ),
            ("missed.txt", "c.rs:3:3: replace g -> bool with true\r\n"),
            (
                "unviable.txt",
                "d.rs:4:4: replace h -> String with String::new()",
            ),
        ]);
        let outcomes = load_outcomes(dir.path()).unwrap();
        assert_eq!(
            outcomes,
            Outcomes {
                caught: vec![
                    "a.rs:1:1: replace f -> u8 with 0".to_owned(),
                    "b.rs:2:2: delete !".to_owned()
                ],
                missed: vec!["c.rs:3:3: replace g -> bool with true".to_owned()],
                timeout: vec![],
                unviable: vec!["d.rs:4:4: replace h -> String with String::new()".to_owned()],
            }
        );
    }

    #[test]
    fn a_missing_results_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_outcomes(&dir.path().join("mutants.out")).unwrap_err();
        assert!(err.to_string().contains("no results"), "{err}");
    }

    #[test]
    fn report_has_counts_score_and_the_missed_list() {
        let outcomes = Outcomes {
            caught: vec!["x".to_owned(); 7],
            missed: vec!["src/lib.rs:9:9: replace f -> bool with false".to_owned()],
            timeout: vec!["src/lib.rs:1:1: replace slow".to_owned()],
            unviable: vec!["u".to_owned(); 3],
        };
        let report = render_report(&outcomes);
        assert!(report.starts_with("# Mutation testing report\n"));
        assert!(report.contains("| Caught | 7 |\n"));
        assert!(report.contains("| Missed | 1 |\n"));
        assert!(report.contains("| Timeout | 1 |\n"));
        assert!(report.contains("| Unviable | 3 |\n"));
        assert!(report.contains("| Total | 12 |\n"));
        assert!(report.contains("**77.7%**"), "7 of 9: {report}");
        assert!(report.contains("- `src/lib.rs:9:9: replace f -> bool with false`\n"));
        assert!(report.contains("## Timed-out mutants\n\n- `src/lib.rs:1:1: replace slow`\n"));
    }

    #[test]
    fn a_perfect_run_lists_none_and_an_empty_run_has_no_score() {
        let perfect = render_report(&Outcomes {
            caught: vec!["x".to_owned()],
            ..Outcomes::default()
        });
        assert!(perfect.contains("**100.0%**"));
        assert!(perfect.contains("## Missed mutants\n\nNone.\n"));
        assert!(perfect.contains("## Timed-out mutants\n\nNone.\n"));
        assert!(render_report(&Outcomes::default()).contains("**n/a**"));
    }
}
