//! Process startup (architecture "Startup", spec §1, §4.6, §6).
//!
//! The order matters for speed: the Windows build check and the single-instance handshake come
//! first (a forwarded launch exits before anything is loaded), then the Direct3D device is
//! created on a worker thread, and settings, state database, session and recovery listing load
//! on this thread in parallel with it.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::i18n::{Arg, Localizer};
use ssw_core::settings::Settings;
use ssw_io::crash;
use ssw_io::fs::{FsError, RealFs};
use ssw_io::logging::{Level, LogOptions, Logger};
use ssw_io::paths::{AppPaths, Environment, PathsError};
use ssw_io::recovery::{RecoveryEntry, RecoveryStore, now_millis};
use ssw_io::session::Session;
use ssw_io::settings_file::{SettingsFileError, load_settings};
use ssw_io::store::{Store, StoreError};
use ssw_ui::services::{Dialogs, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions};
use ssw_win::gfx::{DeviceStartup, Driver};
use ssw_win::window::{OsVersion, check_os_version};

use crate::args::{Args, Launch};
use crate::clock::{AppClock, SystemAppClock};
use crate::documents::SharedStore;
use crate::emergency::EmergencyCache;
use crate::i18n::{localizer_for, resolve_locale};

/// Process exit code when this launch was handed to the running instance.
pub const EXIT_FORWARDED: i32 = 0;

/// What the single-instance handshake decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceRole {
    /// No other instance runs: this process owns the application.
    Primary,
    /// The launch was handed to the running instance: this process exits.
    Forwarded,
}

/// The single-instance handshake failed.
#[derive(Debug, thiserror::Error)]
#[error("single-instance handshake failed: {0}")]
pub struct InstanceError(pub String);

/// The single-instance seam (spec §6): a named mutex plus `WM_COPYDATA`, implemented over
/// `ssw_win::instance` by the shell.
pub trait InstanceHook {
    /// Becomes the primary instance, or forwards `launch` to the running one.
    ///
    /// # Errors
    /// [`InstanceError`] when the handshake cannot be performed; the process then continues as
    /// if it were the only instance.
    fn claim(&mut self, launch: &Launch) -> Result<InstanceRole, InstanceError>;
}

/// Everything startup takes from the process, injectable for tests.
#[derive(Debug)]
pub struct ProcessEnv {
    /// The running Windows version.
    pub os: OsVersion,
    /// `%LOCALAPPDATA%`, `%APPDATA%` and the executable folder.
    pub env: Environment,
    /// The OS UI language tag (`en-US`).
    pub os_language: String,
    /// Direct3D driver for the startup device (hardware in production).
    pub driver: Driver,
    /// Install the panic hook and the unhandled-exception filter (once per process).
    pub install_crash_handler: bool,
}

/// Why startup failed.
#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    /// Data folders could not be determined or created.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// The log file could not be opened.
    #[error(transparent)]
    Logging(#[from] FsError),
    /// The state database could not be opened.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What a completed startup hands to the application.
#[derive(Debug)]
pub struct Started {
    /// Resolved data locations (portable mode included).
    pub paths: AppPaths,
    /// The user's settings.
    pub settings: Settings,
    /// Set when `settings.json` was unreadable: where its content was moved to.
    pub settings_backup: Option<std::path::PathBuf>,
    /// The locale in use.
    pub locale: String,
    /// Messages for the locale (fallback `en`).
    pub i18n: Rc<Localizer>,
    /// The rolling log.
    pub logger: Arc<Logger>,
    /// The state database.
    pub store: SharedStore,
    /// The crash-recovery folder.
    pub recovery: Arc<RecoveryStore>,
    /// Unsaved text the crash handler writes.
    pub emergency: Arc<EmergencyCache>,
    /// Entries a crashed session left in the recovery folder (offer recovery).
    pub recoverable: Vec<RecoveryEntry>,
    /// The session of the last run.
    pub session: Session,
    /// What this launch asks to open.
    pub launch: Launch,
    /// Clocks.
    pub clock: Rc<dyn AppClock>,
    /// The Direct3D device being created in parallel.
    pub devices: DeviceStartup,
}

/// How startup ended.
#[derive(Debug)]
pub enum Boot {
    /// The application runs.
    Ready(Box<Started>),
    /// The process must exit with this code (unsupported Windows, forwarded launch).
    Exit(i32),
}

impl Boot {
    /// The exit code when the process must end; `None` when the application runs.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Exit(code) => Some(*code),
            Self::Ready(_) => None,
        }
    }

    /// What startup handed to the application; `None` when the process must exit.
    #[must_use]
    pub fn started(self) -> Option<Box<Started>> {
        match self {
            Self::Ready(started) => Some(started),
            Self::Exit(_) => None,
        }
    }
}

