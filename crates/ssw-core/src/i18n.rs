//! Fluent-based localization.
//!
//! The UI never contains string literals: every label is a message id looked up here. A
//! [`Localizer`] holds one bundle per locale of the fallback chain
//! (`tr-TR` → `tr` → `en`); a message missing everywhere formats as its own id, so a gap in
//! a translation is visible but never fatal.
//!
//! Bidi isolation marks around placeables are disabled: the UI lays text out per control and
//! the marks would otherwise show up in native menus and UI Automation names.

use std::fmt;

use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource, FluentValue};
use unic_langid::LanguageIdentifier;

/// The source locale; always the last link of the fallback chain.
pub const FALLBACK_LOCALE: &str = "en";

/// A named argument for [`Localizer::format`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg<'a> {
    Str(&'a str),
    Int(i64),
    Float(f64),
}

impl<'a> From<&'a str> for Arg<'a> {
    fn from(value: &'a str) -> Self {
        Arg::Str(value)
    }
}

impl From<i64> for Arg<'_> {
    fn from(value: i64) -> Self {
        Arg::Int(value)
    }
}

impl From<f64> for Arg<'_> {
    fn from(value: f64) -> Self {
        Arg::Float(value)
    }
}

impl<'a> From<Arg<'a>> for FluentValue<'a> {
    fn from(value: Arg<'a>) -> Self {
        match value {
            Arg::Str(s) => FluentValue::from(s),
            Arg::Int(n) => FluentValue::from(n),
            Arg::Float(n) => FluentValue::from(n),
        }
    }
}

/// Message lookup with locale fallback.
pub struct Localizer {
    /// Bundles in fallback order: most specific locale first, `en` last.
    bundles: Vec<FluentBundle<FluentResource>>,
    /// Problems found while loading sources (parse errors, duplicate ids).
    diagnostics: Vec<String>,
}

impl fmt::Debug for Localizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Localizer")
            .field(
                "chain",
                &self
                    .bundles
                    .iter()
                    .map(|b| {
                        b.locales
                            .first()
                            .map(ToString::to_string)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>(),
            )
            .field("diagnostics", &self.diagnostics.len())
            .finish()
    }
}

/// Normalize a locale tag (`tr_TR` → `tr-TR`) into a language identifier.
fn parse_locale(tag: &str) -> Option<LanguageIdentifier> {
    tag.trim().replace('_', "-").parse().ok()
}

/// The fallback chain for `locale`: the tag itself, its bare language, then `en`.
fn fallback_chain(locale: &str) -> Vec<LanguageIdentifier> {
    let mut chain: Vec<LanguageIdentifier> = Vec::new();
    let mut push = |id: LanguageIdentifier| {
        if !chain.contains(&id) {
            chain.push(id);
        }
    };
    if let Some(id) = parse_locale(locale) {
        let mut language_only = id.clone();
        language_only.script = None;
        language_only.region = None;
        language_only.clear_variants();
        push(id);
        push(language_only);
    }
    if let Some(en) = parse_locale(FALLBACK_LOCALE) {
        push(en);
    }
    chain
}

impl Localizer {
    /// Build a localizer for `locale` from `(locale, ftl source)` pairs. Several sources may
    /// share a locale (one per `.ftl` file); sources for locales outside the fallback chain are
    /// ignored. An unparsable `locale` falls back to `en`.
    #[must_use]
    pub fn new(locale: &str, sources: &[(&str, &str)]) -> Self {
        let mut diagnostics = Vec::new();
        let mut bundles = Vec::new();
        for id in fallback_chain(locale) {
            let mut bundle = FluentBundle::new_concurrent(vec![id.clone()]);
            bundle.set_use_isolating(false);
            let mut any = false;
            for (source_locale, ftl) in sources {
                if parse_locale(source_locale).as_ref() != Some(&id) {
                    continue;
                }
                any = true;
                let resource = match FluentResource::try_new((*ftl).to_owned()) {
                    Ok(resource) => resource,
                    Err((partial, errors)) => {
                        diagnostics.extend(errors.iter().map(|e| format!("{id}: {e:?}")));
                        partial
                    }
                };
                if let Err(errors) = bundle.add_resource(resource) {
                    diagnostics.extend(errors.iter().map(|e| format!("{id}: {e:?}")));
                }
            }
            if any {
                bundles.push(bundle);
            }
        }
        Self {
            bundles,
            diagnostics,
        }
    }

