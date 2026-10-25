//! The autosave scheduler (spec §4.3, §4.4, §4.5).
//!
//! The UI calls [`Autosave::tick`] from a timer (a few times a second), [`Autosave::flush`] on
//! close and [`Autosave::flush_without_waiting`] on tab switch and window deactivation. The
//! scheduler decides, from a fake-able clock:
//!
//! - save a changed file document once typing stopped for the configured delay (autosave on);
//!   the tick never sleeps: a file another program holds is tried again after the backoff of an
//!   explicit save (50, 100, 200, 400, 800 ms) and only then reported,
//! - save immediately on those triggers (autosave on); the tab switch and deactivation
//!   triggers never wait for a file another program holds and leave it to the tick,
//! - back up dirty untitled buffers every 30 s (recovery folder and session store),
//! - snapshot edited files in the version history every 5 minutes,
//! - delete the history older than the retention every hour (startup purges once as well),
//! - keep the crash handler's copy of unsaved text current.
//!
//! With autosave off, and for a document autosave cannot write ([`Autosave::autosavable`]), only
//! the dirty state is tracked (the UI shows the dirty dot and the close prompt); the backups and
//! snapshots still run.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use ssw_core::i18n::Localizer;
use ssw_core::settings::Settings;
use ssw_io::history::{SnapshotOutcome, SnapshotReason, should_snapshot};
use ssw_io::recovery::{RecoveryDoc, RecoveryKind, UNTITLED_BACKUP_INTERVAL};
use ssw_io::save::{SHARING_BACKOFF, SaveError, WriteAccess};
use ssw_io::store::UnixSeconds;

use crate::documents::{DocEntry, DocError, DocId, Documents, ExternalState, Saved, Source};
use crate::emergency::{CachedDoc, EmergencyCache};

/// How often the version history is purged of snapshots older than the retention while the
/// application stays open. The first purge of a run is the one of startup.
pub const HISTORY_PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Autosave and version-history settings (spec §10 General and Advanced).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutosavePolicy {
    /// Whether changes are written without asking.
    pub enabled: bool,
    /// Idle time after typing before the write.
    pub delay: Duration,
    /// Days the version history is kept (0 keeps everything).
    pub history_retention_days: u32,
}

impl AutosavePolicy {
    /// The policy of the user's settings.
    #[must_use]
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            enabled: settings.general.autosave,
            delay: Duration::from_millis(u64::from(settings.general.autosave_delay_ms)),
            history_retention_days: settings.advanced.history_retention_days,
        }
    }
}

/// What one tick did.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Documents written.
    pub saved: Vec<DocId>,
    /// Documents whose autosave failed for good (not retried until the text changes again) with
    /// a failure the user has not been told about yet ([`Autosave::is_new_failure`]).
    pub failed: Vec<(DocId, DocError)>,
    /// Untitled buffers backed up.
    pub backed_up: Vec<DocId>,
    /// Periodic history snapshots stored (a snapshot identical to the newest one is not).
    pub snapshots: usize,
    /// History snapshots deleted because they were older than the retention.
    pub purged: usize,
    /// The history purge that was due failed; it is tried again at the next interval.
    pub purge_failed: Option<DocError>,
}

#[derive(Debug)]
struct Track {
    /// Version seen at the last tick.
    version: u64,
    /// When `version` was first seen.
    changed_at: Duration,
    /// Version of the last periodic snapshot (or of the document when first seen).
    snapshot_version: u64,
    /// Version whose autosave failed for good.
    failed_version: Option<u64>,
    /// How many tries to write `version` found the file held by another program.
    retries: usize,
    /// When the write of `version` is tried again.
    retry_at: Duration,
}

#[derive(Debug)]
struct State {
    policy: AutosavePolicy,
    tracks: HashMap<DocId, Track>,
    /// The kind (`DocError::message_id`) of the failure shown for a document with unsaved
    /// changes; forgotten when the document is written or has nothing unsaved left.
    reported: HashMap<DocId, &'static str>,
    last_backup: Option<Duration>,
    /// When the interval to the next history purge started; `None` until the first tick and
    /// after the retention changed.
    purge_since: Option<Duration>,
}

/// The scheduler.
#[derive(Debug)]
pub struct Autosave {
    docs: Rc<Documents>,
    emergency: Arc<EmergencyCache>,
    /// When the scheduler (this run of the application) started.
    run: UnixSeconds,
    state: RefCell<State>,
}

impl Autosave {
    /// A scheduler over `docs` that keeps `emergency` current.
    #[must_use]
    pub fn new(
        docs: Rc<Documents>,
        emergency: Arc<EmergencyCache>,
        policy: AutosavePolicy,
    ) -> Self {
        Self {
            run: docs.env().clock.unix_seconds(),
            docs,
            emergency,
            state: RefCell::new(State {
                policy,
                tracks: HashMap::new(),
                reported: HashMap::new(),
                last_backup: None,
                purge_since: None,
            }),
        }
    }

    /// The crash copy this scheduler keeps current.
    #[must_use]
    pub fn emergency(&self) -> &Arc<EmergencyCache> {
        &self.emergency
    }

    /// The id of `entry` in the crash copy and the recovery folder. An untitled buffer has its
    /// own id; a file's holds the start of this run, because document numbers begin at 1 in
    /// every run while the recovery folder keeps what earlier runs left unrecovered.
    pub fn recovery_id(&self, entry: &DocEntry) -> String {
        entry
            .untitled_id()
            .unwrap_or_else(|| format!("doc-{}-{}", self.run, entry.id().0))
    }

    /// Applies new settings (autosave toggled, delay or history retention changed). A new
    /// retention takes effect at the next purge, one [`HISTORY_PURGE_INTERVAL`] after the
    /// change: the settings box applies every number while it is typed ("3" on the way to
    /// "365"), and a purge cannot be undone.
    pub fn set_policy(&self, policy: AutosavePolicy) {
        let mut state = self.state.borrow_mut();
        if state.policy.history_retention_days != policy.history_retention_days {
            state.purge_since = None;
        }
        state.policy = policy;
    }