/// Shows the localized "Windows 11 22H2 required" message and returns the exit code when
/// `version` is too old; `None` when it is fine.
pub fn check_os(version: OsVersion, dialogs: &mut dyn Dialogs, i18n: &Localizer) -> Option<i32> {
    check_os_version(version, || {
        let options = MessageBoxOptions {
            title: i18n.text("app-os-too-old-title"),
            message: i18n.format(
                "app-os-too-old-message",
                &[("build", Arg::Int(i64::from(version.build)))],
            ),
            buttons: MessageBoxButtons::Ok,
            icon: Some(MessageBoxIcon::Error),
        };
        // Nothing more can be done when even the message box fails.
        let _ = dialogs.message_box(&options);
    })
}

/// Runs startup. See the module documentation for the order.
///
/// # Errors
/// [`StartupError`] when the data folders, the log or the state database are unusable.
pub fn boot(
    args: &Args,
    process: ProcessEnv,
    dialogs: &mut dyn Dialogs,
    instance: Option<&mut dyn InstanceHook>,
) -> Result<Boot, StartupError> {
    let os_i18n = localizer_for(&process.os_language);
    if let Some(code) = check_os(process.os, dialogs, &os_i18n) {
        return Ok(Boot::Exit(code));
    }
    let launch = args.launch(&RealFs);
    let mut handshake_error = None;
    if !args.test.any()
        && let Some(hook) = instance
    {
        match hook.claim(&launch) {
            Ok(InstanceRole::Forwarded) => return Ok(Boot::Exit(EXIT_FORWARDED)),
            Ok(InstanceRole::Primary) => {}
            Err(error) => handshake_error = Some(error),
        }
    }

    // The device takes 200+ ms: start it before anything else is loaded.
    let devices = DeviceStartup::spawn_with(process.driver);

    let paths = AppPaths::resolve(&process.env)?;
    paths.ensure_dirs()?;
    let logger = Arc::new(Logger::new(&paths.logs_dir(), LogOptions::default())?);
    logger.log(Level::Info, "startup", "SS Writer starting");
    if let Some(error) = handshake_error {
        logger.log(
            Level::Warn,
            "startup",
            &format!("{error}; running as the only instance"),
        );
    }

    let recovery = Arc::new(RecoveryStore::new(Arc::new(RealFs), paths.recovery_dir()));
    let emergency = Arc::new(EmergencyCache::new());
    if process.install_crash_handler {
        let cache = Arc::clone(&emergency);
        let store = Arc::clone(&recovery);
        crash::install(paths.crashes_dir(), move || {
            // Best effort: the process is going down either way.
            let _ = cache.save_all(&store, now_millis());
        });
    }

    let (settings, settings_backup) = match load_settings(&paths.settings_file()) {
        Ok(loaded) => (loaded.settings, loaded.backup),
        Err(error) => {
            log_settings_failure(&logger, &error);
            (Settings::default(), None)
        }
    };
    if let Some(backup) = &settings_backup {
        logger.log(
            Level::Warn,
            "startup",
            &format!("settings.json was unreadable; kept as {}", backup.display()),
        );
    }
    let locale = resolve_locale(settings.general.language.as_deref(), &process.os_language);
    let i18n = Rc::new(localizer_for(&locale));

    let store = Store::open(&paths.state_db())?;
    let store: SharedStore = Rc::new(RefCell::new(store));
    let clock: Rc<dyn AppClock> = Rc::new(SystemAppClock::new());
    let now = clock.unix_seconds();
    if let Err(error) = store
        .borrow_mut()
        .history_purge(now, settings.advanced.history_retention_days)
    {
        logger.log(
            Level::Warn,
            "startup",
            &format!("history purge failed: {error}"),
        );
    }
    let session = store.borrow().load_session().unwrap_or_else(|error| {
        logger.log(
            Level::Warn,
            "startup",
            &format!("session unreadable: {error}"),
        );
        Session::default()
    });
    let recoverable = recovery.list().unwrap_or_else(|error| {
        logger.log(
            Level::Warn,
            "startup",
            &format!("recovery list unreadable: {error}"),
        );
        Vec::new()
    });

    Ok(Boot::Ready(Box::new(Started {
        paths,
        settings,
        settings_backup,
        locale,
        i18n,
        logger,
        store,
        recovery,
        emergency,
        recoverable,
        session,
        launch,
        clock,
        devices,
    })))
}

