//! The command line (spec §5, §6): files and folders to open, `--new-window`, and the test flags
//! the integration tests launch `SSWriter.exe` with.
//!
//! Locations of settings and data (portable mode included) are decided by
//! [`ssw_io::paths::AppPaths`]; nothing here duplicates that.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use ssw_io::fs::FileSystem;
use ssw_io::paths::{Environment, PathsError};

/// `argument` as a path made absolute against the current directory (the instance that finally
/// uses it runs elsewhere).
fn absolute(argument: &OsStr) -> PathBuf {
    let path = PathBuf::from(argument);
    std::path::absolute(&path).unwrap_or(path)
}

/// Opens a new window even when files are given.
pub const NEW_WINDOW_FLAG: &str = "--new-window";
/// Test flag: the window stays hidden.
pub const TEST_HIDDEN_FLAG: &str = "--test-hidden";
/// Test flag: leave right after startup finished (devices ready, session and settings loaded).
pub const TEST_EXIT_AFTER_START_FLAG: &str = "--test-exit-after-start";
/// Test flag with a value: run the script file against the application (`ui::test_script`).
pub const TEST_SCRIPT_FLAG: &str = "--test-script";
/// Test flag with a value: keep settings and data below this folder instead of the user's.
pub const TEST_PROFILE_FLAG: &str = "--test-profile";
/// Test flag with a value: write the performance report (JSON, see [`crate::perf`]) of this run
/// to the file.
pub const PERF_REPORT_FLAG: &str = "--perf-report";
/// Test flag with a value: run the performance scenario of that name ([`PerfScenario::name`])
/// and exit.
pub const PERF_SCENARIO_FLAG: &str = "--perf-scenario";
/// Test flag with a value: the file or folder a scenario opens.
pub const PERF_TARGET_FLAG: &str = "--perf-target";
/// Test flag with a value: how many DIPs the scroll scenario moves per frame.
pub const PERF_SCROLL_STEP_FLAG: &str = "--perf-scroll-step";
/// Test flag: place the first window at (-20000, -20000) and show it without activating it, so
/// frames are really presented while nothing appears on the user's screen.
pub const PERF_OFFSCREEN_FLAG: &str = "--perf-offscreen";

/// A scripted performance run (`--perf-scenario`, see `ui::perf_driver`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PerfScenario {
    /// Start without a document and stop after the first content frame.
    Startup,
    /// Open the target document in the running application, then scroll through all of it.
    Open60k,
    /// Open the target folder in the sidebar, then build its index.
    Folder10k,
    /// Type characters into an untitled document (posted to the window's own queue) and time
    /// message receipt to `Present`.
    TypeLatency,
}

impl PerfScenario {
    /// Every scenario.
    pub const ALL: [Self; 4] = [
        Self::Startup,
        Self::Open60k,
        Self::Folder10k,
        Self::TypeLatency,
    ];

    /// The name used on the command line and in reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Open60k => "open-60k",
            Self::Folder10k => "folder-10k",
            Self::TypeLatency => "type-latency",
        }
    }

    /// The scenario called `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    /// Whether the scenario opens a file or folder given with `--perf-target`.
    #[must_use]
    pub fn needs_target(self) -> bool {
        matches!(self, Self::Open60k | Self::Folder10k)
    }
}

/// Flags that only the automated tests use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestFlags {
    /// `--test-hidden`: never show a window.
    pub hidden: bool,
    /// `--test-exit-after-start`: exit once startup is complete.
    pub exit_after_start: bool,
    /// `--test-script <file>`: run this script instead of the interactive shell.
    pub script: Option<PathBuf>,
    /// `--test-profile <dir>`: `<dir>\local` and `<dir>\roaming` replace `%LOCALAPPDATA%` and
    /// `%APPDATA%`.
    pub profile: Option<PathBuf>,
    /// `--perf-report <file>`: write the performance report of the run to this file.
    pub perf_report: Option<PathBuf>,
    /// `--perf-scenario <name>`: run this scripted performance scenario and exit.
    pub perf_scenario: Option<PerfScenario>,
    /// `--perf-target <path>`: the file (`open-60k`) or folder (`folder-10k`) of the scenario.
    pub perf_target: Option<PathBuf>,
    /// `--perf-scroll-step <dips>`: how far the scroll scenario moves per frame.
    pub perf_scroll_step: Option<u32>,
    /// `--perf-offscreen`: the first window lives off-screen, shown without activation.
    pub perf_offscreen: bool,
}

