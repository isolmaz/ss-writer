//! Local rolling log (spec §4.6): `ssw.log` plus rotated `ssw.1.log` … files of at most 1 MB
//! each, five files in total. Nothing is ever sent anywhere.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fs::{FsError, io};

/// Log verbosity, from least to most verbose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Something failed.
    Error,
    /// Something unexpected but handled.
    Warn,
    /// Normal events.
    Info,
    /// Diagnostics.
    Debug,
}

impl Level {
    /// Fixed-width tag written to the log.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
        }
    }
}

/// Rotation settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// Maximum size of one file.
    pub max_bytes: u64,
    /// Number of files kept (current + rotated).
    pub files: usize,
    /// Most verbose level that is written.
    pub level: Level,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            files: 5,
            level: Level::Info,
        }
    }
}

/// Formats `time` as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC).
#[must_use]
pub fn format_timestamp(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let millis = since.subsec_millis();
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX / 2);
    let rest = seconds % 86_400;
    // Civil-from-days (Howard Hinnant): days since 1970-01-01 → year, month, day.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[derive(Debug)]
struct State {
    file: File,
    size: u64,
    level: Level,
}

/// A rolling file logger.
#[derive(Debug)]
pub struct Logger {
    dir: PathBuf,
    options: LogOptions,
    state: Mutex<State>,
}

impl Logger {
    /// Opens (creating the folder and file when needed) `ssw.log` in `dir`.
    ///
    /// # Errors
    /// Folder or file creation failures.
    pub fn new(dir: &Path, options: LogOptions) -> Result<Self, FsError> {
        io(std::fs::create_dir_all(dir), dir)?;
        let path = Self::file_path(dir, 0);
        let file = io(
            OpenOptions::new().create(true).append(true).open(&path),
            &path,
        )?;
        let size = io(file.metadata(), &path)?.len();
        Ok(Self {
            dir: dir.to_path_buf(),
            options,
            state: Mutex::new(State {
                file,
                size,
                level: options.level,
            }),
        })
    }

    fn file_path(dir: &Path, index: usize) -> PathBuf {
        if index == 0 {
            dir.join("ssw.log")
        } else {
            dir.join(format!("ssw.{index}.log"))
        }
    }

    /// Existing log files, newest first.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        (0..self.file_count())
            .map(|index| Self::file_path(&self.dir, index))
            .filter(|path| path.exists())
            .collect()
    }

    /// Changes the most verbose level that is written.
    pub fn set_level(&self, level: Level) {
        self.state().level = level;
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes one record. Logging never fails the caller: write errors are dropped.
    pub fn log(&self, level: Level, target: &str, message: &str) {
        let mut state = self.state();
        if level > state.level {
            return;
        }
        let line = format!(
            "{} {} {target}: {}\n",
            format_timestamp(SystemTime::now()),
            level.tag(),
            message.replace('\n', "\\n")
        );
        if state.size > 0 && state.size + line.len() as u64 > self.options.max_bytes {
            self.rotate(&mut state);
        }
        if state.file.write_all(line.as_bytes()).is_ok() {
            state.size += line.len() as u64;
        }
    }

    /// How many files are kept; at least the current one.
    fn file_count(&self) -> usize {
        self.options.files.max(1)
    }

    /// Shifts `ssw.N.log` → `ssw.N+1.log`, drops the oldest and starts a fresh `ssw.log`.
    fn rotate(&self, state: &mut State) {
        let last = self.file_count() - 1;
        if last == 0 {
            // Only the current file is kept: start it over in place. A second handle does the
            // truncation, so the open append handle stays valid and continues at offset 0.
            let current = Self::file_path(&self.dir, 0);
            if OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(current)
                .is_ok()
            {
                state.size = 0;
            }
            return;
        }
        let _ = std::fs::remove_file(Self::file_path(&self.dir, last));
        for index in (0..last).rev() {
            let _ = std::fs::rename(
                Self::file_path(&self.dir, index),
                Self::file_path(&self.dir, index + 1),
            );
        }
        let current = Self::file_path(&self.dir, 0);
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(current) {
            state.file = file;
            state.size = 0;
        }
    }
}
