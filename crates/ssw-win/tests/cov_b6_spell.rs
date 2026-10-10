//! The failure of `SpellChecker::remove` before it reaches the user's dictionaries, through the
//! library as the other crates link it (the real removal runs in `integ_spell.rs` under a
//! guard that restores the dictionary files).

#![cfg(feature = "fault-injection")]

use ssw_win::com::ComGuard;
use ssw_win::spell::SpellFactory;

#[test]
fn removing_a_word_fails_cleanly_when_the_checker_has_no_remove_interface() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = SpellFactory::new().unwrap();
    let checker = factory.checker(&[]).unwrap();
    // The interface query is the first checked call of `remove`: with it failing nothing reaches
    // the user's exclusion list.
    ssw_win::fault::arm(0);
    let error = checker.remove("xqzvkjwp").unwrap_err();
    let tripped = ssw_win::fault::tripped();
    ssw_win::fault::disarm();
    assert!(tripped, "the interface query is a checked call");
    assert!(
        error
            .to_string()
            .starts_with("ISpellChecker as ISpellChecker2 failed"),
        "{error}"
    );
}
