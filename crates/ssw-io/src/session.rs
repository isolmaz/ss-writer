//! Session restore (spec §6): windows with their placement, tabs (file path or untitled id),
//! per-tab workspace roots and the active tab/window; recent files and folders (most recently
//! used first, 20 of each kind); per-file caret / scroll / fold state; and the backups of
//! untitled buffers.

use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::store::{Store, StoreError, UnixSeconds, key_of, path_key, path_text};

/// How many recent files (and, separately, recent folders) are remembered.
pub const RECENT_LIMIT: usize = 20;

/// Where a window sits on the virtual desktop (physical pixels, may be negative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowPlacement {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

/// What a tab shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TabDocument {
    /// A file on disk.
    File(PathBuf),
    /// An unsaved buffer; its text is kept with [`Store::untitled_put`].
    Untitled(String),
}

/// One tab of a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionTab {
    pub document: TabDocument,
    /// The workspace root of the tab (spec §5: one root per tab).
    pub workspace: Option<PathBuf>,
}

/// One window of the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionWindow {
    pub placement: WindowPlacement,
    pub tabs: Vec<SessionTab>,
    /// Index into `tabs`.
    pub active_tab: Option<usize>,
}

/// Everything restored at startup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub windows: Vec<SessionWindow>,
    /// Index into `windows`: the window that was last active.
    pub active_window: Option<usize>,
}

/// Kind of a recent item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecentKind {
    File,
    Folder,
}

impl RecentKind {
    const fn code(self) -> i64 {
        match self {
            RecentKind::File => 0,
            RecentKind::Folder => 1,
        }
    }
}

/// An entry of the recent files / folders list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentItem {
    pub path: PathBuf,
    pub opened_at: UnixSeconds,
}

/// Caret, scroll and folds remembered for a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ViewState {
    /// Caret byte offset.
    pub caret: usize,
    /// Byte offset of the block at the top of the viewport.
    pub scroll_anchor: usize,
    /// Distance in DIPs from the top of the anchor block to the top of the viewport.
    pub scroll_delta: f32,
    /// Byte offsets of the headings that are folded.
    pub folds: Vec<usize>,
}

