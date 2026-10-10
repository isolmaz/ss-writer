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
use crate::fault::check;
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
    replacement: &mut dyn FnMut() -> Result<String, WinError>,
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
        check(unsafe { list.Next(&mut slot, Some(&raw mut fetched)) }.ok())
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
    let found = check(unsafe { checker.Check(text) }).ctx("ISpellChecker::Check")?;
    read_errors(&found)
}

/// Every error `found` enumerates.
fn read_errors(found: &IEnumSpellingError) -> Result<Vec<RawError>, WinError> {
    let mut errors = Vec::new();
    loop {
        let mut slot: Option<ISpellingError> = None;
        next_error(found, &mut slot)?;
        let Some(error) = slot else {
            return Ok(errors);
        };
        // SAFETY: `error` is a valid object returned by the enumerator.
        let (start, length, code) = unsafe {
            (
                check(error.StartIndex()).ctx("ISpellingError::StartIndex")?,
                check(error.Length()).ctx("ISpellingError::Length")?,
                check(error.CorrectiveAction()).ctx("ISpellingError::CorrectiveAction")?,
            )
        };
        let action = corrective_action(code, &mut || read_replacement(&error))?;
        errors.push(RawError {
            start: start as usize,
            length: length as usize,
            action,
        });
    }
}

/// The replacement text `error` offers (for [`CORRECTIVE_ACTION_REPLACE`]).
fn read_replacement(error: &ISpellingError) -> Result<String, WinError> {
    // SAFETY: `error` is valid; the replacement is a CoTaskMem string that is ours.
    let replacement = unsafe { error.Replacement() }.ctx("ISpellingError::Replacement")?;
    // SAFETY: see above.
    Ok(unsafe { take_string(replacement) })
}

/// Keeps the errors that `flagged` (what another language found in the same text) reports at
/// the same range too: a word is misspelled only when every language says so.
fn retain_flagged_by(errors: &mut Vec<RawError>, flagged: &[RawError]) {
    errors.retain(|error| {
        flagged
            .iter()
            .any(|other| other.start == error.start && other.length == error.length)
    });
}

