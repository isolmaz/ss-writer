//! Startup over damaged profiles: every failure `boot` handles is reached with a real broken
//! file or folder (a file where a folder belongs, a folder where a file belongs, a database
//! without its tables, a corrupt manifest). Everything lives in temporary folders.

use std::ffi::OsString;
use std::path::Path;

use ssw_app::args::{Args, Launch};
use ssw_app::emergency::CachedDoc;
use ssw_app::startup::{
    Boot, InstanceError, InstanceHook, InstanceRole, ProcessEnv, Started, StartupError, boot,
};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_core::settings::Settings;
use ssw_io::encoding::TextFormat;
use ssw_io::paths::{AppPaths, Environment};
use ssw_io::recovery::{MANIFEST_NAME, RecoveryKind};
use ssw_io::store::Store;
use ssw_ui::services::Dialogs;
use ssw_win::gfx::Driver;
use ssw_win::window::OsVersion;

fn environment(root: &Path) -> Environment {
    Environment {
        local_app_data: Some(root.join("local")),
        app_data: Some(root.join("roaming")),
        exe_dir: root.join("exe"),
    }
}

fn process(root: &Path) -> ProcessEnv {
    ProcessEnv {
        os: OsVersion {
            major: 10,
            build: 22621,
        },
        env: environment(root),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

fn paths(root: &Path) -> AppPaths {
    AppPaths::resolve(&environment(root)).unwrap()
}

fn quiet() -> Box<dyn Dialogs> {
    ScriptedServices::new().dialogs()
}

fn ready(outcome: Boot) -> Box<Started> {
    match outcome {
        Boot::Ready(started) => started,
        Boot::Exit(code) => panic!("startup exited with {code}"),
    }
}

fn log_of(started: &Started) -> String {
    std::fs::read_to_string(started.paths.logs_dir().join("ssw.log")).unwrap()
}

#[test]
fn a_data_folder_that_cannot_be_created_stops_startup() {
    let tmp = tempfile::tempdir().unwrap();
    // `local` is a file: no folder can be created below it.
    std::fs::write(tmp.path().join("local"), "in the way").unwrap();
    let error = boot(&Args::default(), process(tmp.path()), &mut *quiet(), None).unwrap_err();
    assert!(matches!(error, StartupError::Paths(_)), "{error}");
}

#[test]
fn a_log_file_that_cannot_be_opened_stops_startup() {
    let tmp = tempfile::tempdir().unwrap();
    // A folder sits where the log file belongs.
    std::fs::create_dir_all(paths(tmp.path()).logs_dir().join("ssw.log")).unwrap();
    let error = boot(&Args::default(), process(tmp.path()), &mut *quiet(), None).unwrap_err();
    assert!(matches!(error, StartupError::Logging(_)), "{error}");
}

#[test]
fn a_state_database_that_cannot_be_opened_stops_startup() {
    let tmp = tempfile::tempdir().unwrap();
    // A folder sits where the database belongs.
    std::fs::create_dir_all(paths(tmp.path()).state_db()).unwrap();
    let error = boot(&Args::default(), process(tmp.path()), &mut *quiet(), None).unwrap_err();
    assert!(matches!(error, StartupError::Store(_)), "{error}");
}

#[test]
fn a_settings_file_that_cannot_be_read_gives_the_defaults_and_is_logged() {
    let tmp = tempfile::tempdir().unwrap();
    // A folder sits where `settings.json` belongs: reading it fails, it is not "missing".
    std::fs::create_dir_all(paths(tmp.path()).settings_file()).unwrap();
    let started = ready(boot(&Args::default(), process(tmp.path()), &mut *quiet(), None).unwrap());
    assert_eq!(started.settings, Settings::default());
    assert_eq!(started.settings_backup, None);
    let log = log_of(&started);
    assert!(
        log.contains("settings.json could not be read, using defaults"),
        "{log}"
    );
}

#[test]
fn a_database_without_its_tables_starts_with_an_empty_session_and_logs_why() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    paths.ensure_dirs().unwrap();
    // The schema exists (so opening succeeds); another connection then takes two tables away.
    drop(Store::open(&paths.state_db()).unwrap());
    rusqlite::Connection::open(paths.state_db())
        .unwrap()
        .execute_batch("DROP TABLE history_snapshots; DROP TABLE session_windows;")
        .unwrap();
    let started = ready(boot(&Args::default(), process(tmp.path()), &mut *quiet(), None).unwrap());
    assert!(started.session.windows.is_empty());
    let log = log_of(&started);
    assert!(log.contains("history purge failed"), "{log}");
    assert!(log.contains("session unreadable"), "{log}");
}

#[test]
fn a_corrupt_recovery_manifest_offers_nothing_and_is_logged() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths(tmp.path());
    std::fs::create_dir_all(paths.recovery_dir()).unwrap();
    std::fs::write(paths.recovery_dir().join(MANIFEST_NAME), "{ not json").unwrap();
    let started = ready(boot(&Args::default(), process(tmp.path()), &mut *quiet(), None).unwrap());
    assert!(started.recoverable.is_empty());
    let log = log_of(&started);
    assert!(log.contains("recovery list unreadable"), "{log}");
}

struct Hook {
    claimed: Vec<Launch>,
}

impl InstanceHook for Hook {
    fn claim(&mut self, launch: &Launch) -> Result<InstanceRole, InstanceError> {
        self.claimed.push(launch.clone());
        Ok(InstanceRole::Primary)
    }
}

#[test]
fn the_first_instance_goes_on_with_its_own_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let args = Args::parse([OsString::from("--new-window")]);
    let mut hook = Hook {
        claimed: Vec::new(),
    };
    let started = ready(boot(&args, process(tmp.path()), &mut *quiet(), Some(&mut hook)).unwrap());
    assert_eq!(hook.claimed.len(), 1);
    assert!(hook.claimed[0].new_window);
    assert!(started.launch.new_window);
    let log = log_of(&started);
    assert!(!log.contains("running as the only instance"), "{log}");
}

#[test]
fn a_crash_on_the_ui_thread_saves_the_unsaved_text_to_the_recovery_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let mut process = process(tmp.path());
    process.install_crash_handler = true;
    let started = ready(boot(&Args::default(), process, &mut *quiet(), None).unwrap());
    started.emergency.put(
        "untitled-9",
        CachedDoc {
            kind: RecoveryKind::Untitled,
            original_path: None,
            display_name: "Untitled".to_owned(),
            text: "unsaved words".to_owned(),
            format: TextFormat::default(),
            version: 1,
        },
    );
    assert!(started.recovery.list().unwrap().is_empty());

    // The panic hook of the handler runs on the panicking (UI) thread.
    assert!(std::panic::catch_unwind(|| panic!("a simulated crash of the UI thread")).is_err());

    let entries = started.recovery.list().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "untitled-9");
    assert_eq!(
        started.recovery.restore("untitled-9").unwrap().text,
        "unsaved words"
    );
    let dumps = std::fs::read_dir(started.paths.crashes_dir())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "dmp"))
        .count();
    assert_eq!(dumps, 1, "one minidump of the crash");
}