impl Store {
    /// Replace the stored session. Backups of untitled buffers no tab refers to any more are
    /// deleted.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] for a path that is not valid Unicode and
    /// [`StoreError::Sqlite`] on database failure; nothing is changed in either case.
    pub fn save_session(&mut self, session: &Session) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM session_windows", [])?;
        for (w, window) in session.windows.iter().enumerate() {
            let p = window.placement;
            tx.execute(
                "INSERT INTO session_windows (id, x, y, width, height, maximized, active)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    w as i64,
                    p.x,
                    p.y,
                    p.width,
                    p.height,
                    p.maximized,
                    session.active_window == Some(w)
                ],
            )?;
            for (t, tab) in window.tabs.iter().enumerate() {
                let (path, untitled) = match &tab.document {
                    TabDocument::File(path) => (Some(path_text(path)?), None),
                    TabDocument::Untitled(id) => (None, Some(id.as_str())),
                };
                let workspace = tab.workspace.as_deref().map(path_text).transpose()?;
                tx.execute(
                    "INSERT INTO session_tabs (window_id, position, path, untitled_id, workspace, active)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        w as i64,
                        t as i64,
                        path,
                        untitled,
                        workspace,
                        window.active_tab == Some(t)
                    ],
                )?;
            }
        }
        tx.execute(
            "DELETE FROM untitled_buffers WHERE id NOT IN
             (SELECT untitled_id FROM session_tabs WHERE untitled_id IS NOT NULL)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The stored session (empty when none was saved).
    ///
    /// # Errors
    /// [`StoreError::InvalidData`] for a tab row without a document and
    /// [`StoreError::Sqlite`] on database failure.
    pub fn load_session(&self) -> Result<Session, StoreError> {
        let mut windows = self.conn.prepare(
            "SELECT id, x, y, width, height, maximized, active
             FROM session_windows ORDER BY id",
        )?;
        let mut tabs = self.conn.prepare(
            "SELECT path, untitled_id, workspace, active
             FROM session_tabs WHERE window_id = ?1 ORDER BY position",
        )?;
        let mut session = Session::default();
        let mut window_rows = windows.query([])?;
        while let Some(row) = window_rows.next()? {
            let window_id: i64 = row.get(0)?;
            let placement = WindowPlacement {
                x: row.get(1)?,
                y: row.get(2)?,
                width: row.get(3)?,
                height: row.get(4)?,
                maximized: row.get(5)?,
            };
            if row.get::<_, bool>(6)? && session.active_window.is_none() {
                session.active_window = Some(session.windows.len());
            }
            let mut window = SessionWindow {
                placement,
                tabs: Vec::new(),
                active_tab: None,
            };
            let mut tab_rows = tabs.query([window_id])?;
            while let Some(tab_row) = tab_rows.next()? {
                let path: Option<String> = tab_row.get(0)?;
                let untitled: Option<String> = tab_row.get(1)?;
                let workspace: Option<String> = tab_row.get(2)?;
                let document = match (path, untitled) {
                    (Some(path), _) => TabDocument::File(PathBuf::from(path)),
                    (None, Some(id)) => TabDocument::Untitled(id),
                    (None, None) => return Err(StoreError::InvalidData("tab without document")),
                };
                if tab_row.get::<_, bool>(3)? && window.active_tab.is_none() {
                    window.active_tab = Some(window.tabs.len());
                }
                window.tabs.push(SessionTab {
                    document,
                    workspace: workspace.map(PathBuf::from),
                });
            }
            session.windows.push(window);
        }
        Ok(session)
    }

    /// Mark `path` as the most recently used item of `kind`. Only the newest
    /// [`RECENT_LIMIT`] items of each kind are kept.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn recent_add(
        &mut self,
        kind: RecentKind,
        path: &Path,
        now: UnixSeconds,
    ) -> Result<(), StoreError> {
        let text = path_text(path)?;
        let key = key_of(text);
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM recent_items WHERE kind = ?1 AND key = ?2",
            params![kind.code(), key],
        )?;
        tx.execute(
            "INSERT INTO recent_items (kind, key, path, opened_at) VALUES (?1, ?2, ?3, ?4)",
            params![kind.code(), key, text, now],
        )?;
        tx.execute(
            "DELETE FROM recent_items WHERE kind = ?1 AND seq NOT IN
             (SELECT seq FROM recent_items WHERE kind = ?1 ORDER BY seq DESC LIMIT ?2)",
            params![kind.code(), RECENT_LIMIT as i64],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Recent items of `kind`, most recent first.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn recent_items(&self, kind: RecentKind) -> Result<Vec<RecentItem>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT path, opened_at FROM recent_items WHERE kind = ?1 ORDER BY seq DESC",
        )?;
        let items = stmt
            .query_map([kind.code()], |row| {
                Ok(RecentItem {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    opened_at: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items)
    }

    /// Forget one recent item (for example because the file no longer exists).
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn recent_remove(&self, kind: RecentKind, path: &Path) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM recent_items WHERE kind = ?1 AND key = ?2",
            params![kind.code(), path_key(path)?],
        )?;
        Ok(())
    }

    /// Forget every recent item of `kind`.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn recent_clear(&self, kind: RecentKind) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM recent_items WHERE kind = ?1", [kind.code()])?;
        Ok(())
    }

    /// Remember the view state of `path`.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn view_state_put(
        &self,
        path: &Path,
        state: &ViewState,
        now: UnixSeconds,
    ) -> Result<(), StoreError> {
        let text = path_text(path)?;
        let folds = format!(
            "[{}]",
            state
                .folds
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        self.conn.execute(
            "INSERT INTO file_view_state
                 (key, path, caret, scroll_anchor, scroll_delta, folds, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET
                 path = excluded.path, caret = excluded.caret,
                 scroll_anchor = excluded.scroll_anchor, scroll_delta = excluded.scroll_delta,
                 folds = excluded.folds, updated_at = excluded.updated_at",
            params![
                key_of(text),
                text,
                state.caret as i64,
                state.scroll_anchor as i64,
                f64::from(state.scroll_delta),
                folds,
                now
            ],
        )?;
        Ok(())
    }

    /// The remembered view state of `path`, if any.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`], [`StoreError::Json`] (stored folds are not a JSON array of
    /// offsets) or [`StoreError::Sqlite`].
    pub fn view_state(&self, path: &Path) -> Result<Option<ViewState>, StoreError> {
        let row = self
            .conn
            .query_row(
                "SELECT caret, scroll_anchor, scroll_delta, folds FROM file_view_state WHERE key = ?1",
                [path_key(path)?],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(caret, scroll_anchor, scroll_delta, folds)| {
            Ok::<_, StoreError>(ViewState {
                caret: caret as usize,
                scroll_anchor: scroll_anchor as usize,
                scroll_delta: scroll_delta as f32,
                folds: serde_json::from_str(&folds)?,
            })
        })
        .transpose()
    }

    /// Forget the view state of `path`.
    ///
    /// # Errors
    /// [`StoreError::NonUnicodePath`] or [`StoreError::Sqlite`].
    pub fn view_state_forget(&self, path: &Path) -> Result<(), StoreError> {
        self.conn.execute(
            "DELETE FROM file_view_state WHERE key = ?1",
            [path_key(path)?],
        )?;
        Ok(())
    }

    /// Back up the text of the untitled buffer `id`.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn untitled_put(
        &self,
        id: &str,
        content: &str,
        now: UnixSeconds,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO untitled_buffers (id, content, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 content = excluded.content, updated_at = excluded.updated_at",
            params![id, content, now],
        )?;
        Ok(())
    }

    /// The backed-up text of the untitled buffer `id`.
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn untitled_get(&self, id: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT content FROM untitled_buffers WHERE id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Delete the backup of the untitled buffer `id` (it was saved or discarded).
    ///
    /// # Errors
    /// [`StoreError::Sqlite`] on database failure.
    pub fn untitled_delete(&self, id: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM untitled_buffers WHERE id = ?1", [id])?;
        Ok(())
    }
}
