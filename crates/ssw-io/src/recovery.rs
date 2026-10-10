//! Crash recovery (spec §4.4, §4.6): periodic backups of untitled buffers, emergency saves of
//! dirty documents into `recovery\` with a JSON manifest, and list / restore / discard on the
//! next launch. Nothing here touches the user's own files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::encoding::TextFormat;
use crate::fs::{FileSystem, FsError};
use crate::save::{SaveError, ThreadSleeper, save_atomic};

/// Untitled buffers are backed up this often (the caller drives the tick).
pub const UNTITLED_BACKUP_INTERVAL: Duration = Duration::from_secs(30);
/// File name of the manifest inside the recovery folder.
pub const MANIFEST_NAME: &str = "manifest.json";
const MANIFEST_VERSION: u32 = 1;
const MAX_ID_LEN: usize = 64;

/// Rate limiter for untitled-buffer backups: ask [`BackupTimer::due`] on every tick.
#[derive(Clone, Copy, Debug, Default)]
pub struct BackupTimer {
    last: Option<Instant>,
}

impl BackupTimer {
    /// A timer that is due immediately.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` (and restarts the interval) when at least [`UNTITLED_BACKUP_INTERVAL`] has passed
    /// since the last time it returned `true`.
    pub fn due(&mut self, now: Instant) -> bool {
        match self.last {
            Some(last) if now.saturating_duration_since(last) < UNTITLED_BACKUP_INTERVAL => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_millis() -> u64 {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// What a recovery entry protects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryKind {
    /// A buffer that was never saved.
    Untitled,
    /// A file with unsaved changes (the file on disk is untouched).
    Dirty,
}

/// One recoverable document as listed in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryEntry {
    /// Stable id chosen by the caller (`[A-Za-z0-9_-]`, up to 64 characters).
    pub id: String,
    /// Untitled buffer or dirty file.
    pub kind: RecoveryKind,
    /// The file the text belongs to, for dirty documents.
    pub original_path: Option<PathBuf>,
    /// Name shown in the recovery list.
    pub display_name: String,
    /// When the backup was written (Unix milliseconds).
    pub saved_at_ms: u64,
    /// Encoding, BOM and line endings to use when the text is saved again.
    pub format: TextFormat,
    /// Name of the data file next to the manifest.
    pub data_file: String,
}

/// A document handed to the store.
#[derive(Clone, Copy, Debug)]
pub struct RecoveryDoc<'a> {
    /// Stable id (see [`RecoveryEntry::id`]).
    pub id: &'a str,
    /// Untitled buffer or dirty file.
    pub kind: RecoveryKind,
    /// The file the text belongs to, for dirty documents.
    pub original_path: Option<&'a Path>,
    /// Name shown in the recovery list.
    pub display_name: &'a str,
    /// Document text (`\n` line endings).
    pub text: &'a str,
    /// Format to restore on save.
    pub format: TextFormat,
}

/// A recovered document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredDoc {
    /// The manifest entry.
    pub entry: RecoveryEntry,
    /// The recovered text.
    pub text: String,
}

/// Outcome of [`RecoveryStore::emergency_save`].
#[derive(Debug, Default)]
pub struct EmergencyReport {
    /// Ids that were saved.
    pub saved: Vec<String>,
    /// Ids that could not be saved, with the reason.
    pub failed: Vec<(String, RecoveryError)>,
}

/// Recovery failures.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    /// The id is empty, too long or contains characters outside `[A-Za-z0-9_-]`.
    #[error("invalid recovery id {0:?}")]
    InvalidId(String),
    /// No entry with this id.
    #[error("no recovery entry {0:?}")]
    NotFound(String),
    /// The manifest could not be read or written as JSON.
    #[error("recovery manifest: {0}")]
    Manifest(String),
    /// The data file is not valid UTF-8.
    #[error("recovery data of {0:?} is not valid text")]
    InvalidText(String),
    /// File access failed.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// Writing a file failed.
    #[error(transparent)]
    Save(#[from] SaveError),
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    entries: Vec<RecoveryEntry>,
}

fn check_id(id: &str) -> Result<(), RecoveryError> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(RecoveryError::InvalidId(id.to_owned()))
    }
}

/// The `recovery\` folder.
#[derive(Debug)]
pub struct RecoveryStore {
    fs: Arc<dyn FileSystem>,
    dir: PathBuf,
}