/// Fetches the next error into `slot`; the slot stays empty at the end of the list.
fn next_error(
    errors: &IEnumSpellingError,
    slot: &mut Option<ISpellingError>,
) -> Result<(), WinError> {
    // SAFETY: `slot` is a valid out value holding no object.
    let hr: HRESULT = unsafe { errors.Next(std::ptr::from_mut(slot)) };
    check(hr.ok()).ctx("IEnumSpellingError::Next")
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
            check(unsafe { CoCreateInstance(&SpellCheckerFactory, None::<&IUnknown>, CLSCTX_ALL) })
                .ctx("create ISpellCheckerFactory")?;
        Ok(Self { factory })
    }

    /// The language tags the system can spell check (`en-US`, `tr-TR`, ...).
    ///
    /// # Errors
    /// The system could not list them.
    pub fn supported_languages(&self) -> Result<Vec<String>, WinError> {
        // SAFETY: `factory` is a valid object.
        let tags = check(unsafe { self.factory.SupportedLanguages() })
            .ctx("ISpellCheckerFactory::SupportedLanguages")?;
        read_strings(&tags)
    }

    /// True when the system can spell check `tag` (a language the system cannot be asked about
    /// is not supported).
    #[must_use]
    pub fn is_supported(&self, tag: &str) -> bool {
        // SAFETY: `factory` is valid; the tag is NUL-terminated.
        check(unsafe { self.factory.IsSupported(&HSTRING::from(tag)) })
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
            tags.extend(fallback.into_iter().find(|tag| self.is_supported(tag)));
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
            let checker = check(unsafe {
                self.factory
                    .CreateSpellChecker(&HSTRING::from(tag.as_str()))
            })
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
            retain_flagged_by(&mut errors, &raw_errors(other, &wide)?);
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
            let list = check(unsafe { checker.Suggest(&word) }).ctx("ISpellChecker::Suggest")?;
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
        let checker = check(self.primary.1.cast::<ISpellChecker2>())
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
            check(unsafe { checker.Ignore(&word) }).ctx("ISpellChecker::Ignore")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;
    use std::cell::Cell;
    use windows::Win32::Foundation::{E_FAIL, S_FALSE};
    use windows::Win32::Globalization::CORRECTIVE_ACTION_NONE;

    #[test]
    fn corrective_actions_map_from_the_system_codes() {
        let reads = Cell::new(0);
        let mut read_replacement = || -> Result<String, WinError> {
            reads.set(reads.get() + 1);
            Ok("the".to_owned())
        };
        for (code, expected) in [
            (CORRECTIVE_ACTION_NONE, CorrectiveAction::None),
            (CORRECTIVE_ACTION_GET_SUGGESTIONS, CorrectiveAction::Suggest),
            (CORRECTIVE_ACTION_DELETE, CorrectiveAction::Delete),
            // An unknown code is "nothing specific", not a failure.
            (CORRECTIVE_ACTION(99), CorrectiveAction::None),
        ] {
            assert_eq!(
                corrective_action(code, &mut read_replacement).unwrap(),
                expected
            );
        }
        assert_eq!(reads.get(), 0, "the replacement is only read for REPLACE");
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION_REPLACE, &mut read_replacement).unwrap(),
            CorrectiveAction::Replace("the".to_owned())
        );
        assert_eq!(reads.get(), 1);
        let failing = corrective_action(CORRECTIVE_ACTION_REPLACE, &mut || {
            Err(WinError::from_os("replacement", E_FAIL.into()))
        });
        assert!(failing.is_err());
    }

    fn raw(start: usize, length: usize) -> RawError {
        RawError {
            start,
            length,
            action: CorrectiveAction::Suggest,
        }
    }

    #[test]
    fn an_error_stays_only_when_the_other_language_reports_the_same_range() {
        let mut errors = vec![raw(0, 3), raw(4, 5), raw(10, 2), raw(13, 4)];
        // Same start with another length, same length with another start: not the same error.
        let flagged = [raw(4, 5), raw(10, 3), raw(11, 2), raw(0, 3), raw(40, 1)];
        retain_flagged_by(&mut errors, &flagged);
        assert_eq!(errors, [raw(0, 3), raw(4, 5)]);
        retain_flagged_by(&mut errors, &[]);
        assert!(errors.is_empty());
    }

    /// A checker made of `count` separate system checkers of the OS language: the public API
    /// merges repeated languages, this builds the several-languages case from one language.
    fn same_language_checker(factory: &SpellFactory, count: usize) -> SpellChecker {
        let tag = factory.resolve_languages(&[]).remove(0);
        let mut made = (0..count).map(|_| {
            // SAFETY: the factory is valid; the tag is NUL-terminated.
            let checker = unsafe {
                factory
                    .factory
                    .CreateSpellChecker(&HSTRING::from(tag.as_str()))
            }
            .unwrap();
            (tag.clone(), checker)
        });
        SpellChecker {
            primary: made.next().unwrap(),
            others: made.collect(),
        }
    }

    /// A typo of a common word of several languages that `checker` has suggestions for: some of
    /// them have suggestions in any language the system can check.
    fn typo_with_suggestions(checker: &SpellChecker) -> &'static str {
        let typos = [
            "speling", "helo", "wrld", "meraba", "kitapp", "okull", "gelyorum", "bugon", "hause",
            "mesion",
        ];
        typos
            .into_iter()
            .find(|typo| !checker.suggestions(typo).unwrap().is_empty())
            .expect("the system suggests something for one of the typos")
    }

    #[test]
    fn a_checker_of_one_language_twice_reports_what_a_single_checker_reports() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let factory = SpellFactory::new().unwrap();
        let single = same_language_checker(&factory, 1);
        let twice = same_language_checker(&factory, 2);
        assert_eq!(twice.languages().count(), 2);
        // No text, no misspelling: the system is not even asked.
        assert!(single.check("").unwrap().is_empty());
        assert!(twice.check("").unwrap().is_empty());
        // Surrogate pairs and multi-byte characters shift UTF-8 and UTF-16 offsets apart.
        let text = "\u{1F600} \u{2603} xqzvkjwp \u{00FC}\u{011F} qwrtzxcvbn";
        let errors = twice.check(text).unwrap();
        assert!(errors.len() >= 2, "{errors:?}");
        assert_eq!(errors, single.check(text).unwrap());
        for error in &errors {
            assert!(text.is_char_boundary(error.range.start));
            assert!(text.is_char_boundary(error.range.end));
        }

        let word = typo_with_suggestions(&single);
        let suggestions = single.suggestions(word).unwrap();
        // The second checker offers the same suggestions again: none is repeated.
        assert_eq!(twice.suggestions(word).unwrap(), suggestions);
    }

    /// A spelling error that offers a replacement (or fails to), as the system's autocorrect
    /// entries do; the real service only produces one from the user's `default.acl`, which no
    /// test may change.
    #[windows::core::implement(ISpellingError)]
    struct OfferedError {
        start: u32,
        replacement: Option<&'static str>,
        /// The getter that fails (`StartIndex`, `Length` or `CorrectiveAction`).
        broken: Option<&'static str>,
    }

    impl OfferedError {
        /// The outcome of the getter `getter`.
        fn works(&self, getter: &str) -> windows::core::Result<()> {
            if self.broken == Some(getter) {
                Err(E_FAIL.into())
            } else {
                Ok(())
            }
        }
    }

    impl windows::Win32::Globalization::ISpellingError_Impl for OfferedError_Impl {
        fn StartIndex(&self) -> windows::core::Result<u32> {
            self.works("StartIndex").map(|()| self.start)
        }

        fn Length(&self) -> windows::core::Result<u32> {
            self.works("Length").map(|()| 4)
        }

        fn CorrectiveAction(&self) -> windows::core::Result<CORRECTIVE_ACTION> {
            self.works("CorrectiveAction")
                .map(|()| CORRECTIVE_ACTION_REPLACE)
        }

        fn Replacement(&self) -> windows::core::Result<PWSTR> {
            let text = self.replacement.ok_or(E_FAIL)?;
            let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: a CoTaskMem block of exactly the units' size, filled before it is handed
            // out; the caller frees it with `CoTaskMemFree`.
            let pointer = unsafe {
                let block =
                    windows::Win32::System::Com::CoTaskMemAlloc(units.len() * 2).cast::<u16>();
                assert!(!block.is_null(), "out of memory");
                std::ptr::copy_nonoverlapping(units.as_ptr(), block, units.len());
                block
            };
            Ok(PWSTR(pointer))
        }
    }

    #[test]
    fn the_replacement_an_error_offers_is_read_and_a_failing_one_is_reported() {
        let offered: ISpellingError = OfferedError {
            start: 3,
            replacement: Some("Çay 😀"),
            broken: None,
        }
        .into();
        // SAFETY: plain getters of a live object.
        unsafe {
            assert_eq!(
                (offered.StartIndex().unwrap(), offered.Length().unwrap()),
                (3, 4)
            );
            assert_eq!(
                offered.CorrectiveAction().unwrap(),
                CORRECTIVE_ACTION_REPLACE
            );
        }
        assert_eq!(read_replacement(&offered).unwrap(), "Çay 😀");
        assert_eq!(
            corrective_action(CORRECTIVE_ACTION_REPLACE, &mut || read_replacement(
                &offered
            ))
            .unwrap(),
            CorrectiveAction::Replace("Çay 😀".to_owned())
        );
        let broken: ISpellingError = OfferedError {
            start: 3,
            replacement: None,
            broken: None,
        }
        .into();
        let error = read_replacement(&broken).unwrap_err();
        assert_eq!(step(&error), "ISpellingError::Replacement");
    }

    /// The operation a failure reports as failed ("ISpellingError::Length" for "ISpellingError::
    /// Length failed: ...").
    fn step(error: &WinError) -> String {
        error
            .to_string()
            .split(" failed: ")
            .next()
            .unwrap()
            .to_owned()
    }

    /// An enumerator that yields `errors` and then ends with `ending`: `S_FALSE` at the end of
    /// the list, as the system's does, or a failure.
    #[windows::core::implement(IEnumSpellingError)]
    struct ScriptedErrors {
        errors: std::cell::RefCell<std::collections::VecDeque<ISpellingError>>,
        ending: HRESULT,
    }

    impl windows::Win32::Globalization::IEnumSpellingError_Impl for ScriptedErrors_Impl {
        fn Next(&self, value: windows::core::OutRef<ISpellingError>) -> HRESULT {
            match self.errors.borrow_mut().pop_front() {
                Some(error) => value.write(Some(error)).into(),
                None => self.ending,
            }
        }
    }

    fn enumerator(errors: Vec<OfferedError>, ending: HRESULT) -> IEnumSpellingError {
        ScriptedErrors {
            errors: std::cell::RefCell::new(errors.into_iter().map(ISpellingError::from).collect()),
            ending,
        }
        .into()
    }

    fn offering(start: u32, replacement: &'static str) -> OfferedError {
        OfferedError {
            start,
            replacement: Some(replacement),
            broken: None,
        }
    }

    #[test]
    fn the_errors_of_an_enumerator_are_read_in_order_with_their_replacements() {
        let found = enumerator(vec![offering(3, "Çay 😀"), offering(20, "the")], S_FALSE);
        assert_eq!(
            read_errors(&found).unwrap(),
            [
                RawError {
                    start: 3,
                    length: 4,
                    action: CorrectiveAction::Replace("Çay 😀".to_owned()),
                },
                RawError {
                    start: 20,
                    length: 4,
                    action: CorrectiveAction::Replace("the".to_owned()),
                },
            ]
        );
        assert!(
            read_errors(&enumerator(Vec::new(), S_FALSE))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_failing_enumerator_or_error_is_reported_as_the_step_that_failed() {
        let failing = |error: OfferedError| {
            step(&read_errors(&enumerator(vec![error], S_FALSE)).unwrap_err())
        };
        let broken = |getter| OfferedError {
            broken: Some(getter),
            ..offering(3, "the")
        };
        assert_eq!(failing(broken("StartIndex")), "ISpellingError::StartIndex");
        assert_eq!(failing(broken("Length")), "ISpellingError::Length");
        assert_eq!(
            failing(broken("CorrectiveAction")),
            "ISpellingError::CorrectiveAction"
        );
        assert_eq!(
            failing(OfferedError {
                replacement: None,
                ..offering(3, "")
            }),
            "ISpellingError::Replacement"
        );
        // The list itself fails after its first error was read.
        let found = enumerator(vec![offering(3, "the")], E_FAIL);
        assert_eq!(
            step(&read_errors(&found).unwrap_err()),
            "IEnumSpellingError::Next"
        );
    }

    /// The checked calls of reading `strings` strings out of an `IEnumString`: one per string
    /// and the one that finds the end.
    #[cfg(feature = "fault-injection")]
    fn string_steps(strings: usize) -> Vec<&'static str> {
        vec!["IEnumString::Next"; strings + 1]
    }

    /// The checked calls of one language checking text with `errors` misspellings.
    #[cfg(feature = "fault-injection")]
    fn check_steps(errors: usize) -> Vec<&'static str> {
        let mut steps = vec!["ISpellChecker::Check"];
        for _ in 0..errors {
            steps.extend([
                "IEnumSpellingError::Next",
                "ISpellingError::StartIndex",
                "ISpellingError::Length",
                "ISpellingError::CorrectiveAction",
            ]);
        }
        steps.push("IEnumSpellingError::Next");
        steps
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_the_factory_is_reported_by_the_step_that_failed() {
        use crate::fault::{all_failed, failed_step, sweep};

        let _com = ComGuard::apartment_threaded().unwrap();
        assert_eq!(
            sweep(|| failed_step(SpellFactory::new())),
            all_failed(&["create ISpellCheckerFactory"])
        );

        let factory = SpellFactory::new().unwrap();
        let languages = factory.supported_languages().unwrap().len();
        let mut listing = vec!["ISpellCheckerFactory::SupportedLanguages"];
        listing.extend(string_steps(languages));
        assert_eq!(
            sweep(|| failed_step(factory.supported_languages())),
            all_failed(&listing)
        );

        // A configured language that cannot be created is a failure of the checker; one the
        // system cannot be asked about is just not supported, and the OS language steps in.
        let tag = factory.resolve_languages(&[]).remove(0);
        let configured = [tag];
        assert_eq!(
            sweep(|| failed_step(factory.checker(&configured))),
            [
                None,
                Some("ISpellCheckerFactory::CreateSpellChecker".to_owned())
            ]
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn without_any_language_the_system_can_be_asked_about_there_is_no_checker() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let factory = SpellFactory::new().unwrap();
        let configured = [factory.resolve_languages(&[]).remove(0)];
        crate::fault::arm_from(0);
        assert!(!factory.is_supported(&configured[0]));
        assert!(factory.resolve_languages(&configured).is_empty());
        assert!(factory.resolve_languages(&[]).is_empty());
        let error = factory.checker(&configured).unwrap_err();
        crate::fault::disarm();
        assert_eq!(
            error.to_string(),
            "no language is available for spell checking"
        );
        // Asked again, the system answers.
        assert!(factory.is_supported(&configured[0]));
        assert_eq!(factory.resolve_languages(&configured), configured);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_checking_and_suggesting_is_reported_for_each_language() {
        use crate::fault::{all_failed, failed_step, sweep};

        let _com = ComGuard::apartment_threaded().unwrap();
        let factory = SpellFactory::new().unwrap();
        let single = same_language_checker(&factory, 1);
        let twice = same_language_checker(&factory, 2);

        let text = "\u{1F600} \u{2603} xqzvkjwp \u{00FC}\u{011F} qwrtzxcvbn";
        let errors = single.check(text).unwrap().len();
        assert!(errors >= 2, "{errors}");
        let one_language = check_steps(errors);
        let mut both = one_language.clone();
        both.extend(&one_language);
        assert_eq!(
            sweep(|| failed_step(single.check(text))),
            all_failed(&one_language)
        );
        assert_eq!(sweep(|| failed_step(twice.check(text))), all_failed(&both));

        let word = typo_with_suggestions(&single);
        // SAFETY: the checker is valid; the word is NUL-terminated.
        let list = unsafe { single.primary.1.Suggest(&HSTRING::from(word)) }.unwrap();
        let suggested = read_strings(&list).unwrap().len();
        let mut one_language = vec!["ISpellChecker::Suggest"];
        one_language.extend(string_steps(suggested));
        let mut both = one_language.clone();
        both.extend(&one_language);
        assert_eq!(
            sweep(|| failed_step(single.suggestions(word))),
            all_failed(&one_language)
        );
        assert_eq!(
            sweep(|| failed_step(twice.suggestions(word))),
            all_failed(&both)
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_word_is_ignored_by_every_language_or_the_failure_is_reported() {
        use crate::fault::{all_failed, failed_step, sweep};

        let _com = ComGuard::apartment_threaded().unwrap();
        let factory = SpellFactory::new().unwrap();
        let twice = same_language_checker(&factory, 2);
        // Ignoring lasts for the session of the checkers: nothing is stored.
        assert_eq!(
            sweep(|| failed_step(twice.ignore("xqzvkjwp"))),
            all_failed(&["ISpellChecker::Ignore", "ISpellChecker::Ignore"])
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn removing_fails_cleanly_when_the_checker_has_no_remove_interface() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let factory = SpellFactory::new().unwrap();
        let checker = same_language_checker(&factory, 1);
        // Only the first step is failed: running `remove` on to its call would change the
        // user's exclusion list.
        crate::fault::arm(0);
        let error = checker.remove("xqzvkjwp").unwrap_err();
        assert!(crate::fault::tripped());
        crate::fault::disarm();
        assert_eq!(step(&error), "ISpellChecker as ISpellChecker2");
    }
}
