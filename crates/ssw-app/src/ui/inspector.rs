//! The inspector feed (spec §8): the heading tree, the statistics, the file details and the
//! backlinks of the active document, pushed into [`ssw_ui::shell::inspector::Inspector`].
//!
//! The widget only draws what it is given. [`InspectorFeeds`] computes the data from the
//! active tab of a window, remembers what each window's inspector shows so nothing is
//! recomputed while the document, the selection and the index stay the same, and maps the
//! panel's events (a clicked heading or backlink) back to the editor.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

use ssw_core::edit::Selection;
use ssw_core::i18n::{Arg, Localizer};
use ssw_core::stats;
use ssw_core::text::TextRange;
use ssw_core::toc::Toc;
use ssw_io::encoding::LineEnding;
use ssw_io::fs::{FileSystem, FileTime, RealFs};
use ssw_io::index::IndexOptions;
use ssw_io::recovery::now_millis;
use ssw_io::store::Store;
use ssw_ui::shell::inspector::{Backlink, FileInfo, InfoStats, InspectorEvent};
use ssw_win::locale::Locale;

use super::controller::{Controller, WindowCell};
use super::shell_view::ShellView;
use crate::documents::{DocEntry, DocMode};
use crate::workers::{JobKey, JobKind, JobSpec, WindowId};

/// What the inspector feed reads besides the window.
#[derive(Clone, Copy)]
pub struct FeedEnv<'a> {
    /// Messages of the active locale.
    pub i18n: &'a Localizer,
    /// Number and date formatting of the active locale.
    pub locale: &'a Locale,
    /// The file system the file details are read from.
    pub fs: &'a dyn FileSystem,
    /// The state database holding the workspace index.
    pub store: &'a Store,
    /// Reading speed of the reading-time estimate (words per minute).
    pub reading_wpm: u32,
    /// Changes whenever the workspace index changed (backlinks may differ).
    pub index_epoch: u64,
}

impl std::fmt::Debug for FeedEnv<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeedEnv")
            .field("reading_wpm", &self.reading_wpm)
            .field("index_epoch", &self.index_epoch)
            .finish_non_exhaustive()
    }
}

/// The heading tree of `entry`: empty for plain text and diagrams (they have no headings).
#[must_use]
pub fn toc_of(entry: &DocEntry) -> Toc {
    if entry.mode() != DocMode::Markdown {
        return Toc::default();
    }
    let document = entry.document().borrow();
    Toc::build(document.ast(), &document.text())
}

/// The statistics of `entry`: Markdown statistics for Markdown, plain-text statistics for
/// everything else, plus the statistics of the selection when something is selected.
#[must_use]
pub fn info_stats(entry: &DocEntry, reading_wpm: u32) -> InfoStats {
    let document = entry.document().borrow();
    let source = document.text();
    let selection = document.primary_selection().range();
    let markdown = entry.mode() == DocMode::Markdown;
    let whole = if markdown {
        stats::compute(&source, document.ast())
    } else {
        stats::compute_plain(&source)
    };
    let selected = (!selection.is_empty()).then(|| {
        if markdown {
            stats::for_range(&source, document.ast(), selection)
        } else {
            stats::for_range_plain(&source, selection)
        }
    });
    InfoStats {
        document: whole,
        selection: selected,
        reading_wpm,
    }
}

/// The file details of `entry`; `None` for a document without a file. Size and dates read
/// "unavailable" when the file cannot be read any more (deleted behind our back).
#[must_use]
pub fn file_info(entry: &DocEntry, env: &FeedEnv<'_>) -> Option<FileInfo> {
    let path = entry.path()?;
    let i18n = env.i18n;
    let unavailable = i18n.text("panels-inspector-unavailable");
    let meta = env.fs.metadata(&path).ok();
    let date = |time: FileTime| {
        civil_date(time)
            .and_then(|(year, month, day)| env.locale.format_date_short(year, month, day))
            .unwrap_or_else(|| unavailable.clone())
    };
    let format = entry.format();
    let encoding = if format.bom {
        i18n.format(
            "window-encoding-bom",
            &[("name", Arg::Str(format.encoding.name()))],
        )
    } else {
        format.encoding.name().to_owned()
    };
    let line_endings = i18n.text(match format.line_ending {
        LineEnding::Lf => "panels-eol-lf",
        LineEnding::Crlf => "panels-eol-crlf",
        LineEnding::Cr => "panels-eol-cr",
    });
    Some(FileInfo {
        path: path.to_string_lossy().into_owned(),
        size: meta.as_ref().map_or_else(
            || unavailable.clone(),
            |meta| env.locale.format_file_size(meta.size),
        ),
        created: meta
            .as_ref()
            .map_or_else(|| unavailable.clone(), |meta| date(meta.created)),
        modified: meta
            .as_ref()
            .map_or_else(|| unavailable.clone(), |meta| date(meta.modified)),
        encoding,
        line_endings,
    })
}