impl RecoveryStore {
    /// A store rooted at `dir` (created on first write).
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>, dir: PathBuf) -> Self {
        Self { fs, dir }
    }

    /// The recovery folder.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn manifest_path(&self) -> PathBuf {
        self.dir.join(MANIFEST_NAME)
    }

    fn read_manifest(&self) -> Result<Vec<RecoveryEntry>, RecoveryError> {
        let bytes = match self.fs.read(&self.manifest_path()) {
            Ok(bytes) => bytes,
            Err(FsError::NotFound(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        match serde_json::from_slice::<Manifest>(&bytes) {
            Ok(manifest) => Ok(manifest.entries),
            Err(error) => Err(RecoveryError::Manifest(error.to_string())),
        }
    }

    fn write_manifest(&self, entries: Vec<RecoveryEntry>) -> Result<(), RecoveryError> {
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            entries,
        };
        let bytes = match serde_json::to_vec_pretty(&manifest) {
            Ok(bytes) => bytes,
            Err(error) => return Err(RecoveryError::Manifest(error.to_string())),
        };
        self.fs.create_dir_all(&self.dir)?;
        save_atomic(
            self.fs.as_ref(),
            &ThreadSleeper,
            &self.manifest_path(),
            &bytes,
        )?;
        Ok(())
    }

    fn write_data(
        &self,
        doc: &RecoveryDoc<'_>,
        now_ms: u64,
    ) -> Result<RecoveryEntry, RecoveryError> {
        check_id(doc.id)?;
        self.fs.create_dir_all(&self.dir)?;
        let data_file = format!("{}.txt", doc.id);
        save_atomic(
            self.fs.as_ref(),
            &ThreadSleeper,
            &self.dir.join(&data_file),
            doc.text.as_bytes(),
        )?;
        Ok(RecoveryEntry {
            id: doc.id.to_owned(),
            kind: doc.kind,
            original_path: doc.original_path.map(Path::to_path_buf),
            display_name: doc.display_name.to_owned(),
            saved_at_ms: now_ms,
            format: doc.format,
            data_file,
        })
    }

    fn upsert(entries: &mut Vec<RecoveryEntry>, entry: RecoveryEntry) {
        entries.retain(|existing| existing.id != entry.id);
        entries.push(entry);
    }

    /// Writes (or refreshes) the backup of one document, e.g. an untitled buffer on the
    /// 30-second tick.
    ///
    /// # Errors
    /// Invalid ids, manifest and file errors.
    pub fn backup(&self, doc: &RecoveryDoc<'_>, now_ms: u64) -> Result<(), RecoveryError> {
        let entry = self.write_data(doc, now_ms)?;
        let mut entries = self.read_manifest()?;
        Self::upsert(&mut entries, entry);
        self.write_manifest(entries)
    }

    /// Saves every document, continuing past failures, then writes one manifest. Meant for the
    /// crash handler, so it does as little as possible and never stops at the first error.
    ///
    /// # Errors
    /// Only the manifest update can fail the whole call; per-document failures are in the report.
    pub fn emergency_save(
        &self,
        docs: &[RecoveryDoc<'_>],
        now_ms: u64,
    ) -> Result<EmergencyReport, RecoveryError> {
        let mut report = EmergencyReport::default();
        let mut fresh = Vec::new();
        for doc in docs {
            match self.write_data(doc, now_ms) {
                Ok(entry) => {
                    report.saved.push(entry.id.clone());
                    fresh.push(entry);
                }
                Err(error) => report.failed.push((doc.id.to_owned(), error)),
            }
        }
        // An unreadable manifest must not lose the documents just written: start a new one.
        let mut entries = self.read_manifest().unwrap_or_default();
        for entry in fresh {
            Self::upsert(&mut entries, entry);
        }
        self.write_manifest(entries)?;
        Ok(report)
    }

    /// Lists what can be recovered (empty when there is no manifest).
    ///
    /// # Errors
    /// [`RecoveryError::Manifest`] when the manifest is corrupt.
    pub fn list(&self) -> Result<Vec<RecoveryEntry>, RecoveryError> {
        self.read_manifest()
    }

    /// Reads the text of entry `id` without removing it.
    ///
    /// # Errors
    /// [`RecoveryError::NotFound`], [`RecoveryError::InvalidText`], file errors.
    pub fn restore(&self, id: &str) -> Result<RestoredDoc, RecoveryError> {
        let entry = self
            .read_manifest()?
            .into_iter()
            .find(|entry| entry.id == id)
            .ok_or(RecoveryError::NotFound(id.to_owned()))?;
        let bytes = self.fs.read(&self.dir.join(&entry.data_file))?;
        match String::from_utf8(bytes) {
            Ok(text) => Ok(RestoredDoc { entry, text }),
            Err(_) => Err(RecoveryError::InvalidText(id.to_owned())),
        }
    }

    /// Forgets entry `id` and deletes its data file.
    ///
    /// # Errors
    /// [`RecoveryError::NotFound`], manifest errors.
    pub fn discard(&self, id: &str) -> Result<(), RecoveryError> {
        let mut entries = self.read_manifest()?;
        let position = entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(RecoveryError::NotFound(id.to_owned()))?;
        let removed = entries.remove(position);
        self.write_manifest(entries)?;
        // The data file may already be gone; the manifest no longer points at it either way.
        let _ = self.fs.delete_file(&self.dir.join(removed.data_file));
        Ok(())
    }

    /// Forgets every entry ("Discard all" in the recovery prompt).
    ///
    /// # Errors
    /// Manifest errors.
    pub fn discard_all(&self) -> Result<(), RecoveryError> {
        let entries = self.read_manifest()?;
        self.write_manifest(Vec::new())?;
        for entry in entries {
            let _ = self.fs.delete_file(&self.dir.join(entry.data_file));
        }
        Ok(())
    }
}
