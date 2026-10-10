//! The version history in the window (spec §4.5).
//!
//! [`HistoryHost`] puts the `ssw-ui` [`HistoryPanel`] over the editor area of one window and
//! routes input to it while it is open. The controller side is [`Controller::open_history`]:
//! it lists the snapshots of the active document (`ssw_io::history`); a selected version is
//! diffed against the current text on a worker (`ssw_io::diff`); Restore snapshots the current
//! text first and then replaces it with the version in one undo step, so a restore can be
//! undone with Ctrl+Z or reverted from the history itself.
//!
//! The conflict banner's Compare opens the panel with one more entry ([`DISK_ENTRY_ID`]) that
//! holds the file's text on disk; restoring it is "Load theirs".

use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use ssw_core::i18n::{Arg, Localizer};
use ssw_io::diff::{DEFAULT_CONTEXT, Hunk, LineOp, diff_lines};
use ssw_ui::access::AccessTree;
use ssw_ui::canvas::Canvas;
use ssw_ui::geometry::Rect;
use ssw_ui::input::{Event, PointerEvent, PointerEventKind};
use ssw_ui::shell::history::{
    DiffHunk, DiffLine, DiffLineKind, HistoryEntry, HistoryEvent, HistoryPanel,
};
use ssw_ui::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;
use ssw_win::locale::Locale;

use super::controller::{Controller, WindowCell};
use super::overlays::OverlayEnv;
use crate::documents::{DocEntry, DocError, Resolution, replace_text};
use crate::workers::{JobKey, JobKind, JobSpec, WindowId};

/// The id of the entry that stands for the file as it is on disk. Snapshot ids come from a
/// SQLite row id and start at 1, so 0 never names a snapshot.
pub const DISK_ENTRY_ID: i64 = 0;

/// Job keys of the diffs start here (above the keys of the other jobs).
const DIFF_KEY_BASE: u64 = 1 << 62;

/// Generation of the diffs (a newer diff of a window supersedes an older one).
static DIFF_GENERATION: AtomicU64 = AtomicU64::new(1);

/// What the open panel is about.
#[derive(Debug)]
struct Session {
    /// The document whose versions are listed.
    entry: Rc<DocEntry>,
    /// The file's text on disk, when the panel was opened to compare with it.
    disk: Option<String>,
}

/// The history panel of one window: layout, painting, input and accessibility while it is open.
pub struct HistoryHost {
    localizer: Rc<Localizer>,
    panel: Option<HistoryPanel>,
    session: Option<Session>,
    bounds: Rect,
    focus_request: Option<WidgetId>,
    refocus: bool,
    pointer_inside: bool,
}

