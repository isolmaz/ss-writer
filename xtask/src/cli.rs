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
  icons                    rasterize assets/icons/*.svg into the multi-size assets/icons/*.ico
                           files that crates/ssw-app/build.rs embeds into SSWriter.exe
  perf [--runs N] [--write-docs]
                           release build, fixtures in target/perf, every performance scenario N times
                           (default 5, off-screen window, fresh profile per run), report in
                           target/perf/report.md; --write-docs fills docs/performance.md too
  screenshots [--scale N] [--only NAME]... [--no-build]
                           release build, demo documents and scenario scripts in target/screens,
                           every scenario run hidden in the light and dark theme (offscreen
                           rendering, nothing is shown), PNG files in docs/screenshots;
                           --scale 2 for hi-DPI (default 1)
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
    /// `icons`.
    Icons,
    /// `perf`.
    Perf(PerfArgs),
    /// `screenshots`.
    Screenshots(ScreenshotArgs),
}

/// Arguments of `screenshots`.
#[derive(Debug, PartialEq, Eq)]
pub struct ScreenshotArgs {
    /// The DPI scale in percent (100 is 96 DPI).
    pub scale_percent: u32,
    /// Only these scenarios (all when empty).
    pub only: Vec<String>,
    /// Use the release build that is there.
    pub no_build: bool,
}

/// Arguments of `perf`.
#[derive(Debug, PartialEq, Eq)]
pub struct PerfArgs {
    /// Runs of every scenario.
    pub runs: usize,
    /// Replace the results section of `docs/performance.md`.
    pub write_docs: bool,
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
        "icons" => no_more_args(args).map(|()| Command::Icons),
        "perf" => parse_perf(args),
        "screenshots" => parse_screenshots(args),
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

/// Runs of every scenario of `perf` unless `--runs` says otherwise.
pub const DEFAULT_PERF_RUNS: usize = 5;

fn parse_perf(mut args: impl Iterator<Item = String>) -> Result<Command, Error> {
    let mut runs = DEFAULT_PERF_RUNS;
    let mut write_docs = false;
    while let Some(arg) = args.next() {
        let (flag, inline) = split_flag(&arg);
        match (flag, inline) {
            ("--write-docs", None) => write_docs = true,
            ("--runs", _) => {
                let raw = flag_value(flag, inline, &mut args)?;
                runs = raw
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| {
                        Error::Usage(format!("`{flag}` needs a positive integer, got `{raw}`"))
                    })?;
            }
            _ => return Err(unknown_argument(&arg)),
        }
    }
    Ok(Command::Perf(PerfArgs { runs, write_docs }))
}

fn parse_screenshots(mut args: impl Iterator<Item = String>) -> Result<Command, Error> {
    let mut parsed = ScreenshotArgs {
        scale_percent: 100,
        only: Vec::new(),
        no_build: false,
    };
    while let Some(arg) = args.next() {
        let (flag, inline) = split_flag(&arg);
        match (flag, inline) {
            ("--no-build", None) => parsed.no_build = true,
            ("--only", _) => parsed.only.push(flag_value(flag, inline, &mut args)?),
            ("--scale", _) => {
                let raw = flag_value(flag, inline, &mut args)?;
                parsed.scale_percent = raw
                    .parse::<f32>()
                    .ok()
                    .filter(|scale| (0.5..=4.0).contains(scale))
                    .map(|scale| (scale * 100.0).round() as u32)
                    .ok_or_else(|| {
                        Error::Usage(format!(
                            "`{flag}` needs a number from 0.5 to 4, got `{raw}`"
                        ))
                    })?;
            }
            _ => return Err(unknown_argument(&arg)),
        }
    }
    Ok(Command::Screenshots(parsed))
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
        assert_eq!(parse_strs(&["icons"]).unwrap(), Command::Icons);
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
    fn perf_defaults_to_five_runs_and_takes_runs_and_write_docs() {
        assert_eq!(
            parse_strs(&["perf"]).unwrap(),
            Command::Perf(PerfArgs {
                runs: DEFAULT_PERF_RUNS,
                write_docs: false
            })
        );
        assert_eq!(
            parse_strs(&["perf", "--runs", "2", "--write-docs"]).unwrap(),
            Command::Perf(PerfArgs {
                runs: 2,
                write_docs: true
            })
        );
        assert_eq!(
            parse_strs(&["perf", "--runs=9"]).unwrap(),
            Command::Perf(PerfArgs {
                runs: 9,
                write_docs: false
            })
        );
        for bad in [
            &["perf", "--runs", "0"][..],
            &["perf", "--runs", "x"][..],
            &["perf", "--runs"][..],
            &["perf", "--bogus"][..],
            &["perf", "--write-docs=1"][..],
        ] {
            assert!(matches!(parse_strs(bad), Err(Error::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn screenshots_take_scale_scenarios_and_no_build() {
        assert_eq!(
            parse_strs(&["screenshots"]).unwrap(),
            Command::Screenshots(ScreenshotArgs {
                scale_percent: 100,
                only: vec![],
                no_build: false
            })
        );
        assert_eq!(
            parse_strs(&[
                "screenshots",
                "--scale",
                "2",
                "--only",
                "settings",
                "--only=zen-focus",
                "--no-build"
            ])
            .unwrap(),
            Command::Screenshots(ScreenshotArgs {
                scale_percent: 200,
                only: vec!["settings".to_owned(), "zen-focus".to_owned()],
                no_build: true
            })
        );
        assert_eq!(
            parse_strs(&["screenshots", "--scale=1.5"]).unwrap(),
            Command::Screenshots(ScreenshotArgs {
                scale_percent: 150,
                only: vec![],
                no_build: false
            })
        );
        for bad in [
            &["screenshots", "--scale", "9"][..],
            &["screenshots", "--scale", "x"][..],
            &["screenshots", "--scale"][..],
            &["screenshots", "--only"][..],
            &["screenshots", "--no-build=1"][..],
            &["screenshots", "--bogus"][..],
        ] {
            assert!(matches!(parse_strs(bad), Err(Error::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn unknown_and_missing_commands_and_stray_arguments() {
        assert!(usage_message(parse_strs(&[])).contains("missing command"));
        assert!(usage_message(parse_strs(&["frobnicate"])).contains("frobnicate"));
        assert!(usage_message(parse_strs(&["coverage", "--x"])).contains("--x"));
        assert!(usage_message(parse_strs(&["icons", "--size"])).contains("--size"));
        assert!(usage_message(parse_strs(&["docs-check", "--loose"])).contains("--loose"));
    }
}
