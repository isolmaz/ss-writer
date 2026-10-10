//! SS Writer application binary.
//!
//! Runs startup, then the visible shell ([`UiShell`]): windows, tabs, editor and panels.
//! `--test-exit-after-start` waits for the graphics device and exits 0; `--test-script` runs the
//! scripted harness instead of the windows.

use std::process::ExitCode;

use ssw_app::app::{EXIT_STARTUP_FAILED, run};
use ssw_app::args::Args;
use ssw_app::i18n::localizer_for;
use ssw_app::startup::{InstanceHook, ProcessEnv};
use ssw_app::ui::host::{UiShell, WinInstance};
use ssw_app::ui::scripted_services::ScriptedServices;
use ssw_app::ui::test_script::ScriptHost;
use ssw_win::gfx::Driver;
use ssw_win::locale::ui_language;
use ssw_win::services::dialogs::{DialogStrings, WindowsDialogs};
use ssw_win::window::current_os_version;

fn main() -> ExitCode {
    let args = Args::parse(std::env::args_os().skip(1));
    if let Some(problem) = args.perf_problem() {
        eprintln!("SS Writer cannot start: {problem}");
        return ExitCode::from(u8::try_from(EXIT_STARTUP_FAILED).unwrap_or(1));
    }
    if args.test.perf_report.is_some() {
        // Time zero is the creation of the process; the marks start here.
        ssw_win::perf::enable();
        ssw_win::perf::mark(ssw_app::perf::MAIN_ENTERED);
    }
    let os_language = ui_language();
    let i18n = localizer_for(&os_language);
    let mut dialogs = WindowsDialogs::new(
        None,
        DialogStrings {
            save: i18n.text("app-dialog-save"),
            dont_save: i18n.text("app-dialog-dont-save"),
        },
    );
    let env = match args.environment() {
        Ok(env) => env,
        Err(error) => {
            eprintln!("SS Writer cannot start: {error}");
            return ExitCode::from(u8::try_from(EXIT_STARTUP_FAILED).unwrap_or(1));
        }
    };
    let process = ProcessEnv {
        os: current_os_version(),
        env,
        os_language,
        driver: Driver::Hardware,
        install_crash_handler: true,
    };
    let code = if args.test.script.is_some() {
        // `--test-script`: the scripted harness (no window, no dialog is ever shown).
        let mut scripted = ScriptedServices::echoing().dialogs();
        run(&args, process, &mut *scripted, None, &mut ScriptHost)
    } else {
        // Test runs never take part in the single-instance handshake (they must not forward
        // their launch to the user's running editor).
        let single = !args.test.any();
        let mut instance = WinInstance::new();
        let mut shell = UiShell::new(single.then(|| instance.link()));
        let hook = single.then_some(&mut instance as &mut dyn InstanceHook);
        run(&args, process, &mut dialogs, hook, &mut shell)
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
