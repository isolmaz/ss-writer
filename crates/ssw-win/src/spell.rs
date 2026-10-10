//! Spell checking with the Windows spell checker (`ISpellCheckerFactory`).
//!
//! [`SpellFactory`] creates a [`SpellChecker`] for the languages of the settings (the OS
//! language when the setting is empty or names nothing the system supports). A checker with
//! several languages reports a word only when it is wrong in every one of them. Ranges are
//! UTF-8 byte ranges of the checked text (the system speaks UTF-16 indices; the conversion
//! happens here, with [`ssw_core::text::Utf16Map`]). Which text is checked at all (code, URLs,
//! math, wikilink targets, front matter are skipped) is the editor's decision, not this
//! module's.
//!
//! Create and use these objects on a COM-initialized thread and drop them before that thread's
//! [`crate::com::ComGuard`].

use ssw_core::text::{TextRange, Utf16Map};
use windows::Win32::Globalization::{
    CORRECTIVE_ACTION, CORRECTIVE_ACTION_DELETE, CORRECTIVE_ACTION_GET_SUGGESTIONS,
    CORRECTIVE_ACTION_REPLACE, IEnumSpellingError, ISpellChecker, ISpellChecker2,
    ISpellCheckerFactory, ISpellingError, SpellCheckerFactory,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree, IEnumString};
use windows::core::{HRESULT, HSTRING, IUnknown, Interface, PWSTR};

use crate::com::{OsResultExt, WinError, from_wide};
use crate::locale::ui_language;

/// The language used when neither the settings nor the OS language can be spell checked.
const LAST_RESORT_LANGUAGE: &str = "en-US";

/// What the spell checker suggests doing about a misspelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorrectiveAction {
    /// Nothing specific.
    None,
    /// Offer the suggestions ([`SpellChecker::suggestions`]).
    Suggest,
    /// Replace the word with this text.
    Replace(String),
    /// Delete the word (a repeated word).
    Delete,
}

/// A misspelled range of the checked text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpellingError {
    /// The misspelled word as UTF-8 byte offsets into the checked text.
    pub range: TextRange,
    /// What to do about it.
    pub action: CorrectiveAction,
}

/// Maps the system's corrective action; `replacement` reads the replacement text and runs only
/// for [`CORRECTIVE_ACTION_REPLACE`].
fn corrective_action(
    code: CORRECTIVE_ACTION,
    replacement: impl FnOnce() -> Result<String, WinError>,
) -> Result<CorrectiveAction, WinError> {
    Ok(match code {
        CORRECTIVE_ACTION_GET_SUGGESTIONS => CorrectiveAction::Suggest,
        CORRECTIVE_ACTION_REPLACE => CorrectiveAction::Replace(replacement()?),
        CORRECTIVE_ACTION_DELETE => CorrectiveAction::Delete,
        _ => CorrectiveAction::None,
    })
}

/// A misspelling as the system reports it: UTF-16 start and length.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawError {
    start: usize,
    length: usize,
    action: CorrectiveAction,
}

/// Reads a CoTaskMem string and frees it.
///
/// # Safety
/// `text` is a valid NUL-terminated string allocated with `CoTaskMemAlloc`, not used again.
unsafe fn take_string(text: PWSTR) -> String {
    // SAFETY: by the caller's contract the string is valid until freed below.
    let copy = from_wide(unsafe { text.as_wide() });
    // SAFETY: by the caller's contract the string came from `CoTaskMemAlloc`.
    unsafe { CoTaskMemFree(Some(text.0.cast_const().cast())) };
    copy
}

/// Every string of `list`.
fn read_strings(list: &IEnumString) -> Result<Vec<String>, WinError> {
    let mut strings = Vec::new();
    loop {
        let mut slot = [PWSTR::null()];
        let mut fetched = 0_u32;
        // SAFETY: `slot` has room for the one requested string; `fetched` is a valid out value.
        unsafe { list.Next(&mut slot, Some(&raw mut fetched)) }
            .ok()
            .ctx("IEnumString::Next")?;
        if fetched == 0 {
            return Ok(strings);
        }
        // SAFETY: the enumerator filled the slot with a CoTaskMem string that is ours now.
        strings.push(unsafe { take_string(slot[0]) });
    }
}

