//! The inspector panel (spec §8): a **Contents** tab with the collapsible heading tree and a
//! **Info** tab with statistics, file information and backlinks.
//!
//! Contents: the heading tree of an [`ssw_core::toc::Toc`] with one hairline per indentation
//! level (the staircase), the current section highlighted, a level filter (H1…H6) and chevrons to
//! collapse subtrees. Clicking a heading (or pressing Enter) emits
//! [`InspectorEvent::HeadingClicked`]; the app scrolls the editor and flashes the heading.
//!
//! Info: statistics from [`ssw_core::stats`] (of the selection when there is one), the file's
//! details as the app formats them and the list of backlinks.
//!
//! The panel has these focus stops: the two tab buttons, the level filter (Contents only) and
//! the list.

use std::collections::HashSet;

use ssw_core::i18n::{Arg, Localizer};
use ssw_core::stats::Stats;
use ssw_core::toc::Toc;

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Event, FocusEvent, KeyEvent, PointerButton, PointerEvent, PointerEventKind, vk,
};
use crate::text::FontWeight;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::scroll::{Axis, BAR_WIDTH, ScrollBar, scroll_info};
use crate::widgets::{
    Control, FONT_SIZE, draw_centered, draw_left_centered, focus_ring, glyph, layout_line, ui_style,
};

/// Height of the tab header.
const HEADER_HEIGHT: f32 = 40.0;
/// Height of the level filter row.
const LEVEL_ROW_HEIGHT: f32 = 36.0;
/// Height of a heading row and of a plain info row.
const ROW_HEIGHT: f32 = 28.0;
/// Height of an info section heading.
const SECTION_HEIGHT: f32 = 36.0;
/// Height of a backlink row (title and path).
const LINK_HEIGHT: f32 = 48.0;
/// Indentation per heading depth.
const INDENT: f32 = 16.0;
/// Width of the chevron zone.
const CHEVRON: f32 = 16.0;
/// Inset of the content from the panel edges.
const PADDING: f32 = 12.0;
/// Size of one level filter button.
const LEVEL_BUTTON: Size = Size::new(32.0, 24.0);
/// Gap between the level buttons.
const LEVEL_GAP: f32 = 4.0;
/// Number of heading levels.
const LEVELS: u8 = 6;

/// The panel's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InspectorTab {
    Contents,
    Info,
}

/// Localized strings of the inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectorStrings {
    /// Accessible name of the panel.
    pub label: String,
    pub contents_tab: String,
    pub info_tab: String,
    /// Accessible name of the level filter.
    pub level_filter: String,
    /// `H1`…`H6`.
    pub levels: [String; 6],
    /// Shown when the document has no headings.
    pub no_headings: String,
    pub document: String,
    pub selection: String,
    pub words: String,
    pub characters: String,
    pub characters_no_spaces: String,
    pub paragraphs: String,
    pub sentences: String,
    pub reading_time: String,
    pub file: String,
    pub no_file: String,
    pub path: String,
    pub size: String,
    pub created: String,
    pub modified: String,
    pub encoding: String,
    pub line_endings: String,
    pub no_backlinks: String,
}

impl InspectorStrings {
    /// Looks the strings up in `shell.ftl`.
    #[must_use]
    pub fn from_localizer(localizer: &Localizer) -> Self {
        let text = |id: &str| localizer.text(id);
        let level = |level: i64| localizer.format("inspector-level", &[("level", Arg::Int(level))]);
        Self {
            label: text("inspector-label"),
            contents_tab: text("inspector-contents"),
            info_tab: text("inspector-info"),
            level_filter: text("inspector-level-filter"),
            levels: [level(1), level(2), level(3), level(4), level(5), level(6)],
            no_headings: text("inspector-no-headings"),
            document: text("inspector-document"),
            selection: text("inspector-selection"),
            words: text("inspector-words"),
            characters: text("inspector-characters"),
            characters_no_spaces: text("inspector-characters-no-spaces"),
            paragraphs: text("inspector-paragraphs"),
            sentences: text("inspector-sentences"),
            reading_time: text("inspector-reading-time"),
            file: text("inspector-file"),
            no_file: text("inspector-no-file"),
            path: text("inspector-path"),
            size: text("inspector-size"),
            created: text("inspector-created"),
            modified: text("inspector-modified"),
            encoding: text("inspector-encoding"),
            line_endings: text("inspector-line-endings"),
            no_backlinks: text("inspector-no-backlinks"),
        }
    }
}

/// Statistics shown on the Info tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoStats {
    pub document: Stats,
    /// The statistics of the selection; shown instead of the document's when present.
    pub selection: Option<Stats>,
    /// Reading speed in words per minute (a setting, default 200).
    pub reading_wpm: u32,
}

/// File details, formatted by the app for the active locale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    pub path: String,
    pub size: String,
    pub created: String,
    pub modified: String,
    pub encoding: String,
    pub line_endings: String,
}

/// A document that links to the active one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlink {
    pub title: String,
    pub path: String,
}

/// What the user did in the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorEvent {
    TabChanged(InspectorTab),
    /// A heading was clicked (or Enter pressed on it): scroll to `offset` and flash it. `index` is
    /// the heading's index in the [`Toc`].
    HeadingClicked {
        index: usize,
        offset: usize,
    },
    /// The level filter changed: headings deeper than this level are hidden.
    LevelFilterChanged(u8),
    /// The backlink at this index was activated.
    BacklinkClicked(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InfoRow {
    Section(String),
    Pair {
        label: String,
        value: String,
    },
    Link {
        index: usize,
        title: String,
        path: String,
    },
    Note(String),
}

impl InfoRow {
    fn height(&self) -> f32 {
        match self {
            InfoRow::Section(_) => SECTION_HEIGHT,
            InfoRow::Pair { .. } | InfoRow::Note(_) => ROW_HEIGHT,
            InfoRow::Link { .. } => LINK_HEIGHT,
        }
    }
}

/// A visible heading row.
#[derive(Debug, Clone, Copy)]
struct HeadingRow {
    entry: usize,
    depth: usize,
    expandable: bool,
    expanded: bool,
}

/// Where a point is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Tab(InspectorTab),
    Level(u8),
    Item(usize),
    Chevron(usize),
}

/// The inspector panel.
#[derive(Debug, Clone)]
pub struct Inspector {
    id: WidgetId,
    strings: InspectorStrings,
    tab: InspectorTab,
    header_id: WidgetId,
    contents_tab_id: WidgetId,
    info_tab_id: WidgetId,
    levels_id: WidgetId,
    list_id: WidgetId,
    /// Access ids of the info rows, in order.
    info_ids: Vec<WidgetId>,
    toc: Toc,
    heading_ids: Vec<WidgetId>,
    collapsed: HashSet<String>,
    max_level: u8,
    rows: Vec<HeadingRow>,
    caret: usize,
    cursor: Option<usize>,
    stats_rows: Vec<InfoRow>,
    file_rows: Vec<InfoRow>,
    link_rows: Vec<InfoRow>,
    link_cursor: Option<usize>,
    bounds: Rect,
    bar: ScrollBar,
    hover: Option<Hit>,
    events: Vec<InspectorEvent>,
}

impl Inspector {
    /// An empty panel on the Contents tab with every level shown.
    #[must_use]
    pub fn new(strings: InspectorStrings) -> Self {
        let mut inspector = Self {
            id: WidgetId::allocate(),
            tab: InspectorTab::Contents,
            header_id: WidgetId::allocate(),
            contents_tab_id: WidgetId::allocate(),
            info_tab_id: WidgetId::allocate(),
            levels_id: WidgetId::allocate(),
            list_id: WidgetId::allocate(),
            info_ids: Vec::new(),
            toc: Toc::default(),
            heading_ids: Vec::new(),
            collapsed: HashSet::new(),
            max_level: LEVELS,
            rows: Vec::new(),
            caret: 0,
            cursor: None,
            stats_rows: Vec::new(),
            file_rows: Vec::new(),
            link_rows: Vec::new(),
            link_cursor: None,
            bounds: Rect::ZERO,
            bar: ScrollBar::new(Axis::Vertical, ""),
            hover: None,
            events: Vec::new(),
            strings,
        };
        inspector.rebuild_info();
        inspector
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.bar = self.bar.with_motion(motion);
        self
    }

    #[must_use]
    pub fn tab(&self) -> InspectorTab {
        self.tab
    }

    /// Switches the tab without an event.
    pub fn set_tab(&mut self, tab: InspectorTab) {
        if self.tab != tab {
            self.tab = tab;
            self.hover = None;
            self.sync_scroll();
            self.bar.model_mut().jump(0.0);
        }
    }

    /// The current level filter (1..=6).
    #[must_use]
    pub fn level_filter(&self) -> u8 {
        self.max_level
    }

    /// Sets the level filter without an event; values are clamped to 1..=6.
    pub fn set_level_filter(&mut self, level: u8) {
        self.max_level = level.clamp(1, LEVELS);
        self.rebuild_rows();
    }

    /// Replaces the heading tree. Collapsed subtrees stay collapsed when their heading still
    /// exists.
    pub fn set_toc(&mut self, toc: &Toc) {
        self.toc = toc.clone();
        self.heading_ids = toc.entries().iter().map(|_| WidgetId::allocate()).collect();
        let known: HashSet<&str> = toc.entries().iter().map(|e| e.slug.as_str()).collect();
        self.collapsed.retain(|slug| known.contains(slug.as_str()));
        self.rebuild_rows();
    }

