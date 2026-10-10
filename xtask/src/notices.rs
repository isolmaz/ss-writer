//! `THIRD-PARTY-NOTICES.txt`: the licenses of everything SS Writer ships that it did not write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::fsx;
use crate::metadata::Package;

/// The directory holding vendored upstream code, relative to the workspace root.
const THIRD_PARTY_DIR: &str = "third_party";

/// A license or notice file found next to a component's sources.
#[derive(Debug, PartialEq, Eq)]
pub struct LicenseText {
    /// The file name it came from.
    pub file: String,
    /// Its text, line endings normalized, surrounding blank space trimmed.
    pub text: String,
}

/// A third-party component: a Rust crate or a vendored directory.
#[derive(Debug, PartialEq, Eq)]
pub struct Component {
    /// Crate or directory name.
    pub name: String,
    /// Crate version (`vendored` for directories that are not crates).
    pub version: String,
    /// SPDX expression from the manifest, when there is one.
    pub license: Option<String>,
    /// Upstream repository, when the manifest names one.
    pub repository: Option<String>,
    /// Whether the code lives in `third_party/` (path dependency or plain directory).
    pub vendored: bool,
    /// The license files shipped with it.
    pub texts: Vec<LicenseText>,
}

/// The bundled font files and their license texts.
#[derive(Debug, PartialEq, Eq)]
pub struct Fonts {
    /// Font file names.
    pub files: Vec<String>,
    /// License files from the font directory.
    pub licenses: Vec<LicenseText>,
}

fn is_license_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "license",
        "licence",
        "copying",
        "unlicense",
        "notice",
        "copyright",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn is_font_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".ttf", ".otf", ".ttc", ".woff", ".woff2"]
        .iter()
        .any(|ext| lower.ends_with(ext))
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_owned()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The text of one license file, or `None` when it is blank. Files that are not UTF-8 are read
/// lossily.
fn read_license(path: &Path) -> Result<Option<LicenseText>, Error> {
    let text = normalize(&String::from_utf8_lossy(&fsx::read_bytes(path)?));
    Ok((!text.is_empty()).then(|| LicenseText {
        file: file_name(path),
        text,
    }))
}

/// The non-empty license-looking files directly inside `dir`, sorted by name.
fn license_texts(dir: &Path) -> Result<Vec<LicenseText>, Error> {
    let mut texts = Vec::new();
    for (path, is_dir) in fsx::read_dir_sorted(dir)? {
        if is_dir || !is_license_file(&file_name(&path)) {
            continue;
        }
        texts.extend(read_license(&path)?);
    }
    Ok(texts)
}

/// The notices of the shipped crates `packages` (read from their source directories): the
/// license-looking files next to `Cargo.toml` plus the manifest's `license-file`.
pub fn crate_components(packages: &[&Package]) -> Result<Vec<Component>, Error> {
    let mut components = Vec::with_capacity(packages.len());
    for package in packages {
        let Some(dir) = package.manifest_path.parent() else {
            return Err(Error::Invalid(format!(
                "manifest path of {} has no parent directory",
                package.name
            )));
        };
        let mut texts = license_texts(dir)?;
        if let Some(declared) = &package.license_file {
            let path = dir.join(declared);
            if path.is_file() && !texts.iter().any(|t| t.file == file_name(&path)) {
                texts.extend(read_license(&path)?);
            }
        }
        components.push(Component {
            name: package.name.clone(),
            version: package.version.clone(),
            license: package.license.clone(),
            repository: package.repository.clone(),
            vendored: package.source.is_none(),
            texts,
        });
    }
    Ok(components)
}

/// Directories of `third_party_dir` that are not already covered by a vendored crate whose
/// manifest sits in one of `crate_dirs`: plain vendored data such as spec files.
pub fn vendored_directories(
    third_party_dir: &Path,
    crate_dirs: &[PathBuf],
) -> Result<Vec<Component>, Error> {
    let mut components = Vec::new();
    for (path, is_dir) in fsx::read_dir_sorted(third_party_dir)? {
        if !is_dir {
            continue;
        }
        let name = file_name(&path);
        let relative = Path::new(THIRD_PARTY_DIR).join(&name);
        if crate_dirs.iter().any(|dir| dir.ends_with(&relative)) {
            continue;
        }
        components.push(Component {
            name,
            version: "vendored".to_owned(),
            license: None,
            repository: None,
            vendored: true,
            texts: license_texts(&path)?,
        });
    }
    Ok(components)
}

