//! What the preview pane says when there is no document to show: the file is too large or its
//! stream failed. The text comes from Fluent (`assets/i18n/en/preview.ftl`) in the user's UI
//! language and is shown as an ordinary paragraph of the read-only view.

use ssw_core::i18n::{Arg, Localizer};

use crate::source::SourceError;

/// The English strings of the preview (`preview-…` ids).
pub const PREVIEW_FTL: &str = include_str!("../../../assets/i18n/en/preview.ftl");

/// A localizer for the preview's notices in the `locale` (a BCP-47 tag; unknown locales fall
/// back to English).
#[must_use]
pub fn localizer(locale: &str) -> Localizer {
    Localizer::new(locale, &[("en", PREVIEW_FTL)])
}

/// The notice for a stream that could not be turned into a document.
#[must_use]
pub fn for_error(localizer: &Localizer, error: &SourceError) -> String {
    match error {
        SourceError::TooLarge { limit } => localizer.format(
            "preview-too-large",
            &[("megabytes", Arg::Int((limit / (1024 * 1024)) as i64))],
        ),
        SourceError::Read(_) => localizer.text("preview-unreadable"),
    }
}