    /// The active policy.
    #[must_use]
    pub fn policy(&self) -> AutosavePolicy {
        self.state.borrow().policy
    }

    /// Whether a document may be written without asking: a file (not a converted document),
    /// not read-only, no unresolved external change, no lossy decode. Autosave never writes any
    /// other document, so the user has to be asked about its unsaved changes.
    #[must_use]
    pub fn autosavable(entry: &DocEntry) -> bool {
        entry.path().is_some()
            && !matches!(entry.source(), Source::Converted(_))
            && entry.access() == WriteAccess::Writable
            && entry.external() == ExternalState::InSync
            && !entry.is_lossy()
    }

    fn refresh_emergency(&self, entry: &DocEntry, version: u64) {
        let id = self.recovery_id(entry);
        if self.emergency.version(&id) == Some(version) {
            return;
        }
        let (kind, original_path) = match entry.path() {
            Some(path) => (RecoveryKind::Dirty, Some(path)),
            None => (RecoveryKind::Untitled, None),
        };
        let display_name = entry
            .path()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        self.emergency.put(
            &id,
            CachedDoc {
                kind,
                original_path,
                display_name,
                text: entry.document().borrow().text(),
                format: entry.format(),
                version,
            },
        );
    }

    fn back_up_untitled(&self, entry: &DocEntry, id: &str, i18n: &Localizer) -> bool {
        let text = entry.document().borrow().text();
        let env = self.docs.env();
        let now = env.clock.unix_seconds();
        let title = entry.title(i18n);
        let backup = RecoveryDoc {
            id,
            kind: RecoveryKind::Untitled,
            original_path: None,
            display_name: &title,
            text: &text,
            format: entry.format(),
        };
        let recovered = env
            .recovery
            .backup(
                &backup,
                u64::try_from(now).unwrap_or(0).saturating_mul(1000),
            )
            .is_ok();
        let stored = env.store.borrow().untitled_put(id, &text, now).is_ok();
        recovered && stored
    }

    /// Takes the periodic snapshot of `entry` when the last one is five minutes old. `None`
    /// when none was due or the database failed (tried again at the next tick), otherwise
    /// whether a new version was stored (`false`: the text equals the newest version).
    fn snapshot_if_due(&self, entry: &DocEntry) -> Option<bool> {
        let path = entry.path()?;
        let env = self.docs.env();
        let now = env.clock.unix_seconds();
        let last = env
            .store
            .borrow()
            .history_last_snapshot_time(&path)
            .ok()
            .flatten();
        if !should_snapshot(now, last, SnapshotReason::Periodic) {
            return None;
        }
        match self.docs.snapshot(entry, SnapshotReason::Periodic) {
            Ok(Some(SnapshotOutcome::Created(_))) => Some(true),
            Ok(Some(SnapshotOutcome::Unchanged(_))) => Some(false),
            Ok(None) | Err(_) => None,
        }
    }

    /// Deletes the snapshots older than the retention.
    fn purge_history(&self, retention_days: u32) -> Result<usize, DocError> {
        let env = self.docs.env();
        let now = env.clock.unix_seconds();
        Ok(env.store.borrow_mut().history_purge(now, retention_days)?)
    }

    /// Runs every due job. Call a few times per second.
    pub fn tick(&self, i18n: &Localizer) -> TickReport {
        let mut report = TickReport::default();
        let now = self.docs.env().clock.monotonic();
        let entries = self.docs.all();
        let (policy, backup_due, purge_due) = {
            let mut state = self.state.borrow_mut();
            let open: Vec<DocId> = entries.iter().map(|entry| entry.id()).collect();
            state.tracks.retain(|id, _| open.contains(id));
            let due = state
                .last_backup
                .is_none_or(|last| now.saturating_sub(last) >= UNTITLED_BACKUP_INTERVAL);
            if due {
                state.last_backup = Some(now);
            }
            // The interval starts at the first tick (startup purged just before) and again
            // after each purge.
            let since = *state.purge_since.get_or_insert(now);
            let purge = now.saturating_sub(since) >= HISTORY_PURGE_INTERVAL;
            if purge {
                state.purge_since = Some(now);
            }
            (state.policy, due, purge)
        };
        let mut unsaved = Vec::new();
        let mut crash_ids = Vec::new();
        for entry in &entries {
            let version = entry.document().borrow().version();
            let dirty = entry.is_dirty();
            let (save_due, snapshot_pending, retries) = {
                let mut state = self.state.borrow_mut();
                let track = state.tracks.entry(entry.id()).or_insert(Track {
                    version,
                    changed_at: now,
                    snapshot_version: version,
                    failed_version: None,
                    retries: 0,
                    retry_at: Duration::ZERO,
                });
                if track.version != version {
                    track.version = version;
                    track.changed_at = now;
                    track.retries = 0;
                    track.retry_at = Duration::ZERO;
                }
                (
                    now.saturating_sub(track.changed_at) >= policy.delay
                        && track.failed_version != Some(version)
                        && now >= track.retry_at,
                    track.snapshot_version != version,
                    track.retries,
                )
            };
            if dirty {
                self.refresh_emergency(entry, version);
                unsaved.push(entry.id());
                crash_ids.push(self.recovery_id(entry));
            }
            if dirty && policy.enabled && save_due && Self::autosavable(entry) {
                self.write_for_tick(entry, version, retries, now, &mut report);
            }
            if dirty
                && backup_due
                && let Some(id) = entry.untitled_id()
                && self.back_up_untitled(entry, &id, i18n)
            {
                report.backed_up.push(entry.id());
            }
            if snapshot_pending && let Some(stored) = self.snapshot_if_due(entry) {
                report.snapshots += usize::from(stored);
                self.state
                    .borrow_mut()
                    .tracks
                    .entry(entry.id())
                    .and_modify(|track| track.snapshot_version = version);
            }
        }
        // Only a document with unsaved changes keeps a crash copy: one closed without saving,
        // or saved under another name, must not come back from a crash with its old text. A
        // failure counts as shown only while its document has unsaved changes.
        self.emergency.keep_only(&crash_ids);
        self.state
            .borrow_mut()
            .reported
            .retain(|id, _| unsaved.contains(id));
        if purge_due {
            match self.purge_history(policy.history_retention_days) {
                Ok(purged) => report.purged = purged,
                Err(error) => report.purge_failed = Some(error),
            }
        }
        report
    }