/// Font files and license texts in the bundled-fonts directory.
pub fn bundled_fonts(fonts_dir: &Path) -> Result<Fonts, Error> {
    let files = fsx::read_dir_sorted(fonts_dir)?
        .into_iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(path, _)| file_name(&path))
        .filter(|name| is_font_file(name))
        .collect();
    Ok(Fonts {
        files,
        licenses: license_texts(fonts_dir)?,
    })
}

/// `name version`, or just the name for plain directories.
fn label(component: &Component) -> String {
    if component.version == "vendored" {
        component.name.clone()
    } else {
        format!("{} {}", component.name, component.version)
    }
}

fn describe(component: &Component) -> String {
    let mut line = label(component);
    line.push_str(" - ");
    line.push_str(component.license.as_deref().unwrap_or("see license text"));
    if let Some(repository) = &component.repository {
        line.push_str(" - ");
        line.push_str(repository);
    }
    line
}

/// Joins `items` with `, ` wrapping lines at `width` columns.
fn wrap_list(items: &BTreeSet<String>, width: usize) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for item in items {
        if !line.is_empty() && line.len() + item.len() + 2 > width {
            out.push_str(line.trim_end());
            out.push('\n');
            line.clear();
        }
        line.push_str(item);
        line.push_str(", ");
    }
    out.push_str(line.trim_end_matches([',', ' ']));
    out
}

fn heading(out: &mut String, title: &str) {
    out.push_str(title);
    out.push('\n');
    out.push_str(&"-".repeat(title.len()));
    out.push_str("\n\n");
}