impl TestFlags {
    /// Whether any test flag is set. Test runs never take part in the single-instance
    /// handshake: they must not forward their launch to the user's running editor.
    #[must_use]
    pub fn any(&self) -> bool {
        self.hidden
            || self.exit_after_start
            || self.script.is_some()
            || self.profile.is_some()
            || self.perf_report.is_some()
            || self.perf_scenario.is_some()
            || self.perf_offscreen
    }

    /// Stores `value` of the flag `flag` that takes one. Returns false when the value is not
    /// usable (an unknown scenario, a step that is not a positive integer).
    fn set_valued(&mut self, flag: &str, value: &OsStr) -> bool {
        match flag {
            TEST_SCRIPT_FLAG => self.script = Some(absolute(value)),
            TEST_PROFILE_FLAG => self.profile = Some(absolute(value)),
            PERF_REPORT_FLAG => self.perf_report = Some(absolute(value)),
            PERF_TARGET_FLAG => self.perf_target = Some(absolute(value)),
            PERF_SCENARIO_FLAG => match PerfScenario::parse(&value.to_string_lossy()) {
                Some(scenario) => self.perf_scenario = Some(scenario),
                None => return false,
            },
            _ => match value.to_string_lossy().parse::<u32>() {
                Ok(step) if step > 0 => self.perf_scroll_step = Some(step),
                _ => return false,
            },
        }
        true
    }
}

/// What a launch asks to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchItem {
    /// A file (also a file that does not exist yet: it is created on first save).
    File(PathBuf),
    /// A folder to open as workspace.
    Folder(PathBuf),
}

impl LaunchItem {
    /// The path of the item.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::File(path) | Self::Folder(path) => path,
        }
    }
}

/// What one launch of the executable (or a forwarded later launch) asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Launch {
    /// Files and folders in command-line order.
    pub items: Vec<LaunchItem>,
    /// Open a new window instead of a tab in the last active window.
    pub new_window: bool,
}

impl Launch {
    /// The files of the launch.
    pub fn files(&self) -> impl Iterator<Item = &Path> {
        self.items.iter().filter_map(|item| match item {
            LaunchItem::File(path) => Some(path.as_path()),
            LaunchItem::Folder(_) => None,
        })
    }

    /// The folders of the launch.
    pub fn folders(&self) -> impl Iterator<Item = &Path> {
        self.items.iter().filter_map(|item| match item {
            LaunchItem::Folder(path) => Some(path.as_path()),
            LaunchItem::File(_) => None,
        })
    }

    /// Whether the launch asks for nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && !self.new_window
    }
}

/// The parsed command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    /// Paths as given (made absolute), in order.
    pub paths: Vec<PathBuf>,
    /// `--new-window`.
    pub new_window: bool,
    /// Test flags.
    pub test: TestFlags,
    /// Options starting with `--` that this version does not know; they are ignored (a shell
    /// verb or a newer installer may pass more than this version understands).
    pub ignored: Vec<String>,
}