    /// Moves the caret: the heading of the section containing `offset` becomes the current one.
    pub fn set_caret(&mut self, offset: usize) {
        self.caret = offset;
        if self.tab == InspectorTab::Contents
            && let Some(row) = self.current_row()
        {
            let top = row as f32 * ROW_HEIGHT;
            self.bar.model_mut().ensure_visible(top, top + ROW_HEIGHT);
        }
    }

    /// The index in the [`Toc`] of the heading of the current section.
    #[must_use]
    pub fn current_heading(&self) -> Option<usize> {
        self.toc.current_section(self.caret)
    }

    /// The visible row of the current section: the heading itself, or its nearest visible
    /// ancestor when the heading is collapsed away or filtered out.
    fn current_row(&self) -> Option<usize> {
        let mut entry = self.current_heading()?;
        loop {
            if let Some(row) = self.rows.iter().position(|row| row.entry == entry) {
                return Some(row);
            }
            entry = self.toc.entries()[entry].parent?;
        }
    }

    /// The index in the [`Toc`] of each visible heading row, top to bottom.
    #[must_use]
    pub fn visible_headings(&self) -> Vec<usize> {
        self.rows.iter().map(|row| row.entry).collect()
    }

    /// Collapses or expands the heading at Toc index `entry` (no effect on headings without
    /// visible children).
    pub fn set_collapsed(&mut self, entry: usize, collapsed: bool) {
        let Some(slug) = self.toc.entries().get(entry).map(|e| e.slug.clone()) else {
            return;
        };
        if collapsed {
            self.collapsed.insert(slug);
        } else {
            self.collapsed.remove(&slug);
        }
        self.rebuild_rows();
    }

    /// Statistics of the document, or of the selection when `stats.selection` is set.
    pub fn set_stats(&mut self, stats: &InfoStats, localizer: &Localizer) {
        let s = &self.strings;
        let (heading, shown) = match stats.selection {
            Some(selection) => (&s.selection, selection),
            None => (&s.document, stats.document),
        };
        let minutes = localizer.format(
            "inspector-minutes",
            &[(
                "minutes",
                Arg::Int(i64::from(shown.reading_minutes(stats.reading_wpm))),
            )],
        );
        let pair = |label: &String, value: String| InfoRow::Pair {
            label: label.clone(),
            value,
        };
        self.stats_rows = vec![
            InfoRow::Section(heading.clone()),
            pair(&s.words, shown.words.to_string()),
            pair(&s.characters, shown.characters.to_string()),
            pair(
                &s.characters_no_spaces,
                shown.characters_no_spaces.to_string(),
            ),
            pair(&s.paragraphs, shown.paragraphs.to_string()),
            pair(&s.sentences, shown.sentences.to_string()),
            pair(&s.reading_time, minutes),
        ];
        self.rebuild_info();
    }

    /// File details; `None` for a document without a file.
    pub fn set_file(&mut self, file: Option<&FileInfo>) {
        let s = &self.strings;
        let mut rows = vec![InfoRow::Section(s.file.clone())];
        match file {
            Some(file) => {
                for (label, value) in [
                    (&s.path, &file.path),
                    (&s.size, &file.size),
                    (&s.created, &file.created),
                    (&s.modified, &file.modified),
                    (&s.encoding, &file.encoding),
                    (&s.line_endings, &file.line_endings),
                ] {
                    rows.push(InfoRow::Pair {
                        label: label.clone(),
                        value: value.clone(),
                    });
                }
            }
            None => rows.push(InfoRow::Note(s.no_file.clone())),
        }
        self.file_rows = rows;
        self.rebuild_info();
    }

    /// The documents that link here.
    pub fn set_backlinks(&mut self, links: &[Backlink], localizer: &Localizer) {
        let count = i64::try_from(links.len()).unwrap_or(i64::MAX);
        let heading = localizer.format("inspector-backlinks", &[("count", Arg::Int(count))]);
        let mut rows = vec![InfoRow::Section(heading)];
        if links.is_empty() {
            rows.push(InfoRow::Note(self.strings.no_backlinks.clone()));
        }
        for (index, link) in links.iter().enumerate() {
            rows.push(InfoRow::Link {
                index,
                title: link.title.clone(),
                path: link.path.clone(),
            });
        }
        self.link_rows = rows;
        self.link_cursor = None;
        self.rebuild_info();
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<InspectorEvent> {
        std::mem::take(&mut self.events)
    }

    /// The vertical scroll offset of the list.
    #[must_use]
    pub fn scroll_offset(&self) -> f32 {
        self.bar.offset()
    }

    fn info_rows(&self) -> impl Iterator<Item = &InfoRow> {
        self.stats_rows
            .iter()
            .chain(&self.file_rows)
            .chain(&self.link_rows)
    }

    fn content_height(&self) -> f32 {
        match self.tab {
            InspectorTab::Contents => {
                let rows = self.rows.len().max(1);
                rows as f32 * ROW_HEIGHT
            }
            InspectorTab::Info => self.info_rows().map(InfoRow::height).sum::<f32>(),
        }
    }

    /// Gives the info rows fresh access ids and resizes the scroll extent.
    fn rebuild_info(&mut self) {
        let count = self.info_rows().count();
        self.info_ids = (0..count).map(|_| WidgetId::allocate()).collect();
        self.sync_scroll();
    }

    fn rebuild_rows(&mut self) {
        let entries = self.toc.entries();
        let visible = |index: usize| entries[index].level <= self.max_level;
        let mut rows = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            if !visible(index) {
                continue;
            }
            let hidden = {
                let mut parent = entry.parent;
                let mut hidden = false;
                while let Some(p) = parent {
                    if self.collapsed.contains(&entries[p].slug) {
                        hidden = true;
                        break;
                    }
                    parent = entries[p].parent;
                }
                hidden
            };
            if hidden {
                continue;
            }
            let expandable = entry.children.iter().any(|&child| visible(child));
            rows.push(HeadingRow {
                entry: index,
                depth: entry.depth,
                expandable,
                expanded: expandable && !self.collapsed.contains(&entry.slug),
            });
        }
        self.rows = rows;
        self.cursor = self.cursor.filter(|&row| row < self.rows.len());
        self.sync_scroll();
    }

    fn sync_scroll(&mut self) {
        let viewport = self.list_rect().height;
        let content = self.content_height();
        self.bar.model_mut().set_extent(viewport, content);
    }

