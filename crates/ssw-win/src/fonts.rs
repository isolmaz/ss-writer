//! Bundled font bytes and font family fallback rules.

use ssw_ui::canvas::IconFont;

/// The bundled Cascadia Mono (OFL), loaded through DirectWrite's in-memory loader.
pub const CASCADIA_MONO: &[u8] = include_bytes!("../../../assets/fonts/CascadiaMono.ttf");

/// Family name of the bundled monospace font.
pub const MONO_FAMILY: &str = "Cascadia Mono";

/// Family used when nothing else matches.
pub const LAST_RESORT_FAMILY: &str = "Segoe UI";

/// Fallback chains: when the first family of a row is missing, the following ones are tried in
/// order. Matching is case-insensitive.
const FALLBACKS: &[(&str, &[&str])] = &[
    (MONO_FAMILY, &["Consolas", "Courier New"]),
    ("Segoe UI Variable", &["Segoe UI"]),
    ("Segoe UI Variable Text", &["Segoe UI Variable", "Segoe UI"]),
    (
        "Segoe UI Variable Display",
        &["Segoe UI Variable", "Segoe UI"],
    ),
    ("Segoe Fluent Icons", &["Segoe MDL2 Assets"]),
];

/// The family DirectWrite should use for a requested family: the request itself when
/// `has_family` knows it, else the first existing entry of its fallback chain, else
/// [`LAST_RESORT_FAMILY`] when that exists, else the request unchanged.
#[must_use]
pub fn resolve_family(requested: &str, has_family: impl Fn(&str) -> bool) -> String {
    if has_family(requested) {
        return requested.to_owned();
    }
    let chain = FALLBACKS
        .iter()
        .find(|(family, _)| family.eq_ignore_ascii_case(requested))
        .map_or(&[][..], |(_, chain)| *chain);
    chain
        .iter()
        .chain(std::iter::once(&LAST_RESORT_FAMILY))
        .find(|candidate| has_family(candidate))
        .map_or_else(|| requested.to_owned(), |found| (*found).to_owned())
}

/// Family name of an icon font.
#[must_use]
pub fn icon_family(font: IconFont) -> &'static str {
    match font {
        IconFont::SegoeFluent => "Segoe Fluent Icons",
        IconFont::SegoeMdl2 => "Segoe MDL2 Assets",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name| names.contains(&name)
    }

    #[test]
    fn existing_family_is_kept() {
        assert_eq!(
            resolve_family("Cascadia Mono", installed(&["Cascadia Mono", "Consolas"])),
            "Cascadia Mono"
        );
    }

    #[test]
    fn missing_families_follow_their_chain() {
        let cases: [(&str, &[&str], &str); 6] = [
            ("Cascadia Mono", &["Consolas", "Segoe UI"], "Consolas"),
            ("cascadia mono", &["Courier New"], "Courier New"),
            (
                "Segoe UI Variable Text",
                &["Segoe UI Variable", "Segoe UI"],
                "Segoe UI Variable",
            ),
            ("Segoe UI Variable Display", &["Segoe UI"], "Segoe UI"),
            (
                "Segoe Fluent Icons",
                &["Segoe MDL2 Assets"],
                "Segoe MDL2 Assets",
            ),
            ("Unknown Family", &["Segoe UI"], "Segoe UI"),
        ];
        for (requested, present, expected) in cases {
            assert_eq!(
                resolve_family(requested, |name| present.contains(&name)),
                expected,
                "{requested}"
            );
        }
    }

    #[test]
    fn nothing_installed_returns_the_request() {
        assert_eq!(resolve_family("Whatever", |_| false), "Whatever");
        assert_eq!(resolve_family("Cascadia Mono", |_| false), "Cascadia Mono");
    }

    #[test]
    fn icon_families_and_bundled_font_are_valid() {
        assert_eq!(icon_family(IconFont::SegoeFluent), "Segoe Fluent Icons");
        assert_eq!(icon_family(IconFont::SegoeMdl2), "Segoe MDL2 Assets");
        // A TrueType font starts with 0x00010000 or `true`; OpenType CFF with `OTTO`.
        assert!(
            CASCADIA_MONO.starts_with(&[0, 1, 0, 0])
                || CASCADIA_MONO.starts_with(b"OTTO")
                || CASCADIA_MONO.starts_with(b"true")
        );
    }
}