/// The (UTC) calendar date of `time`; `None` outside the years 1601 to 9999.
fn civil_date(time: FileTime) -> Option<(u16, u16, u16)> {
    let days = time.unix_millis().div_euclid(86_400_000);
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
    if !(1601..=9999).contains(&year) {
        return None;
    }
    // The range check bounds the year; the algorithm yields months 1..=12 and days 1..=31.
    Some((year as u16, month as u16, day as u16))
}

/// The documents of the workspace `root` that link to `file`, as the inspector lists them: the
/// file stem as the title and the path relative to the root. Sorted by path; empty when the
/// index has nothing (not built yet, or the file is not part of the workspace).
#[must_use]
pub fn backlinks_of(store: &Store, root: &Path, file: &Path) -> Vec<(Backlink, PathBuf)> {
    let Ok(mut sources) = store.index_backlinks(root, file) else {
        return Vec::new();
    };
    sources.sort_by_key(|path| path.to_string_lossy().to_lowercase());
    sources
        .into_iter()
        .map(|source| {
            let title = source
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let relative = source.strip_prefix(root).unwrap_or(&source);
            let shown = Backlink {
                title,
                path: relative.to_string_lossy().into_owned(),
            };
            (shown, source)
        })
        .collect()
}

/// What one window's inspector was fed from; equal keys mean nothing needs recomputing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FeedKey {
    tab: u64,
    version: u64,
    /// The selected range; `None` for a caret (moving the caret changes nothing but the
    /// current section).
    selection: Option<(usize, usize)>,
    path: Option<PathBuf>,
    reading_wpm: u32,
    index_epoch: u64,
}

#[derive(Debug)]
struct WindowFeed {
    key: FeedKey,
    toc: Toc,
    backlinks: Vec<PathBuf>,
}

/// The inspector state of every window.
#[derive(Debug, Default)]
pub struct InspectorFeeds {
    windows: HashMap<WindowId, WindowFeed>,
    /// The workspace roots whose index was started this session (lower-cased paths).
    indexed: HashSet<String>,
}

impl InspectorFeeds {
    /// Brings the inspector of `view` up to date with its active tab. The caller calls this
    /// only while the inspector is shown (`Controller::sync_inspector` forgets the window
    /// otherwise, so the next call after it is shown recomputes).
    pub fn sync(&mut self, window: WindowId, view: &mut ShellView, env: &FeedEnv<'_>) {
        let Some((tab_key, entry, workspace)) = view
            .active_tab()
            .map(|tab| (tab.key, Rc::clone(&tab.entry), tab.workspace.clone()))
        else {
            if self.windows.remove(&window).is_some() {
                let inspector = view.inspector_mut();
                inspector.set_toc(&Toc::default());
                inspector.set_file(None);
                inspector.set_backlinks(&[], env.i18n);
            }
            return;
        };
        let (version, selection) = {
            let document = entry.document().borrow();
            (document.version(), document.primary_selection())
        };
        let range = selection.range();
        let key = FeedKey {
            tab: tab_key,
            version,
            selection: (!range.is_empty()).then_some((range.start, range.end)),
            path: entry.path(),
            reading_wpm: env.reading_wpm,
            index_epoch: env.index_epoch,
        };
        let fresh = self
            .windows
            .get(&window)
            .is_some_and(|feed| feed.key == key);
        if !fresh {
            let toc = toc_of(&entry);
            let backlinks = match (&workspace, entry.path()) {
                (Some(root), Some(path)) => backlinks_of(env.store, root, &path),
                _ => Vec::new(),
            };
            let shown: Vec<Backlink> = backlinks.iter().map(|(shown, _)| shown.clone()).collect();
            let inspector = view.inspector_mut();
            inspector.set_toc(&toc);
            inspector.set_stats(&info_stats(&entry, env.reading_wpm), env.i18n);
            inspector.set_file(file_info(&entry, env).as_ref());
            inspector.set_backlinks(&shown, env.i18n);
            self.windows.insert(
                window,
                WindowFeed {
                    key,
                    toc,
                    backlinks: backlinks.into_iter().map(|(_, path)| path).collect(),
                },
            );
        }
        view.inspector_mut().set_caret(selection.head);
    }

    /// The source range of the heading `index` of the Contents tab of `window`.
    #[must_use]
    pub fn heading_range(&self, window: WindowId, index: usize) -> Option<TextRange> {
        self.windows
            .get(&window)?
            .toc
            .entries()
            .get(index)
            .map(|entry| entry.range)
    }

    /// The file behind the backlink `index` of the Info tab of `window`.
    #[must_use]
    pub fn backlink_path(&self, window: WindowId, index: usize) -> Option<PathBuf> {
        self.windows.get(&window)?.backlinks.get(index).cloned()
    }

    /// Forgets `window` (closed).
    pub fn forget(&mut self, window: WindowId) {
        self.windows.remove(&window);
    }

    /// Records that the index of `root` is being built; `None` when that was already done or
    /// is under way. The number identifies the job.
    fn mark_indexing(&mut self, root: &Path) -> Option<u64> {
        let key = root.to_string_lossy().to_lowercase();
        if !self.indexed.insert(key) {
            return None;
        }
        Some(self.indexed.len() as u64)
    }

