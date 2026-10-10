//! The error type shared by every xtask command.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;

/// Why an xtask command failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file or directory operation failed.
    #[error("{}: {source}", .path.display())]
    Io {
        /// The path the operation was applied to.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// A tool could not be started (missing, not on `PATH`).
    #[error("cannot start `{program}`: {source}")]
    Spawn {
        /// The program name as given to the OS.
        program: String,
        /// The underlying error.
        source: io::Error,
    },
    /// A tool ran and reported failure.
    #[error("`{command}` failed ({status})")]
    CommandFailed {
        /// The command line, for the message.
        command: String,
        /// How it ended.
        status: ExitStatus,
    },
    /// A TOML file does not parse.
    #[error("{}: {source}", .path.display())]
    Toml {
        /// The offending file.
        path: PathBuf,
        /// The parse error (boxed: it is large and `Error` is returned everywhere).
        source: Box<toml::de::Error>,
    },
    /// `cargo metadata` output does not parse.
    #[error("cannot parse `cargo metadata` output: {0}")]
    Metadata(#[from] serde_json::Error),
    /// Writing the portable zip failed.
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// Bad command line; the message is followed by the usage text.
    #[error("{0}")]
    Usage(String),
    /// An input or precondition is wrong; the message is complete.
    #[error("{0}")]
    Invalid(String),
    /// A gate (coverage, mutants, docs sync) did not pass; the message is complete.
    #[error("{0}")]
    Failed(String),
}