impl fmt::Debug for HistoryHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryHost")
            .field("open", &self.panel.is_some())
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl HistoryHost {
    /// A host with no panel open.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>) -> Self {
        Self {
            localizer,
            panel: None,
            session: None,
            bounds: Rect::ZERO,
            focus_request: None,
            refocus: false,
            pointer_inside: false,
        }
    }

    /// Whether the panel is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.panel.is_some()
    }

    /// The panel, while it is open.
    #[must_use]
    pub fn panel(&self) -> Option<&HistoryPanel> {
        self.panel.as_ref()
    }

    /// The document whose versions are listed.
    #[must_use]
    pub fn entry(&self) -> Option<&Rc<DocEntry>> {
        self.session.as_ref().map(|session| &session.entry)
    }

    /// The file's text on disk, when the panel compares with it.
    #[must_use]
    pub fn disk_text(&self) -> Option<&str> {
        self.session
            .as_ref()
            .and_then(|session| session.disk.as_deref())
    }

    /// Opens the panel over `entry` with `entries` (newest first). With `disk`, the entry
    /// [`DISK_ENTRY_ID`] must be the first one: it is selected, so its comparison is requested.
    pub fn open(&mut self, entry: Rc<DocEntry>, entries: Vec<HistoryEntry>, disk: Option<String>) {
        let mut panel = HistoryPanel::new(Rc::clone(&self.localizer), Motion::default());
        panel.set_entries(entries);
        if disk.is_some() {
            panel.select(DISK_ENTRY_ID);
        }
        self.focus_request = Some(panel.focus_id());
        self.panel = Some(panel);
        self.session = Some(Session { entry, disk });
        self.pointer_inside = false;
    }

    /// Closes the panel; the keyboard focus goes back to the editor.
    pub fn close(&mut self) {
        if self.panel.take().is_some() {
            self.refocus = true;
        }
        self.session = None;
        self.focus_request = None;
        self.pointer_inside = false;
    }

    /// Shows the changes of version `id` (see [`HistoryPanel::set_diff`]); false when the
    /// panel is closed or `id` is no longer the selected version.
    pub fn set_diff(&mut self, id: i64, hunks: Vec<DiffHunk>) -> bool {
        self.panel
            .as_mut()
            .is_some_and(|panel| panel.set_diff(id, hunks))
    }

    /// What the user did in the panel since the last call.
    pub fn take_events(&mut self) -> Vec<HistoryEvent> {
        self.panel
            .as_mut()
            .map(HistoryPanel::take_events)
            .unwrap_or_default()
    }

    /// The control that should get the keyboard focus after the next layout (the panel was
    /// just opened); once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// Whether the focus should go back to the editor (the panel closed); once.
    pub fn take_refocus(&mut self) -> bool {
        std::mem::take(&mut self.refocus)
    }

    /// Places the panel over `rect` (the editor area below the banners).
    pub fn arrange(&mut self, cx: &mut LayoutCx, rect: Rect) {
        self.bounds = rect;
        if let Some(panel) = self.panel.as_mut() {
            let id = panel.id();
            panel.arrange(&mut cx.for_widget(id), rect);
        }
    }

    /// Paints the panel when it is open.
    pub fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if let Some(panel) = &self.panel {
            panel.paint(&cx.for_widget(panel.id()), canvas);
        }
    }

    /// Describes the panel when it is open; returns its node id.
    pub fn access(&self, cx: &AccessCx, out: &mut AccessTree) -> Option<WidgetId> {
        self.panel.as_ref().map(|panel| {
            panel.access(&cx.for_widget(panel.id()), out);
            panel.id()
        })
    }

    /// Routes `event` to the open panel. Pointer events count when they are over the panel (or
    /// the panel captured the pointer); keys and text are the panel's alone while it is open,
    /// so nothing typed reaches the editor underneath. Timers and animation frames are offered
    /// without being consumed.
    pub fn event(&mut self, event: &Event, env: &mut OverlayEnv<'_>) -> EventResult {
        let Some(panel) = self.panel.as_mut() else {
            return EventResult::ignored();
        };
        let inside = event
            .position()
            .is_some_and(|point| self.bounds.contains(point));
        let captured = env.capture.owner().is_some_and(|id| panel.owns(id));
        let wanted = match event {
            Event::Pointer(_) | Event::Wheel(_) => inside || captured,
            Event::Key(_)
            | Event::Text(_)
            | Event::Ime(_)
            | Event::Timer(_)
            | Event::AnimationFrame(_) => true,
            Event::Focus(_) => false,
        };
        if !wanted {
            if let Event::Pointer(pointer) = event
                && self.pointer_inside
            {
                self.pointer_inside = false;
                let leave = PointerEvent::mouse(PointerEventKind::Leave, None, pointer.position);
                route(panel, &Event::Pointer(leave), env);
            }
            return EventResult::ignored();
        }
        if matches!(event, Event::Pointer(_)) {
            self.pointer_inside = inside;
        }
        let result = route(panel, event, env);
        match event {
            Event::Timer(_) | Event::AnimationFrame(_) => result,
            _ => EventResult::handled(),
        }
    }
}

/// Runs `panel.event` with a fresh context; a focus request is granted.
fn route(panel: &mut HistoryPanel, event: &Event, env: &mut OverlayEnv<'_>) -> EventResult {
    let id = panel.id();
    let result = {
        let mut cx = EventCx::new(
            id,
            env.theme,
            env.scale,
            env.text,
            env.focus,
            env.capture,
            env.requests,
        );
        panel.event(&mut cx, event)
    };
    if result.request_focus {
        env.focus.focus(id);
    }
    result
}

/// The hunks that turn the version `old` into the current text `new`, in the panel's shape:
/// lines only in the version are removed, lines only in the current text are added.
#[must_use]
pub fn diff_hunks(old: &str, new: &str) -> Vec<DiffHunk> {
    diff_lines(old, new, DEFAULT_CONTEXT)
        .into_iter()
        .map(panel_hunk)
        .collect()
}

fn panel_hunk(hunk: Hunk) -> DiffHunk {
    DiffHunk {
        old_start: hunk.old_start,
        old_count: hunk.old_count,
        new_start: hunk.new_start,
        new_count: hunk.new_count,
        lines: hunk
            .lines
            .into_iter()
            .map(|line| DiffLine {
                kind: match line.op {
                    LineOp::Equal => DiffLineKind::Context,
                    LineOp::Insert => DiffLineKind::Added,
                    LineOp::Delete => DiffLineKind::Removed,
                },
                old_line: line.old_line,
                new_line: line.new_line,
                text: line.text,
            })
            .collect(),
    }
}

