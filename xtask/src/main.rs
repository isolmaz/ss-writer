//! Project automation entry point: `cargo xtask <command>` (see [`cli::USAGE`]).

// The `.res` writer of crates/ssw-app/build.rs: plain std code that the build script includes
// as a module. Its tests run here, against icon files this crate generates.
#[cfg(test)]
#[path = "../../crates/ssw-app/build_resources.rs"]
mod build_resources;
mod cli;
mod coverage;
mod dist;
mod docs_check;
mod error;
mod fsx;
mod hash;
mod icons;
mod metadata;
mod mutants;
mod notices;
mod package;
mod perf;
mod run;
mod winget;
mod wix;
mod workspace;

use std::process::ExitCode;

use cli::Command;
use error::Error;
use workspace::Workspace;

fn execute(args: impl Iterator<Item = String>) -> Result<(), Error> {
    match cli::parse(args)? {
        Command::Help => {
            print!("{}", cli::USAGE);
            Ok(())
        }
        Command::Coverage => coverage::run(&Workspace::locate()?),
        Command::Mutants(args) => mutants::run(&Workspace::locate()?, &args),
        Command::Dist(args) => dist::run(&Workspace::locate()?, &args),
        Command::DocsCheck { strict } => docs_check::run(&Workspace::locate()?, strict),
        Command::Icons => icons::run(&Workspace::locate()?),
        Command::Perf(args) => perf::run(&Workspace::locate()?, &args),
    }
}

fn main() -> ExitCode {
    match execute(std::env::args().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Error::Usage(message)) => {
            eprintln!("error: {message}\n\n{}", cli::USAGE);
            ExitCode::from(2)
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
