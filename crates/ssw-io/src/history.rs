//! Version history (spec §4.5): snapshots of document text kept in the state database,
//! deduplicated by content hash, with a retention purge and restore.
//!
//! The policy lives in [`should_snapshot`]; the storage in the `history_*` methods of
//! [`Store`]. Document text is stored as UTF-8 exactly as given (raw, no compression); blobs
//! are keyed by their BLAKE3 hash, so identical content is stored once however many snapshots
//! refer to it.

use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::store::{Store, StoreError, UnixSeconds, key_of, path_key, path_text};

/// Minimum time between periodic snapshots of an edited document.
pub const SNAPSHOT_INTERVAL_SECS: i64 = 5 * 60;

const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

/// Why a snapshot is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotReason {
    /// The periodic snapshot while the document is being edited.
    Periodic,
    /// The document or window is closing.
    Close,
    /// Right before an external change replaces the buffer.
    BeforeExternalReload,
    /// Right before saving back to Word / ODT / RTF.
    BeforeSaveBack,
    /// Right before a snapshot is restored over the current text.
    BeforeRestore,
}

impl SnapshotReason {
    const fn code(self) -> i64 {
        match self {
            SnapshotReason::Periodic => 0,
            SnapshotReason::Close => 1,
            SnapshotReason::BeforeExternalReload => 2,
            SnapshotReason::BeforeSaveBack => 3,
            SnapshotReason::BeforeRestore => 4,
        }
    }

    fn from_code(code: i64) -> Result<Self, StoreError> {
        Ok(match code {
            0 => SnapshotReason::Periodic,
            1 => SnapshotReason::Close,
            2 => SnapshotReason::BeforeExternalReload,
            3 => SnapshotReason::BeforeSaveBack,
            4 => SnapshotReason::BeforeRestore,
            _ => return Err(StoreError::InvalidData("unknown snapshot reason")),
        })
    }
}

/// Whether a snapshot should be taken now. `last` is the time of the newest snapshot of the
/// document ([`Store::history_last_snapshot_time`]). Periodic snapshots wait for
/// [`SNAPSHOT_INTERVAL_SECS`] (and a clock that went backwards never triggers one); every
/// other reason is an explicit event and always snapshots.
#[must_use]
pub fn should_snapshot(
    now: UnixSeconds,
    last: Option<UnixSeconds>,
    reason: SnapshotReason,
) -> bool {
    match reason {
        SnapshotReason::Periodic => {
            last.is_none_or(|last| now.saturating_sub(last) >= SNAPSHOT_INTERVAL_SECS)
        }
        SnapshotReason::Close
        | SnapshotReason::BeforeExternalReload
        | SnapshotReason::BeforeSaveBack
        | SnapshotReason::BeforeRestore => true,
    }
}

/// A snapshot without its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub id: i64,
    pub created_at: UnixSeconds,
    pub reason: SnapshotReason,
    /// Size of the text in bytes.
    pub size: usize,
}

/// A snapshot with its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub info: SnapshotInfo,
    pub content: String,
}

/// Result of [`Store::history_snapshot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// A new snapshot with this id was stored.
    Created(i64),
    /// The newest snapshot of the file (this id) already has identical content.
    Unchanged(i64),
}

fn info_from(
    id: i64,
    created_at: UnixSeconds,
    reason: i64,
    size: i64,
) -> Result<SnapshotInfo, StoreError> {
    Ok(SnapshotInfo {
        id,
        created_at,
        reason: SnapshotReason::from_code(reason)?,
        size: size as usize,
    })
}

