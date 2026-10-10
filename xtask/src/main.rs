//! Project automation entry point: `cargo xtask <command>` (see [`cli::USAGE`]).

mod cli;
mod coverage;
mod dist;
mod docs_check;
mod error;
mod fsx;
mod hash;
mod metadata;
mod mutants;
mod notices;
mod package;
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
