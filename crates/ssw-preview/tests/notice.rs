//! The notices of the preview pane: the text shown instead of a document that cannot be shown.

use ssw_preview::notice::{PREVIEW_FTL, for_error, localizer};
use ssw_preview::source::{MAX_PREVIEW_BYTES, SourceError};
use windows::Win32::Foundation::E_FAIL;

#[test]
fn the_strings_load_without_diagnostics() {
    let english = localizer("en");
    assert!(
        english.diagnostics().is_empty(),
        "{:?}",
        english.diagnostics()
    );
    assert!(english.has_message("preview-too-large"));
    assert!(english.has_message("preview-unreadable"));
    assert!(PREVIEW_FTL.contains("preview-too-large"));
}

#[test]
fn an_oversized_file_names_the_limit_in_megabytes() {
    let text = for_error(
        &localizer("en"),
        &SourceError::TooLarge {
            limit: MAX_PREVIEW_BYTES,
        },
    );
    assert_eq!(
        text,
        "This file is larger than 4 MB, which is too large to preview. Open it in SS Writer to read it."
    );
    let small = for_error(
        &localizer("en"),
        &SourceError::TooLarge {
            limit: 10 * 1024 * 1024,
        },
    );
    assert!(small.contains("10 MB"), "{small}");
}

#[test]
fn an_unreadable_file_says_so() {
    let error = SourceError::Read(windows::core::Error::from(E_FAIL));
    assert_eq!(
        for_error(&localizer("en"), &error),
        "This file could not be read, so there is nothing to preview."
    );
}

#[test]
fn unknown_languages_fall_back_to_english() {
    let english = for_error(
        &localizer("en"),
        &SourceError::Read(windows::core::Error::from(E_FAIL)),
    );
    let other = for_error(
        &localizer("xx-YY"),
        &SourceError::Read(windows::core::Error::from(E_FAIL)),
    );
    assert_eq!(english, other);
}
