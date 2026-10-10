//! The real Windows spell checker: UTF-16 results mapped to UTF-8 ranges, the user dictionary
//! with a throw-away word, session-only ignoring and languages from the settings. Tests that
//! need English skip themselves on a machine without an English spell checker.
//!
//! The user's dictionary files (`%APPDATA%\Microsoft\Spelling`) are the user's: a test that
//! can change them holds a [`DictionaryFiles`] guard, which puts every file back byte for byte
//! (or removes it again when it did not exist), also when the test fails.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ssw_win::com::ComGuard;
use ssw_win::spell::{CorrectiveAction, SpellChecker, SpellFactory, SpellingError};
use support::MachineLock;

/// The kinds of user dictionary file of a language folder.
const DICTIONARY_FILE_NAMES: [&str; 3] = ["default.dic", "default.exc", "default.acl"];

/// Snapshot of the user dictionary files of the language-neutral folder and of some languages;
/// puts them back on drop.
struct DictionaryFiles {
    /// Every file with its content, `None` for a file that did not exist.
    saved: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Declared after `saved`: released once the files are restored. Tests that touch the
    /// dictionary files run one at a time, also across the processes of a parallel test run: a
    /// snapshot taken while another test's changes are in place would "restore" them.
    _exclusive: MachineLock,
}

impl DictionaryFiles {
    /// Snapshots the files of the `neutral` folder and of the folders of `languages`.
    fn protect(languages: &[String]) -> Self {
        let exclusive = MachineLock::acquire("spelling-dictionaries");
        let appdata = std::env::var_os("APPDATA").expect("APPDATA is set for a user session");
        let root = Path::new(&appdata).join(r"Microsoft\Spelling");
        let mut saved = Vec::new();
        let folders = std::iter::once("neutral").chain(languages.iter().map(String::as_str));
        for folder in folders {
            for name in DICTIONARY_FILE_NAMES {
                let path = root.join(folder).join(name);
                let content = std::fs::read(&path).ok();
                saved.push((path, content));
            }
        }
        Self {
            saved,
            _exclusive: exclusive,
        }
    }

    /// True when every file is as it was.
    fn unchanged(&self) -> bool {
        self.saved
            .iter()
            .all(|(path, content)| std::fs::read(path).ok() == *content)
    }

    fn restore(&self) {
        for (path, content) in &self.saved {
            // Best effort: a file the service holds open for a moment is retried by `drop`.
            match content {
                Some(bytes) => {
                    if std::fs::read(path).ok().as_ref() != Some(bytes) {
                        if let Some(folder) = path.parent() {
                            let _ = std::fs::create_dir_all(folder);
                        }
                        let _ = std::fs::write(path, bytes);
                    }
                }
                None => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
}

impl Drop for DictionaryFiles {
    fn drop(&mut self) {
        // The spell checking service may write its files a moment after the call that caused
        // it: restore until the files have been unchanged twice in a row.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stable = 0;
        while stable < 2 && Instant::now() < deadline {
            self.restore();
            std::thread::sleep(Duration::from_millis(150));
            stable = if self.unchanged() { stable + 1 } else { 0 };
        }
    }
}

/// Polls until `condition` holds (the service applies dictionary changes on its own schedule).
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

/// True when `checker` flags `word` inside `text`.
fn is_flagged(checker: &SpellChecker, text: &str, word: &str) -> bool {
    flagged(&checker.check(text).unwrap(), text).contains(&word)
}

/// A made-up word of letters only (words with digits are never checked) no dictionary knows.
fn unique_word() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let letters: String = nanos
        .to_string()
        .bytes()
        .map(|digit| char::from(b'a' + (digit - b'0')))
        .collect();
    format!(
        "zqx{letters}{}",
        char::from(b'a' + (std::process::id() % 26) as u8)
    )
}

/// The words of `text` the checker flags.
fn flagged<'a>(errors: &[SpellingError], text: &'a str) -> Vec<&'a str> {
    errors
        .iter()
        .map(|error| &text[error.range.start..error.range.end])
        .collect()
}

fn english(factory: &SpellFactory) -> Option<SpellChecker> {
    factory
        .is_supported("en-US")
        .then(|| factory.checker(&["en-US".to_owned()]).unwrap())
}

#[test]
fn supported_languages_are_listed_and_settings_resolve_to_supported_ones() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let supported = factory.supported_languages().unwrap();
    assert!(!supported.is_empty());
    assert!(supported.iter().all(|tag| factory.is_supported(tag)));
    assert!(!factory.is_supported("xx-YY"));

    let first = supported[0].clone();
    assert_eq!(
        factory.resolve_languages(&[first.clone(), "xx-YY".to_owned(), first.clone()]),
        [first],
        "unsupported tags are dropped, repeats merged"
    );
    // Nothing usable in the settings (or nothing at all): the OS language.
    for configured in [Vec::new(), vec!["xx-YY".to_owned()]] {
        let resolved = factory.resolve_languages(&configured);
        assert_eq!(resolved.len(), 1, "{configured:?}");
        assert!(factory.is_supported(&resolved[0]));
    }
}

