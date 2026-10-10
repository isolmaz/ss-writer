//! The failures [`AppCore`] survives and reports in its log: a folder that can no longer be
//! watched, a history purge the database refuses, a session that cannot be stored. The
//! database is broken by a second connection, the watched folder is really deleted.

use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ssw_app::app::AppCore;
use ssw_app::args::Args;
use ssw_app::clock::{AppClock, ManualClock};
use ssw_app::startup::{Boot, ProcessEnv, boot};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::workers::DeferredWaker;
use ssw_io::paths::Environment;
use ssw_io::session::Session;
use ssw_win::gfx::Driver;
use ssw_win::window::OsVersion;

/// A core over a fresh profile in `root`, with a clock the test moves by hand.
fn core_in(root: &Path) -> (AppCore, Rc<ManualClock>) {
    let process = ProcessEnv {
        os: OsVersion {
            major: 10,
            build: 22621,
        },
        env: Environment {
            local_app_data: Some(root.join("local")),
            app_data: Some(root.join("roaming")),
            exe_dir: root.join("exe"),
        },
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    };
    let mut dialogs = ScriptedServices::new().dialogs();
    let Boot::Ready(mut started) = boot(&Args::default(), process, &mut *dialogs, None).unwrap()
    else {
        panic!("startup did not finish");
    };
    let clock = Rc::new(ManualClock::new(1_000_000));
    started.clock = Rc::clone(&clock) as Rc<dyn AppClock>;
    (
        AppCore::new(*started, Arc::new(DeferredWaker::new())),
        clock,
    )
}

fn log_of(core: &AppCore) -> String {
    std::fs::read_to_string(core.paths.logs_dir().join("ssw.log")).unwrap()
}

fn break_tables(core: &AppCore, sql: &str) {
    rusqlite::Connection::open(core.paths.state_db())
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

#[test]
fn a_folder_that_vanished_cannot_be_watched_and_the_pump_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut core, _) = core_in(tmp.path());
    let notes = tmp.path().join("notes");
    std::fs::create_dir(&notes).unwrap();
    let file = notes.join("a.md");
    std::fs::write(&file, "one\n").unwrap();
    let entry = core.docs.open(&file).unwrap();

    std::fs::remove_dir_all(&notes).unwrap();
    core.pump();

    assert!(core.watches.folders().is_empty());
    let log = log_of(&core);
    assert!(log.contains("cannot watch"), "{log}");
    assert!(log.contains("notes"), "{log}");
    drop(entry);
}

#[test]
fn a_history_purge_the_database_refuses_is_logged_and_the_pump_goes_on() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut core, clock) = core_in(tmp.path());
    // The first turn starts the purge interval; the table then disappears.
    core.pump();
    break_tables(&core, "DROP TABLE history_snapshots");
    clock.advance(Duration::from_secs(2 * 3600));

    let report = core.pump();

    assert!(report.tick.purge_failed.is_some());
    assert_eq!(report.tick.purged, 0);
    let log = log_of(&core);
    assert!(log.contains("history purge failed"), "{log}");
}

#[test]
fn a_session_the_database_refuses_is_logged_and_the_exit_still_completes() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut core, _) = core_in(tmp.path());
    break_tables(&core, "DROP TABLE session_windows");

    core.shutdown(&Session::default());

    let log = log_of(&core);
    assert!(log.contains("session not saved"), "{log}");
    assert!(log.contains("SS Writer exiting"), "{log}");
}