    /// Allows indexing `root` again (the job could not be started).
    fn unmark_indexing(&mut self, root: &Path) {
        self.indexed.remove(&root.to_string_lossy().to_lowercase());
    }
}

impl Controller {
    /// Brings the inspector of window `id` up to date; builds the workspace index of the active
    /// tab's folder in the background the first time (backlinks come from it).
    pub(super) fn sync_inspector(&self, id: WindowId, view: &mut ShellView) {
        if !view.layout().inspector_visible() {
            self.inspector_feeds.borrow_mut().forget(id);
            return;
        }
        if let Some(root) = view.active_tab().and_then(|tab| tab.workspace.clone()) {
            self.ensure_index(id, &root);
        }
        let locale = Locale::user_default();
        let docs = self.docs();
        let store = docs.env().store.borrow();
        let index_epoch = self.index_epoch.get();
        let env = FeedEnv {
            i18n: self.i18n().as_ref(),
            locale: &locale,
            fs: docs.env().fs.as_ref(),
            store: &store,
            reading_wpm: self.settings().editor.reading_speed_wpm,
            index_epoch,
        };
        self.inspector_feeds.borrow_mut().sync(id, view, &env);
    }

    /// Starts the first full index of `root` in the background (once per session).
    pub(super) fn ensure_index(&self, window: WindowId, root: &Path) {
        let Some(number) = self.inspector_feeds.borrow_mut().mark_indexing(root) else {
            return;
        };
        let Ok(core) = self.core().try_borrow() else {
            self.inspector_feeds.borrow_mut().unmark_indexing(root);
            return;
        };
        let database = core.paths.state_db();
        let job_root = root.to_path_buf();
        let epoch = Rc::clone(&self.index_epoch);
        let spec = JobSpec {
            kind: JobKind::Index,
            window: Some(window),
            key: JobKey(number),
            generation: 0,
        };
        let submitted = core.workers.submit(
            spec,
            move |_cancel| {
                ssw_win::perf::mark_latest(crate::perf::INDEX_STARTED);
                let cancel = AtomicBool::new(false);
                let now = i64::try_from(now_millis() / 1000).unwrap_or(i64::MAX);
                let fs = RealFs;
                // OneDrive placeholders are listed, never read (spec §4.3).
                let placeholder =
                    |path: &Path| fs.metadata(path).is_ok_and(|meta| meta.is_placeholder());
                let indexed = Store::open(&database)
                    .ok()
                    .and_then(|mut store| {
                        store
                            .index_workspace(
                                &job_root,
                                &IndexOptions::default(),
                                &placeholder,
                                &cancel,
                                now,
                            )
                            .ok()
                    })
                    .is_some();
                ssw_win::perf::mark_latest(crate::perf::INDEX_FINISHED);
                indexed
            },
            move |result| {
                if matches!(result, Ok(true)) {
                    epoch.set(epoch.get() + 1);
                }
            },
        );
        if submitted.is_err() {
            self.inspector_feeds.borrow_mut().unmark_indexing(root);
        }
    }

    /// Carries out what the user did in the inspector panel of window `cell`: a clicked heading
    /// moves the caret there and scrolls it into view, a clicked backlink opens the document.
    pub(super) fn inspector_event(&self, cell: &Rc<WindowCell>, event: InspectorEvent) {
        match event {
            InspectorEvent::HeadingClicked { index, offset } => {
                let start = self
                    .inspector_feeds
                    .borrow()
                    .heading_range(cell.id, index)
                    .map_or(offset, |range| range.start);
                let mut view = cell.view.borrow_mut();
                if let Some(editor) = view.active_editor_mut() {
                    editor.go_to(Selection::caret(start));
                }
            }
            InspectorEvent::BacklinkClicked(index) => {
                let path = self.inspector_feeds.borrow().backlink_path(cell.id, index);
                if let Some(path) = path {
                    self.open_path(cell, &path, false);
                }
            }
            InspectorEvent::TabChanged(_) | InspectorEvent::LevelFilterChanged(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn time_of(unix_millis: i64) -> FileTime {
        // FILETIME ticks are 100 ns since 1601-01-01; the Unix epoch is 11 644 473 600 s later.
        let ticks = (unix_millis + 11_644_473_600_000) * 10_000;
        FileTime(u64::try_from(ticks).unwrap())
    }

    #[test]
    fn civil_dates_follow_the_calendar() {
        assert_eq!(civil_date(time_of(0)), Some((1970, 1, 1)));
        // 2026-10-10 12:00 UTC.
        assert_eq!(civil_date(time_of(1_791_633_600_000)), Some((2026, 10, 10)));
        // A leap day.
        assert_eq!(civil_date(time_of(951_782_400_000)), Some((2000, 2, 29)));
        // Just before and after midnight.
        assert_eq!(civil_date(time_of(86_399_999)), Some((1970, 1, 1)));
        assert_eq!(civil_date(time_of(86_400_000)), Some((1970, 1, 2)));
        // Before 1970.
        assert_eq!(civil_date(time_of(-86_400_000)), Some((1969, 12, 31)));
        // The FILETIME epoch itself and below.
        assert_eq!(civil_date(FileTime(0)), Some((1601, 1, 1)));
    }
}