    fn header_rect(&self) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y,
            self.bounds.width,
            HEADER_HEIGHT,
        )
    }

    fn tab_rect(&self, tab: InspectorTab) -> Rect {
        let header = self.header_rect();
        let width = ((header.width - 2.0 * 8.0 - LEVEL_GAP) / 2.0).max(0.0);
        let index = match tab {
            InspectorTab::Contents => 0.0,
            InspectorTab::Info => 1.0,
        };
        Rect::new(
            header.x + 8.0 + index * (width + LEVEL_GAP),
            header.y + 4.0,
            width,
            HEADER_HEIGHT - 8.0,
        )
    }

    fn levels_rect(&self) -> Rect {
        if self.tab == InspectorTab::Contents {
            Rect::new(
                self.bounds.x,
                self.bounds.y + HEADER_HEIGHT,
                self.bounds.width,
                LEVEL_ROW_HEIGHT,
            )
        } else {
            Rect::ZERO
        }
    }

    fn level_rect(&self, level: u8) -> Rect {
        let row = self.levels_rect();
        Rect::new(
            row.x + PADDING + f32::from(level - 1) * (LEVEL_BUTTON.width + LEVEL_GAP),
            row.y + (row.height - LEVEL_BUTTON.height) / 2.0,
            LEVEL_BUTTON.width,
            LEVEL_BUTTON.height,
        )
    }

    fn list_rect(&self) -> Rect {
        let top = self.bounds.y + HEADER_HEIGHT + self.levels_rect().height;
        Rect::new(
            self.bounds.x,
            top,
            self.bounds.width,
            (self.bounds.bottom() - top).max(0.0),
        )
    }

    /// The rectangle of list item `index` (a heading row or an info row), scrolled.
    fn item_rect(&self, index: usize) -> Rect {
        let list = self.list_rect();
        let offset = self.bar.offset();
        match self.tab {
            InspectorTab::Contents => Rect::new(
                list.x,
                list.y + index as f32 * ROW_HEIGHT - offset,
                list.width,
                ROW_HEIGHT,
            ),
            InspectorTab::Info => {
                let top: f32 = self.info_rows().take(index).map(InfoRow::height).sum();
                let height = self.info_rows().nth(index).map_or(0.0, InfoRow::height);
                Rect::new(list.x, list.y + top - offset, list.width, height)
            }
        }
    }

    fn item_count(&self) -> usize {
        match self.tab {
            InspectorTab::Contents => self.rows.len(),
            InspectorTab::Info => self.info_rows().count(),
        }
    }

    fn chevron_rect(&self, row: usize) -> Rect {
        let item = self.item_rect(row);
        Rect::new(
            item.x + PADDING + self.rows[row].depth as f32 * INDENT,
            item.y,
            CHEVRON,
            item.height,
        )
    }

    fn hit(&self, point: Point) -> Option<Hit> {
        if !self.bounds.contains(point) {
            return None;
        }
        for tab in [InspectorTab::Contents, InspectorTab::Info] {
            if self.tab_rect(tab).contains(point) {
                return Some(Hit::Tab(tab));
            }
        }
        if self.tab == InspectorTab::Contents {
            for level in 1..=LEVELS {
                if self.level_rect(level).contains(point) {
                    return Some(Hit::Level(level));
                }
            }
        }
        if !self.list_rect().contains(point) {
            return None;
        }
        let item = (0..self.item_count()).find(|&index| self.item_rect(index).contains(point))?;
        if self.tab == InspectorTab::Contents
            && self.rows[item].expandable
            && self.chevron_rect(item).contains(point)
        {
            Some(Hit::Chevron(item))
        } else {
            Some(Hit::Item(item))
        }
    }

    fn select_tab(&mut self, tab: InspectorTab) {
        if self.tab != tab {
            self.set_tab(tab);
            self.events.push(InspectorEvent::TabChanged(tab));
        }
    }

    fn set_level(&mut self, level: u8) {
        let level = level.clamp(1, LEVELS);
        if level != self.max_level {
            self.max_level = level;
            self.rebuild_rows();
            self.events.push(InspectorEvent::LevelFilterChanged(level));
        }
    }

    fn activate_row(&mut self, row: usize) {
        self.cursor = Some(row);
        let entry = self.rows[row].entry;
        let offset = self.toc.entries()[entry].range.start;
        self.events.push(InspectorEvent::HeadingClicked {
            index: entry,
            offset,
        });
    }

    fn toggle_row(&mut self, row: usize) {
        let entry = self.rows[row].entry;
        let collapsed = self.rows[row].expanded;
        self.set_collapsed(entry, collapsed);
    }

    /// Activates info row `item` when it is a backlink. Returns whether it was one.
    fn activate_link(&mut self, item: usize) -> bool {
        let index = match self.info_rows().nth(item) {
            Some(InfoRow::Link { index, .. }) => *index,
            _ => return false,
        };
        self.link_cursor = Some(item);
        self.events.push(InspectorEvent::BacklinkClicked(index));
        true
    }

    fn reveal_item(&mut self, item: usize) {
        let list = self.list_rect();
        let rect = self.item_rect(item);
        let top = rect.y - list.y + self.bar.offset();
        self.bar.model_mut().ensure_visible(top, top + rect.height);
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let bar = self.bar.event(cx, &Event::Pointer(*event));
        if self.bar.take_changed() {
            cx.invalidate(self.bounds);
        }
        if bar.handled {
            return bar;
        }
        let hit = self.hit(event.position);
        match (event.kind, event.button) {
            (PointerEventKind::Down, Some(PointerButton::Left)) => {
                let Some(hit) = hit else {
                    return EventResult::ignored();
                };
                match hit {
                    Hit::Tab(tab) => self.select_tab(tab),
                    Hit::Level(level) => self.set_level(level),
                    Hit::Chevron(row) => self.toggle_row(row),
                    Hit::Item(item) => match self.tab {
                        InspectorTab::Contents => self.activate_row(item),
                        InspectorTab::Info => {
                            self.activate_link(item);
                        }
                    },
                }
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            (PointerEventKind::Move | PointerEventKind::Enter, _) => self.hover_to(cx, hit),
            (PointerEventKind::Leave, _) => self.hover_to(cx, None),
            _ => EventResult::ignored(),
        }
    }

    fn hover_to(&mut self, cx: &mut EventCx, hover: Option<Hit>) -> EventResult {
        if hover == self.hover {
            return EventResult::ignored();
        }
        self.hover = hover;
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    fn focused(&self, cx: &EventCx, id: WidgetId) -> bool {
        cx.focus.is_focused(id)
    }

    fn key(&mut self, cx: &mut EventCx, key: &KeyEvent) -> bool {
        if !key.mods.is_empty() {
            return false;
        }
        if self.focused(cx, self.contents_tab_id) || self.focused(cx, self.info_tab_id) {
            return self.tab_key(cx, key.vk);
        }
        if self.focused(cx, self.levels_id) {
            let level = self.max_level;
            match key.vk {
                vk::LEFT => self.set_level(level.saturating_sub(1)),
                vk::RIGHT => self.set_level(level + 1),
                vk::HOME => self.set_level(1),
                vk::END => self.set_level(LEVELS),
                _ => return false,
            }
            return true;
        }
        if self.focused(cx, self.list_id) {
            return match self.tab {
                InspectorTab::Contents => self.contents_key(key.vk),
                InspectorTab::Info => self.info_key(key.vk),
            };
        }
        false
    }

    fn tab_key(&mut self, cx: &mut EventCx, key: u16) -> bool {
        let (target, id) = match key {
            vk::SPACE | vk::RETURN => {
                let tab = if self.focused(cx, self.info_tab_id) {
                    InspectorTab::Info
                } else {
                    InspectorTab::Contents
                };
                self.select_tab(tab);
                return true;
            }
            vk::LEFT => (InspectorTab::Contents, self.contents_tab_id),
            vk::RIGHT => (InspectorTab::Info, self.info_tab_id),
            _ => return false,
        };
        self.select_tab(target);
        cx.focus.focus(id);
        true
    }

    fn contents_key(&mut self, key: u16) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let last = self.rows.len() - 1;
        let current = self.cursor.or_else(|| self.current_row());
        let page = ((self.list_rect().height / ROW_HEIGHT) as usize)
            .saturating_sub(1)
            .max(1);
        let target = match key {
            vk::UP => current.map_or(0, |row| row.saturating_sub(1)),
            vk::DOWN => current.map_or(0, |row| (row + 1).min(last)),
            vk::PAGE_UP => current.map_or(0, |row| row.saturating_sub(page)),
            vk::PAGE_DOWN => current.map_or(0, |row| (row + page).min(last)),
            vk::HOME => 0,
            vk::END => last,
            vk::RIGHT | vk::LEFT => {
                let Some(row) = current else { return true };
                let data = self.rows[row];
                if key == vk::RIGHT && data.expandable && !data.expanded {
                    self.toggle_row(row);
                } else if key == vk::RIGHT && data.expanded {
                    // The first visible child follows its parent.
                    self.move_cursor(row + 1);
                } else if key == vk::LEFT && data.expanded {
                    self.toggle_row(row);
                } else if key == vk::LEFT
                    && let Some(parent) = self.parent_row(row)
                {
                    self.move_cursor(parent);
                }
                return true;
            }
            vk::RETURN => {
                if let Some(row) = current {
                    self.activate_row(row);
                }
                return true;
            }
            _ => return false,
        };
        self.move_cursor(target);
        true
    }

    fn move_cursor(&mut self, row: usize) {
        self.cursor = Some(row);
        self.reveal_item(row);
    }

    /// The visible row of the heading `row` nests under.
    fn parent_row(&self, row: usize) -> Option<usize> {
        let parent = self.toc.entries()[self.rows[row].entry].parent?;
        self.rows.iter().position(|r| r.entry == parent)
    }

    fn info_key(&mut self, key: u16) -> bool {
        let links: Vec<usize> = self
            .info_rows()
            .enumerate()
            .filter_map(|(item, row)| matches!(row, InfoRow::Link { .. }).then_some(item))
            .collect();
        if links.is_empty() {
            return false;
        }
        let position = self
            .link_cursor
            .and_then(|item| links.iter().position(|&link| link == item));
        let target = match key {
            vk::UP => position.map_or(0, |p| p.saturating_sub(1)),
            vk::DOWN => position.map_or(0, |p| (p + 1).min(links.len() - 1)),
            vk::HOME => 0,
            vk::END => links.len() - 1,
            vk::RETURN => {
                if let Some(p) = position {
                    self.activate_link(links[p]);
                }
                return true;
            }
            _ => return false,
        };
        self.link_cursor = Some(links[target]);
        self.reveal_item(links[target]);
        true
    }

    fn row_label(&self, row: usize) -> &str {
        &self.toc.entries()[self.rows[row].entry].title
    }

    fn heading_node(&self, list_focused: bool, row: usize) -> AccessNode {
        let data = self.rows[row];
        let selected = self.current_row() == Some(row);
        let mut node = AccessNode::new(
            self.heading_ids[data.entry],
            Role::TreeItem,
            self.row_label(row),
            self.item_rect(row),
        );
        node.state.selected = selected;
        node.state.focusable = true;
        node.state.focused = list_focused && self.cursor == Some(row);
        node.state.expanded = data.expandable.then_some(data.expanded);
        node.patterns.selection_item = true;
        node.patterns.expand_collapse = data.expandable;
        node.patterns.invoke = true;
        node
    }

    /// Carries out an access action on the heading or backlink whose node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let selecting = matches!(action, AccessAction::Select | AccessAction::Invoke);
        if item == self.contents_tab_id || item == self.info_tab_id {
            if !selecting {
                return false;
            }
            let tab = if item == self.contents_tab_id {
                InspectorTab::Contents
            } else {
                InspectorTab::Info
            };
            self.select_tab(tab);
            cx.focus.focus(item);
            cx.invalidate(self.bounds);
            return true;
        }
        if let Some(row) = self.info_ids.iter().position(|id| *id == item) {
            let done = selecting && self.tab == InspectorTab::Info && self.activate_link(row);
            if done {
                cx.invalidate(self.bounds);
            }
            return done;
        }
        let Some(entry) = self.heading_ids.iter().position(|id| *id == item) else {
            return false;
        };
        let Some(row) = self.rows.iter().position(|row| row.entry == entry) else {
            return false;
        };
        let data = self.rows[row];
        let done = match action {
            AccessAction::Select | AccessAction::Invoke => {
                self.activate_row(row);
                true
            }
            AccessAction::Expand if data.expandable => {
                self.set_collapsed(entry, false);
                true
            }
            AccessAction::Collapse if data.expandable => {
                self.set_collapsed(entry, true);
                true
            }
            AccessAction::ScrollIntoView => {
                self.reveal_item(row);
                true
            }
            _ => false,
        };
        if done {
            cx.invalidate(self.bounds);
        }
        done
    }
}