    /// Format message `id` with `args`. Falls back through the locale chain; a message missing
    /// from every bundle yields `id` itself.
    #[must_use]
    pub fn format(&self, id: &str, args: &[(&str, Arg<'_>)]) -> String {
        let fluent_args = (!args.is_empty()).then(|| {
            let mut fa = FluentArgs::with_capacity(args.len());
            for (name, value) in args {
                fa.set(*name, FluentValue::from(*value));
            }
            fa
        });
        for bundle in &self.bundles {
            let Some(pattern) = bundle.get_message(id).and_then(|m| m.value()) else {
                continue;
            };
            let mut errors = Vec::new();
            return bundle
                .format_pattern(pattern, fluent_args.as_ref(), &mut errors)
                .into_owned();
        }
        id.to_owned()
    }

    /// Format message `id` without arguments.
    #[must_use]
    pub fn text(&self, id: &str) -> String {
        self.format(id, &[])
    }

    /// Whether any bundle of the chain defines message `id`.
    #[must_use]
    pub fn has_message(&self, id: &str) -> bool {
        self.bundles.iter().any(|b| b.has_message(id))
    }

    /// The locales actually loaded, most specific first (always ends with `en` when `en`
    /// sources were supplied).
    #[must_use]
    pub fn loaded_locales(&self) -> Vec<String> {
        self.bundles
            .iter()
            .filter_map(|b| b.locales.first().map(ToString::to_string))
            .collect()
    }

    /// Problems found while loading the sources; empty for well-formed `.ftl` files.
    #[must_use]
    pub fn diagnostics(&self) -> &[String] {
        &self.diagnostics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandGroup, CommandId};
    use crate::settings::SettingsSection;

    const EN: &str = "hello = Hello\nonly-en = English only\ngreet = Hi { $name }!\nfiles = { $n ->\n    [one] one file\n   *[other] { $n } files\n}\n";
    const TR: &str = "hello = Merhaba\ngreet = Selam { $name }!\n";
    const TR_TR: &str = "hello = Merhaba (TR)\n";

    fn sources() -> Vec<(&'static str, &'static str)> {
        vec![("en", EN), ("tr", TR), ("tr-TR", TR_TR)]
    }

    #[test]
    fn exact_locale_wins_then_language_then_english() {
        let l = Localizer::new("tr-TR", &sources());
        assert_eq!(l.loaded_locales(), ["tr-TR", "tr", "en"]);
        assert_eq!(l.text("hello"), "Merhaba (TR)");
        assert_eq!(
            l.format("greet", &[("name", Arg::Str("Ada"))]),
            "Selam Ada!"
        );
        assert_eq!(l.text("only-en"), "English only");
    }

    #[test]
    fn unknown_regions_and_garbage_fall_back_to_english() {
        let l = Localizer::new("tr_CY", &sources());
        assert_eq!(l.text("hello"), "Merhaba");
        let l = Localizer::new("de-DE", &sources());
        assert_eq!(l.loaded_locales(), ["en"]);
        assert_eq!(l.text("hello"), "Hello");
        let l = Localizer::new("not a locale!!", &sources());
        assert_eq!(l.text("hello"), "Hello");
        let l = Localizer::new("", &sources());
        assert_eq!(l.text("hello"), "Hello");
    }

    #[test]
    fn missing_message_returns_its_id() {
        let l = Localizer::new("en", &sources());
        assert_eq!(l.text("no-such-message"), "no-such-message");
        assert!(!l.has_message("no-such-message"));
        assert!(l.has_message("hello"));
        let empty = Localizer::new("en", &[]);
        assert_eq!(empty.text("hello"), "hello");
        assert!(empty.loaded_locales().is_empty());
    }

    #[test]
    fn arguments_and_plurals() {
        let l = Localizer::new("en", &sources());
        assert_eq!(l.format("greet", &[("name", "Ada".into())]), "Hi Ada!");
        assert_eq!(l.format("files", &[("n", Arg::Int(1))]), "one file");
        assert_eq!(l.format("files", &[("n", Arg::Int(3))]), "3 files");
        // A missing argument does not abort formatting.
        assert!(l.format("greet", &[]).starts_with("Hi "));
    }

    #[test]
    fn multiple_sources_for_one_locale_are_merged() {
        let l = Localizer::new("en", &[("en", "a = A\n"), ("en", "b = B\n")]);
        assert_eq!((l.text("a"), l.text("b")), ("A".to_owned(), "B".to_owned()));
        assert!(l.diagnostics().is_empty());
    }

    #[test]
    fn malformed_sources_are_reported_but_valid_entries_survive() {
        let l = Localizer::new("en", &[("en", "ok = fine\nthis is junk\n")]);
        assert_eq!(l.text("ok"), "fine");
        assert!(!l.diagnostics().is_empty());
    }

    #[test]
    fn shipped_english_catalog_is_well_formed_and_complete() {
        let ftl = include_str!("../../../assets/i18n/en/main.ftl");
        let l = Localizer::new("en-US", &[("en", ftl)]);
        assert!(l.diagnostics().is_empty(), "{:?}", l.diagnostics());
        for cmd in CommandId::all() {
            assert!(l.has_message(cmd.label_id()), "{}", cmd.label_id());
        }
        for group in CommandGroup::ALL {
            assert!(l.has_message(group.label_id()), "{}", group.label_id());
        }
        for section in SettingsSection::ALL {
            assert!(l.has_message(section.label_id()), "{}", section.label_id());
        }
        assert_eq!(l.text(CommandId::FormatBold.label_id()), "Bold");
        assert_eq!(l.text("settings-section-shortcuts"), "Shortcuts");
    }
}
