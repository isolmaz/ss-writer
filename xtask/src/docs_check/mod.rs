//! `cargo xtask docs-check`: keeps the spec, the keymap, `docs/features.toml`, the README and the
//! Fluent files in sync.
//!
//! * every default shortcut in `ssw-core` equals the spec §11 table;
//! * every feature in `docs/features.toml` points at an existing spec section, every spec
//!   section is covered, and every feature title appears on a README line (a missing README or
//!   missing lines are warnings, failures only with `--strict`);
//! * every message id written as a literal in the code exists in `assets/i18n/en/*.ftl`.

mod features;
mod fluent;
mod shortcuts;
mod spec;

use ssw_core::keymap::Keymap;

use crate::error::Error;
use crate::fsx;
use crate::workspace::Workspace;

/// What the checks found.
#[derive(Debug, Default)]
pub struct Report {
    /// Disagreements: always a failure.
    pub problems: Vec<String>,
    /// Gaps that only fail a `--strict` run.
    pub warnings: Vec<String>,
}

impl Report {
    fn problem(&mut self, message: String) {
        self.problems.push(message);
    }

    fn warning(&mut self, message: String) {
        self.warnings.push(message);
    }
}

/// Runs every check against the checkout and prints the findings.
pub fn run(workspace: &Workspace, strict: bool) -> Result<(), Error> {
    let report = collect(workspace)?;
    for problem in &report.problems {
        eprintln!("error: {problem}");
    }
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    let failed = !report.problems.is_empty() || (strict && !report.warnings.is_empty());
    println!(
        "docs-check: {} problem(s), {} warning(s){}",
        report.problems.len(),
        report.warnings.len(),
        if strict { " (strict)" } else { "" }
    );
    if failed {
        Err(Error::Failed("docs-check failed".to_owned()))
    } else {
        Ok(())
    }
}

fn collect(workspace: &Workspace) -> Result<Report, Error> {
    let mut report = Report::default();
    let spec_text = fsx::read_text(&workspace.path("docs/spec.md"))?;

    report
        .problems
        .extend(shortcuts::check(&spec_text, &Keymap::default()));

    let features: features::Features = {
        let path = workspace.path("docs/features.toml");
        toml::from_str(&fsx::read_text(&path)?).map_err(|source| Error::Toml {
            path,
            source: Box::new(source),
        })?
    };
    features::check_features(&features, &spec::section_numbers(&spec_text), &mut report);
    let readme_path = workspace.path("README.md");
    let readme = if readme_path.is_file() {
        Some(fsx::read_text(&readme_path)?)
    } else {
        None
    };
    features::check_readme(&features, readme.as_deref(), &mut report);

    let ftl = fluent::read_ftl_sources(&workspace.path("assets/i18n/en"))?;
    let defined = fluent::defined_ids(&ftl, &mut report);
    let code = fluent::read_rust_sources(&workspace.root)?;
    fluent::check_references(&defined, &code, &mut report);
    Ok(report)
}