fn log_settings_failure(logger: &Logger, error: &SettingsFileError) {
    logger.log(
        Level::Error,
        "startup",
        &format!("settings.json could not be read, using defaults: {error}"),
    );
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use ssw_io::encoding::TextFormat;
    use ssw_io::history::SnapshotReason;
    use ssw_io::recovery::{RecoveryDoc, RecoveryKind};

    use super::*;
    use crate::app::{AppCore, EXIT_STARTUP_FAILED, ShellHost, run};
    use crate::ui::scripted_services::ScriptedServices;

    /// Dialogs nobody answers (and nobody expects).
    fn quiet() -> Box<dyn Dialogs> {
        ScriptedServices::new().dialogs()
    }

    struct FakeHook {
        answer: Result<InstanceRole, String>,
        claimed: Vec<Launch>,
    }

    impl InstanceHook for FakeHook {
        fn claim(&mut self, launch: &Launch) -> Result<InstanceRole, InstanceError> {
            self.claimed.push(launch.clone());
            self.answer.clone().map_err(InstanceError)
        }
    }

    fn process(root: &Path, build: u32) -> ProcessEnv {
        ProcessEnv {
            os: OsVersion { major: 10, build },
            env: Environment {
                local_app_data: Some(root.join("local")),
                app_data: Some(root.join("roaming")),
                exe_dir: root.join("exe"),
            },
            os_language: "en-US".to_owned(),
            driver: Driver::Warp,
            install_crash_handler: false,
        }
    }

    fn ready(boot: Boot) -> Box<Started> {
        boot.started().expect("startup went on")
    }

    #[test]
    fn an_old_windows_build_shows_the_message_and_exits_with_the_os_code() {
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
        assert_eq!(
            outcome.exit_code(),
            Some(ssw_win::window::EXIT_UNSUPPORTED_OS)
        );
        assert!(outcome.started().is_none(), "nothing runs after the exit");
        let shown = services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].message.contains("22621"));
        assert!(shown[0].message.contains("19045"));
        assert!(
            !tmp.path().join("local").exists(),
            "nothing is created on an unsupported system"
        );
    }

    #[test]
    fn a_launch_forwarded_to_the_running_instance_exits_before_loading_anything() {
        let tmp = tempfile::tempdir().unwrap();
        let args = Args::parse([std::ffi::OsString::from("--new-window")]);
        let mut hook = FakeHook {
            answer: Ok(InstanceRole::Forwarded),
            claimed: Vec::new(),
        };
        let outcome = boot(
            &args,
            process(tmp.path(), 22621),
            &mut *quiet(),
            Some(&mut hook),
        )
        .unwrap();
        assert_eq!(outcome.exit_code(), Some(EXIT_FORWARDED));
        assert_eq!(hook.claimed.len(), 1);
        assert!(hook.claimed[0].new_window);
        assert!(!tmp.path().join("local").exists());

        // Test runs never talk to the user's running editor.
        let mut test_args = Args::default();
        test_args.test.hidden = true;
        let mut other = FakeHook {
            answer: Ok(InstanceRole::Forwarded),
            claimed: Vec::new(),
        };
        let outcome = boot(
            &test_args,
            process(tmp.path(), 22621),
            &mut *quiet(),
            Some(&mut other),
        )
        .unwrap();
        assert!(other.claimed.is_empty());
        assert_eq!(outcome.exit_code(), None);
    }

    #[test]
    fn startup_loads_settings_session_and_recovery_while_the_device_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let roaming = tmp.path().join("roaming").join("SS Writer");
        std::fs::create_dir_all(&roaming).unwrap();
        std::fs::write(
            roaming.join("settings.json"),
            r#"{"general":{"language":"tr-TR"}}"#,
        )
        .unwrap();
        // A crashed session left a recovery entry behind.
        let recovery = RecoveryStore::new(
            Arc::new(RealFs),
            tmp.path().join("local").join("SS Writer").join("recovery"),
        );
        let doc = RecoveryDoc {
            id: "untitled-1",
            kind: RecoveryKind::Untitled,
            original_path: None,
            display_name: "Untitled",
            text: "lost words",
            format: TextFormat::default(),
        };
        recovery.backup(&doc, 5).unwrap();

        let mut hook = FakeHook {
            answer: Err("no window".to_owned()),
            claimed: Vec::new(),
        };
        let started = ready(
            boot(
                &Args::default(),
                process(tmp.path(), 22621),
                &mut *quiet(),
                Some(&mut hook),
            )
            .unwrap(),
        );
        assert_eq!(started.locale, "tr-TR");
        assert_eq!(started.settings.general.language.as_deref(), Some("tr-TR"));
        assert_eq!(started.recoverable.len(), 1);
        assert_eq!(started.recoverable[0].id, "untitled-1");
        assert!(started.session.windows.is_empty());
        assert!(started.paths.state_db().is_file());
        assert!(started.i18n.has_message("app-untitled"));
        let log = std::fs::read_to_string(started.paths.logs_dir().join("ssw.log")).unwrap();
        assert!(log.contains("SS Writer starting"), "{log}");
        assert!(
            log.contains("no window"),
            "the failed handshake is logged: {log}"
        );
        assert!(
            started.devices.wait().is_ok(),
            "the device was created in parallel"
        );
    }

    #[test]
    fn an_unreadable_settings_file_is_kept_aside_and_defaults_are_used() {
        let tmp = tempfile::tempdir().unwrap();
        let roaming = tmp.path().join("roaming").join("SS Writer");
        std::fs::create_dir_all(&roaming).unwrap();
        std::fs::write(roaming.join("settings.json"), "{ not json").unwrap();
        let started = ready(
            boot(
                &Args::default(),
                process(tmp.path(), 22621),
                &mut *quiet(),
                None,
            )
            .unwrap(),
        );
        assert_eq!(started.settings, Settings::default());
        assert_eq!(
            started.settings_backup,
            Some(roaming.join("settings.json.bak"))
        );
        assert_eq!(started.locale, "en-US");
    }

    #[test]
    fn startup_purges_the_history_older_than_the_retention_setting() {
        let tmp = tempfile::tempdir().unwrap();
        let roaming = tmp.path().join("roaming").join("SS Writer");
        std::fs::create_dir_all(&roaming).unwrap();
        std::fs::write(
            roaming.join("settings.json"),
            r#"{"advanced":{"history_retention_days":7}}"#,
        )
        .unwrap();
        let process = process(tmp.path(), 22621);
        let file = Path::new("C:\\notes\\a.md");
        let now = SystemAppClock::new().unix_seconds();
        let db = AppPaths::resolve(&process.env).unwrap().state_db();
        {
            let mut store = Store::open(&db).unwrap();
            for (days_ago, text) in [(10_i64, "ten days ago"), (6, "six days ago"), (0, "today")] {
                let at = now - days_ago * 86_400;
                store
                    .history_snapshot(file, text, SnapshotReason::Periodic, at)
                    .unwrap();
            }
        }
        let started = ready(boot(&Args::default(), process, &mut *quiet(), None).unwrap());
        let kept: Vec<usize> = started
            .store
            .borrow()
            .history_list(file)
            .unwrap()
            .iter()
            .map(|version| version.size)
            .collect();
        assert_eq!(kept, ["today".len(), "six days ago".len()]);
    }

    #[test]
    fn a_portable_marker_moves_the_data_next_to_the_executable() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("exe")).unwrap();
        std::fs::write(tmp.path().join("exe").join("portable"), "").unwrap();
        let started = ready(
            boot(
                &Args::default(),
                process(tmp.path(), 22621),
                &mut *quiet(),
                None,
            )
            .unwrap(),
        );
        assert!(started.paths.is_portable());
        assert!(
            tmp.path()
                .join("exe")
                .join("data")
                .join("state.db")
                .is_file()
        );
        assert!(!tmp.path().join("local").exists());
    }

    struct ExitCodeShell {
        ran: bool,
    }

    impl ShellHost for ExitCodeShell {
        fn run(&mut self, mut core: AppCore, _args: &Args) -> i32 {
            self.ran = true;
            assert!(core.docs.all().is_empty());
            assert_eq!(core.plan.windows.len(), 1, "a fresh start plans one window");
            assert!(core.wait_devices().is_ok());
            assert!(core.wait_devices().is_err(), "the device can be taken once");
            let report = core.pump();
            assert_eq!(report.jobs, 0);
            core.shutdown(&Session::default());
            17
        }
    }

    #[test]
    fn run_hands_the_core_to_the_shell_and_returns_its_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let mut shell = ExitCodeShell { ran: false };
        let code = run(
            &Args::default(),
            process(tmp.path(), 22621),
            &mut *quiet(),
            None,
            &mut shell,
        );
        assert_eq!(code, 17);
        assert!(shell.ran);
        let mut old_shell = ExitCodeShell { ran: false };
        let code = run(
            &Args::default(),
            process(tmp.path(), 100),
            &mut *quiet(),
            None,
            &mut old_shell,
        );
        assert_eq!(code, ssw_win::window::EXIT_UNSUPPORTED_OS);
        assert!(!old_shell.ran);
    }

    #[test]
    fn a_startup_failure_is_shown_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let mut process = process(tmp.path(), 22621);
        process.env.local_app_data = None;
        let services = ScriptedServices::new();
        let mut dialogs = services.dialogs();
        let mut shell = ExitCodeShell { ran: false };
        let code = run(&Args::default(), process, &mut *dialogs, None, &mut shell);
        assert_eq!(code, EXIT_STARTUP_FAILED);
        assert!(!shell.ran);
        let shown = services.dialog_log();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].message.contains("LOCALAPPDATA"));
    }
}