/// The calendar date and the time of day (UTC) of `unix_seconds`:
/// `(year, month, day, hour, minute)`.
fn civil_utc(unix_seconds: i64) -> (i64, u16, u16, u16, u16) {
    let days = unix_seconds.div_euclid(86_400);
    let seconds = unix_seconds.rem_euclid(86_400);
    // Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    // Every value is below its modulus, so the narrowing cannot fail.
    let narrow = |value: i64| u16::try_from(value).unwrap_or(u16::MAX);
    (
        year,
        narrow(month),
        narrow(day),
        narrow(seconds / 3600),
        narrow(seconds % 3600 / 60),
    )
}

/// The time of a snapshot as the list shows it: the short date of `locale` and the time of day
/// in UTC (`10/10/2026 14:05 UTC`).
#[must_use]
pub fn snapshot_time(locale: &Locale, i18n: &Localizer, unix_seconds: i64) -> String {
    let (year, month, day, hour, minute) = civil_utc(unix_seconds);
    let date = u16::try_from(year)
        .ok()
        .and_then(|year| locale.format_date_short(year, month, day))
        .unwrap_or_else(|| format!("{year:04}-{month:02}-{day:02}"));
    i18n.format(
        "flows-history-time",
        &[
            ("date", Arg::Str(&date)),
            ("hour", Arg::Str(&format!("{hour:02}"))),
            ("minute", Arg::Str(&format!("{minute:02}"))),
        ],
    )
}

/// The worker key of the diffs of window `id`.
fn diff_key(id: WindowId) -> JobKey {
    JobKey(DIFF_KEY_BASE + id.0)
}

impl Controller {
    /// Opens the version history of the active document of window `window`: the saved versions
    /// of its file, newest first. With `compare_with_disk` (the text of the file on disk, from
    /// the conflict banner's Compare) the panel starts with an entry for the file as it is on
    /// disk, selected so that the changes against the current text are shown.
    pub fn open_history(&self, window: WindowId, compare_with_disk: Option<String>) {
        let Some(cell) = self.window(window) else {
            return;
        };
        let Some(entry) = cell
            .view
            .borrow()
            .active_tab()
            .map(|tab| Rc::clone(&tab.entry))
        else {
            return;
        };
        let entries = self.history_entries(&entry, compare_with_disk.as_deref());
        {
            let mut view = cell.view.borrow_mut();
            view.history_mut().open(entry, entries, compare_with_disk);
            view.requests_mut().invalidate_all();
        }
        cell.dirty.set(true);
        let events = cell.view.borrow_mut().history_mut().take_events();
        for event in events {
            self.history_event(&cell, event);
        }
    }

    /// The list entries of `entry`: the file on disk first (when given), then its snapshots.
    fn history_entries(&self, entry: &DocEntry, disk: Option<&str>) -> Vec<HistoryEntry> {
        let mut entries = Vec::new();
        if let Some(text) = disk {
            entries.push(HistoryEntry {
                id: DISK_ENTRY_ID,
                time: self.i18n().text("flows-history-on-disk"),
                size: text.len(),
            });
        }
        let Some(path) = entry.path() else {
            return entries;
        };
        let listed = self.docs().env().store.borrow().history_list(&path);
        match listed {
            Ok(snapshots) => {
                let locale = Locale::user_default();
                entries.extend(snapshots.into_iter().map(|snapshot| HistoryEntry {
                    id: snapshot.id,
                    time: snapshot_time(&locale, self.i18n(), snapshot.created_at),
                    size: snapshot.size,
                }));
            }
            Err(error) => self.report(&DocError::Store(error)),
        }
        entries
    }

    /// What the panel of `cell` is about: the document and the file's text on disk.
    fn history_session(&self, cell: &WindowCell) -> Option<(Rc<DocEntry>, Option<String>)> {
        let view = cell.view.borrow();
        let history = view.history();
        let entry = Rc::clone(history.entry()?);
        Some((entry, history.disk_text().map(str::to_owned)))
    }

    /// Carries out what the user did in the history panel of `cell`.
    pub(super) fn history_event(&self, cell: &Rc<WindowCell>, event: HistoryEvent) {
        match event {
            HistoryEvent::Selected(id) => self.show_diff(cell, id),
            HistoryEvent::Restore(id) => self.restore_version(cell, id),
            HistoryEvent::Close => self.close_history(cell),
        }
    }

    /// Closes the history panel of `cell`.
    pub fn close_history(&self, cell: &WindowCell) {
        let mut view = cell.view.borrow_mut();
        view.history_mut().close();
        view.requests_mut().invalidate_all();
        cell.dirty.set(true);
    }

