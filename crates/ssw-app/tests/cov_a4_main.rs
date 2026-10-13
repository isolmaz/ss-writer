//! The executable's own start: flags it refuses before anything is loaded, and a run that
//! records a performance report. Windows are never shown (`--test-hidden`) and every run uses
//! a profile in a temporary folder.

use std::process::Command;

fn executable() -> Command {
    Command::new(env!("CARGO_BIN_EXE_SSWriter"))
}

#[test]
fn the_updater_role_leaves_before_anything_is_created_and_installs_nothing_without_a_package() {
    let tmp = tempfile::tempdir().unwrap();
    // A process that already ended: the updater's wait for its parent is over at once. Its own
    // package is missing, so it stops before `msiexec` and shows nothing.
    let mut parent = Command::new("cmd")
        .args(["/c", "exit", "0"])
        .spawn()
        .unwrap();
    let parent_id = parent.id();
    parent.wait().unwrap();
    let output = executable()
        .arg("--apply-update")
        .arg(tmp.path().join("SSWriter-99.0.0-x64.msi"))
        .args(["--updater-parent", &parent_id.to_string()])
        .arg("--updater-target")
        .arg(tmp.path().join("SSWriter.exe"))
        .env("LOCALAPPDATA", tmp.path().join("local"))
        .env("APPDATA", tmp.path().join("roaming"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(11), "the install failed");
    assert!(
        !tmp.path().join("local").exists() && !tmp.path().join("roaming").exists(),
        "the updater creates no profile"
    );
}

#[test]
fn perf_flags_that_cannot_work_together_stop_the_start_with_the_reason() {
    let tmp = tempfile::tempdir().unwrap();
    let output = executable()
        .args(["--perf-scenario", "startup", "--perf-report"])
        .arg(tmp.path().join("report.json"))
        .env("LOCALAPPDATA", tmp.path().join("local"))
        .env("APPDATA", tmp.path().join("roaming"))
        .output()
        .unwrap();
    // A scenario without `--test-profile` would store a session in the user's profile.
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("SS Writer cannot start"), "{stderr}");
    assert!(stderr.contains("--test-profile"), "{stderr}");
    assert!(
        !tmp.path().join("local").exists(),
        "nothing is created before the flags are accepted"
    );
}

#[test]
fn a_report_run_enables_the_marks_and_starts_like_any_test_run() {
    let tmp = tempfile::tempdir().unwrap();
    let report = tmp.path().join("report.json");
    let status = executable()
        .args(["--test-hidden", "--test-exit-after-start", "--perf-report"])
        .arg(&report)
        .env("LOCALAPPDATA", tmp.path().join("local"))
        .env("APPDATA", tmp.path().join("roaming"))
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    assert!(
        tmp.path()
            .join("local")
            .join("SS Writer")
            .join("state.db")
            .is_file()
    );
}