impl Control for Inspector {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for Inspector {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            280.0
        };
        let height = if available.height.is_finite() {
            available.height
        } else {
            HEADER_HEIGHT + self.levels_rect().height + self.content_height()
        };
        Size::new(width, height)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let list = self.list_rect();
        self.sync_scroll();
        let bar_id = self.bar.id();
        self.bar.arrange(
            &mut cx.for_widget(bar_id),
            Rect::new(list.right() - BAR_WIDTH, list.y, BAR_WIDTH, list.height),
        );
        cx.for_widget(self.contents_tab_id).add_tab_stop();
        cx.for_widget(self.info_tab_id).add_tab_stop();
        if self.tab == InspectorTab::Contents {
            cx.for_widget(self.levels_id).add_tab_stop();
        }
        cx.for_widget(self.list_id).add_tab_stop();
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let radius = tokens.metrics.control_radius;
        // Tab header.
        for tab in [InspectorTab::Contents, InspectorTab::Info] {
            let rect = self.tab_rect(tab);
            let active = tab == self.tab;
            if self.hover == Some(Hit::Tab(tab)) && !active {
                canvas.fill_rounded_rect(rect, CornerRadii::uniform(radius), tokens.hover_bg);
            }
            let (label, id) = match tab {
                InspectorTab::Contents => (&self.strings.contents_tab, self.contents_tab_id),
                InspectorTab::Info => (&self.strings.info_tab, self.info_tab_id),
            };
            let weight = if active {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::NORMAL
            };
            let style = ui_style(cx.theme, FONT_SIZE).with_weight(weight);
            let text = cx.text.layout(label, &style, &[], &[], f32::INFINITY);
            let color = if active {
                tokens.text
            } else {
                tokens.text_muted
            };
            draw_centered(canvas, text.as_ref(), rect, color);
            if active {
                canvas.fill_rounded_rect(
                    Rect::new(rect.x + 12.0, rect.bottom() - 3.0, rect.width - 24.0, 3.0),
                    CornerRadii::uniform(1.5),
                    tokens.accent,
                );
            }
            if cx.for_widget(id).is_focused() {
                focus_ring::paint(canvas, rect, radius, cx.theme);
            }
        }
        if self.tab == InspectorTab::Contents {
            self.paint_levels(cx, canvas);
        }
        let list = self.list_rect();
        canvas.push_clip_rect(list);
        match self.tab {
            InspectorTab::Contents => self.paint_headings(cx, canvas),
            InspectorTab::Info => self.paint_info(cx, canvas),
        }
        canvas.pop_clip();
        if cx.for_widget(self.list_id).is_focused() {
            focus_ring::paint(canvas, list.inset(Insets::uniform(1.0)), radius, cx.theme);
        }
        self.bar.paint(&cx.for_widget(self.bar.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) if self.list_rect().contains(event.position) => {
                if self.bar.wheel(cx, event) {
                    self.bar.take_changed();
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Key(key) if key.is_down => {
                if self.key(cx, key) {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                cx.invalidate(self.bounds);
                EventResult::ignored()
            }
            Event::AnimationFrame(now) => {
                if self.bar.animate(cx, *now) {
                    cx.invalidate(self.bounds);
                }
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let tabs = [
            (
                InspectorTab::Contents,
                self.contents_tab_id,
                &self.strings.contents_tab,
            ),
            (InspectorTab::Info, self.info_tab_id, &self.strings.info_tab),
        ];
        let mut header_children = Vec::new();
        for (tab, id, label) in tabs {
            let child = cx.for_widget(id);
            let mut node = AccessNode::new(id, Role::TabItem, label.clone(), self.tab_rect(tab));
            node.state.selected = tab == self.tab;
            node.state.focusable = child.is_focusable();
            node.state.focused = child.is_focused();
            node.patterns.selection_item = true;
            out.insert(node);
            header_children.push(id);
        }
        out.insert(
            AccessNode::new(
                self.header_id,
                Role::Tab,
                self.strings.label.clone(),
                self.header_rect(),
            )
            .with_children(header_children),
        );
        let mut children = vec![self.header_id];
        if self.tab == InspectorTab::Contents {
            let child = cx.for_widget(self.levels_id);
            let mut node = AccessNode::new(
                self.levels_id,
                Role::Group,
                self.strings.level_filter.clone(),
                self.levels_rect(),
            )
            .with_value(self.strings.levels[usize::from(self.max_level - 1)].clone());
            node.state.focusable = child.is_focusable();
            node.state.focused = child.is_focused();
            out.insert(node);
            children.push(self.levels_id);
        }
        let list = self.list_rect();
        let list_cx = cx.for_widget(self.list_id);
        let mut list_children = Vec::new();
        let list_role = match self.tab {
            InspectorTab::Contents => {
                for row in 0..self.rows.len() {
                    let item = self.item_rect(row);
                    if !item.intersects(list) {
                        continue;
                    }
                    let node = self.heading_node(list_cx.is_focused(), row);
                    list_children.push(node.id);
                    out.insert(node);
                }
                Role::Tree
            }
            InspectorTab::Info => {
                for (index, row) in self.info_rows().enumerate() {
                    let rect = self.item_rect(index);
                    let id = self.info_ids[index];
                    let node = match row {
                        InfoRow::Section(text) | InfoRow::Note(text) => {
                            AccessNode::new(id, Role::Text, text.clone(), rect)
                        }
                        InfoRow::Pair { label, value } => {
                            AccessNode::new(id, Role::ListItem, label.clone(), rect)
                                .with_value(value.clone())
                        }
                        InfoRow::Link { title, path, .. } => {
                            let mut node =
                                AccessNode::new(id, Role::Hyperlink, title.clone(), rect)
                                    .with_description(path.clone());
                            node.patterns.invoke = true;
                            node
                        }
                    };
                    list_children.push(id);
                    out.insert(node);
                }
                Role::List
            }
        };
        let mut node = AccessNode::new(self.list_id, list_role, self.strings.label.clone(), list)
            .with_children(list_children);
        node.state.focusable = list_cx.is_focusable();
        node.state.focused = list_cx.is_focused();
        node.patterns.scroll = Some(scroll_info(&Default::default(), self.bar.model()));
        out.insert(node);
        children.push(self.list_id);
        out.insert(
            AccessNode::new(self.id, Role::Pane, self.strings.label.clone(), self.bounds)
                .with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

impl Inspector {
    fn paint_levels(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        for level in 1..=LEVELS {
            let rect = self.level_rect(level);
            let active = level == self.max_level;
            let hovered = self.hover == Some(Hit::Level(level));
            let fill = if active {
                tokens.accent
            } else if hovered {
                tokens.hover_bg
            } else {
                tokens.hover_bg.with_alpha(0.0)
            };
            canvas.fill_rounded_rect(
                rect,
                CornerRadii::uniform(tokens.metrics.control_radius),
                fill,
            );
            let color = if active {
                tokens.accent_text_on
            } else {
                tokens.text_muted
            };
            let text = layout_line(
                cx.text,
                cx.theme,
                &self.strings.levels[usize::from(level - 1)],
                12.0,
            );
            draw_centered(canvas, text.as_ref(), rect, color);
        }
        if cx.for_widget(self.levels_id).is_focused() {
            let row = self.levels_rect().inset(Insets::uniform(1.0));
            focus_ring::paint(canvas, row, tokens.metrics.control_radius, cx.theme);
        }
    }

    fn paint_headings(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        let radius = tokens.metrics.control_radius;
        let list = self.list_rect();
        if self.rows.is_empty() {
            let text = layout_line(cx.text, cx.theme, &self.strings.no_headings, FONT_SIZE);
            let row = Rect::new(list.x, list.y, list.width, ROW_HEIGHT);
            draw_left_centered(
                canvas,
                text.as_ref(),
                list.x + PADDING,
                row,
                tokens.text_muted,
            );
            return;
        }
        let current = self.current_row();
        for (index, data) in self.rows.iter().enumerate() {
            let rect = self.item_rect(index);
            if !rect.intersects(list) {
                continue;
            }
            // The staircase: one hairline per indentation level.
            for level in 0..data.depth {
                let x = rect.x + PADDING + level as f32 * INDENT + CHEVRON / 2.0 + 0.5;
                canvas.stroke_line(
                    Point::new(x, rect.y),
                    Point::new(x, rect.bottom()),
                    tokens.hairline,
                    1.0,
                    Dash::Solid,
                );
            }
            let pill = rect.inset(Insets::symmetric(4.0, 1.0));
            if current == Some(index) {
                canvas.fill_rounded_rect(
                    pill,
                    CornerRadii::uniform(radius),
                    tokens.accent.with_alpha(0.12),
                );
                canvas.fill_rounded_rect(
                    Rect::new(rect.x + 2.0, rect.center().y - 8.0, 3.0, 16.0),
                    CornerRadii::uniform(1.5),
                    tokens.accent,
                );
            } else if self.hover == Some(Hit::Item(index)) {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.hover_bg);
            }
            let color = if current == Some(index) {
                tokens.text
            } else {
                tokens.text_muted
            };
            let chevron = self.chevron_rect(index);
            if data.expandable {
                let symbol = if data.expanded {
                    glyph::CHEVRON_DOWN
                } else {
                    glyph::CHEVRON_RIGHT
                };
                canvas.draw_icon(symbol, IconFont::SegoeFluent, 12.0, color, chevron.center());
            }
            let label = layout_line(cx.text, cx.theme, self.row_label(index), FONT_SIZE);
            draw_left_centered(canvas, label.as_ref(), chevron.right() + 4.0, rect, color);
            if cx.for_widget(self.list_id).is_focused() && self.cursor == Some(index) {
                focus_ring::paint(canvas, pill, radius, cx.theme);
            }
        }
    }

    fn paint_info(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        let list = self.list_rect();
        for (index, row) in self.info_rows().enumerate() {
            let rect = self.item_rect(index);
            if !rect.intersects(list) {
                continue;
            }
            match row {
                InfoRow::Section(text) => {
                    let style = ui_style(cx.theme, FONT_SIZE).with_weight(FontWeight::SEMIBOLD);
                    let layout = cx.text.layout(text, &style, &[], &[], f32::INFINITY);
                    draw_left_centered(
                        canvas,
                        layout.as_ref(),
                        rect.x + PADDING,
                        rect,
                        tokens.text,
                    );
                }
                InfoRow::Note(text) => {
                    let layout = layout_line(cx.text, cx.theme, text, FONT_SIZE);
                    draw_left_centered(
                        canvas,
                        layout.as_ref(),
                        rect.x + PADDING,
                        rect,
                        tokens.text_muted,
                    );
                }
                InfoRow::Pair { label, value } => {
                    let name = layout_line(cx.text, cx.theme, label, FONT_SIZE);
                    draw_left_centered(
                        canvas,
                        name.as_ref(),
                        rect.x + PADDING,
                        rect,
                        tokens.text_muted,
                    );
                    let shown = layout_line(cx.text, cx.theme, value, FONT_SIZE);
                    let left = rect.x + PADDING + name.size().width + PADDING;
                    let x = (rect.right() - PADDING - BAR_WIDTH - shown.size().width).max(left);
                    canvas.push_clip_rect(Rect::new(
                        left,
                        rect.y,
                        (rect.right() - left).max(0.0),
                        rect.height,
                    ));
                    draw_left_centered(canvas, shown.as_ref(), x, rect, tokens.text);
                    canvas.pop_clip();
                }
                InfoRow::Link { title, path, .. } => {
                    let pill = rect.inset(Insets::symmetric(4.0, 1.0));
                    let radius = CornerRadii::uniform(tokens.metrics.control_radius);
                    if self.hover == Some(Hit::Item(index)) {
                        canvas.fill_rounded_rect(pill, radius, tokens.hover_bg);
                    }
                    let top = Rect::new(rect.x, rect.y + 4.0, rect.width, 20.0);
                    let bottom = Rect::new(rect.x, rect.y + 24.0, rect.width, 20.0);
                    let name = layout_line(cx.text, cx.theme, title, FONT_SIZE);
                    draw_left_centered(canvas, name.as_ref(), rect.x + PADDING, top, tokens.link);
                    let detail = layout_line(cx.text, cx.theme, path, 12.0);
                    draw_left_centered(
                        canvas,
                        detail.as_ref(),
                        rect.x + PADDING,
                        bottom,
                        tokens.text_muted,
                    );
                    if cx.for_widget(self.list_id).is_focused() && self.link_cursor == Some(index) {
                        focus_ring::paint(canvas, pill, tokens.metrics.control_radius, cx.theme);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{Modifiers, WheelEvent};
    use crate::widget::Invalidation;
    use crate::widgets::testkit::Harness;
    use ssw_core::markdown::parse_document;

    const SHELL_FTL: &str = include_str!("../../../../assets/i18n/en/shell.ftl");
    const DOC: &str = "# Intro\ntext\n## Setup\n### Install\n## Usage\n# Reference\n#### Deep\n";

    fn localizer() -> Localizer {
        let localizer = Localizer::new("en", &[("en", SHELL_FTL)]);
        let diagnostics = localizer.diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        localizer
    }

    fn toc_of(src: &str) -> Toc {
        Toc::build(&parse_document(src), src)
    }

    /// The panel at the right of a 1080 DIP window: x 800..1080, y 80..480.
    fn setup() -> (Harness, Inspector) {
        let mut harness = Harness::new();
        let mut inspector = Inspector::new(InspectorStrings::from_localizer(&localizer()))
            .with_motion(Motion::reduced());
        inspector.set_toc(&toc_of(DOC));
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        (harness, inspector)
    }

    fn focus_list(harness: &mut Harness, inspector: &Inspector) {
        harness.focus.focus(inspector.list_id);
    }

    fn stats(words: usize) -> Stats {
        Stats {
            words,
            characters: words * 6,
            characters_no_spaces: words * 5,
            paragraphs: 12,
            sentences: 20,
        }
    }

    fn info() -> FileInfo {
        FileInfo {
            path: "C:\\notes\\todo.md".to_owned(),
            size: "1.2 KB".to_owned(),
            created: "1 Oct 2026".to_owned(),
            modified: "2 Oct 2026".to_owned(),
            encoding: "UTF-8".to_owned(),
            line_endings: "LF".to_owned(),
        }
    }

    fn links() -> Vec<Backlink> {
        vec![
            Backlink {
                title: "Plan".to_owned(),
                path: "plans\\plan.md".to_owned(),
            },
            Backlink {
                title: "Journal".to_owned(),
                path: "journal.md".to_owned(),
            },
        ]
    }

    fn fill_info(inspector: &mut Inspector) {
        let localizer = localizer();
        inspector.set_stats(
            &InfoStats {
                document: stats(340),
                selection: None,
                reading_wpm: 200,
            },
            &localizer,
        );
        inspector.set_file(Some(&info()));
        inspector.set_backlinks(&links(), &localizer);
    }

    #[test]
    fn strings_resolve_from_the_ftl_ids() {
        let strings = InspectorStrings::from_localizer(&localizer());
        assert_eq!(strings.contents_tab, "Contents");
        assert_eq!(strings.info_tab, "Info");
        assert_eq!(
            strings.levels,
            ["H1", "H2", "H3", "H4", "H5", "H6"].map(String::from)
        );
        for text in [
            &strings.label,
            &strings.level_filter,
            &strings.no_headings,
            &strings.document,
            &strings.selection,
            &strings.words,
            &strings.characters,
            &strings.characters_no_spaces,
            &strings.paragraphs,
            &strings.sentences,
            &strings.reading_time,
            &strings.file,
            &strings.no_file,
            &strings.path,
            &strings.size,
            &strings.created,
            &strings.modified,
            &strings.encoding,
            &strings.line_endings,
            &strings.no_backlinks,
        ] {
            assert!(
                !text.is_empty() && !text.starts_with("inspector-"),
                "{text}"
            );
        }
    }

    #[test]
    fn the_level_filter_and_collapsing_decide_the_visible_headings() {
        let (_, mut inspector) = setup();
        assert_eq!(inspector.visible_headings(), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(inspector.level_filter(), 6);
        inspector.set_level_filter(2);
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3, 4]);
        inspector.set_level_filter(9);
        assert_eq!(inspector.level_filter(), 6);
        inspector.set_level_filter(0);
        assert_eq!(inspector.level_filter(), 1);
        assert_eq!(inspector.visible_headings(), vec![0, 4]);
        inspector.set_level_filter(6);

        inspector.set_collapsed(1, true);
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3, 4, 5]);
        inspector.set_collapsed(0, true);
        assert_eq!(inspector.visible_headings(), vec![0, 4, 5]);
        inspector.set_collapsed(0, false);
        inspector.set_collapsed(99, true);
        // The collapsed state follows the heading across TOC updates.
        inspector.set_toc(&toc_of("# Intro\n## Setup\n### Install\n## Other\n"));
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3]);
        inspector.set_toc(&toc_of("# Intro\n## Elsewhere\n### Install\n"));
        assert_eq!(inspector.visible_headings(), vec![0, 1, 2]);
    }

    #[test]
    fn the_current_section_follows_the_caret_and_falls_back_to_a_visible_ancestor() {
        let (harness, mut inspector) = setup();
        let toc = toc_of(DOC);
        assert_eq!(inspector.current_heading(), Some(0));
        inspector.set_caret(toc.entries()[2].range.start + 2);
        assert_eq!(inspector.current_heading(), Some(2));
        let selected = |harness: &Harness, inspector: &Inspector| -> Vec<String> {
            harness
                .tree(inspector)
                .nodes()
                .iter()
                .filter(|n| n.role == Role::TreeItem && n.state.selected)
                .map(|n| n.name.clone())
                .collect()
        };
        assert_eq!(selected(&harness, &inspector), vec!["Install"]);
        inspector.set_collapsed(1, true);
        assert_eq!(selected(&harness, &inspector), vec!["Setup"]);
        inspector.set_collapsed(1, false);
        inspector.set_level_filter(2);
        assert_eq!(selected(&harness, &inspector), vec!["Setup"]);
    }

    #[test]
    fn clicking_headings_chevrons_levels_and_tabs_reports_events() {
        let (mut harness, mut inspector) = setup();
        let toc = toc_of(DOC);
        // Row 1 ("Setup", depth 1) is at y 184..212; its label starts right of the chevron.
        harness.click(&mut inspector, Point::new(900.0, 198.0));
        assert_eq!(
            inspector.take_events(),
            vec![InspectorEvent::HeadingClicked {
                index: 1,
                offset: toc.entries()[1].range.start
            }]
        );
        // The chevron collapses without navigating.
        harness.click(&mut inspector, Point::new(836.0, 198.0));
        assert!(inspector.take_events().is_empty());
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3, 4, 5]);
        harness.click(&mut inspector, Point::new(836.0, 198.0));
        assert_eq!(inspector.visible_headings(), vec![0, 1, 2, 3, 4, 5]);
        // The level filter: H2 is the second button.
        let h2 = inspector.level_rect(2).center();
        harness.click(&mut inspector, h2);
        assert_eq!(
            inspector.take_events(),
            vec![InspectorEvent::LevelFilterChanged(2)]
        );
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3, 4]);
        harness.click(&mut inspector, h2);
        assert!(inspector.take_events().is_empty());
        // Tabs.
        let info_tab = inspector.tab_rect(InspectorTab::Info).center();
        harness.click(&mut inspector, info_tab);
        assert_eq!(
            inspector.take_events(),
            vec![InspectorEvent::TabChanged(InspectorTab::Info)]
        );
        assert_eq!(inspector.tab(), InspectorTab::Info);
        harness.click(&mut inspector, info_tab);
        assert!(inspector.take_events().is_empty());
        // Clicks outside every target are ignored.
        let result = harness.click(&mut inspector, Point::new(10.0, 10.0));
        assert!(!result.handled);
    }

    #[test]
    fn the_keyboard_walks_collapses_and_activates_headings() {
        let (mut harness, mut inspector) = setup();
        let toc = toc_of(DOC);
        assert!(
            !harness.key(&mut inspector, vk::DOWN).handled,
            "nothing focused"
        );
        focus_list(&mut harness, &inspector);
        assert!(harness.key(&mut inspector, vk::DOWN).handled);
        assert_eq!(inspector.cursor, Some(1));
        harness.key(&mut inspector, vk::END);
        assert_eq!(inspector.cursor, Some(5));
        harness.key(&mut inspector, vk::HOME);
        assert_eq!(inspector.cursor, Some(0));
        harness.key(&mut inspector, vk::PAGE_DOWN);
        assert_eq!(inspector.cursor, Some(5));
        harness.key(&mut inspector, vk::PAGE_UP);
        assert_eq!(inspector.cursor, Some(0));
        harness.key(&mut inspector, vk::UP);
        assert_eq!(inspector.cursor, Some(0));
        // Right on an expanded heading enters it, Left on a leaf goes to the parent.
        harness.key(&mut inspector, vk::RIGHT);
        assert_eq!(inspector.cursor, Some(1));
        harness.key(&mut inspector, vk::DOWN);
        harness.key(&mut inspector, vk::LEFT);
        assert_eq!(inspector.cursor, Some(1));
        // Left collapses, Right expands.
        harness.key(&mut inspector, vk::LEFT);
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3, 4, 5]);
        harness.key(&mut inspector, vk::RIGHT);
        assert_eq!(inspector.visible_headings(), vec![0, 1, 2, 3, 4, 5]);
        // Left collapses a root; a second Left has no parent to go to.
        harness.key(&mut inspector, vk::HOME);
        harness.key(&mut inspector, vk::LEFT);
        assert_eq!(inspector.visible_headings(), vec![0, 4, 5]);
        harness.key(&mut inspector, vk::LEFT);
        assert_eq!(inspector.cursor, Some(0));
        harness.key(&mut inspector, vk::RETURN);
        assert_eq!(
            inspector.take_events(),
            vec![InspectorEvent::HeadingClicked {
                index: 0,
                offset: toc.entries()[0].range.start
            }]
        );
        assert!(!harness.key(&mut inspector, vk::TAB).handled);
        assert!(
            !harness
                .key_with(&mut inspector, vk::DOWN, Modifiers::CTRL)
                .handled
        );
    }