/// Every misspelling `checker` finds in `text`.
fn raw_errors(checker: &ISpellChecker, text: &HSTRING) -> Result<Vec<RawError>, WinError> {
    // SAFETY: `text` is a valid NUL-terminated string.
    let found = unsafe { checker.Check(text) }.ctx("ISpellChecker::Check")?;
    let mut errors = Vec::new();
    loop {
        let mut slot: Option<ISpellingError> = None;
        next_error(&found, &mut slot)?;
        let Some(error) = slot else {
            return Ok(errors);
        };
        // SAFETY: `error` is a valid object returned by the enumerator.
        let (start, length, code) = unsafe {
            (
                error.StartIndex().ctx("ISpellingError::StartIndex")?,
                error.Length().ctx("ISpellingError::Length")?,
                error
                    .CorrectiveAction()
                    .ctx("ISpellingError::CorrectiveAction")?,
            )
        };
        let action = corrective_action(code, || {
            // SAFETY: `error` is valid; the replacement is a CoTaskMem string that is ours.
            let replacement = unsafe { error.Replacement() }.ctx("ISpellingError::Replacement")?;
            // SAFETY: see above.
            Ok(unsafe { take_string(replacement) })
        })?;
        errors.push(RawError {
            start: start as usize,
            length: length as usize,
            action,
        });
    }
}

/// Fetches the next error into `slot`; the slot stays empty at the end of the list.
fn next_error(
    errors: &IEnumSpellingError,
    slot: &mut Option<ISpellingError>,
) -> Result<(), WinError> {
    // SAFETY: `slot` is a valid out value holding no object.
    let hr: HRESULT = unsafe { errors.Next(std::ptr::from_mut(slot)) };
    hr.ok().ctx("IEnumSpellingError::Next")
}

/// Creates spell checkers.
#[derive(Debug)]
pub struct SpellFactory {
    factory: ISpellCheckerFactory,
}

impl SpellFactory {
    /// Connects to the system's spell checker factory.
    ///
    /// # Errors
    /// COM is not initialized on this thread or the system has no spell checker.
    pub fn new() -> Result<Self, WinError> {
        // SAFETY: `SpellCheckerFactory` is the registered class of `ISpellCheckerFactory`.
        let factory =
            unsafe { CoCreateInstance(&SpellCheckerFactory, None::<&IUnknown>, CLSCTX_ALL) }
                .ctx("create ISpellCheckerFactory")?;
        Ok(Self { factory })
    }

    /// The language tags the system can spell check (`en-US`, `tr-TR`, ...).
    ///
    /// # Errors
    /// The system could not list them.
    pub fn supported_languages(&self) -> Result<Vec<String>, WinError> {
        // SAFETY: `factory` is a valid object.
        let tags = unsafe { self.factory.SupportedLanguages() }
            .ctx("ISpellCheckerFactory::SupportedLanguages")?;
        read_strings(&tags)
    }

    /// True when the system can spell check `tag`.
    #[must_use]
    pub fn is_supported(&self, tag: &str) -> bool {
        // SAFETY: `factory` is valid; the tag is NUL-terminated.
        unsafe { self.factory.IsSupported(&HSTRING::from(tag)) }
            .is_ok_and(|supported| supported.as_bool())
    }

    /// The tags to check with: the supported ones of `configured` (the settings), or when there
    /// are none the OS language (also as its neutral language), or `en-US`.
    #[must_use]
    pub fn resolve_languages(&self, configured: &[String]) -> Vec<String> {
        let mut tags: Vec<String> = Vec::new();
        for tag in configured {
            if self.is_supported(tag) && !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
        if tags.is_empty() {
            let os = ui_language();
            let neutral = os.split('-').next().unwrap_or_default().to_owned();
            let fallback = [os, neutral, LAST_RESORT_LANGUAGE.to_owned()];
            if let Some(tag) = fallback.into_iter().find(|tag| self.is_supported(tag)) {
                tags.push(tag);
            }
        }
        tags
    }

    /// A checker for the languages of the settings (`configured`; empty means the OS language).
    ///
    /// # Errors
    /// The system supports none of the languages, not even `en-US`, or could not create a
    /// checker.
    pub fn checker(&self, configured: &[String]) -> Result<SpellChecker, WinError> {
        let mut created = Vec::new();
        for tag in self.resolve_languages(configured) {
            // SAFETY: `factory` is valid; the tag is NUL-terminated.
            let checker = unsafe {
                self.factory
                    .CreateSpellChecker(&HSTRING::from(tag.as_str()))
            }
            .ctx("ISpellCheckerFactory::CreateSpellChecker")?;
            created.push((tag, checker));
        }
        let mut created = created.into_iter();
        let primary = created.next().ok_or(WinError::Invalid(
            "no language is available for spell checking",
        ))?;
        Ok(SpellChecker {
            primary,
            others: created.collect(),
        })
    }
}

/// A language tag and its checker.
type Language = (String, ISpellChecker);

/// A spell checker for one or more languages.
#[derive(Debug)]
pub struct SpellChecker {
    /// The first language: the one "Add" writes the user dictionary of.
    primary: Language,
    others: Vec<Language>,
}

impl SpellChecker {
    /// Every language, the primary one first.
    fn all(&self) -> impl Iterator<Item = &Language> {
        std::iter::once(&self.primary).chain(&self.others)
    }

    /// The language tags, primary first.
    pub fn languages(&self) -> impl Iterator<Item = &str> {
        self.all().map(|(tag, _)| tag.as_str())
    }

