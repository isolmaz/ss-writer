//! Running external tools (cargo, cargo-llvm-cov, cargo-mutants, wix).

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};

use crate::error::Error;

/// The cargo that started xtask (so the pinned toolchain is kept), else `cargo` from `PATH`.
fn cargo_program() -> OsString {
    env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"))
}

/// `PATH` with `extra` directories appended (those already present are skipped).
/// Returns `current` unchanged when the result cannot be joined.
pub fn augmented_path(current: Option<OsString>, extra: &[PathBuf]) -> Option<OsString> {
    let mut dirs: Vec<PathBuf> = current
        .as_deref()
        .map(|p| env::split_paths(p).collect())
        .unwrap_or_default();
    for dir in extra {
        if !dirs.contains(dir) {
            dirs.push(dir.clone());
        }
    }
    match env::join_paths(dirs) {
        Ok(joined) => Some(joined),
        Err(_) => current,
    }
}

/// Directories that hold tools the build needs but that are not always on `PATH`:
/// the .NET host (WiX is a .NET tool) and the .NET global tools folder.
fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from(r"C:\Program Files\dotnet")];
    if let Some(profile) = env::var_os("USERPROFILE") {
        dirs.push(PathBuf::from(profile).join(".dotnet").join("tools"));
    }
    dirs.retain(|d| d.is_dir());
    dirs
}

/// A [`Command`] for `program` whose `PATH` includes [`tool_dirs`].
pub fn tool(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    if let Some(path) = augmented_path(env::var_os("PATH"), &tool_dirs()) {
        command.env("PATH", path);
    }
    command
}

/// A [`Command`] running the current cargo.
pub fn cargo() -> Command {
    tool(cargo_program())
}

fn describe(command: &Command) -> String {
    let mut text = command.get_program().to_string_lossy().into_owned();
    for arg in command.get_args() {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}

fn spawn_error(program: String) -> impl FnOnce(std::io::Error) -> Error {
    move |source| Error::Spawn { program, source }
}

fn program_name(command: &Command) -> String {
    command.get_program().to_string_lossy().into_owned()
}

/// Runs a command with inherited stdio and returns its exit status.
pub fn status(command: &mut Command) -> Result<ExitStatus, Error> {
    println!("> {}", describe(command));
    let program = program_name(command);
    command.status().map_err(spawn_error(program))
}

/// Runs a command with inherited stdio; a non-zero exit is an error.
pub fn run(command: &mut Command) -> Result<(), Error> {
    let exit = status(command)?;
    if exit.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: describe(command),
            status: exit,
        })
    }
}

/// Runs a command capturing stdout (stderr stays on the console); the exit status is returned
/// in the [`Output`], not turned into an error.
pub fn capture(command: &mut Command) -> Result<Output, Error> {
    println!("> {}", describe(command));
    let program = program_name(command);
    command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(spawn_error(program))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn split(path: &OsStr) -> Vec<PathBuf> {
        env::split_paths(path).collect()
    }

    #[test]
    fn extra_directories_are_appended_once() {
        let current = env::join_paths(dirs(&["a", "b"])).unwrap();
        let joined = augmented_path(Some(current), &dirs(&["b", "c", "c"])).unwrap();
        assert_eq!(split(&joined), dirs(&["a", "b", "c"]));
    }

    #[test]
    fn a_missing_path_becomes_just_the_extras() {
        let joined = augmented_path(None, &dirs(&["x"])).unwrap();
        assert_eq!(split(&joined), dirs(&["x"]));
    }

    #[test]
    fn an_unjoinable_directory_keeps_the_original_path() {
        let current = env::join_paths(dirs(&["a"])).unwrap();
        // Windows cannot join a directory that contains a double quote.
        let joined = augmented_path(Some(current.clone()), &dirs(&["has\"quote"]));
        assert_eq!(joined, Some(current));
    }

    #[test]
    fn captured_output_and_failures_are_reported() {
        let mut ok = tool(cargo_program());
        ok.arg("--version");
        let output = capture(&mut ok).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).starts_with("cargo "));

        let mut bad = tool(cargo_program());
        bad.arg("this-subcommand-does-not-exist");
        let err = run(&mut bad).unwrap_err();
        assert!(matches!(err, Error::CommandFailed { .. }), "{err}");

        let mut missing = tool("definitely-not-a-real-program-xtask");
        let err = run(&mut missing).unwrap_err();
        assert!(matches!(err, Error::Spawn { .. }), "{err}");
    }
}