    #[test]
    fn the_tab_buttons_and_level_filter_respond_to_the_keyboard() {
        let (mut harness, mut inspector) = setup();
        harness.focus.focus(inspector.contents_tab_id);
        assert!(harness.key(&mut inspector, vk::RIGHT).handled);
        assert_eq!(inspector.tab(), InspectorTab::Info);
        assert!(harness.focus.is_focused(inspector.info_tab_id));
        assert!(harness.key(&mut inspector, vk::LEFT).handled);
        assert_eq!(inspector.tab(), InspectorTab::Contents);
        assert!(harness.focus.is_focused(inspector.contents_tab_id));
        harness.key(&mut inspector, vk::SPACE);
        harness.focus.focus(inspector.info_tab_id);
        harness.key(&mut inspector, vk::RETURN);
        assert_eq!(inspector.tab(), InspectorTab::Info);
        assert!(!harness.key(&mut inspector, vk::UP).handled);
        assert_eq!(
            inspector.take_events(),
            vec![
                InspectorEvent::TabChanged(InspectorTab::Info),
                InspectorEvent::TabChanged(InspectorTab::Contents),
                InspectorEvent::TabChanged(InspectorTab::Info),
            ]
        );

        inspector.set_tab(InspectorTab::Contents);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        harness.focus.focus(inspector.levels_id);
        harness.key(&mut inspector, vk::LEFT);
        assert_eq!(inspector.level_filter(), 5);
        harness.key(&mut inspector, vk::HOME);
        assert_eq!(inspector.level_filter(), 1);
        harness.key(&mut inspector, vk::RIGHT);
        assert_eq!(inspector.level_filter(), 2);
        harness.key(&mut inspector, vk::END);
        assert_eq!(inspector.level_filter(), 6);
        assert!(!harness.key(&mut inspector, vk::DOWN).handled);
        assert_eq!(
            inspector.take_events(),
            vec![
                InspectorEvent::LevelFilterChanged(5),
                InspectorEvent::LevelFilterChanged(1),
                InspectorEvent::LevelFilterChanged(2),
                InspectorEvent::LevelFilterChanged(6),
            ]
        );
    }

