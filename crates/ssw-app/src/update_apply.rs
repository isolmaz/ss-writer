//! Restart and install: the second half of the self-update (the first half downloads and
//! verifies `SSWriter-<version>-x64.msi`).
//!
//! The running `SSWriter.exe` cannot install over itself: its own file is locked. So
//! [`start`] copies the executable to a fresh folder below the temp directory and launches the
//! copy with the internal flags `--apply-update <msi> --updater-parent <pid> --updater-target
//! <exe>` ([`UpdaterFlags`]); the caller then quits the application. In that copy `main` calls
//! [`run_updater`] before it creates anything else (no window, no settings, no session), and
//! [`run_updater`]:
//!
//! 1. waits (at most [`PARENT_EXIT_TIMEOUT`]) for the application process to end,
//! 2. runs `msiexec /i <msi> /qb /norestart` (per-user package, no elevation; the basic UI of
//!    msiexec is the only window the updater ever shows) and waits for it,
//! 3. when the exit code is 0, 1641 or 3010: starts the installed executable and deletes the
//!    package.
//!
//! Failure paths (the process exit code of the updater says which; nothing else survives, the
//! application is gone by then):
//!
//! - the application does not end in time, or cannot be waited for:
//!   [`EXIT_UPDATE_PARENT_RUNNING`]; nothing is installed or started, the package stays;
//! - `msiexec` cannot be started or ends with any other code (a cancelled setup is 1602):
//!   [`EXIT_UPDATE_INSTALL_FAILED`]; the package stays, no executable is started (msiexec shows
//!   its own error dialog for a failed setup);
//! - the package is installed but the new executable cannot be started:
//!   [`EXIT_UPDATE_START_FAILED`]; the package is deleted all the same.
//!
//! The OS seams are the [`Launcher`], [`CommandRunner`] and [`ProcessWaiter`] traits;
//! [`SystemProcesses`] is the real implementation of all three.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use ssw_io::fs::{FileSystem, FsError, RealFs};
use ssw_win::com::WinError;
use ssw_win::process::{ExitWait, wait_for_exit};

use crate::args::UpdaterFlags;

/// Exit code of the updater after a complete update (the new build was started).
pub const EXIT_UPDATED: i32 = 0;
/// Exit code of the updater when the application did not end within [`PARENT_EXIT_TIMEOUT`] or
/// could not be waited for: nothing was installed.
pub const EXIT_UPDATE_PARENT_RUNNING: i32 = 10;
/// Exit code of the updater when the package could not be installed: `msiexec` did not start or
/// reported a failure. The package stays where it is.
pub const EXIT_UPDATE_INSTALL_FAILED: i32 = 11;
/// Exit code of the updater when the package was installed but the new executable could not be
/// started.
pub const EXIT_UPDATE_START_FAILED: i32 = 12;

/// How long the updater waits for the application to end before it gives up.
pub const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(120);

/// The file name of the executable copy the updater runs as.
pub const UPDATER_FILE_NAME: &str = "SSWriter-updater.exe";

/// Name of the folders below the temp directory that hold an updater copy; the application's
/// process id and a counter follow.
const UPDATER_FOLDER_PREFIX: &str = "SSWriter-update";

/// How many folder names [`start`] tries before it reports the last "already exists".
const FOLDER_ATTEMPTS: u32 = 16;

/// `msiexec` exit codes that mean "installed": success, success with a restart already
/// initiated (`ERROR_SUCCESS_REBOOT_INITIATED`) and success with a restart required
/// (`ERROR_SUCCESS_REBOOT_REQUIRED`). A per-user package that replaces no file in use needs
/// none, but it is installed in all three cases.
const INSTALLED_CODES: [i32; 3] = [0, 1641, 3010];