impl Args {
    /// Parses the arguments after the program name. `--` ends the options; every argument
    /// without a leading `--` is a path, made absolute against the current directory (the
    /// instance that finally opens it runs elsewhere).
    pub fn parse<I>(args: I) -> Self
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut parsed = Self::default();
        let mut options_ended = false;
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            if argument.is_empty() {
                continue;
            }
            let text = argument.to_string_lossy();
            if !options_ended && text == "--" {
                options_ended = true;
            } else if !options_ended && text.starts_with("--") {
                match &*text {
                    NEW_WINDOW_FLAG => parsed.new_window = true,
                    TEST_HIDDEN_FLAG => parsed.test.hidden = true,
                    TEST_EXIT_AFTER_START_FLAG => parsed.test.exit_after_start = true,
                    PERF_OFFSCREEN_FLAG => parsed.test.perf_offscreen = true,
                    TEST_SCRIPT_FLAG
                    | TEST_PROFILE_FLAG
                    | PERF_REPORT_FLAG
                    | PERF_SCENARIO_FLAG
                    | PERF_TARGET_FLAG
                    | PERF_SCROLL_STEP_FLAG => {
                        match args.next() {
                            Some(value) => {
                                if !parsed.test.set_valued(&text, &value) {
                                    // An unusable value: kept visible as `--flag=value`.
                                    parsed
                                        .ignored
                                        .push(format!("{text}={}", value.to_string_lossy()));
                                }
                            }
                            // A valued flag at the end of the line has no value to take.
                            None => parsed.ignored.push(text.to_string()),
                        }
                    }
                    other => parsed.ignored.push(other.to_owned()),
                }
            } else {
                parsed.paths.push(absolute(&argument));
            }
        }
        parsed
    }

    /// The environment that decides where settings and data live: the process's, or the one
    /// below `--test-profile` (which also hides a `portable` marker next to the executable, so
    /// a test never touches the real data).
    ///
    /// # Errors
    /// [`PathsError`] when the executable location is unavailable.
    pub fn environment(&self) -> Result<Environment, PathsError> {
        Environment::from_process().map(|mut env| {
            if let Some(profile) = &self.test.profile {
                env.local_app_data = Some(profile.join("local"));
                env.app_data = Some(profile.join("roaming"));
                env.exe_dir = profile.join("exe");
            }
            env
        })
    }

    /// The environment this launch starts with, or why it must not start: the `--perf-*` flags
    /// that cannot work together ([`Args::perf_problem`]) are refused first, then a process whose
    /// executable location is unavailable (`environment`, the outcome of
    /// [`Args::environment`]). Both refusals end the process the same way, so the executable has
    /// one place that says "cannot start".
    ///
    /// # Errors
    /// The reason, worded for the message the user reads.
    pub fn startup_environment(
        &self,
        environment: Result<Environment, PathsError>,
    ) -> Result<Environment, String> {
        match self.perf_problem() {
            Some(problem) => Err(problem),
            None => environment.map_err(|error| error.to_string()),
        }
    }

    /// What is wrong with the `--perf-*` flags, if anything: an unusable value, a scenario or an
    /// off-screen window without `--test-profile` (they change the session the application
    /// stores, which must never be the user's), a scenario without `--perf-report`, a scenario
    /// that opens something without `--perf-target`.
    #[must_use]
    pub fn perf_problem(&self) -> Option<String> {
        let test = &self.test;
        if let Some(bad) = self
            .ignored
            .iter()
            .find(|option| option.starts_with("--perf-"))
        {
            return Some(format!("`{bad}` is not usable"));
        }
        if (test.perf_scenario.is_some() || test.perf_offscreen) && test.profile.is_none() {
            return Some(format!(
                "{PERF_SCENARIO_FLAG} and {PERF_OFFSCREEN_FLAG} need {TEST_PROFILE_FLAG}"
            ));
        }
        let scenario = test.perf_scenario?;
        if test.perf_report.is_none() {
            return Some(format!("{PERF_SCENARIO_FLAG} needs {PERF_REPORT_FLAG}"));
        }
        if scenario.needs_target() && test.perf_target.is_none() {
            return Some(format!(
                "{PERF_SCENARIO_FLAG} {} needs {PERF_TARGET_FLAG}",
                scenario.name()
            ));
        }
        None
    }

    /// Splits the paths into files and folders, asking `fs`. A path that does not exist is a
    /// file (it is created by the first save).
    #[must_use]
    pub fn launch(&self, fs: &dyn FileSystem) -> Launch {
        Launch {
            items: self
                .paths
                .iter()
                .map(|path| {
                    if fs.metadata(path).is_ok_and(|meta| meta.is_dir()) {
                        LaunchItem::Folder(path.clone())
                    } else {
                        LaunchItem::File(path.clone())
                    }
                })
                .collect(),
            new_window: self.new_window,
        }
    }
}

#[cfg(test)]
mod tests {
    use ssw_io::fs::RealFs;

    use super::*;

