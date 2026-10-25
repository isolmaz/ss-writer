//! The composed application core and the process entry point.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::i18n::{Arg, Localizer};
use ssw_core::settings::Settings;
use ssw_io::fs::RealFs;
use ssw_io::logging::{Level, Logger};
use ssw_io::paths::AppPaths;
use ssw_io::recovery::{RecoveryEntry, RecoveryStore};
use ssw_io::save::ThreadSleeper;
use ssw_io::session::Session;
use ssw_ui::services::{Dialogs, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions};
use ssw_win::com::WinError;
use ssw_win::gfx::{DeviceStartup, GfxDevices};

use crate::args::Args;
use crate::autosave::{Autosave, AutosavePolicy, TickReport};
use crate::commands::CommandDispatcher;
use crate::documents::{DocEnv, Documents, ExternalOutcome, FolderWatches};
use crate::i18n::localizer_for;
use crate::session::{SessionManager, StartupPlan, plan_startup};
use crate::startup::{Boot, InstanceHook, ProcessEnv, Started, boot};
use crate::workers::{DeferredWaker, Workers};

/// Process exit code when startup failed.
pub const EXIT_STARTUP_FAILED: i32 = 2;
/// Process exit code of a build without the visible shell.
pub const EXIT_NO_SHELL: i32 = 4;
/// Process exit code when the graphics device could not be created.
pub const EXIT_NO_DEVICE: i32 = 5;

/// What one [`AppCore::pump`] did.
#[derive(Debug)]
pub struct PumpReport {
    /// Worker results delivered.
    pub jobs: usize,
    /// Documents changed by other programs.
    pub external: Vec<ExternalOutcome>,
    /// Autosave, backups, snapshots and the history purge.
    pub tick: TickReport,
}

/// The application core. Owned by the UI thread.
#[derive(Debug)]
pub struct AppCore {
    /// Data locations.
    pub paths: AppPaths,
    /// The user's settings.
    pub settings: Settings,
    /// Messages for the active locale.
    pub i18n: Rc<Localizer>,
    /// The rolling log.
    pub logger: Arc<Logger>,
    /// Open documents.
    pub docs: Rc<Documents>,
    /// Autosave scheduler.
    pub autosave: Autosave,
    /// Session persistence and the reopen stack.
    pub session: SessionManager,
    /// Command routing.
    pub commands: CommandDispatcher,
    /// Worker pool.
    pub workers: Rc<Workers>,
    /// Watchers of the folders of open files.
    pub watches: FolderWatches,
    /// Wakes the message loop; connect it once the first window exists.
    pub waker: Arc<DeferredWaker>,
    /// Windows and tabs to create at startup.
    pub plan: StartupPlan,
    /// Entries a crashed session left behind (offer recovery).
    pub recoverable: Vec<RecoveryEntry>,
    /// Crash-recovery folder.
    pub recovery: Arc<RecoveryStore>,
    /// What `recoverable` held at startup. The offer takes `recoverable` away, so this is what
    /// tells a clean exit which entries an earlier run left and the user has not answered yet.
    inherited: Vec<RecoveryEntry>,
    devices: Option<DeviceStartup>,
}

impl AppCore {
    /// Builds the core from a finished startup.
    #[must_use]
    pub fn new(started: Started, waker: Arc<DeferredWaker>) -> Self {
        let env = DocEnv {
            fs: Arc::new(RealFs),
            sleeper: Arc::new(ThreadSleeper),
            clock: Rc::clone(&started.clock),
            store: Rc::clone(&started.store),
            recovery: Arc::clone(&started.recovery),
            tld: ssw_io::encoding::system_tld_hint(),
        };
        let docs = Rc::new(Documents::new(env));
        let policy = AutosavePolicy::from_settings(&started.settings);
        let default_folder = started
            .settings
            .general
            .default_folder
            .as_deref()
            .map(PathBuf::from);
        let plan = plan_startup(
            started.settings.general.on_startup,
            default_folder.as_deref(),
            &started.session,
            &started.launch,
        );
        Self {
            autosave: Autosave::new(Rc::clone(&docs), Arc::clone(&started.emergency), policy),
            session: SessionManager::new(Rc::clone(&docs)),
            commands: CommandDispatcher::new(Rc::clone(&docs)),
            workers: Rc::new(Workers::new(Workers::default_thread_count(), waker.clone())),
            watches: FolderWatches::new(waker.clone(), false),
            docs,
            waker,
            plan,
            paths: started.paths,
            settings: started.settings,
            i18n: started.i18n,
            logger: started.logger,
            inherited: started.recoverable.clone(),
            recoverable: started.recoverable,
            recovery: started.recovery,
            devices: Some(started.devices),
        }
    }