    /// The misspelled words of `text`, in text order: the ones wrong in every language.
    ///
    /// # Errors
    /// The system failed to check.
    pub fn check(&self, text: &str) -> Result<Vec<SpellingError>, WinError> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        let wide = HSTRING::from(text);
        let mut errors = raw_errors(&self.primary.1, &wide)?;
        for (_, other) in &self.others {
            let flagged = raw_errors(other, &wide)?;
            errors.retain(|error| {
                flagged
                    .iter()
                    .any(|other| other.start == error.start && other.length == error.length)
            });
        }
        let map = Utf16Map::new(text);
        Ok(errors
            .into_iter()
            .map(|error| SpellingError {
                range: TextRange::new(
                    map.utf16_to_byte_floor(error.start),
                    map.utf16_to_byte_floor(error.start + error.length),
                ),
                action: error.action,
            })
            .collect())
    }

    /// Suggested corrections for `word`: the primary language's first, then the other
    /// languages', without repeats.
    ///
    /// # Errors
    /// The system failed to suggest.
    pub fn suggestions(&self, word: &str) -> Result<Vec<String>, WinError> {
        let word = HSTRING::from(word);
        let mut all: Vec<String> = Vec::new();
        for (_, checker) in self.all() {
            // SAFETY: `checker` is valid; the word is NUL-terminated.
            let list = unsafe { checker.Suggest(&word) }.ctx("ISpellChecker::Suggest")?;
            for suggestion in read_strings(&list)? {
                if !all.contains(&suggestion) {
                    all.push(suggestion);
                }
            }
        }
        Ok(all)
    }

    /// Adds `word` to the user dictionary (persistent: Windows 11 writes it to the
    /// language-neutral `%APPDATA%\Microsoft\Spelling\neutral\default.dic`, so every language
    /// accepts it). The spell checking service applies the change on its own schedule: a
    /// [`SpellChecker::check`] made right after may still report the word.
    ///
    /// # Errors
    /// The system refused the word.
    pub fn add(&self, word: &str) -> Result<(), WinError> {
        // SAFETY: the checker is valid; the word is NUL-terminated.
        unsafe { self.primary.1.Add(&HSTRING::from(word)) }.ctx("ISpellChecker::Add")
    }

    /// Takes `word` out of the words the checker accepts: it is withdrawn from the added words
    /// or, for an ignored word, from the ignored ones. Windows keeps added words in a file it
    /// treats as read-only (see [`SpellChecker::add`]) and offers no call that deletes from it;
    /// for such a word `remove` puts it on the language's exclusion list
    /// (`%APPDATA%\Microsoft\Spelling\<language>\default.exc`) instead, and the word is
    /// reported as misspelled from then on. Neither file is restored by this call: code that
    /// must leave the user's dictionaries as they were has to restore the files itself.
    ///
    /// # Errors
    /// The system refused, or does not offer `ISpellChecker2` (before Windows 10).
    pub fn remove(&self, word: &str) -> Result<(), WinError> {
        let checker = self
            .primary
            .1
            .cast::<ISpellChecker2>()
            .ctx("ISpellChecker as ISpellChecker2")?;
        // SAFETY: the checker is valid; the word is NUL-terminated.
        unsafe { checker.Remove(&HSTRING::from(word)) }.ctx("ISpellChecker2::Remove")
    }

    /// Stops reporting `word` for the lifetime of this checker's session (nothing is stored).
    ///
    /// # Errors
    /// The system refused the word.
    pub fn ignore(&self, word: &str) -> Result<(), WinError> {
        let word = HSTRING::from(word);
        for (_, checker) in self.all() {
            // SAFETY: `checker` is valid; the word is NUL-terminated.
            unsafe { checker.Ignore(&word) }.ctx("ISpellChecker::Ignore")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::Globalization::CORRECTIVE_ACTION_NONE;

    fn replacement_never_read() -> Result<String, WinError> {
        panic!("the replacement is only read for REPLACE")
    }

    #[test]
    fn corrective_actions_map_from_the_system_codes() {
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION_NONE, replacement_never_read).unwrap(),
            CorrectiveAction::None
        );
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION_GET_SUGGESTIONS, replacement_never_read).unwrap(),
            CorrectiveAction::Suggest
        );
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION_DELETE, replacement_never_read).unwrap(),
            CorrectiveAction::Delete
        );
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION_REPLACE, || Ok("the".to_owned())).unwrap(),
            CorrectiveAction::Replace("the".to_owned())
        );
        // An unknown code is "nothing specific", not a failure.
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION(99), replacement_never_read).unwrap(),
            CorrectiveAction::None
        );
        let failing = corrective_action(CORRECTIVE_ACTION_REPLACE, || {
            Err(WinError::from_os("replacement", E_FAIL.into()))
        });
        assert!(failing.is_err());
    }
}
