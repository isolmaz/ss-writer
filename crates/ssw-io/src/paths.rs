//! Locations of the app's data (architecture "Storage").
//!
//! Normal mode keeps state under `%LOCALAPPDATA%\SS Writer` and `settings.json` under
//! `%APPDATA%\SS Writer`. A file named `portable` next to the executable switches to portable
//! mode: everything lives in `<exe dir>\data\`. The environment is injected through
//! [`Environment`] so tests never touch the process environment.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Folder name below `%LOCALAPPDATA%` / `%APPDATA%`.
pub const APP_DIR_NAME: &str = "SS Writer";
/// Name of the marker file that enables portable mode.
pub const PORTABLE_MARKER: &str = "portable";
/// Data folder next to the executable in portable mode.
pub const PORTABLE_DATA_DIR: &str = "data";

/// Why app data locations could not be determined or created.
#[derive(Debug, Error)]
pub enum PathsError {
    #[error("environment variable {0} is not set")]
    MissingEnv(&'static str),
    #[error("cannot determine the executable location: {0}")]
    CurrentExe(#[source] io::Error),
    #[error("executable path {} has no parent folder", .0.display())]
    NoParent(PathBuf),
    #[error("cannot create folder {}: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The value of the process environment variable `name`.
fn process_var(name: &str) -> Option<OsString> {
    std::env::var_os(name)
}

/// The inputs that decide where data lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// `%LOCALAPPDATA%`.
    pub local_app_data: Option<PathBuf>,
    /// `%APPDATA%`.
    pub app_data: Option<PathBuf>,
    /// The folder containing the executable.
    pub exe_dir: PathBuf,
}

impl Environment {
    /// The real process environment and executable location.
    ///
    /// # Errors
    /// [`PathsError::CurrentExe`] / [`PathsError::NoParent`] when the executable path is
    /// unavailable.
    pub fn from_process() -> Result<Self, PathsError> {
        let exe = std::env::current_exe().map_err(PathsError::CurrentExe)?;
        Self::from_vars(process_var, &exe)
    }

    /// An environment read through `get` (variable name → value) for the executable at `exe`.
    ///
    /// # Errors
    /// [`PathsError::NoParent`] when `exe` has no parent folder.
    pub fn from_vars(
        get: impl Fn(&str) -> Option<OsString>,
        exe: &Path,
    ) -> Result<Self, PathsError> {
        let exe_dir = exe
            .parent()
            .ok_or_else(|| PathsError::NoParent(exe.to_path_buf()))?
            .to_path_buf();
        Ok(Self {
            local_app_data: get("LOCALAPPDATA").map(PathBuf::from),
            app_data: get("APPDATA").map(PathBuf::from),
            exe_dir,
        })
    }
}

/// Resolved data locations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppPaths {
    local: PathBuf,
    roaming: PathBuf,
    portable: bool,
}

impl AppPaths {
    /// Resolve the locations for `env`.
    ///
    /// # Errors
    /// [`PathsError::MissingEnv`] when not portable and `LOCALAPPDATA` / `APPDATA` is unset.
    pub fn resolve(env: &Environment) -> Result<Self, PathsError> {
        if env.exe_dir.join(PORTABLE_MARKER).is_file() {
            let data = env.exe_dir.join(PORTABLE_DATA_DIR);
            return Ok(Self {
                local: data.clone(),
                roaming: data,
                portable: true,
            });
        }
        let local = env
            .local_app_data
            .as_ref()
            .ok_or(PathsError::MissingEnv("LOCALAPPDATA"))?;
        let roaming = env
            .app_data
            .as_ref()
            .ok_or(PathsError::MissingEnv("APPDATA"))?;
        Ok(Self {
            local: local.join(APP_DIR_NAME),
            roaming: roaming.join(APP_DIR_NAME),
            portable: false,
        })
    }

    /// Resolve for the running process.
    ///
    /// # Errors
    /// See [`Environment::from_process`] and [`AppPaths::resolve`].
    pub fn from_process() -> Result<Self, PathsError> {
        Self::resolve(&Environment::from_process()?)
    }

    /// True when the `portable` marker was found next to the executable.
    #[must_use]
    pub fn is_portable(&self) -> bool {
        self.portable
    }

    /// Folder holding machine-local state (`%LOCALAPPDATA%\SS Writer`).
    #[must_use]
    pub fn local_dir(&self) -> &Path {
        &self.local
    }

    /// Folder holding `settings.json` (`%APPDATA%\SS Writer`).
    #[must_use]
    pub fn roaming_dir(&self) -> &Path {
        &self.roaming
    }

    /// The SQLite state database.
    #[must_use]
    pub fn state_db(&self) -> PathBuf {
        self.local.join("state.db")
    }

    /// The user settings file.
    #[must_use]
    pub fn settings_file(&self) -> PathBuf {
        self.roaming.join("settings.json")
    }

    /// Emergency-saved documents of crashed sessions.
    #[must_use]
    pub fn recovery_dir(&self) -> PathBuf {
        self.local.join("recovery")
    }

    /// Minidumps.
    #[must_use]
    pub fn crashes_dir(&self) -> PathBuf {
        self.local.join("crashes")
    }

    /// Rolling log files.
    #[must_use]
    pub fn logs_dir(&self) -> PathBuf {
        self.local.join("logs")
    }

    /// Disposable caches.
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.local.join("cache")
    }

    /// Downloaded remote images.
    #[must_use]
    pub fn image_cache_dir(&self) -> PathBuf {
        self.cache_dir().join("images")
    }

    /// Create every folder of the layout.
    ///
    /// # Errors
    /// [`PathsError::CreateDir`] for the first folder that cannot be created.
    pub fn ensure_dirs(&self) -> Result<(), PathsError> {
        for dir in [
            self.local.clone(),
            self.roaming.clone(),
            self.recovery_dir(),
            self.crashes_dir(),
            self.logs_dir(),
            self.image_cache_dir(),
        ] {
            std::fs::create_dir_all(&dir)
                .map_err(|source| PathsError::CreateDir { path: dir, source })?;
        }
        Ok(())
    }
}
