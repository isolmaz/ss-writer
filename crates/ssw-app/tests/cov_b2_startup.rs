//! The decisions of startup that end or bend the launch before the application runs: a Windows
//! build that is too old, a launch handed to the running instance, a handshake that failed, a
//! settings file that had to be moved aside, data locations that cannot be determined, and the
//! refusals of the executable's own command line. Every run lives in a temporary folder.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use ssw_app::args::{Args, Launch};
use ssw_app::startup::{
    Boot, EXIT_FORWARDED, InstanceError, InstanceHook, InstanceRole, ProcessEnv, Started,
    StartupError, boot,
};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_io::paths::{Environment, PathsError};
use ssw_win::gfx::Driver;
use ssw_win::window::{EXIT_UNSUPPORTED_OS, OsVersion};

fn environment(root: &Path) -> Environment {
    Environment {
        local_app_data: Some(root.join("local")),
        app_data: Some(root.join("roaming")),
        exe_dir: root.join("exe"),
    }
}

fn process(root: &Path, build: u32) -> ProcessEnv {
    ProcessEnv {
        os: OsVersion { major: 10, build },
        env: environment(root),
        os_language: "en-US".to_owned(),
        driver: Driver::Warp,
        install_crash_handler: false,
    }
}

/// The handshake answers `answer` and remembers what it was asked to hand over.
struct Hook {
    answer: Result<InstanceRole, String>,
    claimed: Vec<Launch>,
}

impl Hook {
    fn answering(answer: Result<InstanceRole, String>) -> Self {
        Self {
            answer,
            claimed: Vec::new(),
        }
    }
}

impl InstanceHook for Hook {
    fn claim(&mut self, launch: &Launch) -> Result<InstanceRole, InstanceError> {
        self.claimed.push(launch.clone());
        self.answer.clone().map_err(InstanceError)
    }
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
fn an_old_windows_build_is_told_so_and_nothing_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let services = ScriptedServices::new();
    let mut dialogs = services.dialogs();
    let outcome = boot(
        &Args::default(),
        process(tmp.path(), 19045),
        &mut *dialogs,
        None,
    )
    .unwrap();
    assert_eq!(outcome.exit_code(), Some(EXIT_UNSUPPORTED_OS));
    let shown = services.dialog_log();
    assert_eq!(shown.len(), 1);
    assert!(shown[0].message.contains("22621"), "{}", shown[0].message);
    assert!(shown[0].message.contains("19045"), "{}", shown[0].message);
    assert!(
        !tmp.path().join("local").exists(),
        "nothing is created on an unsupported system"
    );
}

#[test]
fn a_launch_handed_to_the_running_instance_ends_the_process_before_anything_is_loaded() {
    let tmp = tempfile::tempdir().unwrap();
    let args = Args::parse([OsString::from("--new-window")]);
    let mut hook = Hook::answering(Ok(InstanceRole::Forwarded));
    let outcome = boot(
        &args,
        process(tmp.path(), 22621),
        &mut *ScriptedServices::new().dialogs(),
        Some(&mut hook),
    )
    .unwrap();
    assert_eq!(outcome.exit_code(), Some(EXIT_FORWARDED));
    assert_eq!(hook.claimed.len(), 1);
    assert!(hook.claimed[0].new_window, "the launch travels as it was");
    assert!(!tmp.path().join("local").exists());
}

#[test]
fn a_failed_handshake_is_logged_and_the_process_runs_as_the_only_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let mut hook = Hook::answering(Err("no window".to_owned()));
    let started = ready(
        boot(
            &Args::default(),
            process(tmp.path(), 22621),
            &mut *ScriptedServices::new().dialogs(),
            Some(&mut hook),
        )
        .unwrap(),
    );
    assert_eq!(hook.claimed.len(), 1);
    let log = log_of(&started);
    assert!(
        log.contains("single-instance handshake failed: no window; running as the only instance"),
        "{log}"
    );
}

#[test]
fn a_settings_file_that_had_to_be_moved_aside_is_logged_with_its_new_place() {
    let tmp = tempfile::tempdir().unwrap();
    let roaming = tmp.path().join("roaming").join("SS Writer");
    std::fs::create_dir_all(&roaming).unwrap();
    std::fs::write(roaming.join("settings.json"), "{ not json").unwrap();
    let started = ready(
        boot(
            &Args::default(),
            process(tmp.path(), 22621),
            &mut *ScriptedServices::new().dialogs(),
            None,
        )
        .unwrap(),
    );
    let backup = roaming.join("settings.json.bak");
    assert_eq!(started.settings_backup, Some(backup.clone()));
    let log = log_of(&started);
    assert!(
        log.contains(&format!(
            "settings.json was unreadable; kept as {}",
            backup.display()
        )),
        "{log}"
    );
}

#[test]
fn data_locations_that_cannot_be_determined_stop_startup() {
    let tmp = tempfile::tempdir().unwrap();
    let mut process = process(tmp.path(), 22621);
    process.env.local_app_data = None;
    let error = boot(
        &Args::default(),
        process,
        &mut *ScriptedServices::new().dialogs(),
        None,
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            StartupError::Paths(PathsError::MissingEnv("LOCALAPPDATA"))
        ),
        "{error}"
    );
    assert_eq!(
        error.to_string(),
        "environment variable LOCALAPPDATA is not set"
    );
}

// ----- the refusals of the command line and the process environment --------------------------

fn parse(flags: &[&str]) -> Args {
    Args::parse(flags.iter().map(OsString::from))
}

#[test]
fn a_usable_launch_starts_with_the_environment_it_was_given() {
    let tmp = tempfile::tempdir().unwrap();
    let env = environment(tmp.path());
    let started = Args::default().startup_environment(Ok(env.clone()));
    assert_eq!(started, Ok(env));
}

#[test]
fn an_environment_that_could_not_be_read_is_refused_with_its_reason() {
    let reason = Args::default()
        .startup_environment(Err(PathsError::NoParent(PathBuf::from(r"C:\"))))
        .unwrap_err();
    assert_eq!(reason, r"executable path C:\ has no parent folder");
    let reason = Args::default()
        .startup_environment(Err(PathsError::MissingEnv("APPDATA")))
        .unwrap_err();
    assert_eq!(reason, "environment variable APPDATA is not set");
}

#[test]
fn flags_that_cannot_work_together_are_refused_before_the_environment_is_looked_at() {
    let tmp = tempfile::tempdir().unwrap();
    let args = parse(&["--perf-scenario", "startup"]);
    let problem = args.perf_problem().expect("a scenario needs a profile");
    // The reason is the flags', whether or not the environment is usable.
    assert_eq!(
        args.startup_environment(Ok(environment(tmp.path()))),
        Err(problem.clone())
    );
    assert_eq!(
        args.startup_environment(Err(PathsError::MissingEnv("APPDATA"))),
        Err(problem)
    );
}