    /// Writes `entry` for the tick without sleeping. A file another program holds (antivirus,
    /// a sync client, the indexer) is tried again by a later tick after the next delay of the
    /// backoff of an explicit save, and nothing is reported meanwhile. Any other failure, and
    /// the violation that outlasts the backoff, is final: the document is not tried again until
    /// its text changes, and the report gets the failure when it is news.
    fn write_for_tick(
        &self,
        entry: &DocEntry,
        version: u64,
        retries: usize,
        now: Duration,
        report: &mut TickReport,
    ) {
        let Err(error) = self.docs.save_without_waiting(entry) else {
            report.saved.push(entry.id());
            self.written(entry);
            return;
        };
        let wait = match &error {
            DocError::Save(SaveError::SharingViolation(_)) => SHARING_BACKOFF.get(retries),
            _ => None,
        };
        // The track of this document was created by the tick.
        self.state
            .borrow_mut()
            .tracks
            .entry(entry.id())
            .and_modify(|track| match wait {
                Some(delay) => {
                    track.retries = retries + 1;
                    track.retry_at = now + *delay;
                }
                None => track.failed_version = Some(version),
            });
        if wait.is_none() && self.is_new_failure(entry, &error) {
            report.failed.push((entry.id(), error));
        }
    }

    /// Whether `error`, the failure of a save of `entry`, is news to the user: the first failure
    /// of the document since it was written, or one of another kind than the failure shown
    /// last. The same failure again, after every pause in typing and at every tab switch, is
    /// not. Asking counts the failure as shown.
    pub fn is_new_failure(&self, entry: &DocEntry, error: &DocError) -> bool {
        let kind = error.message_id();
        self.state.borrow_mut().reported.insert(entry.id(), kind) != Some(kind)
    }

    /// `entry` is on disk: it needs no crash copy and no failure counts as shown for it.
    fn written(&self, entry: &DocEntry) {
        self.emergency.remove(&self.recovery_id(entry));
        self.state.borrow_mut().reported.remove(&entry.id());
    }

    /// Saves `entry` now when autosave is on and it has unsaved changes: the close trigger; a
    /// file another program holds is tried again after 50 to 800 ms. `None`
    /// when there is nothing to do (autosave off, clean, not autosavable). A failure is returned
    /// as it is; whether it is news is [`Autosave::is_new_failure`].
    pub fn flush(&self, entry: &DocEntry) -> Option<Result<Saved, DocError>> {
        self.flush_by(entry, Documents::save)
    }

    /// [`Autosave::flush`] for the triggers that must not stall the UI (a tab switch, and window
    /// deactivation: a message box or an Alt+Tab deactivates the window while a spreadsheet may
    /// hold the file).
    /// It never sleeps. A file another program holds is not a failure here: the document stays
    /// unsaved and `None` is returned, and the tick tries again after its backoff and reports
    /// the file if it stays held.
    pub fn flush_without_waiting(&self, entry: &DocEntry) -> Option<Result<Saved, DocError>> {
        match self.flush_by(entry, Documents::save_without_waiting) {
            Some(Err(DocError::Save(SaveError::SharingViolation(_)))) => None,
            outcome => outcome,
        }
    }

    fn flush_by(
        &self,
        entry: &DocEntry,
        save: fn(&Documents, &DocEntry) -> Result<Saved, DocError>,
    ) -> Option<Result<Saved, DocError>> {
        if !self.policy().enabled || !entry.is_dirty() || !Self::autosavable(entry) {
            return None;
        }
        let result = save(&self.docs, entry);
        if result.is_ok() {
            self.written(entry);
        }
        Some(result)
    }