#[test]
fn a_checker_without_configured_languages_uses_the_os_language() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let checker = factory.checker(&[]).unwrap();
    let languages: Vec<&str> = checker.languages().collect();
    assert_eq!(languages, factory.resolve_languages(&[]));
}

#[test]
fn misspellings_come_back_as_utf8_byte_ranges_of_the_text() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let checker = factory.checker(&[]).unwrap();
    // Multi-byte and surrogate-pair characters in front of the words shift UTF-8 and UTF-16
    // offsets apart.
    let text = "\u{1F600} \u{2603} xqzvkjwp \u{00FC}\u{011F} qwrtzxcvbn";
    let errors = checker.check(text).unwrap();
    let words = flagged(&errors, text);
    assert!(words.contains(&"xqzvkjwp"), "{words:?}");
    assert!(words.contains(&"qwrtzxcvbn"), "{words:?}");
    assert!(
        errors
            .windows(2)
            .all(|pair| pair[0].range.end <= pair[1].range.start)
    );
    assert!(checker.check("").unwrap().is_empty());
}

#[test]
fn english_text_is_checked_and_corrections_are_suggested() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let Some(checker) = english(&factory) else {
        return;
    };
    let text = "This is a speling mistake in a short sentence.";
    let errors = checker.check(text).unwrap();
    let words = flagged(&errors, text);
    assert!(words.contains(&"speling"), "{words:?}");
    let error = &errors[words.iter().position(|word| *word == "speling").unwrap()];
    assert_eq!(error.action, CorrectiveAction::Suggest);
    for correct in ["This", "is", "a", "mistake", "short", "sentence"] {
        assert!(!words.contains(&correct), "{correct} is flagged: {words:?}");
    }
    let suggestions = checker.suggestions("speling").unwrap();
    assert!(
        suggestions.iter().any(|word| word == "spelling"),
        "{suggestions:?}"
    );
    assert_eq!(checker.languages().collect::<Vec<_>>(), ["en-US"]);
}

#[test]
fn a_word_added_to_the_user_dictionary_is_accepted_until_it_is_removed() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let checker = factory.checker(&[]).unwrap();
    let languages: Vec<String> = checker.languages().map(str::to_owned).collect();
    // Declared last: the files are restored before the checker is released.
    let _files = DictionaryFiles::protect(&languages);
    let word = unique_word();
    let text = format!("{word} stays here");
    assert!(is_flagged(&checker, &text, &word));

    checker.add(&word).unwrap();
    let accepted = eventually(|| !is_flagged(&checker, &text, &word));
    let fresh = factory.checker(&[]).unwrap();
    assert!(
        accepted,
        "the checker still flags an added word (a checker created afterwards flags it: {})",
        is_flagged(&fresh, &text, &word)
    );

    checker.remove(&word).unwrap();
    assert!(
        eventually(|| is_flagged(&checker, &text, &word)),
        "a removed word is flagged again"
    );
}

#[test]
fn an_ignored_word_is_not_flagged_for_the_session() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let checker = factory.checker(&[]).unwrap();
    let languages: Vec<String> = checker.languages().map(str::to_owned).collect();
    let _files = DictionaryFiles::protect(&languages);
    let word = unique_word();
    let text = format!("{word} again");
    assert!(is_flagged(&checker, &text, &word));
    checker.ignore(&word).unwrap();
    assert!(
        eventually(|| !is_flagged(&checker, &text, &word)),
        "an ignored word is accepted"
    );
}

#[test]
fn with_several_languages_a_word_is_flagged_only_when_every_language_rejects_it() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    if !(factory.is_supported("en-US") && factory.is_supported("en-GB")) {
        return;
    }
    let both = factory
        .checker(&["en-US".to_owned(), "en-GB".to_owned()])
        .unwrap();
    assert_eq!(both.languages().collect::<Vec<_>>(), ["en-US", "en-GB"]);
    let us = factory.checker(&["en-US".to_owned()]).unwrap();
    let nonsense = unique_word();
    let text = format!("The colour of {nonsense}");
    // Some installations (Windows Server) report en-GB as supported but check it with the
    // American word list; there is no second language to combine then.
    let gb = factory.checker(&["en-GB".to_owned()]).unwrap();
    if flagged(&gb.check(&text).unwrap(), &text).contains(&"colour") {
        return;
    }
    // "colour" is wrong in American English only.
    assert!(flagged(&us.check(&text).unwrap(), &text).contains(&"colour"));
    let words = flagged(&both.check(&text).unwrap(), &text);
    assert!(!words.contains(&"colour"), "{words:?}");
    assert!(words.contains(&nonsense.as_str()), "{words:?}");
    // Suggestions come from every language, without repeats.
    let suggestions = both.suggestions("speling").unwrap();
    let mut unique = suggestions.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), suggestions.len());
}
