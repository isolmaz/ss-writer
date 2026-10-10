//! Command-line parsing for `cargo xtask`.

use crate::error::Error;

/// Default number of parallel cargo-mutants jobs.
pub const DEFAULT_MUTANTS_JOBS: u32 = 4;

/// Usage text printed for `--help` and after a usage error.
pub const USAGE: &str = "\
Usage: cargo xtask <command>

Commands:
  coverage                 cargo llvm-cov nextest over the workspace; fails below 100 % lines,
                           regions and functions; writes lcov.info and target/coverage-summary.txt
  mutants [-j N] [-- ARGS] run cargo-mutants (N parallel jobs, default 4; ARGS go to cargo-mutants)
                           and write target/mutants-report.md
  dist [--accept-wix-eula] [--expect-tag vX.Y.Z]
                           release build, then dist/: MSI, portable zip, SHA256SUMS.txt,
                           THIRD-PARTY-NOTICES.txt and winget manifests
  docs-check [--strict]    spec <-> keymap <-> features.toml <-> README <-> .ftl sync check;
                           --strict turns warnings (missing README lines) into failures
  help                     this text
";

/// A parsed `cargo xtask` invocation.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Print [`USAGE`].
    Help,
    /// `coverage`.
    Coverage,
    /// `mutants`.
    Mutants(MutantsArgs),
    /// `dist`.
    Dist(DistArgs),
    /// `docs-check`.
    DocsCheck {
        /// Fail on warnings too.
        strict: bool,
    },
}

/// Arguments of `mutants`.
#[derive(Debug, PartialEq, Eq)]
pub struct MutantsArgs {
    /// Parallel cargo-mutants jobs.
    pub jobs: u32,
    /// Extra arguments forwarded to cargo-mutants (everything after `--`).
    pub extra: Vec<String>,
}

/// Arguments of `dist`.
#[derive(Debug, PartialEq, Eq)]
pub struct DistArgs {
    /// Pass `-acceptEula wix7` to WiX (the WiX 7 OSMF EULA must be accepted by someone).
    pub accept_wix_eula: bool,
    /// Refuse to build unless this tag equals `v<workspace version>`.
    pub expect_tag: Option<String>,
}

/// Parses the arguments after `cargo xtask`.
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Command, Error> {
    let mut args = args.into_iter();
    let Some(name) = args.next() else {
        return Err(Error::Usage("missing command".to_owned()));
    };
    match name.as_str() {
        "help" | "-h" | "--help" => Ok(Command::Help),
        "coverage" => no_more_args(args).map(|()| Command::Coverage),
        "mutants" => parse_mutants(args),
        "dist" => parse_dist(args),
        "docs-check" => parse_docs_check(args),
        other => Err(Error::Usage(format!("unknown command `{other}`"))),
    }
}

fn unknown_argument(arg: &str) -> Error {
    Error::Usage(format!("unknown argument `{arg}`"))
}

fn no_more_args(mut args: impl Iterator<Item = String>) -> Result<(), Error> {
    match args.next() {
        Some(arg) => Err(unknown_argument(&arg)),
        None => Ok(()),
    }
}

/// Splits `--flag=value` into `("--flag", Some("value"))`.
fn split_flag(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((flag, value)) => (flag, Some(value)),
        None => (arg, None),
    }
}

/// The value of `flag`: the part after `=` or the next argument.
fn flag_value(
    flag: &str,
    inline: Option<&str>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String, Error> {
    match inline {
        Some(value) => Ok(value.to_owned()),
        None => args
            .next()
            .ok_or_else(|| Error::Usage(format!("`{flag}` needs a value"))),
    }
}

fn parse_mutants(mut args: impl Iterator<Item = String>) -> Result<Command, Error> {
    let mut jobs = DEFAULT_MUTANTS_JOBS;
    let mut extra = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            extra.extend(args.by_ref());
            break;
        }
        let (flag, inline) = split_flag(&arg);
        match flag {
            "--jobs" | "-j" => {
                let raw = flag_value(flag, inline, &mut args)?;
                jobs = raw.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| {
                    Error::Usage(format!("`{flag}` needs a positive integer, got `{raw}`"))
                })?;
            }
            _ => return Err(unknown_argument(&arg)),
        }
    }
    Ok(Command::Mutants(MutantsArgs { jobs, extra }))
}