    #[test]
    fn the_info_tab_shows_statistics_file_details_and_backlinks() {
        let (mut harness, mut inspector) = setup();
        fill_info(&mut inspector);
        inspector.set_tab(InspectorTab::Info);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        assert_eq!(
            harness.focus.tab_order().len(),
            3,
            "no level filter on Info"
        );
        let canvas = harness.paint(&inspector);
        let texts = canvas.texts();
        for expected in [
            "Document",
            "Words",
            "340",
            "Characters",
            "2040",
            "Characters (no spaces)",
            "1700",
            "Paragraphs",
            "12",
            "Sentences",
            "20",
            "Reading time",
            "2 min",
            "File",
            "Path",
        ] {
            assert!(texts.contains(&expected), "missing {expected}: {texts:?}");
        }
        // The tab has more content than fits: the rest is scrolled into view.
        assert!(inspector.bar.model().is_scrollable());
        let max = inspector.bar.model().max_offset();
        inspector.bar.model_mut().jump(max);
        let canvas = harness.paint(&inspector);
        let texts = canvas.texts();
        for expected in [
            "Backlinks (2)",
            "Plan",
            "plans\\plan.md",
            "Journal",
            "journal.md",
        ] {
            assert!(texts.contains(&expected), "missing {expected}: {texts:?}");
        }
    }