/// Assembles the notices file. The license texts are listed once at the end, each with the
/// components that use it (many crates share the same MIT or Apache-2.0 text).
pub fn render(fonts: &Fonts, components: &[Component]) -> String {
    let vendored: Vec<&Component> = components.iter().filter(|c| c.vendored).collect();
    let crates: Vec<&Component> = components.iter().filter(|c| !c.vendored).collect();

    let mut out = String::new();
    out.push_str("SS Writer - third-party notices\n");
    out.push_str("===============================\n\n");
    out.push_str(
        "SS Writer is released under the MIT License (see LICENSE). It includes the third-party\n\
         components listed below. Their license texts are reproduced in full at the end of this\n\
         file.\n\n",
    );

    heading(&mut out, "1. Bundled fonts (assets/fonts)");
    for file in &fonts.files {
        out.push_str(file);
        out.push('\n');
    }
    out.push_str("\nLicenses: ");
    let font_license_files: BTreeSet<String> =
        fonts.licenses.iter().map(|l| l.file.clone()).collect();
    out.push_str(&wrap_list(&font_license_files, 90));
    out.push_str("\n\n");

    heading(&mut out, "2. Vendored sources (third_party)");
    for component in &vendored {
        out.push_str(&describe(component));
        out.push('\n');
    }
    out.push('\n');

    heading(&mut out, &format!("3. Rust crates ({})", crates.len()));
    for component in &crates {
        out.push_str(&describe(component));
        out.push('\n');
    }
    out.push('\n');

    heading(&mut out, "4. License texts");
    let mut by_text: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for license in &fonts.licenses {
        by_text
            .entry(license.text.as_str())
            .or_default()
            .insert(format!("font license {}", license.file));
    }
    for component in components {
        for license in &component.texts {
            by_text
                .entry(license.text.as_str())
                .or_default()
                .insert(label(component));
        }
    }
    let mut groups: Vec<(&str, BTreeSet<String>)> = by_text.into_iter().collect();
    groups.sort_by(|a, b| a.1.iter().next().cmp(&b.1.iter().next()).then(a.0.cmp(b.0)));
    for (text, users) in groups {
        out.push_str(&"=".repeat(80));
        out.push('\n');
        out.push_str("Used by: ");
        out.push_str(&wrap_list(&users, 90));
        out.push('\n');
        out.push_str(&"-".repeat(80));
        out.push('\n');
        out.push_str(text);
        out.push_str("\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        fsx::write_file(&dir.join(name), text).unwrap();
    }

    fn package(name: &str, dir: &Path, source: Option<&str>, license: Option<&str>) -> Package {
        Package {
            id: format!("{name} 1.0.0"),
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            license: license.map(str::to_owned),
            license_file: None,
            repository: Some(format!("https://example.com/{name}")),
            source: source.map(str::to_owned),
            manifest_path: dir.join("Cargo.toml"),
        }
    }

    #[test]
    fn license_files_are_found_normalized_and_other_files_ignored() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "LICENSE-MIT", "MIT text\r\nsecond line\r\n\r\n");
        write(dir.path(), "license-apache.txt", "  Apache text\n");
        write(dir.path(), "COPYING", "   \n");
        write(dir.path(), "README.md", "not a license");
        write(dir.path(), "Cargo.toml", "[package]");
        fsx::create_dir(&dir.path().join("LICENSES")).unwrap();

        let texts = license_texts(dir.path()).unwrap();
        assert_eq!(
            texts,
            vec![
                LicenseText {
                    file: "LICENSE-MIT".to_owned(),
                    text: "MIT text\nsecond line".to_owned()
                },
                LicenseText {
                    file: "license-apache.txt".to_owned(),
                    text: "Apache text".to_owned()
                },
            ]
        );
    }

    #[test]
    fn non_utf8_license_files_are_read_lossily() {
        let dir = tempfile::tempdir().unwrap();
        fsx::write_file(&dir.path().join("LICENSE"), b"Copyright \xA9 Someone").unwrap();
        let texts = license_texts(dir.path()).unwrap();
        assert_eq!(texts[0].text, "Copyright \u{FFFD} Someone");
    }

    #[test]
    fn crate_components_carry_manifest_data_and_license_files() {
        let root = tempfile::tempdir().unwrap();
        let registry_dir = root.path().join("reg");
        let vendored_dir = root.path().join("third_party").join("vend");
        write(&registry_dir, "LICENSE", "MIT for reg");
        write(&vendored_dir, "LICENSE", "BSD for vend");
        let reg = package("reg", &registry_dir, Some("registry+x"), Some("MIT"));
        let vend = package("vend", &vendored_dir, None, Some("BSD-3-Clause"));

        let components = crate_components(&[&reg, &vend]).unwrap();
        assert_eq!(components.len(), 2);
        assert_eq!(components[0].name, "reg");
        assert_eq!(components[0].license.as_deref(), Some("MIT"));
        assert_eq!(
            components[0].repository.as_deref(),
            Some("https://example.com/reg")
        );
        assert!(!components[0].vendored);
        assert_eq!(components[0].texts[0].text, "MIT for reg");
        assert!(
            components[1].vendored,
            "path dependencies count as vendored"
        );
        assert_eq!(components[1].texts[0].text, "BSD for vend");
    }

    #[test]
    fn the_manifest_license_file_is_included_even_when_its_name_is_unusual() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("pkg");
        write(&dir, "LICENSE", "same file");
        write(&dir, "legal.txt", "declared text");
        let mut declared = package("pkg", &dir, Some("registry+x"), None);
        declared.license_file = Some(PathBuf::from("legal.txt"));
        let mut duplicate = package("dup", &dir, Some("registry+x"), None);
        duplicate.license_file = Some(PathBuf::from("LICENSE"));

        let components = crate_components(&[&declared, &duplicate]).unwrap();
        let files = |c: &Component| c.texts.iter().map(|t| t.file.clone()).collect::<Vec<_>>();
        assert_eq!(files(&components[0]), vec!["LICENSE", "legal.txt"]);
        assert_eq!(files(&components[1]), vec!["LICENSE"], "no duplicate entry");
    }

    #[test]
    fn a_manifest_path_without_a_parent_is_an_error() {
        let mut bad = package("bad", Path::new("x"), None, None);
        bad.manifest_path = PathBuf::new();
        let err = crate_components(&[&bad]).unwrap_err().to_string();
        assert!(err.contains("bad"), "{err}");
    }

    #[test]
    fn vendored_directories_skip_those_already_listed_as_crates() {
        let root = tempfile::tempdir().unwrap();
        let third_party = root.path().join("third_party");
        write(&third_party.join("a-crate"), "LICENSE", "crate license");
        write(&third_party.join("spec-data"), "LICENSE", "spec license");
        write(&third_party, "stray-file.txt", "ignored");
        let crate_dirs = vec![PathBuf::from("C:/elsewhere/third_party/a-crate")];

        let components = vendored_directories(&third_party, &crate_dirs).unwrap();
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].name, "spec-data");
        assert_eq!(components[0].version, "vendored");
        assert!(components[0].vendored);
        assert_eq!(components[0].texts[0].text, "spec license");
    }

    #[test]
    fn bundled_fonts_list_font_files_and_license_texts() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Mono.TTF", "x");
        write(dir.path(), "Serif.otf", "x");
        write(dir.path(), "LICENSE-Mono-OFL.txt", "OFL text");
        write(dir.path(), "notes.txt", "x");
        let fonts = bundled_fonts(dir.path()).unwrap();
        assert_eq!(fonts.files, vec!["Mono.TTF", "Serif.otf"]);
        assert_eq!(fonts.licenses.len(), 1);
        assert_eq!(fonts.licenses[0].file, "LICENSE-Mono-OFL.txt");
    }

    fn component(name: &str, vendored: bool, texts: &[&str]) -> Component {
        Component {
            name: name.to_owned(),
            version: if name == "plain-dir" {
                "vendored".to_owned()
            } else {
                "2.0.0".to_owned()
            },
            license: (name != "plain-dir").then(|| "MIT".to_owned()),
            repository: (name == "with-repo").then(|| "https://example.com/r".to_owned()),
            vendored,
            texts: texts
                .iter()
                .enumerate()
                .map(|(i, t)| LicenseText {
                    file: format!("LICENSE-{i}"),
                    text: (*t).to_owned(),
                })
                .collect(),
        }
    }

    #[test]
    fn rendered_notices_list_sections_and_deduplicate_license_texts() {
        let fonts = Fonts {
            files: vec!["Mono.ttf".to_owned()],
            licenses: vec![LicenseText {
                file: "LICENSE-Mono.txt".to_owned(),
                text: "OFL text".to_owned(),
            }],
        };
        let components = vec![
            component("alpha", false, &["MIT text"]),
            component("with-repo", false, &["MIT text", "Apache text"]),
            component("vend", true, &["MIT text"]),
            component("plain-dir", true, &["Spec license"]),
            component("no-text", false, &[]),
        ];
        let out = render(&fonts, &components);

        assert!(out.starts_with("SS Writer - third-party notices\n"));
        assert!(out.contains("1. Bundled fonts (assets/fonts)\n"));
        assert!(out.contains("Mono.ttf\n"));
        assert!(out.contains("Licenses: LICENSE-Mono.txt\n"));
        assert!(out.contains("2. Vendored sources (third_party)\n"));
        assert!(out.contains("vend 2.0.0 - MIT\n"));
        assert!(out.contains("plain-dir - see license text\n"));
        assert!(out.contains("3. Rust crates (3)\n"));
        assert!(out.contains("alpha 2.0.0 - MIT\n"));
        assert!(out.contains("with-repo 2.0.0 - MIT - https://example.com/r\n"));
        assert!(out.contains("no-text 2.0.0 - MIT\n"));

        assert_eq!(
            out.matches("\nMIT text\n").count(),
            1,
            "shared text appears once"
        );
        assert!(out.contains("Used by: alpha 2.0.0, vend 2.0.0, with-repo 2.0.0\n"));
        assert_eq!(out.matches("\nApache text\n").count(), 1);
        assert!(out.contains("Used by: with-repo 2.0.0\n"));
        assert!(out.contains("Used by: font license LICENSE-Mono.txt\n"));
        assert!(out.contains("\nOFL text\n"));
        assert!(out.contains("Used by: plain-dir\n"));
    }

    #[test]
    fn long_user_lists_wrap_without_losing_names() {
        let users: BTreeSet<String> = (0..30)
            .map(|i| format!("crate-number-{i:02} 1.0.0"))
            .collect();
        let wrapped = wrap_list(&users, 60);
        assert!(wrapped.lines().count() > 1);
        assert!(wrapped.lines().all(|l| l.len() <= 60), "{wrapped}");
        for user in &users {
            assert!(wrapped.contains(user.as_str()), "{user}");
        }
        assert!(!wrapped.ends_with(',') && !wrapped.ends_with(' '));
    }
}
