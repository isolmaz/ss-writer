//! `docs/features.toml` ↔ spec sections ↔ README.

use std::collections::BTreeSet;

use serde::Deserialize;

use super::Report;

/// `docs/features.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Features {
    /// Spec sections that describe no user-visible feature (performance targets…).
    #[serde(default)]
    pub non_feature_sections: Vec<String>,
    /// The features.
    #[serde(default)]
    pub feature: Vec<Feature>,
}

/// One user-visible feature.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    /// Stable lowercase identifier (`live-rendering`).
    pub id: String,
    /// Short name; the README lists the feature under this exact title.
    pub title: String,
    /// The spec section describing it (`2.1`).
    pub spec: String,
    /// One or two sentences for the README and the website.
    pub description: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// A section is covered when a feature names it, or names one of its subsections, or the file
/// lists it as a non-feature section.
fn covered(section: &str, features: &Features) -> bool {
    let prefix = format!("{section}.");
    features.non_feature_sections.iter().any(|s| s == section)
        || features
            .feature
            .iter()
            .any(|f| f.spec == section || f.spec.starts_with(&prefix))
}

/// Checks the feature list against the spec's sections. Problems: malformed or duplicate
/// entries, references to sections that do not exist, spec sections nobody covers.
pub fn check_features(features: &Features, sections: &BTreeSet<String>, report: &mut Report) {
    let mut ids = BTreeSet::new();
    for feature in &features.feature {
        let id = &feature.id;
        if !valid_id(id) {
            report.problem(format!(
                "features.toml: id `{id}` must be lowercase words joined by `-`"
            ));
        }
        if !ids.insert(id.as_str()) {
            report.problem(format!("features.toml: duplicate id `{id}`"));
        }
        if feature.title.trim().is_empty() || feature.description.trim().is_empty() {
            report.problem(format!(
                "features.toml: `{id}` needs a title and a description"
            ));
        }
        if !sections.contains(&feature.spec) {
            report.problem(format!(
                "features.toml: `{id}` references spec section `{}` which docs/spec.md does not have",
                feature.spec
            ));
        }
    }
    for section in &features.non_feature_sections {
        if !sections.contains(section) {
            report.problem(format!(
                "features.toml: non_feature_sections names `{section}` which docs/spec.md does not have"
            ));
        }
    }
    for section in sections {
        if !covered(section, features) {
            report.problem(format!(
                "spec section {section} has no feature in docs/features.toml (add one, or list it \
                 in non_feature_sections if it describes no user-visible feature)"
            ));
        }
    }
}

/// Every feature title must appear on some README line (case-insensitive). A missing README or
/// missing lines are warnings: they fail the run only with `--strict`.
pub fn check_readme(features: &Features, readme: Option<&str>, report: &mut Report) {
    let Some(readme) = readme else {
        report.warning("README.md does not exist yet; no feature has a README line".to_owned());
        return;
    };
    let lines: Vec<String> = readme.lines().map(str::to_lowercase).collect();
    for feature in &features.feature {
        let title = feature.title.to_lowercase();
        if !lines.iter().any(|line| line.contains(&title)) {
            report.warning(format!(
                "README.md has no line for feature `{}` (title: \"{}\")",
                feature.id, feature.title
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Features {
        toml::from_str(text).unwrap()
    }

    fn sections(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    const GOOD: &str = r#"
non_feature_sections = ["3"]

[[feature]]
id = "live-rendering"
title = "Live rendering"
spec = "2.1"
description = "Markers hide until the caret touches them."

[[feature]]
id = "editor"
title = "Editor"
spec = "2"
description = "The editor."
"#;

    #[test]
    fn a_consistent_file_has_no_problems() {
        let mut report = Report::default();
        check_features(&parse(GOOD), &sections(&["2", "2.1", "3"]), &mut report);
        assert_eq!(report.problems, Vec::<String>::new());
    }

    #[test]
    fn parent_sections_are_covered_by_their_subsections() {
        let features =
            parse("[[feature]]\nid = \"a\"\ntitle = \"A\"\nspec = \"2.1\"\ndescription = \"d\"\n");
        let mut report = Report::default();
        check_features(&features, &sections(&["2", "2.1"]), &mut report);
        assert!(report.problems.is_empty(), "{:?}", report.problems);
    }

    #[test]
    fn a_prefix_that_is_not_a_subsection_does_not_cover() {
        let features =
            parse("[[feature]]\nid = \"a\"\ntitle = \"A\"\nspec = \"11\"\ndescription = \"d\"\n");
        let mut report = Report::default();
        check_features(&features, &sections(&["1", "11"]), &mut report);
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(report.problems[0].contains("spec section 1 has no feature"));
    }

    #[test]
    fn every_kind_of_inconsistency_is_reported() {
        let features = parse(
            r#"
non_feature_sections = ["99"]

[[feature]]
id = "Bad_Id"
title = "T"
spec = "2"
description = "d"

[[feature]]
id = "dup"
title = " "
spec = "7.7"
description = "d"

[[feature]]
id = "dup"
title = "T"
spec = "2"
description = ""
"#,
        );
        let mut report = Report::default();
        check_features(&features, &sections(&["2", "5"]), &mut report);
        let has = |needle: &str| report.problems.iter().any(|p| p.contains(needle));
        assert!(
            has("id `Bad_Id` must be lowercase"),
            "{:?}",
            report.problems
        );
        assert!(has("duplicate id `dup`"));
        assert!(has("`dup` needs a title and a description"));
        assert!(has("spec section `7.7` which docs/spec.md does not have"));
        assert!(has("names `99` which docs/spec.md does not have"));
        assert!(has("spec section 5 has no feature"));
        assert!(!has("spec section 2 has no feature"));
    }

    #[test]
    fn unknown_keys_are_rejected_by_the_parser() {
        assert!(
            toml::from_str::<Features>(
                "[[feature]]\nid = \"a\"\ntitle = \"A\"\nspec = \"1\"\ndescription = \"d\"\nextra = 1\n"
            )
            .is_err()
        );
    }

    #[test]
    fn readme_lines_are_matched_by_title_case_insensitively() {
        let features = parse(GOOD);
        let mut report = Report::default();
        check_readme(
            &features,
            Some("# SS Writer\n- **LIVE RENDERING**: x\n"),
            &mut report,
        );
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("feature `editor`"));
        assert!(report.problems.is_empty());
    }

    #[test]
    fn a_missing_readme_is_one_warning_not_a_problem() {
        let mut report = Report::default();
        check_readme(&parse(GOOD), None, &mut report);
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("README.md does not exist"));
        assert!(report.problems.is_empty());
    }

    #[test]
    fn the_real_features_file_covers_the_real_spec() {
        let workspace = crate::workspace::Workspace::locate().unwrap();
        let spec = crate::fsx::read_text(&workspace.path("docs/spec.md")).unwrap();
        let text = crate::fsx::read_text(&workspace.path("docs/features.toml")).unwrap();
        let features: Features = toml::from_str(&text).unwrap();
        let mut report = Report::default();
        check_features(
            &features,
            &super::super::spec::section_numbers(&spec),
            &mut report,
        );
        assert_eq!(report.problems, Vec::<String>::new());
        assert!(features.feature.len() >= 25);
    }
}
