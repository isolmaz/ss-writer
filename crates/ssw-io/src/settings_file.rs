//! Loading and saving `settings.json` (spec §10) through [`ssw_core::settings::Settings`].
//!
//! Keys this version does not know are preserved by the model. Writes are atomic (temp file in
//! the same folder, then rename over the target). A file that cannot be parsed is moved to
//! `settings.json.bak` and the defaults are used, so a hand-editing mistake never blocks startup
//! and never destroys the user's text.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use ssw_core::settings::{Settings, SettingsError};
use thiserror::Error;

/// Failure to read or write the settings file.
#[derive(Debug, Error)]
pub enum SettingsFileError {
    #[error("i/o error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

/// Result of [`load_settings`].
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedSettings {
    pub settings: Settings,
    /// Set when the file was unreadable as settings: the path its content was moved to.
    pub backup: Option<PathBuf>,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> SettingsFileError + '_ {
    move |source| SettingsFileError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Read the settings at `path`. A missing or empty file yields the defaults; a file that is not
/// valid UTF-8 / JSON of the settings shape is moved to `<path>.bak` (replacing an older
/// backup) and the defaults are returned with [`LoadedSettings::backup`] set. A leading byte
/// order mark is ignored.
///
/// # Errors
/// [`SettingsFileError::Io`] when the file cannot be read (other than missing) or a corrupt file
/// cannot be moved aside.
pub fn load_settings(path: &Path) -> Result<LoadedSettings, SettingsFileError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(source) => {
            return Err(SettingsFileError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let parsed = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| Settings::from_json_str(text.trim_start_matches('\u{feff}')).ok());
    match parsed {
        Some(settings) => Ok(LoadedSettings {
            settings,
            backup: None,
        }),
        None => {
            let backup = with_suffix(path, ".bak");
            std::fs::rename(path, &backup).map_err(io_error(path))?;
            Ok(LoadedSettings {
                settings: Settings::default(),
                backup: Some(backup),
            })
        }
    }
}

/// Write `settings` to `path` atomically, creating the folder when needed.
///
/// # Errors
/// [`SettingsFileError::Settings`] when serialization fails and [`SettingsFileError::Io`] when
/// the folder, the temporary file or the final rename fails (the temporary file is removed).
pub fn save_settings(path: &Path, settings: &Settings) -> Result<(), SettingsFileError> {
    write_atomic(path, settings.to_json_string_pretty())
}

/// Write `text` (or report why the settings could not be serialized, writing nothing) to `path`
/// through a temporary file. Split from [`save_settings`] so the serialization failure, which
/// the settings model cannot produce today, still has a tested path.
fn write_atomic(path: &Path, text: Result<String, SettingsError>) -> Result<(), SettingsFileError> {
    let text = text?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    let temp = with_suffix(path, ".tmp");
    // The file is closed when the closure returns, before the rename.
    std::fs::File::create(&temp)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())
                .and_then(|()| file.sync_all())
        })
        .map_err(io_error(&temp))?;
    std::fs::rename(&temp, path).map_err(|source| {
        // Best effort: the rename error is the one worth reporting.
        let _ = std::fs::remove_file(&temp);
        SettingsFileError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use serde::ser::Error as _;

    use super::*;

    #[test]
    fn a_serialization_failure_is_reported_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        let failure = SettingsError::Serialize(serde_json::Error::custom("not representable"));
        let error = write_atomic(&path, Err(failure)).unwrap_err();
        assert!(matches!(error, SettingsFileError::Settings(_)), "{error:?}");
        assert!(error.to_string().contains("not representable"));
        assert!(!path.exists());
        assert!(!with_suffix(&path, ".tmp").exists());
    }
}