    /// [`Autosave::flush`] for every open document.
    pub fn flush_all(&self) -> Vec<(DocId, Result<Saved, DocError>)> {
        self.docs
            .all()
            .iter()
            .filter_map(|entry| self.flush(entry).map(|result| (entry.id(), result)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use ssw_core::edit::{Edit, Selection, Transaction};
    use ssw_core::text::TextRange;
    use ssw_io::encoding::TextEncoding;
    use ssw_io::fs::{FaultFs, FileSystem, FsFailure, FsOp, RealFs};
    use ssw_io::recovery::RecoveryStore;
    use ssw_io::save::{Sleeper, ThreadSleeper};
    use ssw_io::store::Store;

    use super::*;
    use crate::clock::ManualClock;
    use crate::documents::{DocEnv, SharedStore};

    /// The retention of the rig's policy (the Advanced setting's default).
    const RETENTION_DAYS: u32 = 30;
    /// The wall-clock time the rig's clock starts at (Unix seconds).
    const START: i64 = 10_000;
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    struct Rig {
        tmp: tempfile::TempDir,
        clock: Rc<ManualClock>,
        docs: Rc<Documents>,
        emergency: Arc<EmergencyCache>,
        autosave: Autosave,
        i18n: Localizer,
    }

    /// Remembers how long it was asked to sleep and returns at once.
    #[derive(Debug, Default)]
    struct RecordingSleeper(Mutex<Vec<Duration>>);

    impl Sleeper for RecordingSleeper {
        fn sleep(&self, duration: Duration) {
            self.0.lock().unwrap().push(duration);
        }
    }

    impl RecordingSleeper {
        fn slept(&self) -> Vec<Duration> {
            self.0.lock().unwrap().clone()
        }
    }

    fn rig(enabled: bool) -> Rig {
        rig_on(Arc::new(RealFs), Arc::new(ThreadSleeper), enabled)
    }

    /// A rig whose documents reach the disk through `fs` and wait with `sleeper`.
    fn rig_on(fs: Arc<dyn FileSystem>, sleeper: Arc<dyn Sleeper>, enabled: bool) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let clock = Rc::new(ManualClock::new(START));
        let shared: Rc<dyn crate::clock::AppClock> = clock.clone();
        let store: SharedStore = Rc::new(RefCell::new(
            Store::open(&tmp.path().join("state.db")).unwrap(),
        ));
        let docs = Rc::new(Documents::new(DocEnv {
            fs,
            sleeper,
            clock: shared,
            store,
            recovery: Arc::new(RecoveryStore::new(
                Arc::new(RealFs),
                tmp.path().join("recovery"),
            )),
            tld: None,
        }));
        let emergency = Arc::new(EmergencyCache::new());
        let policy = AutosavePolicy {
            enabled,
            delay: Duration::from_millis(1000),
            history_retention_days: RETENTION_DAYS,
        };
        Rig {
            autosave: Autosave::new(Rc::clone(&docs), Arc::clone(&emergency), policy),
            i18n: Localizer::new("en", &[("en", "app-untitled = Untitled\n")]),
            tmp,
            clock,
            docs,
            emergency,
        }
    }

    impl Rig {
        fn file(&self, text: &str) -> (PathBuf, Rc<DocEntry>) {
            let path = self.tmp.path().join("a.md");
            std::fs::write(&path, text).unwrap();
            let entry = self.docs.open(&path).unwrap();
            (path, entry)
        }

        fn advance(&self, millis: u64) {
            self.clock.advance(Duration::from_millis(millis));
        }

        /// The versions of `entry`'s file in the history, newest first.
        fn versions(&self, entry: &DocEntry) -> Vec<ssw_io::history::SnapshotInfo> {
            let path = entry.path().unwrap();
            self.docs.env().store.borrow().history_list(&path).unwrap()
        }

        /// Stores `text` as a version of `entry`'s file taken at Unix time `at`.
        fn keep(&self, entry: &DocEntry, text: &str, at: i64) {
            let path = entry.path().unwrap();
            self.docs
                .env()
                .store
                .borrow_mut()
                .history_snapshot(&path, text, SnapshotReason::Periodic, at)
                .unwrap();
        }
    }

    fn set_text(entry: &DocEntry, text: &str) {
        let mut document = entry.document().borrow_mut();
        let end = document.buffer().len();
        let tx = Transaction::single(
            Edit::replace(TextRange::new(0, end), text),
            Selection::caret(end),
            Selection::caret(text.len()),
        );
        document.apply(tx).unwrap();
    }

    fn type_text(entry: &DocEntry, text: &str) {
        let mut document = entry.document().borrow_mut();
        let end = document.buffer().len();
        let tx = Transaction::single(
            Edit::insert(end, text),
            Selection::caret(end),
            Selection::caret(end + text.len()),
        );
        document.apply(tx).unwrap();
    }

    #[test]
    fn a_changed_file_is_saved_once_typing_stopped_for_the_delay() {
        let rig = rig(true);
        let (path, entry) = rig.file("a\n");
        assert!(rig.autosave.tick(&rig.i18n).saved.is_empty());
        type_text(&entry, "b\n");
        rig.advance(10);
        assert!(rig.autosave.tick(&rig.i18n).saved.is_empty());
        rig.advance(900);
        type_text(&entry, "c\n");
        assert!(
            rig.autosave.tick(&rig.i18n).saved.is_empty(),
            "typing restarts the delay"
        );
        rig.advance(999);
        assert!(rig.autosave.tick(&rig.i18n).saved.is_empty());
        assert!(entry.is_dirty());
        rig.advance(1);
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(report.saved, [entry.id()]);
        assert!(report.failed.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
        assert!(!entry.is_dirty());
        assert!(rig.emergency.is_empty());
        // Nothing to do while clean.
        rig.advance(5000);
        assert!(rig.autosave.tick(&rig.i18n).saved.is_empty());
    }

    #[test]
    fn with_autosave_off_only_the_dirty_state_and_the_crash_copy_change() {
        let rig = rig(false);
        let (path, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        type_text(&entry, "b\n");
        rig.advance(60_000);
        let report = rig.autosave.tick(&rig.i18n);
        assert!(report.saved.is_empty() && report.failed.is_empty());
        assert!(entry.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert!(rig.autosave.flush(&entry).is_none());
        assert_eq!(rig.emergency.len(), 1);
        rig.autosave.set_policy(AutosavePolicy {
            enabled: true,
            delay: Duration::from_millis(1000),
            history_retention_days: RETENTION_DAYS,
        });
        assert!(rig.autosave.policy().enabled);
        assert!(rig.autosave.flush(&entry).unwrap().is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
    }

    #[test]
    fn flush_all_saves_every_changed_document_immediately() {
        let rig = rig(true);
        let (path, entry) = rig.file("a\n");
        type_text(&entry, "b\n");
        let results = rig.autosave.flush_all();
        assert_eq!(results.len(), 1);
        assert!(results[0].1.is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
        assert!(
            rig.autosave.flush(&entry).is_none(),
            "clean documents are skipped"
        );
    }

    #[test]
    fn read_only_documents_are_not_autosaved() {
        let rig = rig(true);
        let (path, entry) = rig.file("a\n");
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        let locked = rig.docs.open(&path).unwrap();
        // The document was opened writable before the attribute changed; refresh sees it.
        rig.docs.refresh_access(&locked);
        type_text(&entry, "b\n");
        rig.autosave.tick(&rig.i18n);
        rig.advance(2000);
        assert!(rig.autosave.tick(&rig.i18n).saved.is_empty());
        assert!(rig.autosave.flush(&entry).is_none());
    }

    #[test]
    fn only_a_writable_file_in_sync_and_without_decoding_loss_is_autosavable() {
        let rig = rig(true);
        let (path, entry) = rig.file("a\n");
        assert!(Autosave::autosavable(&entry));

        let scratch = rig.docs.new_untitled();
        type_text(&scratch, "draft\n");
        assert!(!Autosave::autosavable(&scratch));
        assert!(rig.autosave.flush(&scratch).is_none());

        let locked_path = rig.tmp.path().join("locked.md");
        std::fs::write(&locked_path, "x\n").unwrap();
        set_readonly(&locked_path, true);
        let locked = rig.docs.open(&locked_path).unwrap();
        set_readonly(&locked_path, false);
        type_text(&locked, "y\n");
        assert!(!Autosave::autosavable(&locked));
        assert!(rig.autosave.flush(&locked).is_none());

        let word = rig.tmp.path().join("w.docx");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/docx/mammoth_empty.docx"),
            &word,
        )
        .unwrap();
        let converted = rig.docs.open(&word).unwrap();
        type_text(&converted, "z\n");
        assert!(!Autosave::autosavable(&converted));
        assert!(rig.autosave.flush(&converted).is_none());

        let latin = rig.tmp.path().join("latin.txt");
        std::fs::write(&latin, b"caf\xE9\n").unwrap();
        let lossy = rig.docs.open(&latin).unwrap();
        rig.docs.reload_as(&lossy, TextEncoding::Utf8).unwrap();
        type_text(&lossy, "!\n");
        assert!(!Autosave::autosavable(&lossy));
        assert!(rig.autosave.flush(&lossy).is_none());

        let gone_path = rig.tmp.path().join("gone.md");
        std::fs::write(&gone_path, "g\n").unwrap();
        let gone = rig.docs.open(&gone_path).unwrap();
        std::fs::remove_file(&gone_path).unwrap();
        rig.docs.check_external(&gone).unwrap();
        assert_eq!(gone.external(), ExternalState::Deleted);
        assert!(!Autosave::autosavable(&gone));

        type_text(&entry, "b\n");
        std::fs::write(&path, "elsewhere\n").unwrap();
        rig.docs.check_external(&entry).unwrap();
        assert_eq!(entry.external(), ExternalState::Conflict);
        assert!(!Autosave::autosavable(&entry));
        assert!(rig.autosave.flush(&entry).is_none());
    }

    #[test]
    fn dirty_untitled_buffers_are_backed_up_every_thirty_seconds() {
        let rig = rig(true);
        let scratch = rig.docs.new_untitled();
        let id = scratch.untitled_id().unwrap();
        assert!(
            rig.autosave.tick(&rig.i18n).backed_up.is_empty(),
            "clean buffers are not backed up"
        );
        rig.advance(31_000);
        type_text(&scratch, "first");
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(report.backed_up, [scratch.id()]);
        assert_eq!(
            rig.docs
                .env()
                .store
                .borrow()
                .untitled_get(&id)
                .unwrap()
                .as_deref(),
            Some("first")
        );
        assert_eq!(rig.docs.env().recovery.restore(&id).unwrap().text, "first");
        assert_eq!(
            rig.docs
                .env()
                .recovery
                .restore(&id)
                .unwrap()
                .entry
                .display_name,
            "Untitled"
        );
        rig.advance(10_000);
        type_text(&scratch, " second");
        assert!(rig.autosave.tick(&rig.i18n).backed_up.is_empty());
        rig.advance(20_000);
        assert_eq!(rig.autosave.tick(&rig.i18n).backed_up, [scratch.id()]);
        assert_eq!(
            rig.docs.env().recovery.restore(&id).unwrap().text,
            "first second"
        );
        assert_eq!(
            rig.emergency.len(),
            1,
            "the crash handler has the latest text too"
        );
    }

    #[test]
    fn edited_files_are_snapshotted_every_five_minutes() {
        let rig = rig(true);
        let (path, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        type_text(&entry, "b\n");
        rig.advance(1500);
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 1);
        let canonical = crate::documents::canonical_path(&path);
        let history = |rig: &Rig| {
            rig.docs
                .env()
                .store
                .borrow()
                .history_list(&canonical)
                .unwrap()
                .len()
        };
        assert_eq!(history(&rig), 1);
        type_text(&entry, "c\n");
        rig.advance(60_000);
        assert_eq!(
            rig.autosave.tick(&rig.i18n).snapshots,
            0,
            "too soon after the last snapshot"
        );
        rig.advance(300_000);
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 1);
        assert_eq!(history(&rig), 2);
        // Unedited since: no snapshot even when due.
        rig.advance(600_000);
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 0);
    }

    #[test]
    fn a_text_identical_to_the_newest_version_is_not_stored_again() {
        let rig = rig(true);
        let (_, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        set_text(&entry, "b\n");
        rig.advance(1500);
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 1);
        // Edited and edited back to the stored text: a snapshot is due again, nothing is new.
        set_text(&entry, "c\n");
        set_text(&entry, "b\n");
        rig.advance(301_000);
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 0);
        assert_eq!(rig.versions(&entry).len(), 1);
        // Closing with that text keeps the one copy as well.
        assert!(rig.docs.release(&entry));
        let versions = rig.versions(&entry);
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].reason, SnapshotReason::Periodic);
        assert_eq!(versions[0].size, "b\n".len());
    }

    #[test]
    fn history_older_than_the_retention_is_purged_once_an_hour() {
        let rig = rig(true);
        let (_, entry) = rig.file("a\n");
        // The first tick starts the interval: startup purged just before.
        assert_eq!(rig.autosave.tick(&rig.i18n).purged, 0);
        rig.keep(&entry, "ancient\n", START);
        rig.keep(&entry, "recent\n", START + 20 * 86_400);
        rig.clock.advance(DAY * 40);
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(report.purged, 1, "only the version older than 30 days goes");
        assert!(report.purge_failed.is_none());
        assert_eq!(rig.versions(&entry).len(), 1);
        // Another expired version waits for the next hour.
        rig.keep(&entry, "stale\n", START);
        rig.clock.advance(Duration::from_secs(59 * 60));
        assert_eq!(rig.autosave.tick(&rig.i18n).purged, 0);
        assert_eq!(rig.versions(&entry).len(), 2);
        rig.clock.advance(Duration::from_secs(60));
        assert_eq!(rig.autosave.tick(&rig.i18n).purged, 1);
        let kept = rig.versions(&entry);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].created_at, START + 20 * 86_400);
    }

    #[test]
    fn a_new_retention_applies_at_the_next_purge_and_zero_keeps_everything() {
        let rig = rig(true);
        let (_, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        rig.keep(&entry, "two days old\n", START);
        rig.clock.advance(DAY * 2);
        assert_eq!(rig.autosave.tick(&rig.i18n).purged, 0, "30 days keep it");
        rig.clock.advance(Duration::from_secs(90 * 60));
        let one_day = AutosavePolicy {
            history_retention_days: 1,
            ..rig.autosave.policy()
        };
        rig.autosave.set_policy(one_day);
        // A purge would be due, but the number may still be typing ("1" on the way to "14"):
        // it waits a full interval after the change.
        assert_eq!(rig.autosave.tick(&rig.i18n).purged, 0);
        assert_eq!(rig.versions(&entry).len(), 1);
        rig.clock.advance(HISTORY_PURGE_INTERVAL);
        assert_eq!(rig.autosave.tick(&rig.i18n).purged, 1);
        assert!(rig.versions(&entry).is_empty());

        let forever = AutosavePolicy {
            history_retention_days: 0,
            ..rig.autosave.policy()
        };
        rig.autosave.set_policy(forever);
        rig.keep(&entry, "kept\n", START);
        rig.autosave.tick(&rig.i18n);
        rig.clock.advance(DAY * 365);
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(report.purged, 0);
        assert!(report.purge_failed.is_none());
        assert_eq!(rig.versions(&entry).len(), 1);
    }

    #[test]
    fn a_failed_purge_is_reported_and_tried_again_an_hour_later() {
        let rig = rig(true);
        rig.autosave.tick(&rig.i18n);
        // Another connection takes the history table away.
        rusqlite::Connection::open(rig.tmp.path().join("state.db"))
            .unwrap()
            .execute_batch("DROP TABLE history_snapshots")
            .unwrap();
        rig.clock.advance(HISTORY_PURGE_INTERVAL);
        let report = rig.autosave.tick(&rig.i18n);
        let failure = report.purge_failed.expect("the purge was due");
        assert!(
            failure
                .to_string()
                .contains("no such table: history_snapshots"),
            "{failure}"
        );
        assert_eq!(report.purged, 0);
        assert!(rig.autosave.tick(&rig.i18n).purge_failed.is_none());
        rig.clock.advance(HISTORY_PURGE_INTERVAL);
        assert!(rig.autosave.tick(&rig.i18n).purge_failed.is_some());
    }

    fn set_readonly(path: &std::path::Path, readonly: bool) {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(readonly);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn a_failed_autosave_is_reported_once_and_the_crash_copy_stays() {
        let rig = rig(true);
        let (path, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        // The file turns read-only behind the document's back: it still believes it can write.
        set_readonly(&path, true);
        type_text(&entry, "b\n");
        rig.autosave.tick(&rig.i18n);
        rig.advance(1000);
        let report = rig.autosave.tick(&rig.i18n);
        assert!(report.saved.is_empty());
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, entry.id());
        assert_eq!(report.failed[0].1.message_id(), "app-error-read-only");
        assert!(entry.is_dirty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        // The same text is not tried again at every tick.
        rig.advance(5000);
        assert!(rig.autosave.tick(&rig.i18n).failed.is_empty());
        // A tab switch (flush) does try, fails the same way and keeps the crash copy; the user
        // has been told already.
        let flushed = rig.autosave.flush(&entry).expect("dirty and autosavable");
        let error = flushed.unwrap_err();
        assert_eq!(error.message_id(), "app-error-read-only");
        assert!(!rig.autosave.is_new_failure(&entry, &error));
        assert_eq!(rig.emergency.len(), 1);
        // Once the file is writable again, new typing is saved.
        set_readonly(&path, false);
        type_text(&entry, "c\n");
        rig.autosave.tick(&rig.i18n);
        rig.advance(1000);
        assert_eq!(rig.autosave.tick(&rig.i18n).saved, [entry.id()]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
        assert!(rig.emergency.is_empty());
    }

    #[test]
    fn a_snapshot_the_history_refuses_is_tried_again_at_the_next_tick() {
        let rig = rig(true);
        let (_, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        let history = rusqlite::Connection::open(rig.tmp.path().join("state.db")).unwrap();
        history
            .execute_batch(
                "CREATE TRIGGER refuse_history BEFORE INSERT ON history_snapshots
                 BEGIN SELECT RAISE(ABORT, 'history refused'); END;",
            )
            .unwrap();
        type_text(&entry, "b\n");
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 0);
        assert!(rig.versions(&entry).is_empty());
        history
            .execute_batch("DROP TRIGGER refuse_history")
            .unwrap();
        assert_eq!(rig.autosave.tick(&rig.i18n).snapshots, 1);
        assert_eq!(rig.versions(&entry).len(), 1);
    }

    /// How many saves reached the disk's first step through `fault`.
    fn attempts(fault: &FaultFs) -> usize {
        fault.calls(FsOp::WriteNew)
    }

    /// Types `text`, lets the delay pass and ticks: what the tick does about the new text.
    fn type_and_wait(rig: &Rig, entry: &DocEntry, text: &str) -> TickReport {
        type_text(entry, text);
        rig.autosave.tick(&rig.i18n);
        rig.advance(1000);
        rig.autosave.tick(&rig.i18n)
    }

    /// The kinds of the failures a tick leaves for the user.
    fn shown(report: &TickReport) -> Vec<&'static str> {
        report
            .failed
            .iter()
            .map(|(_, error)| error.message_id())
            .collect()
    }

    #[test]
    fn the_tick_never_sleeps_for_a_file_another_program_holds_and_reports_it_after_the_backoff() {
        let fault = Arc::new(FaultFs::new());
        let sleeper = Arc::new(RecordingSleeper::default());
        let rig = rig_on(fault.clone(), sleeper.clone(), true);
        let (path, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        fault.fail_always(FsOp::ReplaceFile, FsFailure::SharingViolation);
        type_text(&entry, "b\n");
        rig.autosave.tick(&rig.i18n);
        rig.advance(1000);

        // One try, then one more after each delay of the backoff of an explicit save, and not
        // a word about it meanwhile.
        for (index, delay) in SHARING_BACKOFF.iter().enumerate() {
            let report = rig.autosave.tick(&rig.i18n);
            assert!(report.saved.is_empty(), "try {index}");
            assert!(report.failed.is_empty(), "try {index}");
            assert_eq!(attempts(&fault), index + 1);
            rig.clock.advance(*delay - Duration::from_millis(1));
            rig.autosave.tick(&rig.i18n);
            assert_eq!(attempts(&fault), index + 1, "{delay:?} has not passed");
            rig.advance(1);
        }
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(attempts(&fault), SHARING_BACKOFF.len() + 1);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, entry.id());
        assert_eq!(report.failed[0].1.message_id(), "app-error-in-use");
        assert!(sleeper.slept().is_empty(), "the tick never sleeps");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");
        assert!(entry.is_dirty());
        assert_eq!(rig.emergency.len(), 1, "the crash copy stays");

        // That was the last try for this text.
        rig.advance(10_000);
        assert!(rig.autosave.tick(&rig.i18n).failed.is_empty());
        assert_eq!(attempts(&fault), SHARING_BACKOFF.len() + 1);

        // A save the user asks for waits through the whole backoff, and has nothing new to say.
        let flushed = rig.autosave.flush(&entry).expect("dirty and autosavable");
        let error = flushed.unwrap_err();
        assert_eq!(error.message_id(), "app-error-in-use");
        assert_eq!(sleeper.slept(), SHARING_BACKOFF);
        assert!(!rig.autosave.is_new_failure(&entry, &error));
    }

    #[test]
    fn a_file_released_during_the_backoff_is_saved_without_a_word() {
        let fault = Arc::new(FaultFs::new());
        let sleeper = Arc::new(RecordingSleeper::default());
        let rig = rig_on(fault.clone(), sleeper.clone(), true);
        let (path, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        fault.fail_always(FsOp::ReplaceFile, FsFailure::SharingViolation);
        let report = type_and_wait(&rig, &entry, "b\n");
        assert!(report.saved.is_empty() && report.failed.is_empty());
        assert_eq!(attempts(&fault), 1);

        fault.clear();
        rig.advance(50);
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(report.saved, [entry.id()]);
        assert!(report.failed.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");
        assert!(!entry.is_dirty());
        assert!(rig.emergency.is_empty());
        assert!(sleeper.slept().is_empty());
    }

    #[test]
    fn a_flush_without_waiting_leaves_a_held_file_to_the_tick() {
        let fault = Arc::new(FaultFs::new());
        let sleeper = Arc::new(RecordingSleeper::default());
        let rig = rig_on(fault.clone(), sleeper.clone(), true);
        let (path, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        assert!(
            rig.autosave.flush_without_waiting(&entry).is_none(),
            "nothing to write"
        );
        type_text(&entry, "b\n");
        rig.autosave.tick(&rig.i18n);

        // The file is held: one try, no sleep, no failure, and the text stays unsaved.
        fault.fail_always(FsOp::ReplaceFile, FsFailure::SharingViolation);
        assert!(rig.autosave.flush_without_waiting(&entry).is_none());
        assert_eq!(attempts(&fault), 1);
        assert!(sleeper.slept().is_empty());
        assert!(entry.is_dirty());
        assert_eq!(rig.emergency.len(), 1, "the crash copy stays");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\n");

        // Any other failure is returned for the caller to report.
        fault.clear();
        fault.fail_always(FsOp::ReplaceFile, FsFailure::AccessDenied);
        let error = rig
            .autosave
            .flush_without_waiting(&entry)
            .expect("dirty and autosavable")
            .unwrap_err();
        assert_eq!(error.message_id(), "app-error-access-denied");
        assert_eq!(attempts(&fault), 2);

        // Released, the tick writes it after the delay, as it would have without the flush.
        fault.clear();
        rig.advance(1000);
        let report = rig.autosave.tick(&rig.i18n);
        assert_eq!(report.saved, [entry.id()]);
        assert!(report.failed.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\n");

        // A flush that finds the file free writes it.
        type_text(&entry, "c\n");
        rig.autosave.tick(&rig.i18n);
        assert!(
            rig.autosave
                .flush_without_waiting(&entry)
                .expect("dirty and autosavable")
                .is_ok()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
        assert!(rig.emergency.is_empty());
        assert!(sleeper.slept().is_empty());
    }

    #[test]
    fn new_text_starts_the_backoff_over() {
        let fault = Arc::new(FaultFs::new());
        let rig = rig_on(fault.clone(), Arc::new(ThreadSleeper), true);
        let (_, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);
        fault.fail_always(FsOp::ReplaceFile, FsFailure::SharingViolation);
        type_and_wait(&rig, &entry, "b\n");
        assert_eq!(attempts(&fault), 1);

        // Typing on while the file is held: the delay counts from the new text, and the first
        // delay of the backoff (50 ms, not 100 ms) follows its first try.
        type_text(&entry, "c\n");
        rig.autosave.tick(&rig.i18n);
        rig.advance(999);
        rig.autosave.tick(&rig.i18n);
        assert_eq!(attempts(&fault), 1);
        rig.advance(1);
        rig.autosave.tick(&rig.i18n);
        assert_eq!(attempts(&fault), 2);
        rig.advance(50);
        rig.autosave.tick(&rig.i18n);
        assert_eq!(attempts(&fault), 3);
    }

    #[test]
    fn a_failure_is_shown_once_until_the_document_is_written() {
        let fault = Arc::new(FaultFs::new());
        let rig = rig_on(fault.clone(), Arc::new(ThreadSleeper), true);
        let (_, entry) = rig.file("a\n");
        rig.autosave.tick(&rig.i18n);

        fault.fail_always(FsOp::ReplaceFile, FsFailure::AccessDenied);
        let report = type_and_wait(&rig, &entry, "b\n");
        assert_eq!(shown(&report), ["app-error-access-denied"]);
        // The next text fails the same way: it is tried, and the user knows already.
        let report = type_and_wait(&rig, &entry, "c\n");
        assert!(shown(&report).is_empty());
        assert_eq!(attempts(&fault), 2);

        // Another failure is news.
        fault.clear();
        fault.fail_always(FsOp::ReplaceFile, FsFailure::DiskFull);
        let report = type_and_wait(&rig, &entry, "d\n");
        assert_eq!(shown(&report), ["app-error-disk-full"]);

        // A write that works ends the story, and so does a save the user made.
        fault.clear();
        let report = type_and_wait(&rig, &entry, "e\n");
        assert_eq!(report.saved, [entry.id()]);
        fault.fail_always(FsOp::ReplaceFile, FsFailure::DiskFull);
        let report = type_and_wait(&rig, &entry, "f\n");
        assert_eq!(shown(&report), ["app-error-disk-full"]);
        fault.clear();
        rig.docs.save(&entry).unwrap();
        rig.autosave.tick(&rig.i18n);
        fault.fail_always(FsOp::ReplaceFile, FsFailure::DiskFull);
        let report = type_and_wait(&rig, &entry, "g\n");
        assert_eq!(shown(&report), ["app-error-disk-full"]);
    }

    #[test]
    fn a_document_closed_without_saving_leaves_no_crash_copy() {
        let rig = rig(false);
        let (_, entry) = rig.file("a\n");
        type_text(&entry, "b\n");
        rig.autosave.tick(&rig.i18n);
        assert_eq!(rig.emergency.len(), 1);

        // "Don't save": the tab closes with the edits.
        assert!(rig.docs.release(&entry));
        rig.autosave.tick(&rig.i18n);
        assert!(rig.emergency.is_empty());
        let recovery = RecoveryStore::new(Arc::new(RealFs), rig.tmp.path().join("crash"));
        rig.emergency.save_all(&recovery, 1).unwrap();
        assert!(recovery.list().unwrap().is_empty());
    }

    #[test]
    fn an_untitled_buffer_saved_under_a_name_leaves_no_crash_copy_under_its_old_id() {
        let rig = rig(false);
        let scratch = rig.docs.new_untitled();
        type_text(&scratch, "words");
        rig.autosave.tick(&rig.i18n);
        let old_id = scratch.untitled_id().unwrap();
        assert!(rig.emergency.version(&old_id).is_some());

        rig.docs
            .save_as(&scratch, &rig.tmp.path().join("named.md"))
            .unwrap();
        rig.autosave.tick(&rig.i18n);
        assert!(rig.emergency.is_empty(), "saved: nothing to recover");

        // Edited again, the crash copy belongs to the file.
        type_text(&scratch, " more");
        rig.autosave.tick(&rig.i18n);
        assert_eq!(rig.emergency.len(), 1);
        assert_eq!(rig.emergency.version(&old_id), None);
    }

    #[test]
    fn crash_copies_of_two_runs_do_not_overwrite_each_other() {
        let rig = rig(false);
        let (_, entry) = rig.file("a\n");
        let recovery = RecoveryStore::new(Arc::new(RealFs), rig.tmp.path().join("crash"));

        // One run crashes with its text of the document...
        type_text(&entry, "first run\n");
        rig.autosave.tick(&rig.i18n);
        rig.emergency.save_all(&recovery, 1).unwrap();

        // ...the next run, a minute later, numbers its documents from 1 again and crashes too.
        rig.advance(60_000);
        let cache = Arc::new(EmergencyCache::new());
        let later = Autosave::new(
            Rc::clone(&rig.docs),
            Arc::clone(&cache),
            rig.autosave.policy(),
        );
        type_text(&entry, "second run\n");
        later.tick(&rig.i18n);
        cache.save_all(&recovery, 2).unwrap();

        let mut saved = recovery.list().unwrap();
        saved.sort_by_key(|doc| doc.saved_at_ms);
        let ids: Vec<String> = saved.iter().map(|doc| doc.id.clone()).collect();
        let number = entry.id().0;
        let first = format!("doc-{START}-{number}");
        let second = format!("doc-{}-{number}", START + 60);
        assert_eq!(ids, [first, second]);
        assert!(saved.iter().all(|doc| doc.original_path == entry.path()));
        assert_eq!(recovery.restore(&ids[0]).unwrap().text, "a\nfirst run\n");
        assert_eq!(
            recovery.restore(&ids[1]).unwrap().text,
            "a\nfirst run\nsecond run\n"
        );
    }
}