fn parse_dist(mut args: impl Iterator<Item = String>) -> Result<Command, Error> {
    let mut accept_wix_eula = false;
    let mut expect_tag = None;
    while let Some(arg) = args.next() {
        let (flag, inline) = split_flag(&arg);
        match (flag, inline) {
            ("--accept-wix-eula", None) => accept_wix_eula = true,
            ("--expect-tag", _) => expect_tag = Some(flag_value(flag, inline, &mut args)?),
            _ => return Err(unknown_argument(&arg)),
        }
    }
    Ok(Command::Dist(DistArgs {
        accept_wix_eula,
        expect_tag,
    }))
}

fn parse_docs_check(args: impl Iterator<Item = String>) -> Result<Command, Error> {
    let mut strict = false;
    for arg in args {
        match arg.as_str() {
            "--strict" => strict = true,
            _ => return Err(unknown_argument(&arg)),
        }
    }
    Ok(Command::DocsCheck { strict })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command, Error> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    fn usage_message(result: Result<Command, Error>) -> String {
        match result {
            Err(Error::Usage(message)) => message,
            other => panic!("expected a usage error, got {other:?}"),
        }
    }

    #[test]
    fn simple_commands() {
        assert_eq!(parse_strs(&["coverage"]).unwrap(), Command::Coverage);
        assert_eq!(parse_strs(&["help"]).unwrap(), Command::Help);
        assert_eq!(parse_strs(&["--help"]).unwrap(), Command::Help);
        assert_eq!(
            parse_strs(&["docs-check"]).unwrap(),
            Command::DocsCheck { strict: false }
        );
        assert_eq!(
            parse_strs(&["docs-check", "--strict"]).unwrap(),
            Command::DocsCheck { strict: true }
        );
    }

    #[test]
    fn mutants_defaults_jobs_and_forwards_arguments_after_the_separator() {
        assert_eq!(
            parse_strs(&["mutants"]).unwrap(),
            Command::Mutants(MutantsArgs {
                jobs: DEFAULT_MUTANTS_JOBS,
                extra: vec![]
            })
        );
        assert_eq!(
            parse_strs(&["mutants", "--jobs", "2", "--", "-p", "ssw-core"]).unwrap(),
            Command::Mutants(MutantsArgs {
                jobs: 2,
                extra: vec!["-p".to_owned(), "ssw-core".to_owned()]
            })
        );
        assert_eq!(
            parse_strs(&["mutants", "-j=7"]).unwrap(),
            Command::Mutants(MutantsArgs {
                jobs: 7,
                extra: vec![]
            })
        );
    }

    #[test]
    fn mutants_rejects_bad_jobs() {
        assert!(usage_message(parse_strs(&["mutants", "--jobs", "0"])).contains("positive"));
        assert!(usage_message(parse_strs(&["mutants", "--jobs", "x"])).contains("`x`"));
        assert!(usage_message(parse_strs(&["mutants", "--jobs"])).contains("needs a value"));
        assert!(usage_message(parse_strs(&["mutants", "--bogus"])).contains("--bogus"));
    }

    #[test]
    fn dist_flags() {
        assert_eq!(
            parse_strs(&["dist"]).unwrap(),
            Command::Dist(DistArgs {
                accept_wix_eula: false,
                expect_tag: None
            })
        );
        assert_eq!(
            parse_strs(&["dist", "--accept-wix-eula", "--expect-tag", "v1.2.3"]).unwrap(),
            Command::Dist(DistArgs {
                accept_wix_eula: true,
                expect_tag: Some("v1.2.3".to_owned())
            })
        );
        assert_eq!(
            parse_strs(&["dist", "--expect-tag=v0.1.0"]).unwrap(),
            Command::Dist(DistArgs {
                accept_wix_eula: false,
                expect_tag: Some("v0.1.0".to_owned())
            })
        );
        assert!(usage_message(parse_strs(&["dist", "--accept-wix-eula=yes"])).contains("yes"));
    }

    #[test]
    fn unknown_and_missing_commands_and_stray_arguments() {
        assert!(usage_message(parse_strs(&[])).contains("missing command"));
        assert!(usage_message(parse_strs(&["screenshots"])).contains("screenshots"));
        assert!(usage_message(parse_strs(&["coverage", "--x"])).contains("--x"));
        assert!(usage_message(parse_strs(&["docs-check", "--loose"])).contains("--loose"));
    }
}