    fn parse(args: &[&str]) -> Args {
        Args::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn options_and_paths_are_separated() {
        let args = parse(&[
            "--new-window",
            "notes.md",
            "--test-hidden",
            "--test-exit-after-start",
            "--frobnicate",
            "",
        ]);
        assert!(args.new_window);
        assert_eq!(
            args.test,
            TestFlags {
                hidden: true,
                exit_after_start: true,
                ..TestFlags::default()
            }
        );
        assert!(args.test.any());
        assert_eq!(args.ignored, ["--frobnicate"]);
        assert_eq!(args.paths.len(), 1);
        assert!(args.paths[0].is_absolute());
        assert!(args.paths[0].ends_with("notes.md"));
    }

    #[test]
    fn script_and_profile_flags_take_the_next_argument() {
        let args = parse(&[
            "--test-script",
            "run.txt",
            "a.md",
            "--test-profile",
            "profile",
        ]);
        let script = args.test.script.as_deref().unwrap();
        assert!(script.is_absolute());
        assert!(script.ends_with("run.txt"));
        let profile = args.test.profile.as_deref().unwrap();
        assert!(profile.is_absolute());
        assert!(profile.ends_with("profile"));
        assert_eq!(args.paths.len(), 1, "the values are not paths to open");
        assert!(args.ignored.is_empty());
        assert!(args.test.any());
        for flag in ["--test-script", "--test-profile"] {
            let only = parse(&[flag]);
            assert_eq!(only.ignored, [flag], "no value to take");
            assert_eq!(only.test, TestFlags::default());
        }
        let script_only = parse(&["--test-script", "x.txt"]);
        assert!(script_only.test.any());
        let profile_only = parse(&["--test-profile", "p"]);
        assert!(profile_only.test.any());
    }

    #[test]
    fn a_test_profile_moves_every_data_location_below_it() {
        let tmp = tempfile::tempdir().unwrap();
        let args = Args {
            test: TestFlags {
                profile: Some(tmp.path().to_path_buf()),
                ..TestFlags::default()
            },
            ..Args::default()
        };
        let env = args.environment().unwrap();
        assert_eq!(env.local_app_data, Some(tmp.path().join("local")));
        assert_eq!(env.app_data, Some(tmp.path().join("roaming")));
        let paths = ssw_io::paths::AppPaths::resolve(&env).unwrap();
        assert!(!paths.is_portable());
        assert_eq!(
            paths.settings_file(),
            tmp.path()
                .join("roaming")
                .join("SS Writer")
                .join("settings.json")
        );
        // Without a profile the process environment is used unchanged.
        let plain = Args::default().environment().unwrap();
        assert_eq!(plain, Environment::from_process().unwrap());
    }

    #[test]
    fn double_dash_ends_the_options() {
        let args = parse(&["--", "--new-window", "a.md"]);
        assert!(!args.new_window);
        assert!(args.ignored.is_empty());
        assert_eq!(args.paths.len(), 2);
        assert!(args.paths[0].ends_with("--new-window"));
    }

    #[test]
    fn no_arguments_means_nothing_to_open() {
        let args = parse(&[]);
        assert_eq!(args, Args::default());
        assert!(!args.test.any());
        assert!(args.launch(&RealFs).is_empty());
    }

    #[test]
    fn launch_splits_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.md");
        std::fs::write(&file, "x").unwrap();
        let missing = tmp.path().join("new.md");
        let folder = tmp.path().to_path_buf();
        let args = Args {
            paths: vec![file.clone(), folder.clone(), missing.clone()],
            new_window: true,
            ..Args::default()
        };
        let launch = args.launch(&RealFs);
        assert!(launch.new_window);
        assert!(!launch.is_empty());
        assert_eq!(
            launch.files().collect::<Vec<_>>(),
            [file.as_path(), missing.as_path()]
        );
        assert_eq!(launch.folders().collect::<Vec<_>>(), [folder.as_path()]);
        assert_eq!(launch.items[0].path(), file.as_path());
        assert_eq!(launch.items[1].path(), folder.as_path());
        assert!(Launch::default().is_empty());
        assert!(
            !Launch {
                new_window: true,
                ..Launch::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn scenarios_are_found_by_their_names() {
        for scenario in PerfScenario::ALL {
            assert_eq!(PerfScenario::parse(scenario.name()), Some(scenario));
        }
        assert_eq!(PerfScenario::parse("open-60k"), Some(PerfScenario::Open60k));
        assert_eq!(PerfScenario::parse("Open-60k"), None);
        assert_eq!(PerfScenario::parse(""), None);
        let targets: Vec<bool> = PerfScenario::ALL
            .into_iter()
            .map(PerfScenario::needs_target)
            .collect();
        assert_eq!(targets, [false, true, true, false]);
    }

    #[test]
    fn perf_flags_take_their_values_and_count_as_test_flags() {
        let args = parse(&[
            "--perf-scenario",
            "folder-10k",
            "--perf-report",
            "out.json",
            "--perf-target",
            "tree",
            "--perf-scroll-step",
            "250",
            "--perf-offscreen",
            "doc.md",
        ]);
        let test = &args.test;
        assert_eq!(test.perf_scenario, Some(PerfScenario::Folder10k));
        assert!(test.perf_report.as_deref().unwrap().is_absolute());
        assert!(test.perf_report.as_deref().unwrap().ends_with("out.json"));
        assert!(test.perf_target.as_deref().unwrap().ends_with("tree"));
        assert_eq!(test.perf_scroll_step, Some(250));
        assert!(test.perf_offscreen);
        assert!(args.ignored.is_empty());
        assert_eq!(args.paths.len(), 1, "the values are not paths to open");
        // Each of them alone already makes this a test run.
        for single in [
            &["--perf-scenario", "startup"][..],
            &["--perf-report", "r.json"][..],
            &["--perf-offscreen"][..],
        ] {
            assert!(parse(single).test.any(), "{single:?}");
        }
        // The target and the step only parameterize a scenario.
        assert!(!parse(&["--perf-target", "x"]).test.any());
        assert!(!parse(&["--perf-scroll-step", "9"]).test.any());
    }

    #[test]
    fn unusable_perf_values_are_ignored_but_visible() {
        let args = parse(&[
            "--perf-scenario",
            "typo",
            "--perf-scroll-step",
            "0",
            "--perf-scroll-step",
            "fast",
            "--perf-report",
        ]);
        assert_eq!(
            args.ignored,
            [
                "--perf-scenario=typo",
                "--perf-scroll-step=0",
                "--perf-scroll-step=fast",
                "--perf-report"
            ]
        );
        assert_eq!(args.test, TestFlags::default());
        assert!(
            args.perf_problem()
                .unwrap()
                .contains("`--perf-scenario=typo` is not usable")
        );
    }

    #[test]
    fn perf_flags_are_checked_for_a_complete_and_safe_combination() {
        assert_eq!(parse(&[]).perf_problem(), None);
        assert_eq!(parse(&["--perf-report", "r.json"]).perf_problem(), None);
        let message = |args: &[&str]| parse(args).perf_problem().unwrap();
        // They would store a session: only with a throw-away profile.
        assert!(message(&["--perf-offscreen"]).contains("--test-profile"));
        assert!(
            message(&["--perf-scenario", "startup", "--perf-report", "r"])
                .contains("--test-profile")
        );
        let with_profile = ["--test-profile", "p"];
        let build = |extra: &[&str]| {
            let mut all: Vec<&str> = with_profile.to_vec();
            all.extend_from_slice(extra);
            parse(&all).perf_problem()
        };
        assert_eq!(build(&["--perf-offscreen"]), None);
        assert_eq!(
            build(&["--perf-scenario", "startup"]),
            Some("--perf-scenario needs --perf-report".to_owned())
        );
        assert_eq!(
            build(&["--perf-scenario", "startup", "--perf-report", "r"]),
            None
        );
        assert_eq!(
            build(&["--perf-scenario", "open-60k", "--perf-report", "r"]),
            Some("--perf-scenario open-60k needs --perf-target".to_owned())
        );
        assert_eq!(
            build(&[
                "--perf-scenario",
                "open-60k",
                "--perf-report",
                "r",
                "--perf-target",
                "t"
            ]),
            None
        );
    }
}