impl Store {
    /// Snapshot `content` as a version of `path`, unless the newest snapshot of `path` is
    /// identical.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn history_snapshot(
        &mut self,
        path: &Path,
        content: &str,
        reason: SnapshotReason,
        now: UnixSeconds,
    ) -> Result<SnapshotOutcome, StoreError> {
        let text = path_text(path)?;
        let key = key_of(text);
        let hash = blake3::hash(content.as_bytes());
        let tx = self.conn.transaction()?;
        let newest: Option<(i64, Vec<u8>)> = tx
            .query_row(
                "SELECT id, hash FROM history_snapshots WHERE key = ?1 ORDER BY id DESC LIMIT 1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, newest_hash)) = newest
            && newest_hash.as_slice() == hash.as_bytes()
        {
            return Ok(SnapshotOutcome::Unchanged(id));
        }
        tx.execute(
            "INSERT OR IGNORE INTO history_blobs (hash, content) VALUES (?1, ?2)",
            params![hash.as_bytes(), content.as_bytes()],
        )?;
        tx.execute(
            "INSERT INTO history_snapshots (key, path, created_at, reason, hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, text, now, reason.code(), hash.as_bytes()],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(SnapshotOutcome::Created(id))
    }

    /// Time of the newest snapshot of `path`, for [`should_snapshot`].
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn history_last_snapshot_time(
        &self,
        path: &Path,
    ) -> Result<Option<UnixSeconds>, StoreError> {
        Ok(self.conn.query_row(
            "SELECT MAX(created_at) FROM history_snapshots WHERE key = ?1",
            [path_key(path)?],
            |row| row.get(0),
        )?)
    }

    /// Snapshots of `path`, newest first.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`], [`StoreError::InvalidData`] (unknown reason code) or
    /// [`StoreError::Sqlite`].
    pub fn history_list(&self, path: &Path) -> Result<Vec<SnapshotInfo>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.created_at, s.reason, length(b.content)
             FROM history_snapshots s JOIN history_blobs b ON b.hash = s.hash
             WHERE s.key = ?1 ORDER BY s.created_at DESC, s.id DESC",
        )?;
        let rows = stmt
            .query_map([path_key(path)?], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<Result<Vec<(i64, i64, i64, i64)>, _>>()?;
        rows.into_iter()
            .map(|(id, created_at, reason, size)| info_from(id, created_at, reason, size))
            .collect()
    }

    /// The snapshot with this id.
    ///
    /// # Errors
    /// [`StoreError::InvalidData`] (unknown reason code or text that is not UTF-8) or
    /// [`StoreError::Sqlite`].
    pub fn history_load(&self, id: i64) -> Result<Option<Snapshot>, StoreError> {
        let row: Option<(i64, i64, i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT s.id, s.created_at, s.reason, b.content
                 FROM history_snapshots s JOIN history_blobs b ON b.hash = s.hash
                 WHERE s.id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((id, created_at, reason, bytes)) = row else {
            return Ok(None);
        };
        let info = info_from(id, created_at, reason, bytes.len() as i64)?;
        let content = String::from_utf8(bytes)
            .map_err(|_| StoreError::InvalidData("snapshot text is not UTF-8"))?;
        Ok(Some(Snapshot { info, content }))
    }

    /// Restore snapshot `id` of `path` over the buffer whose current text is `current`: the
    /// current text is snapshotted first (reason [`SnapshotReason::BeforeRestore`], skipped when
    /// identical to the newest snapshot) so the restore can be undone, then the snapshot text is
    /// returned. `None` when the snapshot does not exist (nothing is stored then).
    ///
    /// # Errors
    /// See [`Store::history_snapshot`] and [`Store::history_load`].
    pub fn history_restore(
        &mut self,
        path: &Path,
        id: i64,
        current: &str,
        now: UnixSeconds,
    ) -> Result<Option<String>, StoreError> {
        let Some(snapshot) = self.history_load(id)? else {
            return Ok(None);
        };
        self.history_snapshot(path, current, SnapshotReason::BeforeRestore, now)?;
        Ok(Some(snapshot.content))
    }

    /// Delete snapshots older than `retention_days` days (0 keeps everything) and the blobs no
    /// snapshot refers to. Returns the number of snapshots deleted.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn history_purge(
        &mut self,
        now: UnixSeconds,
        retention_days: u32,
    ) -> Result<usize, StoreError> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = now.saturating_sub(i64::from(retention_days) * SECONDS_PER_DAY);
        let tx = self.conn.transaction()?;
        let deleted = tx.execute(
            "DELETE FROM history_snapshots WHERE created_at < ?1",
            [cutoff],
        )?;
        tx.execute(
            "DELETE FROM history_blobs WHERE hash NOT IN (SELECT hash FROM history_snapshots)",
            [],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    /// Make the history of `from` the history of `to` (the file was renamed or moved).
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn history_move(&self, from: &Path, to: &Path) -> Result<(), StoreError> {
        let to_text = path_text(to)?;
        self.conn.execute(
            "UPDATE history_snapshots SET key = ?1, path = ?2 WHERE key = ?3",
            params![key_of(to_text), to_text, path_key(from)?],
        )?;
        Ok(())
    }
}