/// `CREATE_NO_WINDOW`: console programs get no console window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Why [`start`] could not launch the updater. Nothing is left behind in the temp directory.
#[derive(Debug, thiserror::Error)]
pub enum UpdateApplyError {
    /// The folder for the updater copy could not be created.
    #[error("cannot create the updater folder: {0}")]
    CreateFolder(#[source] FsError),
    /// The running executable could not be copied.
    #[error("cannot copy the application for the updater: {0}")]
    CopyExecutable(#[source] FsError),
    /// The copy could not be started.
    #[error("cannot start the updater: {0}")]
    Launch(#[source] io::Error),
}

/// Starts a program without waiting for it.
pub trait Launcher: fmt::Debug {
    /// Starts `program` with `args`; the program runs on after the caller ends.
    ///
    /// # Errors
    /// The program could not be started.
    fn launch(&self, program: &Path, args: &[OsString]) -> io::Result<()>;
}

/// Runs a program to its end; it can start programs as well.
pub trait CommandRunner: Launcher {
    /// Runs `program` with `args`, waits for it and returns its exit code.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<i32>;
}

/// Waits for another process to end.
pub trait ProcessWaiter {
    /// Waits at most `timeout` for the process `pid` to end.
    ///
    /// # Errors
    /// The process cannot be waited for.
    fn wait_for_exit(&self, pid: u32, timeout: Duration) -> Result<ExitWait, WinError>;
}

/// The real [`Launcher`], [`CommandRunner`] and [`ProcessWaiter`]: `std::process::Command` (no
/// console window, no inherited standard handles) and the bounded process wait of `ssw-win`.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemProcesses;

impl SystemProcesses {
    /// `program` with `args`, without a console window and with the standard handles closed: a
    /// parent that redirected its own (the pipes of a tool that started the editor) must not be
    /// kept alive by a process that outlives it.
    fn command(program: &Path, args: &[OsString]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        command
    }
}

/// The exit code of a finished process; on Windows every process has one.
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

impl Launcher for SystemProcesses {
    fn launch(&self, program: &Path, args: &[OsString]) -> io::Result<()> {
        // The child handle is dropped: the process is neither waited for nor killed.
        Self::command(program, args).spawn().map(drop)
    }
}

impl CommandRunner for SystemProcesses {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<i32> {
        Self::command(program, args).status().map(exit_code)
    }
}

impl ProcessWaiter for SystemProcesses {
    fn wait_for_exit(&self, pid: u32, timeout: Duration) -> Result<ExitWait, WinError> {
        wait_for_exit(pid, timeout)
    }
}

/// Prepares and starts the detached updater for `installer`: copies the running executable
/// `app_exe` to a fresh folder under `temp_root` and launches that copy with the hidden flags
/// (`pid` is the application process the updater waits for, `app_exe` the executable it starts
/// after the installation). The caller quits the application right after this returns.
///
/// # Errors
/// [`UpdateApplyError`] when the folder cannot be created, the executable cannot be copied or
/// the copy cannot be started; what was created is removed again.
pub fn start(
    installer: &Path,
    app_exe: &Path,
    temp_root: &Path,
    pid: u32,
    fs: &dyn FileSystem,
    launcher: &dyn Launcher,
) -> Result<(), UpdateApplyError> {
    let folder =
        create_updater_folder(fs, temp_root, pid).map_err(UpdateApplyError::CreateFolder)?;
    let updater = folder.join(UPDATER_FILE_NAME);
    let flags = UpdaterFlags {
        installer: installer.to_path_buf(),
        parent: pid,
        target: app_exe.to_path_buf(),
    };
    let started = fs
        .copy_file(app_exe, &updater)
        .map_err(UpdateApplyError::CopyExecutable)
        .and_then(|()| {
            launcher
                .launch(&updater, &flags.arguments())
                .map_err(UpdateApplyError::Launch)
        });
    if started.is_err() {
        // Best effort: what is left of a failed attempt is a stray file in the temp directory.
        let _ = fs.delete_file(&updater);
        let _ = fs.remove_dir(&folder);
    }
    started
}

/// Creates a folder below `temp_root` that did not exist before: `create_dir` fails for an
/// existing one (the leftover of an earlier attempt of this process id), so the next counter
/// value is tried.
fn create_updater_folder(
    fs: &dyn FileSystem,
    temp_root: &Path,
    pid: u32,
) -> Result<PathBuf, FsError> {
    let mut attempt = 0;
    loop {
        let folder = temp_root.join(format!("{UPDATER_FOLDER_PREFIX}-{pid}-{attempt}"));
        match fs.create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(FsError::AlreadyExists(_)) if attempt + 1 < FOLDER_ATTEMPTS => attempt += 1,
            Err(error) => return Err(error),
        }
    }
}

/// Runs the updater role: waits for the application to exit, installs the package, starts the
/// new build and returns the process exit code ([`EXIT_UPDATED`] or one of the
/// `EXIT_UPDATE_*` codes; see the module documentation for what each failure leaves behind).
///
/// The package is deleted after a successful installation whether or not the new build could
/// be started; a package that cannot be deleted is left in the temp directory.
#[must_use]
pub fn run_updater(
    flags: &UpdaterFlags,
    waiter: &dyn ProcessWaiter,
    runner: &dyn CommandRunner,
) -> i32 {
    let parent_gone = matches!(
        waiter.wait_for_exit(flags.parent, PARENT_EXIT_TIMEOUT),
        Ok(ExitWait::Exited)
    );
    if !parent_gone {
        return EXIT_UPDATE_PARENT_RUNNING;
    }
    let installed = runner
        .run(&msiexec_path(), &install_arguments(&flags.installer))
        .is_ok_and(|code| INSTALLED_CODES.contains(&code));
    if !installed {
        return EXIT_UPDATE_INSTALL_FAILED;
    }
    let started = runner.launch(&flags.target, &[]);
    // Nothing can be done about a package that stays: it is a file in the temp directory.
    let _ = RealFs.delete_file(&flags.installer);
    if started.is_ok() {
        EXIT_UPDATED
    } else {
        EXIT_UPDATE_START_FAILED
    }
}

/// The arguments of the installation: a per-user package with the basic UI, never restarting
/// the computer.
fn install_arguments(installer: &Path) -> Vec<OsString> {
    vec![
        "/i".into(),
        installer.as_os_str().to_owned(),
        "/qb".into(),
        "/norestart".into(),
    ]
}

/// `msiexec.exe` of this system: below the system root (never found through the search path or
/// the folder of the updater), or the bare name when the process has no `SystemRoot`.
fn msiexec_path() -> PathBuf {
    msiexec_in(std::env::var_os("SystemRoot"))
}

/// [`msiexec_path`] for the given `SystemRoot` value.
fn msiexec_in(system_root: Option<OsString>) -> PathBuf {
    system_root.map_or_else(
        || PathBuf::from("msiexec.exe"),
        |root| PathBuf::from(root).join("System32").join("msiexec.exe"),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::windows::process::ExitStatusExt;

    use ssw_io::fs::{FaultFs, FsFailure, FsOp, win32};

    use super::*;
    use crate::args::Args;

    /// What the fakes saw, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Event {
        Wait {
            pid: u32,
            timeout: Duration,
        },
        Run {
            program: PathBuf,
            args: Vec<OsString>,
            /// Whether the package was still on the disk when the program ran.
            package_present: bool,
        },
        Launch {
            program: PathBuf,
            args: Vec<OsString>,
        },
    }

    /// How the application process behaves.
    #[derive(Clone, Copy, Debug)]
    enum Parent {
        Exits,
        Stays,
        CannotBeWaitedFor,
    }

    /// Recording stand-in for the three OS seams.
    #[derive(Debug)]
    struct Fake {
        package: PathBuf,
        events: RefCell<Vec<Event>>,
        parent: Parent,
        /// The exit code of `msiexec`; `None`: it cannot be started.
        install_exit: Option<i32>,
        launch_fails: bool,
    }

    impl Fake {
        fn new(package: &Path) -> Self {
            Self {
                package: package.to_path_buf(),
                events: RefCell::new(Vec::new()),
                parent: Parent::Exits,
                install_exit: Some(0),
                launch_fails: false,
            }
        }

        fn events(&self) -> Vec<Event> {
            self.events.borrow().clone()
        }
    }

    impl Launcher for Fake {
        fn launch(&self, program: &Path, args: &[OsString]) -> io::Result<()> {
            self.events.borrow_mut().push(Event::Launch {
                program: program.to_path_buf(),
                args: args.to_vec(),
            });
            if self.launch_fails {
                Err(io::Error::other("cannot launch"))
            } else {
                Ok(())
            }
        }
    }

    impl CommandRunner for Fake {
        fn run(&self, program: &Path, args: &[OsString]) -> io::Result<i32> {
            self.events.borrow_mut().push(Event::Run {
                program: program.to_path_buf(),
                args: args.to_vec(),
                package_present: self.package.exists(),
            });
            self.install_exit
                .ok_or_else(|| io::Error::other("cannot run"))
        }
    }

    impl ProcessWaiter for Fake {
        fn wait_for_exit(&self, pid: u32, timeout: Duration) -> Result<ExitWait, WinError> {
            self.events.borrow_mut().push(Event::Wait { pid, timeout });
            match self.parent {
                Parent::Exits => Ok(ExitWait::Exited),
                Parent::Stays => Ok(ExitWait::TimedOut),
                Parent::CannotBeWaitedFor => Err(WinError::Invalid("cannot wait")),
            }
        }
    }

    /// A temp directory with the application's executable, an (empty) temp root and a package.
    struct Setup {
        _dir: tempfile::TempDir,
        app_exe: PathBuf,
        temp_root: PathBuf,
        package: PathBuf,
    }

    const EXE_BYTES: &[u8] = b"MZ pretend this is SSWriter.exe";

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let app_dir = dir.path().join("Programs").join("SS Writer");
        std::fs::create_dir_all(&app_dir).unwrap();
        let app_exe = app_dir.join("SSWriter.exe");
        std::fs::write(&app_exe, EXE_BYTES).unwrap();
        let temp_root = dir.path().join("Temp");
        std::fs::create_dir(&temp_root).unwrap();
        let package = dir.path().join("SSWriter-1.2.3-x64.msi");
        std::fs::write(&package, b"msi").unwrap();
        Setup {
            _dir: dir,
            app_exe,
            temp_root,
            package,
        }
    }

    impl Setup {
        fn flags(&self) -> UpdaterFlags {
            UpdaterFlags {
                installer: self.package.clone(),
                parent: 4242,
                target: self.app_exe.clone(),
            }
        }

        /// The names of the entries of the temp root.
        fn temp_entries(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(&self.temp_root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    fn os(parts: &[&OsString]) -> Vec<OsString> {
        parts.iter().map(|part| (*part).clone()).collect()
    }

    // ---- start ----

    #[test]
    fn start_copies_the_executable_and_launches_only_the_copy_with_the_updater_flags() {
        let setup = setup();
        let fake = Fake::new(&setup.package);

        start(
            &setup.package,
            &setup.app_exe,
            &setup.temp_root,
            4242,
            &RealFs,
            &fake,
        )
        .unwrap();

        let events = fake.events();
        let [Event::Launch { program, args }] = &events[..] else {
            panic!("exactly one launch expected, got {events:?}");
        };
        // The copy lives in a new folder below the temp root, and is a byte-for-byte copy.
        assert_ne!(program, &setup.app_exe);
        assert_eq!(program.file_name().unwrap(), UPDATER_FILE_NAME);
        let folder = program.parent().unwrap();
        assert_eq!(folder.parent().unwrap(), setup.temp_root);
        assert_eq!(std::fs::read(program).unwrap(), EXE_BYTES);
        // The installed executable and the package are untouched.
        assert_eq!(std::fs::read(&setup.app_exe).unwrap(), EXE_BYTES);
        assert!(setup.package.exists());
        // Exactly the three flags with their values.
        let expected = os(&[
            &"--apply-update".into(),
            &setup.package.clone().into_os_string(),
            &"--updater-parent".into(),
            &"4242".into(),
            &"--updater-target".into(),
            &setup.app_exe.clone().into_os_string(),
        ]);
        assert_eq!(args, &expected);
        // And the updater reads them back as what `start` meant.
        assert_eq!(Args::parse(args.clone()).updater, Some(setup.flags()));
    }

    #[test]
    fn start_uses_a_fresh_folder_each_time() {
        let setup = setup();
        let fake = Fake::new(&setup.package);
        for _ in 0..3 {
            start(
                &setup.package,
                &setup.app_exe,
                &setup.temp_root,
                4242,
                &RealFs,
                &fake,
            )
            .unwrap();
        }
        let programs: Vec<PathBuf> = fake
            .events()
            .into_iter()
            .map(|event| match event {
                Event::Launch { program, .. } => program,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let mut folders: Vec<&Path> = programs.iter().map(|p| p.parent().unwrap()).collect();
        folders.sort();
        folders.dedup();
        assert_eq!(folders.len(), 3);
        assert!(programs.iter().all(|program| program.is_file()));
        assert_eq!(setup.temp_entries().len(), 3);
    }

    #[test]
    fn start_reports_a_folder_that_cannot_be_created_and_launches_nothing() {
        let setup = setup();
        let fake = Fake::new(&setup.package);
        let fs = FaultFs::new();
        fs.fail(FsOp::CreateDir, FsFailure::AccessDenied, 1);

        let error = start(
            &setup.package,
            &setup.app_exe,
            &setup.temp_root,
            4242,
            &fs,
            &fake,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            UpdateApplyError::CreateFolder(FsError::AccessDenied(_))
        ));
        assert!(
            error
                .to_string()
                .starts_with("cannot create the updater folder: access denied")
        );
        assert!(fake.events().is_empty());
        assert!(setup.temp_entries().is_empty());
    }

    #[test]
    fn start_gives_up_when_every_folder_name_is_taken() {
        let setup = setup();
        let fake = Fake::new(&setup.package);
        let fs = FaultFs::new();
        fs.fail_always(FsOp::CreateDir, FsFailure::Code(win32::ALREADY_EXISTS));

        let error = start(
            &setup.package,
            &setup.app_exe,
            &setup.temp_root,
            4242,
            &fs,
            &fake,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            UpdateApplyError::CreateFolder(FsError::AlreadyExists(_))
        ));
        assert_eq!(fs.calls(FsOp::CreateDir), FOLDER_ATTEMPTS as usize);
        assert!(fake.events().is_empty());
    }

    #[test]
    fn start_removes_the_folder_when_the_executable_cannot_be_copied() {
        let setup = setup();
        let fake = Fake::new(&setup.package);
        let fs = FaultFs::new();
        fs.fail(FsOp::CopyFile, FsFailure::DiskFull, 1);

        let error = start(
            &setup.package,
            &setup.app_exe,
            &setup.temp_root,
            4242,
            &fs,
            &fake,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            UpdateApplyError::CopyExecutable(FsError::DiskFull(_))
        ));
        assert!(
            error
                .to_string()
                .starts_with("cannot copy the application for the updater: disk full")
        );
        assert!(fake.events().is_empty());
        assert!(setup.temp_entries().is_empty());
        assert_eq!(std::fs::read(&setup.app_exe).unwrap(), EXE_BYTES);
    }

    #[test]
    fn start_removes_the_copy_when_it_cannot_be_started() {
        let setup = setup();
        let mut fake = Fake::new(&setup.package);
        fake.launch_fails = true;

        let error = start(
            &setup.package,
            &setup.app_exe,
            &setup.temp_root,
            4242,
            &RealFs,
            &fake,
        )
        .unwrap_err();

        assert!(matches!(error, UpdateApplyError::Launch(_)));
        assert_eq!(error.to_string(), "cannot start the updater: cannot launch");
        // The copy was offered to the launcher, and nothing of it is left.
        assert_eq!(fake.events().len(), 1);
        assert!(setup.temp_entries().is_empty());
        assert_eq!(std::fs::read(&setup.app_exe).unwrap(), EXE_BYTES);
    }

    // ---- run_updater ----

    #[test]
    fn updater_waits_for_the_parent_then_installs_then_starts_the_target_and_deletes_the_msi() {
        let setup = setup();
        let fake = Fake::new(&setup.package);

        let code = run_updater(&setup.flags(), &fake, &fake);

        assert_eq!(code, EXIT_UPDATED);
        let events = fake.events();
        assert_eq!(events.len(), 3, "{events:?}");
        // 1. The parent first, with a bound of two minutes.
        assert_eq!(
            events[0],
            Event::Wait {
                pid: 4242,
                timeout: Duration::from_secs(120)
            }
        );
        // 2. The installer: msiexec of the system with the per-user setup arguments, while
        //    the package is still there.
        let Event::Run {
            program,
            args,
            package_present,
        } = &events[1]
        else {
            panic!("the second step must run msiexec: {events:?}");
        };
        assert!(program.is_absolute());
        assert!(program.ends_with(r"System32\msiexec.exe"), "{program:?}");
        assert_eq!(
            args,
            &os(&[
                &"/i".into(),
                &setup.package.clone().into_os_string(),
                &"/qb".into(),
                &"/norestart".into(),
            ])
        );
        assert!(package_present);
        // 3. The installed executable, without arguments, and the package is gone.
        assert_eq!(
            events[2],
            Event::Launch {
                program: setup.app_exe.clone(),
                args: Vec::new()
            }
        );
        assert!(!setup.package.exists());
    }

    #[test]
    fn updater_counts_the_restart_codes_1641_and_3010_as_installed() {
        for installed in [0, 1641, 3010] {
            let setup = setup();
            let mut fake = Fake::new(&setup.package);
            fake.install_exit = Some(installed);

            assert_eq!(
                run_updater(&setup.flags(), &fake, &fake),
                EXIT_UPDATED,
                "exit code {installed}"
            );
            let events = fake.events();
            assert!(
                matches!(events.last(), Some(Event::Launch { program, .. }) if program == &setup.app_exe),
                "{installed}: {events:?}"
            );
            assert!(!setup.package.exists(), "{installed}");
        }
    }

    #[test]
    fn updater_keeps_the_msi_and_starts_nothing_after_a_failed_install() {
        // 1602: cancelled by the user, 1603: fatal error, 1618: another setup is running,
        // 1625: blocked by policy; the codes next to the successes are failures too.
        for failure in [1, 2, 1602, 1603, 1618, 1625, 1640, 1642, 3009, 3011, -1] {
            let setup = setup();
            let mut fake = Fake::new(&setup.package);
            fake.install_exit = Some(failure);

            assert_eq!(
                run_updater(&setup.flags(), &fake, &fake),
                EXIT_UPDATE_INSTALL_FAILED,
                "exit code {failure}"
            );
            let events = fake.events();
            assert_eq!(events.len(), 2, "{failure}: {events:?}");
            assert!(matches!(events[1], Event::Run { .. }));
            assert!(setup.package.exists(), "{failure}");
        }
    }

    #[test]
    fn updater_reports_msiexec_that_cannot_be_started_like_a_failed_install() {
        let setup = setup();
        let mut fake = Fake::new(&setup.package);
        fake.install_exit = None;

        assert_eq!(
            run_updater(&setup.flags(), &fake, &fake),
            EXIT_UPDATE_INSTALL_FAILED
        );
        assert_eq!(fake.events().len(), 2);
        assert!(setup.package.exists());
    }

    #[test]
    fn updater_does_not_install_while_the_parent_is_still_running_after_the_bound() {
        for parent in [Parent::Stays, Parent::CannotBeWaitedFor] {
            let setup = setup();
            let mut fake = Fake::new(&setup.package);
            fake.parent = parent;

            assert_eq!(
                run_updater(&setup.flags(), &fake, &fake),
                EXIT_UPDATE_PARENT_RUNNING
            );
            // Only the wait happened: no install, no launch, the package stays.
            assert_eq!(
                fake.events(),
                [Event::Wait {
                    pid: 4242,
                    timeout: PARENT_EXIT_TIMEOUT
                }]
            );
            assert!(setup.package.exists());
        }
    }

    #[test]
    fn updater_deletes_the_msi_even_when_the_new_build_cannot_be_started() {
        let setup = setup();
        let mut fake = Fake::new(&setup.package);
        fake.launch_fails = true;

        assert_eq!(
            run_updater(&setup.flags(), &fake, &fake),
            EXIT_UPDATE_START_FAILED
        );
        assert_eq!(fake.events().len(), 3);
        assert!(!setup.package.exists());
    }

    #[test]
    fn updater_still_succeeds_when_the_msi_is_already_gone() {
        let setup = setup();
        let fake = Fake::new(&setup.package);
        std::fs::remove_file(&setup.package).unwrap();

        assert_eq!(run_updater(&setup.flags(), &fake, &fake), EXIT_UPDATED);
    }

    // ---- the real seams ----

    #[test]
    fn system_processes_report_a_program_that_does_not_exist() {
        let setup = setup();
        let missing = setup.temp_root.join("missing.exe");

        let launched = SystemProcesses.launch(&missing, &[]).unwrap_err();
        assert_eq!(launched.kind(), io::ErrorKind::NotFound);
        let run = SystemProcesses.run(&missing, &[]).unwrap_err();
        assert_eq!(run.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_exit_code_of_a_process_is_the_code_it_ended_with() {
        for code in [0_i32, 1603, 1641, 3010] {
            assert_eq!(exit_code(ExitStatus::from_raw(code.cast_unsigned())), code);
        }
    }

    #[test]
    fn system_processes_wait_for_a_process_until_it_ends_or_the_time_is_up() {
        // This test process is running: the wait runs out.
        assert_eq!(
            SystemProcesses
                .wait_for_exit(std::process::id(), Duration::from_millis(1))
                .unwrap(),
            ExitWait::TimedOut
        );
        // No process has this id: it is gone.
        assert_eq!(
            SystemProcesses
                .wait_for_exit(u32::MAX - 3, Duration::from_secs(30))
                .unwrap(),
            ExitWait::Exited
        );
    }

    #[test]
    fn msiexec_is_the_one_of_the_system_directory() {
        assert_eq!(
            msiexec_in(Some(OsString::from(r"D:\Win"))),
            PathBuf::from(r"D:\Win\System32\msiexec.exe")
        );
        assert_eq!(msiexec_in(None), PathBuf::from("msiexec.exe"));
    }
}
