//! The text the crash handler saves. The crash handler runs at the panic point, where the
//! registry's `RefCell`s may be mutably borrowed, so it cannot read the live documents. The
//! autosave tick copies the text of every document with unsaved changes into this cache (shared
//! through an `Arc`, guarded by a mutex the handler only `try_lock`s); the handler writes the
//! cache to the recovery folder. At most one tick of typing is lost.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, TryLockError};

use ssw_io::encoding::TextFormat;
use ssw_io::recovery::{EmergencyReport, RecoveryDoc, RecoveryError, RecoveryKind, RecoveryStore};

/// A document with unsaved changes, as of the last autosave tick.
#[derive(Clone, Debug)]
pub struct CachedDoc {
    /// Untitled buffer or dirty file.
    pub kind: RecoveryKind,
    /// The file the text belongs to, for dirty documents.
    pub original_path: Option<PathBuf>,
    /// Name shown in the recovery list.
    pub display_name: String,
    /// The text.
    pub text: String,
    /// Encoding, BOM and line endings to restore on save.
    pub format: TextFormat,
    /// Document version the text was copied at.
    pub version: u64,
}

/// The unsaved-changes cache.
#[derive(Debug, Default)]
pub struct EmergencyCache {
    docs: Mutex<HashMap<String, CachedDoc>>,
}

impl EmergencyCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, CachedDoc>> {
        self.docs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Stores (or replaces) the text of document `id`.
    pub fn put(&self, id: &str, doc: CachedDoc) {
        self.lock().insert(id.to_owned(), doc);
    }

    /// Forgets document `id` (it was saved or closed).
    pub fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    /// The version cached for `id`.
    #[must_use]
    pub fn version(&self, id: &str) -> Option<u64> {
        self.lock().get(id).map(|doc| doc.version)
    }

    /// How many documents are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Writes every cached document to the recovery folder. Meant for the crash handler: when
    /// the cache is locked by the crashing code itself nothing is written (`Ok(None)`); a
    /// poisoned lock is used as it is.
    ///
    /// # Errors
    /// [`RecoveryError`] when the recovery manifest cannot be written.
    pub fn save_all(
        &self,
        recovery: &RecoveryStore,
        now_ms: u64,
    ) -> Result<Option<EmergencyReport>, RecoveryError> {
        let guard = match self.docs.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(None),
        };
        let docs: Vec<RecoveryDoc<'_>> = guard
            .iter()
            .map(|(id, doc)| RecoveryDoc {
                id,
                kind: doc.kind,
                original_path: doc.original_path.as_deref(),
                display_name: &doc.display_name,
                text: &doc.text,
                format: doc.format,
            })
            .collect();
        recovery.emergency_save(&docs, now_ms).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ssw_io::fs::RealFs;

    use super::*;

    fn cached(text: &str, version: u64) -> CachedDoc {
        CachedDoc {
            kind: RecoveryKind::Dirty,
            original_path: Some(PathBuf::from(r"C:\notes\a.md")),
            display_name: "a".to_owned(),
            text: text.to_owned(),
            format: TextFormat::default(),
            version,
        }
    }

    #[test]
    fn the_cache_tracks_versions_and_removals() {
        let cache = EmergencyCache::new();
        assert!(cache.is_empty());
        cache.put("doc-1", cached("one", 3));
        cache.put("doc-1", cached("one!", 4));
        cache.put("doc-2", cached("two", 1));
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.version("doc-1"), Some(4));
        cache.remove("doc-1");
        assert_eq!(cache.version("doc-1"), None);
        assert!(!cache.is_empty());
    }

    #[test]
    fn save_all_writes_the_cached_text_to_the_recovery_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::new(Arc::new(RealFs), tmp.path().join("recovery"));
        let cache = EmergencyCache::new();
        cache.put("doc-1", cached("unsaved words", 2));
        let mut untitled = cached("scratch", 1);
        untitled.kind = RecoveryKind::Untitled;
        untitled.original_path = None;
        cache.put("untitled-9", untitled);

        let report = cache.save_all(&store, 1234).unwrap().unwrap();
        assert_eq!(report.failed.len(), 0);
        let mut saved = report.saved.clone();
        saved.sort();
        assert_eq!(saved, ["doc-1", "untitled-9"]);
        assert_eq!(store.restore("doc-1").unwrap().text, "unsaved words");
        assert_eq!(store.restore("untitled-9").unwrap().text, "scratch");
        assert_eq!(
            store.list().unwrap().len(),
            2,
            "manifest lists both documents"
        );
    }

    #[test]
    fn a_cache_locked_by_the_crashing_code_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::new(Arc::new(RealFs), tmp.path().join("recovery"));
        let cache = EmergencyCache::new();
        cache.put("doc-1", cached("x", 1));
        let held = cache.lock();
        assert!(cache.save_all(&store, 1).unwrap().is_none());
        drop(held);
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_poisoned_cache_is_still_written() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RecoveryStore::new(Arc::new(RealFs), tmp.path().join("recovery"));
        let cache = Arc::new(EmergencyCache::new());
        cache.put("doc-1", cached("survives", 1));
        let poisoner = Arc::clone(&cache);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("poison the cache");
        })
        .join();
        let report = cache.save_all(&store, 1).unwrap().unwrap();
        assert_eq!(report.saved, ["doc-1"]);
        // The poisoned cache keeps working for the normal paths as well.
        cache.put("doc-2", cached("y", 1));
        assert_eq!(cache.len(), 2);
    }
}