    /// Takes the device creation that runs in parallel with startup (once): the shell decides
    /// whether to wait for it or to open its windows first.
    ///
    /// # Errors
    /// [`WinError::Invalid`] when called twice.
    pub fn take_devices(&mut self) -> Result<DeviceStartup, WinError> {
        self.devices
            .take()
            .ok_or(WinError::Invalid("the device was already taken"))
    }

    /// Waits for the Direct3D device created in parallel with startup (once).
    ///
    /// # Errors
    /// The device creation error; [`WinError::Invalid`] when called twice.
    pub fn wait_devices(&mut self) -> Result<GfxDevices, WinError> {
        self.take_devices()?.wait()
    }

    /// One turn of housekeeping for the message loop: deliver worker results, apply external
    /// file changes, keep the watched folders in step with the open files, run the autosave
    /// tick.
    pub fn pump(&mut self) -> PumpReport {
        let jobs = self.workers.poll();
        let now = self.docs.env().clock.monotonic();
        let failed = self.watches.sync(&self.docs.watch_folders(), now);
        for (folder, error) in failed {
            self.logger.log(
                Level::Warn,
                "watch",
                &format!("cannot watch {}: {error}", folder.display()),
            );
        }
        let external = self.docs.handle_fs_events(&self.watches.drain());
        let tick = self.autosave.tick(&self.i18n);
        if let Some(error) = &tick.purge_failed {
            self.logger.log(
                Level::Warn,
                "history",
                &format!("history purge failed: {error}"),
            );
        }
        PumpReport {
            jobs,
            external,
            tick,
        }
    }

    /// Clean exit: flushes autosave, stores `session` and removes the recovery entries this run
    /// wrote (a clean exit has nothing to recover from it). What an earlier, crashed run left
    /// and the user has not answered stays for the next start to offer again.
    pub fn shutdown(&mut self, session: &Session) {
        let _ = self.autosave.flush_all();
        if let Err(error) = self.session.save(session) {
            self.logger.log(
                Level::Error,
                "shutdown",
                &format!("session not saved: {error}"),
            );
        }
        for entry in self.recovery.list().unwrap_or_default() {
            if !self.inherited.contains(&entry) {
                let _ = self.recovery.discard(&entry.id);
            }
        }
        self.logger
            .log(Level::Info, "shutdown", "SS Writer exiting");
    }
}

/// The visible shell: runs the message loop until the application exits.
pub trait ShellHost {
    /// Runs the application (the shell owns `core` from here on) and returns the process exit
    /// code.
    fn run(&mut self, core: AppCore, args: &Args) -> i32;
}

/// Runs the whole application: startup, the shell, exit code.
pub fn run(
    args: &Args,
    process: ProcessEnv,
    dialogs: &mut dyn Dialogs,
    instance: Option<&mut dyn InstanceHook>,
    shell: &mut dyn ShellHost,
) -> i32 {
    let os_language = process.os_language.clone();
    match boot(args, process, dialogs, instance) {
        Ok(Boot::Exit(code)) => code,
        Ok(Boot::Ready(started)) => {
            ssw_win::perf::mark(crate::perf::STARTUP_DONE);
            let core = AppCore::new(*started, Arc::new(DeferredWaker::new()));
            shell.run(core, args)
        }
        Err(error) => {
            let i18n = localizer_for(&os_language);
            let _ = dialogs.message_box(&MessageBoxOptions {
                title: i18n.text("app-startup-failed-title"),
                message: i18n.format(
                    "app-startup-failed-message",
                    &[("reason", Arg::Str(&error.to_string()))],
                ),
                buttons: MessageBoxButtons::Ok,
                icon: Some(MessageBoxIcon::Error),
            });
            EXIT_STARTUP_FAILED
        }
    }
}