    #[test]
    fn statistics_follow_the_selection_and_missing_data_shows_notes() {
        let (mut harness, mut inspector) = setup();
        let localizer = localizer();
        inspector.set_stats(
            &InfoStats {
                document: stats(340),
                selection: Some(stats(5)),
                reading_wpm: 200,
            },
            &localizer,
        );
        inspector.set_file(None);
        inspector.set_backlinks(&[], &localizer);
        inspector.set_tab(InspectorTab::Info);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 600.0));
        let canvas = harness.paint(&inspector);
        let texts = canvas.texts();
        assert!(texts.contains(&"Selection"));
        assert!(texts.contains(&"5"));
        assert!(texts.contains(&"1 min"));
        assert!(!texts.contains(&"340"));
        assert!(texts.contains(&"No file"));
        assert!(texts.contains(&"Backlinks (0)"));
        assert!(texts.contains(&"No backlinks"));
    }

    #[test]
    fn backlinks_are_activated_by_click_keyboard_and_access() {
        let (mut harness, mut inspector) = setup();
        fill_info(&mut inspector);
        inspector.set_tab(InspectorTab::Info);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        focus_list(&mut harness, &inspector);
        // Down selects the first link and scrolls it into view; Enter activates it.
        assert!(harness.key(&mut inspector, vk::DOWN).handled);
        assert!(inspector.scroll_offset() > 0.0);
        harness.key(&mut inspector, vk::RETURN);
        harness.key(&mut inspector, vk::DOWN);
        harness.key(&mut inspector, vk::RETURN);
        harness.key(&mut inspector, vk::DOWN);
        harness.key(&mut inspector, vk::UP);
        harness.key(&mut inspector, vk::RETURN);
        harness.key(&mut inspector, vk::HOME);
        harness.key(&mut inspector, vk::END);
        harness.key(&mut inspector, vk::RETURN);
        assert!(!harness.key(&mut inspector, vk::LEFT).handled);
        assert_eq!(
            inspector.take_events(),
            vec![
                InspectorEvent::BacklinkClicked(0),
                InspectorEvent::BacklinkClicked(1),
                InspectorEvent::BacklinkClicked(0),
                InspectorEvent::BacklinkClicked(1),
            ]
        );
        // A click on the second link (row 16).
        let center = inspector.item_rect(16).center();
        harness.click(&mut inspector, center);
        assert_eq!(
            inspector.take_events(),
            vec![InspectorEvent::BacklinkClicked(1)]
        );
        // A click on a plain row does nothing.
        let plain = inspector.item_rect(14).center();
        harness.hover(&mut inspector, plain);
        harness.click(&mut inspector, plain);
        assert!(inspector.take_events().is_empty());

        // Through accessibility.
        let tree = harness.tree(&inspector);
        let link = tree
            .nodes()
            .iter()
            .find(|n| n.role == Role::Hyperlink && n.name == "Plan")
            .expect("backlink node");
        assert_eq!(link.description, "plans\\plan.md");
        let (link_id, words_id) = (
            link.id,
            tree.nodes()
                .iter()
                .find(|n| n.name == "Words")
                .expect("row")
                .id,
        );
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, link_id, &AccessAction::Invoke));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, words_id, &AccessAction::Invoke));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, link_id, &AccessAction::Expand));
        assert_eq!(
            inspector.take_events(),
            vec![InspectorEvent::BacklinkClicked(0)]
        );

        // Without backlinks the keyboard has nothing to walk.
        inspector.set_backlinks(&[], &localizer());
        assert!(!harness.key(&mut inspector, vk::DOWN).handled);
    }

    #[test]
    fn the_access_tree_exposes_tabs_filter_and_headings() {
        let (harness, mut inspector) = setup();
        let tree = harness.tree(&inspector);
        let root = tree.root().expect("root");
        assert_eq!(root.role, Role::Pane);
        let children: Vec<_> = tree.children(inspector.id()).collect();
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].role, Role::Tab);
        let tabs: Vec<_> = tree.children(children[0].id).collect();
        assert_eq!(
            tabs.iter()
                .map(|t| (t.name.as_str(), t.state.selected))
                .collect::<Vec<_>>(),
            vec![("Contents", true), ("Info", false)]
        );
        assert_eq!(children[1].role, Role::Group);
        assert_eq!(children[1].value.as_deref(), Some("H6"));
        assert_eq!(children[2].role, Role::Tree);
        let items: Vec<_> = tree.children(children[2].id).collect();
        assert_eq!(items.len(), 6);
        let setup_node = items.iter().find(|n| n.name == "Setup").expect("Setup");
        assert_eq!(setup_node.state.expanded, Some(true));
        let usage = items.iter().find(|n| n.name == "Usage").expect("Usage");
        assert_eq!(usage.state.expanded, None);
        assert!(!usage.patterns.expand_collapse);
        assert_eq!(tree.walk().len(), tree.len());

        // Actions.
        let (setup_id, usage_id) = (setup_node.id, usage.id);
        let mut harness = harness;
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, setup_id, &AccessAction::Collapse));
        assert_eq!(inspector.visible_headings(), vec![0, 1, 3, 4, 5]);
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, setup_id, &AccessAction::Expand));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, usage_id, &AccessAction::Expand));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, usage_id, &AccessAction::Collapse));
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, usage_id, &AccessAction::ScrollIntoView));
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, usage_id, &AccessAction::Select));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, usage_id, &AccessAction::Toggle));
        assert_eq!(inspector.take_events().len(), 1);
        // A heading that is currently hidden or unknown is not found.
        inspector.set_level_filter(2);
        let mut cx = harness.event_cx(inspector.id());
        let hidden = tree
            .nodes()
            .iter()
            .find(|n| n.name == "Install")
            .expect("Install")
            .id;
        assert!(!inspector.perform_item(&mut cx, hidden, &AccessAction::Select));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, WidgetId::new(u64::MAX), &AccessAction::Select));

        // Tab items switch tabs and take focus.
        let (contents, info_tab) = (inspector.contents_tab_id, inspector.info_tab_id);
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, info_tab, &AccessAction::Select));
        assert_eq!(inspector.tab(), InspectorTab::Info);
        assert!(harness.focus.is_focused(info_tab));
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform_item(&mut cx, contents, &AccessAction::Expand));
        let mut cx = harness.event_cx(inspector.id());
        assert!(inspector.perform_item(&mut cx, contents, &AccessAction::Invoke));
        assert_eq!(inspector.tab(), InspectorTab::Contents);
        let mut cx = harness.event_cx(inspector.id());
        assert!(!inspector.perform(&mut cx, &AccessAction::Focus));
    }

    #[test]
    fn painting_draws_the_staircase_the_current_section_and_filter() {
        let (mut harness, mut inspector) = setup();
        inspector.set_caret(toc_of(DOC).entries()[2].range.start);
        harness.focus.focus(inspector.list_id);
        let canvas = harness.paint(&inspector);
        let texts = canvas.texts();
        for expected in [
            "Contents", "Info", "H1", "H6", "Intro", "Setup", "Install", "Deep",
        ] {
            assert!(texts.contains(&expected), "missing {expected}: {texts:?}");
        }
        let tokens = harness.theme.tokens;
        // Hairlines: depth 0 rows none, depth 1 rows one, depth 2 two (Setup, Usage, Deep: 1 each,
        // Install: 2).
        let hairlines = canvas
            .ops()
            .filter(
                |op| matches!(op, DrawOp::StrokeLine { color, .. } if *color == tokens.hairline),
            )
            .count();
        assert_eq!(hairlines, 1 + 2 + 1 + 1);
        // The accent bar marks the current row (Install, row 2: y 212..240).
        let row = Rect::new(800.0, 212.0, 280.0, 28.0);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { rect, color, .. }
                if rect.x == row.x + 2.0 && rect.center().y == row.center().y && *color == tokens.accent
        )));
        // The active level button is filled with the accent color.
        let active = inspector.level_rect(6);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { rect, color, .. } if *rect == active && *color == tokens.accent
        )));
    }

    #[test]
    fn hover_states_repaint_and_an_empty_document_says_so() {
        let (mut harness, mut inspector) = setup();
        harness.hover(&mut inspector, Point::new(900.0, 198.0));
        harness.hover(&mut inspector, Point::new(900.0, 198.0));
        let level = inspector.level_rect(3).center();
        harness.hover(&mut inspector, level);
        let tab = inspector.tab_rect(InspectorTab::Info).center();
        harness.hover(&mut inspector, tab);
        harness.pointer(
            &mut inspector,
            PointerEventKind::Leave,
            None,
            Point::new(0.0, 0.0),
        );
        harness.send(&mut inspector, Event::Focus(FocusEvent::Gained));
        harness.frame(&mut inspector, 16);
        let canvas = harness.paint(&inspector);
        assert!(canvas.is_balanced());

        inspector.set_toc(&Toc::default());
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        let canvas = harness.paint(&inspector);
        assert!(canvas.texts().contains(&"No headings"));
        focus_list(&mut harness, &inspector);
        assert!(!harness.key(&mut inspector, vk::DOWN).handled);
    }

    #[test]
    fn long_outlines_scroll_with_the_wheel_and_keep_the_current_section_visible() {
        let (mut harness, mut inspector) = setup();
        let src: String = (0..60).map(|n| format!("# Heading {n}\n")).collect();
        let toc = toc_of(&src);
        inspector.set_toc(&toc);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        assert_eq!(inspector.scroll_offset(), 0.0);
        let wheel = Event::Wheel(WheelEvent {
            delta: Point::new(0.0, -1.0),
            precise: true,
            position: Point::new(900.0, 300.0),
            mods: Modifiers::NONE,
        });
        assert!(harness.send(&mut inspector, wheel).handled);
        assert!(inspector.scroll_offset() > 0.0);
        // A wheel over the header does not scroll the list.
        let header_wheel = Event::Wheel(WheelEvent {
            delta: Point::new(0.0, -1.0),
            precise: true,
            position: Point::new(900.0, 100.0),
            mods: Modifiers::NONE,
        });
        assert!(!harness.send(&mut inspector, header_wheel).handled);
        // Scrolling back to the top is a no-op wheel at the limit.
        let up = Event::Wheel(WheelEvent {
            delta: Point::new(0.0, 100.0),
            precise: true,
            position: Point::new(900.0, 300.0),
            mods: Modifiers::NONE,
        });
        harness.send(&mut inspector, up.clone());
        assert!(!harness.send(&mut inspector, up).handled);
        // The caret moving to the last heading scrolls it into view.
        inspector.set_caret(toc.entries()[59].range.start);
        assert!(inspector.scroll_offset() > 0.0);
        let canvas = harness.paint(&inspector);
        assert!(canvas.texts().contains(&"Heading 59"));
        assert!(!canvas.texts().contains(&"Heading 0"));
        // The Info tab ignores the caret.
        inspector.set_tab(InspectorTab::Info);
        inspector.set_caret(0);
        harness.frame(&mut inspector, 16);
    }

    #[test]
    fn measuring_asks_for_the_header_filter_and_rows() {
        let (mut harness, mut inspector) = setup();
        let mut cx = LayoutCx::new(
            inspector.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            inspector.measure(&mut cx, Size::new(300.0, 500.0)),
            Size::new(300.0, 500.0)
        );
        assert_eq!(
            inspector.measure(&mut cx, Size::UNBOUNDED),
            Size::new(280.0, 40.0 + 36.0 + 6.0 * 28.0)
        );
        assert_eq!(inspector.bounds(), Rect::new(800.0, 80.0, 280.0, 400.0));
    }

    fn long_outline(motion: Motion) -> (Harness, Inspector) {
        let mut harness = Harness::new();
        let mut inspector =
            Inspector::new(InspectorStrings::from_localizer(&localizer())).with_motion(motion);
        let src: String = (0..60).map(|n| format!("# Heading {n}\n")).collect();
        inspector.set_toc(&toc_of(&src));
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        (harness, inspector)
    }

    fn rings(harness: &mut Harness, inspector: &Inspector) -> usize {
        let ring = harness.theme.tokens.focus_ring;
        harness
            .paint(inspector)
            .ops()
            .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == ring))
            .count()
    }

    fn hover_fills(harness: &mut Harness, inspector: &Inspector) -> Vec<Rect> {
        let hover = harness.theme.tokens.hover_bg;
        harness
            .paint(inspector)
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRoundedRect { rect, color, .. } if *color == hover => Some(*rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_custom_id_names_the_panel_in_the_access_tree() {
        let id = WidgetId::allocate();
        let inspector = Inspector::new(InspectorStrings::from_localizer(&localizer())).with_id(id);
        assert_eq!(inspector.id(), id);
        assert_eq!(Harness::new().node(&inspector).id, id);
    }

    #[test]
    fn hovering_a_tab_a_level_or_a_heading_fills_it() {
        let (mut harness, mut inspector) = setup();
        assert!(hover_fills(&mut harness, &inspector).is_empty());
        let info_tab = inspector.tab_rect(InspectorTab::Info);
        harness.hover(&mut inspector, info_tab.center());
        assert_eq!(hover_fills(&mut harness, &inspector), [info_tab]);
        let level = inspector.level_rect(3);
        harness.hover(&mut inspector, level.center());
        assert_eq!(hover_fills(&mut harness, &inspector), [level]);
        // The active level and the active tab keep their own fill.
        let active = inspector.level_rect(6);
        harness.hover(&mut inspector, active.center());
        assert!(hover_fills(&mut harness, &inspector).is_empty());
        let row = inspector.item_rect(3);
        harness.hover(&mut inspector, row.center());
        let fills = hover_fills(&mut harness, &inspector);
        assert_eq!(fills.len(), 1);
        assert!(row.contains_rect(fills[0]), "{:?} in {row:?}", fills[0]);
    }

    #[test]
    fn collapsed_sections_point_right_and_open_ones_down() {
        let (mut harness, mut inspector) = setup();
        let glyphs = |harness: &mut Harness, inspector: &Inspector| -> Vec<char> {
            harness
                .paint(inspector)
                .ops()
                .filter_map(|op| match op {
                    DrawOp::Icon { glyph, .. } => Some(*glyph),
                    _ => None,
                })
                .collect()
        };
        // Intro, Setup and Reference have children; Install, Usage and Deep are leaves.
        assert_eq!(
            glyphs(&mut harness, &inspector),
            [
                glyph::CHEVRON_DOWN,
                glyph::CHEVRON_DOWN,
                glyph::CHEVRON_DOWN
            ]
        );
        inspector.set_collapsed(1, true);
        assert_eq!(
            glyphs(&mut harness, &inspector),
            [
                glyph::CHEVRON_DOWN,
                glyph::CHEVRON_RIGHT,
                glyph::CHEVRON_DOWN
            ]
        );
    }

    #[test]
    fn the_keyboard_focus_is_ringed_on_the_tab_the_filter_the_list_and_its_row() {
        let (mut harness, mut inspector) = setup();
        assert_eq!(rings(&mut harness, &inspector), 0);
        harness.focus.focus(inspector.contents_tab_id);
        assert_eq!(rings(&mut harness, &inspector), 1);
        harness.focus.focus(inspector.levels_id);
        assert_eq!(rings(&mut harness, &inspector), 1);
        // The list is outlined, and so is the row the keyboard chose.
        focus_list(&mut harness, &inspector);
        assert_eq!(rings(&mut harness, &inspector), 1);
        harness.key(&mut inspector, vk::DOWN);
        assert_eq!(rings(&mut harness, &inspector), 2);
    }

    #[test]
    fn backlink_rows_highlight_on_hover_and_ring_the_keyboard_choice() {
        let (mut harness, mut inspector) = setup();
        fill_info(&mut inspector);
        inspector.set_tab(InspectorTab::Info);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        focus_list(&mut harness, &inspector);
        // Down twice chooses the second link and scrolls it into view.
        harness.key(&mut inspector, vk::DOWN);
        harness.key(&mut inspector, vk::DOWN);
        assert_eq!(rings(&mut harness, &inspector), 2);
        assert!(hover_fills(&mut harness, &inspector).is_empty());
        let second = inspector.item_rect(16);
        harness.hover(&mut inspector, second.center());
        let fills = hover_fills(&mut harness, &inspector);
        assert_eq!(fills.len(), 1);
        assert!(
            second.contains_rect(fills[0]),
            "{:?} in {second:?}",
            fills[0]
        );
    }

    #[test]
    fn nothing_is_painted_outside_the_damaged_area() {
        let (mut harness, inspector) = setup();
        let away = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(
            harness.paint_damaged(&inspector, Some(away)).ops().count(),
            0
        );
        let over = inspector.bounds();
        assert!(harness.paint_damaged(&inspector, Some(over)).ops().count() > 0);
    }

    #[test]
    fn a_press_on_the_header_padding_hits_nothing() {
        let (mut harness, mut inspector) = setup();
        // Inside the panel, left of the first tab and above the filter row.
        let result = harness.click(&mut inspector, Point::new(801.0, 100.0));
        assert!(!result.handled);
        assert!(inspector.take_events().is_empty());
    }

    #[test]
    fn only_the_headings_in_view_are_described_to_accessibility() {
        let (mut harness, mut inspector) = long_outline(Motion::reduced());
        let described = |harness: &Harness, inspector: &Inspector| -> Vec<String> {
            harness
                .tree(inspector)
                .nodes()
                .iter()
                .filter(|node| node.role == Role::TreeItem)
                .map(|node| node.name.clone())
                .collect()
        };
        let top = described(&harness, &inspector);
        assert!(!top.is_empty() && top.len() < 20, "{} described", top.len());
        assert_eq!(top[0], "Heading 0");
        let max = inspector.bar.model().max_offset();
        inspector.bar.model_mut().jump(max);
        harness.arrange(&mut inspector, Rect::new(800.0, 80.0, 280.0, 400.0));
        let bottom = described(&harness, &inspector);
        assert!(bottom.len() < 20);
        assert!(!bottom.contains(&"Heading 0".to_owned()));
        assert_eq!(bottom.last().map(String::as_str), Some("Heading 59"));
    }

    #[test]
    fn dragging_the_scroll_bar_scrolls_the_outline() {
        let (mut harness, mut inspector) = long_outline(Motion::reduced());
        let grab = inspector.bar.thumb_rect().center();
        let left = Some(PointerButton::Left);
        // The bar takes the press: no heading under it is clicked.
        let down = harness.pointer(&mut inspector, PointerEventKind::Down, left, grab);
        assert!(down.handled);
        assert!(inspector.take_events().is_empty());
        harness.requests.take_invalidation();
        let to = Point::new(grab.x, grab.y + 60.0);
        harness.pointer(&mut inspector, PointerEventKind::Move, None, to);
        assert!(inspector.scroll_offset() > 0.0);
        assert!(!harness.requests.take_invalidation().is_clean());
        harness.pointer(&mut inspector, PointerEventKind::Up, left, to);
        assert!(!inspector.bar.is_dragging());
    }

    #[test]
    fn a_notched_wheel_eases_the_outline_and_every_frame_repaints_it() {
        let (mut harness, mut inspector) = long_outline(Motion::full());
        let notch = Event::Wheel(WheelEvent {
            delta: Point::new(0.0, -5.0),
            precise: false,
            position: Point::new(900.0, 300.0),
            mods: Modifiers::NONE,
        });
        assert!(harness.send(&mut inspector, notch).handled);
        assert_eq!(inspector.scroll_offset(), 0.0);
        harness.requests.take_invalidation();
        // The first frame fixes the start; the later ones move the offset and ask to repaint.
        harness.frame(&mut inspector, 16);
        harness.requests.take_invalidation();
        harness.frame(&mut inspector, 100);
        assert!(inspector.scroll_offset() > 0.0);
        assert_eq!(
            harness.requests.take_invalidation(),
            Invalidation::Rect(inspector.bounds())
        );
        harness.run_animations(&mut inspector);
        assert_eq!(inspector.scroll_offset(), inspector.bar.model().target());
    }
}