    /// Computes the changes between version `id` and the current text on a worker; the panel
    /// shows them when they are ready.
    fn show_diff(&self, cell: &Rc<WindowCell>, id: i64) {
        let Some((entry, disk)) = self.history_session(cell) else {
            return;
        };
        let old = if id == DISK_ENTRY_ID {
            disk
        } else {
            let loaded = self.docs().env().store.borrow().history_load(id);
            match loaded {
                Ok(snapshot) => snapshot.map(|snapshot| snapshot.content),
                Err(error) => {
                    self.report(&DocError::Store(error));
                    return;
                }
            }
        };
        let Some(old) = old else {
            self.show_error(self.i18n().text("flows-history-missing"));
            return;
        };
        let new = entry.document().borrow().text();
        let spec = JobSpec {
            kind: JobKind::Parse,
            window: Some(cell.id),
            key: diff_key(cell.id),
            generation: DIFF_GENERATION.fetch_add(1, Ordering::Relaxed),
        };
        let target = Rc::clone(cell);
        // A refused job (the window is closing) has nothing left to show.
        let _ = self.core().borrow().workers.submit(
            spec,
            move |_| diff_hunks(&old, &new),
            // A diff that panicked on its worker (`Err`) has nothing to show.
            move |outcome| {
                outcome.into_iter().for_each(|hunks| {
                    let mut view = target.view.borrow_mut();
                    if view.history_mut().set_diff(id, hunks) {
                        view.requests_mut().invalidate_all();
                        target.dirty.set(true);
                    }
                });
            },
        );
    }

    /// Restores version `id`: the current text is snapshotted first, then replaced with the
    /// version as one undo step. The disk entry is "Load theirs". The panel closes when the
    /// document shows the version.
    fn restore_version(&self, cell: &Rc<WindowCell>, id: i64) {
        let Some((entry, _)) = self.history_session(cell) else {
            return;
        };
        let outcome = if id == DISK_ENTRY_ID {
            self.docs()
                .resolve_conflict(&entry, Resolution::LoadTheirs)
                .map(|()| true)
        } else {
            self.restore_snapshot(&entry, id)
        };
        match outcome {
            Ok(true) => self.close_history(cell),
            Ok(false) => self.show_error(self.i18n().text("flows-history-missing")),
            Err(error) => self.report(&error),
        }
    }

    /// Tells the user `message` in an error box (a failure that is not a [`DocError`]).
    pub(super) fn show_error(&self, message: String) {
        use ssw_ui::services::{MessageBoxButtons, MessageBoxIcon, MessageBoxOptions};
        let _ = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n().text("window-error-title"),
            message,
            buttons: MessageBoxButtons::Ok,
            icon: Some(MessageBoxIcon::Error),
        });
    }

    /// Replaces the text of `entry` with snapshot `id`; false when the snapshot is gone.
    fn restore_snapshot(&self, entry: &DocEntry, id: i64) -> Result<bool, DocError> {
        let path = entry.path().ok_or(DocError::Untitled)?;
        let current = entry.document().borrow().text();
        let env = self.docs().env();
        let now = env.clock.unix_seconds();
        let restored = env
            .store
            .borrow_mut()
            .history_restore(&path, id, &current, now)?;
        let Some(text) = restored else {
            return Ok(false);
        };
        replace_text(&mut entry.document().borrow_mut(), &text);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_utc_follows_the_calendar() {
        assert_eq!(civil_utc(0), (1970, 1, 1, 0, 0));
        // 2026-10-10 14:05:59 UTC.
        assert_eq!(civil_utc(1_791_641_159), (2026, 10, 10, 14, 5));
        // A leap day, and the second before a day ends.
        assert_eq!(civil_utc(951_782_400 + 86_399), (2000, 2, 29, 23, 59));
        // Before the epoch.
        assert_eq!(civil_utc(-1), (1969, 12, 31, 23, 59));
    }

    #[test]
    fn a_year_the_date_format_cannot_show_is_written_in_iso_order() {
        let i18n = crate::i18n::localizer_for("en");
        // 100000-01-01 00:00:00 UTC: the year does not fit the 16 bits of a short date.
        assert_eq!(
            snapshot_time(&Locale::user_default(), &i18n, 3_093_527_980_800),
            "100000-01-01 00:00 UTC"
        );
    }

    #[test]
    fn diff_hunks_mark_added_and_removed_lines() {
        let hunks = diff_hunks("a\nb\nc\n", "a\nB\nc\nd\n");
        assert_eq!(hunks.len(), 1);
        let kinds: Vec<(DiffLineKind, &str)> = hunks[0]
            .lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (DiffLineKind::Context, "a"),
                (DiffLineKind::Removed, "b"),
                (DiffLineKind::Added, "B"),
                (DiffLineKind::Context, "c"),
                (DiffLineKind::Added, "d"),
            ]
        );
        assert!(diff_hunks("same\n", "same\n").is_empty());
    }
}
