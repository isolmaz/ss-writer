//! winget manifests: the templates in `packaging/winget/` with the release values filled in.

use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::fsx;

/// Replaces every `{{Name}}` in `template` with the value registered for `Name`.
/// An unknown name or an unterminated `{{` is an error (a manifest with a hole in it would be
/// rejected by winget much later).
pub fn substitute(template: &str, values: &[(&str, String)]) -> Result<String, Error> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(Error::Invalid(format!(
                "unterminated `{{{{` in winget template near `{}`",
                after.chars().take(20).collect::<String>()
            )));
        };
        let name = after[..end].trim();
        let Some((_, value)) = values.iter().find(|(known, _)| *known == name) else {
            return Err(Error::Invalid(format!(
                "winget template uses unknown placeholder `{{{{{name}}}}}`"
            )));
        };
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Renders every `*.yaml` template of `templates_dir` into `out_dir` (same file names) and
/// returns the written paths.
pub fn generate(
    templates_dir: &Path,
    out_dir: &Path,
    values: &[(&str, String)],
) -> Result<Vec<PathBuf>, Error> {
    let mut written = Vec::new();
    for (path, is_dir) in fsx::read_dir_sorted(templates_dir)? {
        let Some(name) = path.file_name() else {
            continue;
        };
        if is_dir || path.extension().is_none_or(|e| e != "yaml") {
            continue;
        }
        let rendered = substitute(&fsx::read_text(&path)?, values)?;
        let target = out_dir.join(name);
        fsx::write_file(&target, rendered)?;
        written.push(target);
    }
    if written.is_empty() {
        return Err(Error::Invalid(format!(
            "no *.yaml winget templates in {}",
            templates_dir.display()
        )));
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Vec<(&'static str, String)> {
        vec![
            ("Version", "1.2.3".to_owned()),
            ("Sha256", "ABCDEF".to_owned()),
        ]
    }

    #[test]
    fn placeholders_are_replaced_everywhere_including_inside_urls() {
        let out = substitute(
            "v: {{Version}}\nurl: https://x/v{{ Version }}/a-{{Version}}.msi\nsha: {{Sha256}}\n",
            &values(),
        )
        .unwrap();
        assert_eq!(
            out,
            "v: 1.2.3\nurl: https://x/v1.2.3/a-1.2.3.msi\nsha: ABCDEF\n"
        );
    }

    #[test]
    fn braces_in_substituted_values_are_not_expanded_again() {
        let values = vec![("ProductCode", "{{Version}}".to_owned())];
        assert_eq!(
            substitute("code: '{{ProductCode}}'", &values).unwrap(),
            "code: '{{Version}}'"
        );
    }

    #[test]
    fn text_without_placeholders_is_unchanged() {
        assert_eq!(substitute("a: b\n", &values()).unwrap(), "a: b\n");
    }

    #[test]
    fn unknown_and_unterminated_placeholders_are_errors() {
        let err = substitute("x: {{Nope}}", &values())
            .unwrap_err()
            .to_string();
        assert!(err.contains("Nope"), "{err}");
        let err = substitute("x: {{Version", &values())
            .unwrap_err()
            .to_string();
        assert!(err.contains("unterminated"), "{err}");
    }

    #[test]
    fn generate_renders_only_yaml_templates() {
        let dir = tempfile::tempdir().unwrap();
        let templates = dir.path().join("t");
        let out = dir.path().join("out");
        fsx::write_file(&templates.join("a.yaml"), "PackageVersion: {{Version}}\n").unwrap();
        fsx::write_file(&templates.join("b.yaml"), "Sha: {{Sha256}}\n").unwrap();
        fsx::write_file(&templates.join("notes.txt"), "{{Ignored}}").unwrap();
        fsx::create_dir(&templates.join("sub.yaml")).unwrap();

        let written = generate(&templates, &out, &values()).unwrap();
        assert_eq!(written, vec![out.join("a.yaml"), out.join("b.yaml")]);
        assert_eq!(
            fsx::read_text(&out.join("a.yaml")).unwrap(),
            "PackageVersion: 1.2.3\n"
        );
        assert_eq!(
            fsx::read_text(&out.join("b.yaml")).unwrap(),
            "Sha: ABCDEF\n"
        );
        assert!(!out.join("notes.txt").exists());
    }

    #[test]
    fn generate_without_templates_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = generate(dir.path(), &dir.path().join("out"), &values())
            .unwrap_err()
            .to_string();
        assert!(err.contains("no *.yaml"), "{err}");
    }

    #[test]
    fn the_shipped_templates_render_with_the_release_values() {
        let workspace = crate::workspace::Workspace::locate().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let values = vec![
            ("Version", "9.8.7".to_owned()),
            ("Sha256", "0123ABCD".to_owned()),
            (
                "ProductCode",
                "{AAAAAAAA-0000-0000-0000-000000000000}".to_owned(),
            ),
        ];
        let written = generate(&workspace.path("packaging/winget"), dir.path(), &values).unwrap();
        let all: String = written.iter().map(|p| fsx::read_text(p).unwrap()).collect();
        assert!(all.contains("PackageIdentifier: isolmaz.SSWriter"));
        assert!(all.contains("PackageVersion: 9.8.7"));
        assert!(all.contains("releases/download/v9.8.7/SSWriter-9.8.7-x64.msi"));
        assert!(all.contains("InstallerSha256: 0123ABCD"));
        assert!(!all.contains("{{"));
        let kinds: Vec<&str> = ["version", "installer", "defaultLocale"]
            .into_iter()
            .filter(|kind| all.contains(&format!("ManifestType: {kind}")))
            .collect();
        assert_eq!(kinds, vec!["version", "installer", "defaultLocale"]);
    }
}
