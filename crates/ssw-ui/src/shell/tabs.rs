//! Document tabs of the title row (spec §6): file-stem titles, dirty dot, close on hover,
//! middle-click close, drag reorder, drag out to a new window, drop of a tab from another window,
//! context menu requests.
//!
//! [`DocumentTabs`] wraps the widget kit's [`TabStrip`] (painting, pointer and keyboard handling,
//! access nodes) and adds what the shell needs on top: titles derived from paths, tab keys
//! mapped to documents, the close targets of "Close others" / "Close to the right", the
//! hit regions the title bar needs and the events in shell terms ([`TabsEvent`]).

use ssw_core::i18n::Localizer;

use crate::access::{AccessAction, AccessTree};
use crate::canvas::Canvas;
use crate::geometry::{Point, Rect, Size};
use crate::input::{Event, KeyEvent, Modifiers, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::Control;
use crate::widgets::anim::Motion;
use crate::widgets::tabs::{NEW_TAB_SIZE, TabEvent, TabStrip};

/// Localized strings of the tab strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStrings {
    /// Accessible name of the strip.
    pub strip_label: String,
    /// Accessible name of the new-tab button.
    pub new_tab: String,
    /// Announced for tabs with unsaved changes.
    pub unsaved: String,
}

impl TabStrings {
    /// Looks the strings up in `shell.ftl`.
    #[must_use]
    pub fn from_localizer(localizer: &Localizer) -> Self {
        Self {
            strip_label: localizer.text("tabs-label"),
            new_tab: localizer.text("tabs-new"),
            unsaved: localizer.text("tabs-unsaved"),
        }
    }
}

/// The tab title of a file: its name, without the extension for Markdown files (`.md`,
/// `.markdown`, `.mdx`) and complete for everything else.
#[must_use]
pub fn title_for_path(path: &str) -> String {
    let name = path.rsplit(['\\', '/']).next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 && is_markdown_extension(&name[dot + 1..]) => name[..dot].to_owned(),
        _ => name.to_owned(),
    }
}

fn is_markdown_extension(extension: &str) -> bool {
    ["md", "markdown", "mdx"]
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

/// An entry of the tab context menu. The shell requests a native popup; the app builds it from
/// these and applies the choice with [`DocumentTabs::close_targets`] or its own services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAction {
    Close,
    CloseOthers,
    CloseToRight,
    CopyPath,
    RevealInExplorer,
}

/// Which context menu entries are available for a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabMenu {
    /// "Close others" has something to close.
    pub can_close_others: bool,
    /// "Close to the right" has something to close.
    pub can_close_to_right: bool,
    /// The document has a file ("Copy path", "Reveal in Explorer").
    pub has_path: bool,
}

impl TabMenu {
    /// Whether `action` is enabled.
    #[must_use]
    pub fn allows(&self, action: TabAction) -> bool {
        match action {
            TabAction::Close => true,
            TabAction::CloseOthers => self.can_close_others,
            TabAction::CloseToRight => self.can_close_to_right,
            TabAction::CopyPath | TabAction::RevealInExplorer => self.has_path,
        }
    }
}

/// What the user did to the tabs, in terms of document keys.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TabsEvent {
    /// The tab became the active one.
    Activated(u64),
    /// Close button, middle click or Delete.
    CloseRequested(u64),
    /// The new-tab button.
    NewTab,
    /// A drag moved the tab to another position.
    Reordered { key: u64, from: usize, to: usize },
    /// A drag moved the tab far enough off the strip to become its own window; `at` is the pointer
    /// position in window coordinates.
    TornOut { key: u64, at: Point },
    /// Right click, Apps key or Shift+F10: show the context menu at `at`.
    ContextMenu { key: u64, at: Point, menu: TabMenu },
}

/// What is under a point of the tab row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabHit {
    Tab(u64),
    /// The close button of a tab.
    Close(u64),
    NewTab,
}

#[derive(Debug, Clone)]
struct TabMeta {
    key: u64,
    path: Option<String>,
}

/// The tab strip of the title row.
#[derive(Debug, Clone)]
pub struct DocumentTabs {
    strip: TabStrip,
    meta: Vec<TabMeta>,
    last_pointer: Point,
    events: Vec<TabsEvent>,
}

impl DocumentTabs {
    /// An empty strip.
    #[must_use]
    pub fn new(strings: &TabStrings) -> Self {
        Self {
            strip: TabStrip::new(&strings.strip_label, &strings.new_tab, &strings.unsaved),
            meta: Vec::new(),
            last_pointer: Point::ZERO,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.strip = self.strip.with_id(id);
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.strip = self.strip.with_motion(motion);
        self
    }

    /// The wrapped strip (for geometry queries such as [`TabStrip::tab_rect`]).
    #[must_use]
    pub fn strip(&self) -> &TabStrip {
        &self.strip
    }

    /// Appends the tab of the file at `path`.
    pub fn add_file(&mut self, key: u64, path: &str) -> usize {
        self.meta.push(TabMeta {
            key,
            path: Some(path.to_owned()),
        });
        self.strip.add_tab(key, title_for_path(path))
    }

    /// Appends the tab of a document without a file, titled `title`: the localized name the
    /// owner of the document gives it ("Untitled", "Untitled 2", …), so the tab and the rest of
    /// the app never disagree about it.
    pub fn add_untitled(&mut self, key: u64, title: &str) -> usize {
        self.meta.push(TabMeta { key, path: None });
        self.strip.add_tab(key, title)
    }

    /// The index of the tab with `key`.
    #[must_use]
    pub fn index_of(&self, key: u64) -> Option<usize> {
        (0..self.strip.len()).find(|&index| self.strip.key_at(index) == Some(key))
    }

    /// Removes the tab with `key`. Returns whether it existed.
    pub fn remove(&mut self, key: u64) -> bool {
        let Some(index) = self.index_of(key) else {
            return false;
        };
        self.strip.remove_tab(index);
        self.meta.retain(|meta| meta.key != key);
        true
    }

    /// Makes the tab with `key` active without an event.
    pub fn set_active_key(&mut self, key: u64) {
        if let Some(index) = self.index_of(key) {
            self.strip.set_active(index);
        }
    }

    /// The key of the active tab.
    #[must_use]
    pub fn active_key(&self) -> Option<u64> {
        self.strip.key_at(self.strip.active())
    }

    /// Marks the tab with `key` as having unsaved changes (autosave off) or not.
    pub fn set_dirty(&mut self, key: u64, dirty: bool) {
        if let Some(index) = self.index_of(key) {
            self.strip.set_dirty(index, dirty);
        }
    }

    /// The file of the tab changed (save as, rename): updates the title.
    pub fn set_path(&mut self, key: u64, path: &str) {
        let Some(index) = self.index_of(key) else {
            return;
        };
        if let Some(meta) = self.meta.iter_mut().find(|meta| meta.key == key) {
            meta.path = Some(path.to_owned());
        }
        self.strip.set_title(index, title_for_path(path));
    }

    /// The file path of the tab with `key`.
    #[must_use]
    pub fn path_of(&self, key: u64) -> Option<&str> {
        self.meta
            .iter()
            .find(|meta| meta.key == key)
            .and_then(|meta| meta.path.as_deref())
    }

    /// The displayed title of the tab with `key`.
    #[must_use]
    pub fn title_of(&self, key: u64) -> Option<&str> {
        self.index_of(key)
            .and_then(|index| self.strip.title_at(index))
    }

    /// The keys of all tabs in strip order.
    #[must_use]
    pub fn keys(&self) -> Vec<u64> {
        (0..self.strip.len())
            .filter_map(|index| self.strip.key_at(index))
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.strip.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.strip.is_empty()
    }

    /// The tabs to close for a context menu `action` on the tab with `key`: the tab itself,
    /// every other tab, or every tab to its right. Copy path and Reveal close nothing.
    #[must_use]
    pub fn close_targets(&self, action: TabAction, key: u64) -> Vec<u64> {
        let keys = self.keys();
        let Some(index) = keys.iter().position(|&candidate| candidate == key) else {
            return Vec::new();
        };
        match action {
            TabAction::Close => vec![key],
            TabAction::CloseOthers => keys.into_iter().filter(|&other| other != key).collect(),
            TabAction::CloseToRight => keys[index + 1..].to_vec(),
            TabAction::CopyPath | TabAction::RevealInExplorer => Vec::new(),
        }
    }

    /// The context menu entries available for the tab with `key`.
    #[must_use]
    pub fn menu_for(&self, key: u64) -> TabMenu {
        let index = self.index_of(key);
        TabMenu {
            can_close_others: self.len() > 1,
            can_close_to_right: index.is_some_and(|index| index + 1 < self.len()),
            has_path: self.path_of(key).is_some(),
        }
    }

    fn tabs_area(&self) -> Rect {
        let bounds = self.strip.bounds();
        Rect::new(
            bounds.x,
            bounds.y,
            (bounds.width - NEW_TAB_SIZE).max(0.0),
            bounds.height,
        )
    }

    /// What is at `point`: a tab, a tab's close button or the new-tab button. `None` over the
    /// empty rest of the row (which is draggable caption).
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<TabHit> {
        if self.strip.new_tab_rect().contains(point) {
            return Some(TabHit::NewTab);
        }
        if !self.tabs_area().contains(point) {
            return None;
        }
        (0..self.strip.len()).find_map(|index| {
            if !self.strip.tab_rect(index).contains(point) {
                return None;
            }
            let key = self.strip.key_at(index)?;
            if self.strip.close_rect(index).contains(point) {
                Some(TabHit::Close(key))
            } else {
                Some(TabHit::Tab(key))
            }
        })
    }

    /// The position a tab dropped from another window at `point` would take: the boundary
    /// nearest to the pointer. `None` when `point` is outside the tab row.
    #[must_use]
    pub fn drop_index_at(&self, point: Point) -> Option<usize> {
        let bounds = self.strip.bounds();
        if !bounds.contains(point) {
            return None;
        }
        let width = self.strip.tab_width();
        let x = point.x - bounds.x + self.strip.scroll_offset();
        let boundary = (x / width + 0.5).floor().max(0.0) as usize;
        Some(boundary.min(self.strip.len()))
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<TabsEvent> {
        std::mem::take(&mut self.events)
    }

    fn context_event(&mut self, key: u64, at: Point) {
        let menu = self.menu_for(key);
        self.events.push(TabsEvent::ContextMenu { key, at, menu });
    }

    fn drain(&mut self) {
        for event in self.strip.take_events() {
            let mapped = match event {
                TabEvent::Activated(key) => TabsEvent::Activated(key),
                TabEvent::CloseRequested(key) => TabsEvent::CloseRequested(key),
                TabEvent::NewTab => TabsEvent::NewTab,
                TabEvent::Reordered { key, from, to } => TabsEvent::Reordered { key, from, to },
                TabEvent::TornOff(key) => TabsEvent::TornOut {
                    key,
                    at: self.last_pointer,
                },
                TabEvent::ContextMenu(key) => {
                    self.context_event(key, self.last_pointer);
                    continue;
                }
            };
            self.events.push(mapped);
        }
    }

    /// Carries out an access action on the tab whose access node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let done = self.strip.perform_item(cx, item, action);
        self.drain();
        done
    }
}

fn is_context_key(key: &KeyEvent) -> bool {
    let apps = key.vk == vk::APPS && key.mods.is_empty();
    let shift_f10 = key.vk == vk::F10 && key.mods == Modifiers::SHIFT;
    key.is_down && (apps || shift_f10)
}

impl Control for DocumentTabs {
    fn id(&self) -> WidgetId {
        self.strip.id()
    }
}

impl Widget for DocumentTabs {
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size {
        self.strip.measure(cx, available)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.strip.arrange(cx, bounds);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        self.strip.paint(cx, canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let result = match e {
            Event::Key(key) if cx.is_focused() && is_context_key(key) => match self.active_key() {
                Some(key) => {
                    let rect = self.strip.tab_rect(self.strip.active());
                    self.context_event(key, Point::new(rect.x, rect.bottom()));
                    EventResult::handled()
                }
                None => EventResult::ignored(),
            },
            Event::Pointer(pointer) => {
                self.last_pointer = pointer.position;
                self.strip.event(cx, e)
            }
            _ => self.strip.event(cx, e),
        };
        self.drain();
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.strip.access(cx, out);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        self.strip.perform(cx, a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Role;
    use crate::input::{PointerButton, PointerEventKind};
    use crate::widgets::tabs::TAB_HEIGHT;
    use crate::widgets::testkit::Harness;

    fn strings() -> TabStrings {
        TabStrings {
            strip_label: "Open documents".into(),
            new_tab: "New tab".into(),
            unsaved: "Unsaved changes".into(),
        }
    }

    /// Three tabs (keys 10, 20, 30) in a 700 DIP wide strip: 200 DIP tabs.
    fn setup() -> (Harness, DocumentTabs) {
        let mut harness = Harness::new();
        let mut tabs = DocumentTabs::new(&strings()).with_motion(Motion::reduced());
        tabs.add_file(10, "C:\\notes\\todo.md");
        tabs.add_file(20, "C:\\notes\\data.csv");
        tabs.add_untitled(30, "Untitled");
        harness.arrange(&mut tabs, Rect::new(0.0, 0.0, 700.0, TAB_HEIGHT));
        (harness, tabs)
    }

    #[test]
    fn titles_drop_markdown_extensions_only() {
        assert_eq!(title_for_path("C:\\notes\\todo.md"), "todo");
        assert_eq!(title_for_path("/home/me/Todo.MARKDOWN"), "Todo");
        assert_eq!(title_for_path("diagram.mdx"), "diagram");
        assert_eq!(title_for_path("C:\\notes\\data.csv"), "data.csv");
        assert_eq!(title_for_path("diagram.mmd"), "diagram.mmd");
        assert_eq!(title_for_path("README"), "README");
        assert_eq!(title_for_path(".md"), ".md");
        assert_eq!(title_for_path("a.b.md"), "a.b");
        assert_eq!(title_for_path(""), "");
    }

    #[test]
    fn strings_come_from_the_ftl_ids() {
        let localizer = Localizer::new(
            "en",
            &[(
                "en",
                "tabs-label = Docs\ntabs-new = New\ntabs-unsaved = Dirty\n",
            )],
        );
        let strings = TabStrings::from_localizer(&localizer);
        assert_eq!(strings.strip_label, "Docs");
        assert_eq!(strings.new_tab, "New");
        assert_eq!(strings.unsaved, "Dirty");
    }

    #[test]
    fn untitled_documents_show_the_title_their_owner_gives_them() {
        let mut tabs = DocumentTabs::new(&strings());
        tabs.add_untitled(1, "Untitled");
        tabs.add_untitled(2, "Untitled 2");
        tabs.add_untitled(3, "Sans titre 3");
        assert_eq!(tabs.title_of(1), Some("Untitled"));
        assert_eq!(tabs.title_of(2), Some("Untitled 2"));
        assert_eq!(tabs.title_of(3), Some("Sans titre 3"));
        assert_eq!(tabs.path_of(1), None);
        assert_eq!(tabs.title_of(99), None);
    }

    #[test]
    fn tabs_track_keys_titles_dirty_and_paths() {
        let (_, mut tabs) = setup();
        assert_eq!(tabs.len(), 3);
        assert!(!tabs.is_empty());
        assert_eq!(tabs.keys(), vec![10, 20, 30]);
        assert_eq!(tabs.title_of(10), Some("todo"));
        assert_eq!(tabs.title_of(20), Some("data.csv"));
        assert_eq!(tabs.path_of(20), Some("C:\\notes\\data.csv"));
        assert_eq!(tabs.index_of(30), Some(2));
        assert_eq!(tabs.index_of(99), None);

        tabs.set_active_key(20);
        assert_eq!(tabs.active_key(), Some(20));
        tabs.set_active_key(99);
        assert_eq!(tabs.active_key(), Some(20));

        // Save as gives the untitled tab its name.
        tabs.set_path(30, "C:\\notes\\new idea.md");
        assert_eq!(tabs.title_of(30), Some("new idea"));
        assert_eq!(tabs.path_of(30), Some("C:\\notes\\new idea.md"));
        tabs.set_path(99, "x.md");
        assert_eq!(tabs.path_of(99), None);

        tabs.set_dirty(10, true);
        tabs.set_dirty(99, true);

        assert!(tabs.remove(20));
        assert!(!tabs.remove(20));
        assert_eq!(tabs.keys(), vec![10, 30]);
        assert_eq!(tabs.path_of(20), None);
    }

    #[test]
    fn close_targets_and_menu_follow_the_clicked_tab() {
        let (_, tabs) = setup();
        assert_eq!(tabs.close_targets(TabAction::Close, 20), vec![20]);
        assert_eq!(tabs.close_targets(TabAction::CloseOthers, 20), vec![10, 30]);
        assert_eq!(
            tabs.close_targets(TabAction::CloseToRight, 10),
            vec![20, 30]
        );
        assert!(tabs.close_targets(TabAction::CloseToRight, 30).is_empty());
        assert!(tabs.close_targets(TabAction::CopyPath, 10).is_empty());
        assert!(
            tabs.close_targets(TabAction::RevealInExplorer, 10)
                .is_empty()
        );
        assert!(tabs.close_targets(TabAction::Close, 99).is_empty());

        let first = tabs.menu_for(10);
        assert!(first.can_close_others && first.can_close_to_right && first.has_path);
        let last = tabs.menu_for(30);
        assert!(last.can_close_others && !last.can_close_to_right && !last.has_path);
        assert!(first.allows(TabAction::Close));
        assert!(first.allows(TabAction::CloseOthers));
        assert!(first.allows(TabAction::CloseToRight));
        assert!(first.allows(TabAction::CopyPath));
        assert!(!last.allows(TabAction::CloseToRight));
        assert!(!last.allows(TabAction::RevealInExplorer));

        let mut single = DocumentTabs::new(&strings());
        single.add_untitled(1, "Untitled");
        let menu = single.menu_for(1);
        assert!(!menu.can_close_others && !menu.can_close_to_right && !menu.has_path);
        assert!(!single.menu_for(99).can_close_to_right);
    }

    #[test]
    fn clicks_activate_close_and_add() {
        let (mut harness, mut tabs) = setup();
        // Tab 2 (x 200..400): click selects it.
        harness.click(&mut tabs, Point::new(300.0, 20.0));
        assert_eq!(tabs.take_events(), vec![TabsEvent::Activated(20)]);
        assert_eq!(tabs.active_key(), Some(20));
        // The close button of the active tab.
        let close = tabs.strip().close_rect(1).center();
        harness.click(&mut tabs, close);
        assert_eq!(tabs.take_events(), vec![TabsEvent::CloseRequested(20)]);
        // Middle click closes any tab.
        harness.pointer(
            &mut tabs,
            PointerEventKind::Down,
            Some(PointerButton::Middle),
            Point::new(50.0, 20.0),
        );
        assert_eq!(tabs.take_events(), vec![TabsEvent::CloseRequested(10)]);
        // The new-tab button.
        let new_tab = tabs.strip().new_tab_rect().center();
        harness.click(&mut tabs, new_tab);
        assert_eq!(tabs.take_events(), vec![TabsEvent::NewTab]);
    }

    #[test]
    fn right_click_requests_the_context_menu_at_the_pointer() {
        let (mut harness, mut tabs) = setup();
        let at = Point::new(320.0, 22.0);
        harness.pointer(
            &mut tabs,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            at,
        );
        let events = tabs.take_events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], TabsEvent::Activated(20));
        assert_eq!(
            events[1],
            TabsEvent::ContextMenu {
                key: 20,
                at,
                menu: TabMenu {
                    can_close_others: true,
                    can_close_to_right: true,
                    has_path: true,
                },
            }
        );
    }

    #[test]
    fn the_context_key_opens_the_menu_below_the_active_tab() {
        let (mut harness, mut tabs) = setup();
        // Without focus the key is not for the strip.
        assert!(!harness.key(&mut tabs, vk::APPS).handled);
        harness.focus_widget(&mut tabs);
        tabs.set_active_key(20);
        assert!(harness.key(&mut tabs, vk::APPS).handled);
        assert!(
            harness
                .key_with(&mut tabs, vk::F10, Modifiers::SHIFT)
                .handled
        );
        let events = tabs.take_events();
        assert_eq!(events.len(), 2);
        assert!(
            events.iter().all(|event| matches!(
                event,
                TabsEvent::ContextMenu { key: 20, at, .. } if *at == Point::new(200.0, 40.0)
            )),
            "{events:?}"
        );
        // F10 alone is not a context key; arrow keys still move the selection.
        assert!(!harness.key(&mut tabs, vk::F10).handled);
        assert!(harness.key(&mut tabs, vk::RIGHT).handled);
        assert_eq!(tabs.take_events(), vec![TabsEvent::Activated(30)]);

        let mut empty = DocumentTabs::new(&strings());
        harness.arrange(&mut empty, Rect::new(0.0, 0.0, 300.0, TAB_HEIGHT));
        // An empty strip is no tab stop; force focus onto it to reach the key handler.
        harness.focus.set_tab_order([empty.id()]);
        harness.focus_widget(&mut empty);
        assert!(!harness.key(&mut empty, vk::APPS).handled);
        assert!(empty.take_events().is_empty());
    }

    #[test]
    fn dragging_reorders_and_tearing_off_reports_the_pointer() {
        let (mut harness, mut tabs) = setup();
        let press = Point::new(100.0, 20.0);
        harness.pointer(
            &mut tabs,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            press,
        );
        harness.pointer(
            &mut tabs,
            PointerEventKind::Move,
            None,
            Point::new(310.0, 20.0),
        );
        let events = tabs.take_events();
        assert!(events.contains(&TabsEvent::Reordered {
            key: 10,
            from: 0,
            to: 1
        }));
        assert_eq!(tabs.keys(), vec![20, 10, 30]);

        let away = Point::new(310.0, 120.0);
        harness.pointer(&mut tabs, PointerEventKind::Move, None, away);
        assert_eq!(
            tabs.take_events(),
            vec![TabsEvent::TornOut { key: 10, at: away }]
        );
        harness.pointer(
            &mut tabs,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            away,
        );
    }

    #[test]
    fn hit_testing_separates_tabs_close_buttons_and_the_empty_caption() {
        let (_, tabs) = setup();
        assert_eq!(tabs.hit_test(Point::new(60.0, 20.0)), Some(TabHit::Tab(10)));
        assert_eq!(
            tabs.hit_test(Point::new(300.0, 20.0)),
            Some(TabHit::Tab(20))
        );
        let close = tabs.strip().close_rect(0).center();
        assert_eq!(tabs.hit_test(close), Some(TabHit::Close(10)));
        assert_eq!(
            tabs.hit_test(tabs.strip().new_tab_rect().center()),
            Some(TabHit::NewTab)
        );
        // Right of the new-tab button the row is empty caption.
        assert_eq!(tabs.hit_test(Point::new(660.0, 20.0)), None);
        assert_eq!(tabs.hit_test(Point::new(650.0, 20.0)), None);
        assert_eq!(tabs.hit_test(Point::new(300.0, 100.0)), None);
    }

    #[test]
    fn drop_index_is_the_nearest_tab_boundary() {
        let (_, tabs) = setup();
        assert_eq!(tabs.drop_index_at(Point::new(10.0, 20.0)), Some(0));
        assert_eq!(tabs.drop_index_at(Point::new(120.0, 20.0)), Some(1));
        assert_eq!(tabs.drop_index_at(Point::new(290.0, 20.0)), Some(1));
        assert_eq!(tabs.drop_index_at(Point::new(310.0, 20.0)), Some(2));
        assert_eq!(tabs.drop_index_at(Point::new(690.0, 20.0)), Some(3));
        assert_eq!(tabs.drop_index_at(Point::new(300.0, 80.0)), None);
        assert_eq!(tabs.drop_index_at(Point::new(750.0, 20.0)), None);
    }

    #[test]
    fn access_tree_lists_tab_items_with_selection_and_unsaved_state() {
        let (harness, mut tabs) = setup();
        tabs.set_dirty(20, true);
        tabs.set_active_key(20);
        let tree = harness.tree(&tabs);
        let root = tree.root().expect("strip node");
        assert_eq!(root.role, Role::Tab);
        assert_eq!(root.name, "Open documents");
        let items: Vec<_> = tree
            .children(tabs.id())
            .filter(|node| node.role == Role::TabItem)
            .collect();
        let names: Vec<&str> = items.iter().map(|node| node.name.as_str()).collect();
        assert_eq!(names, vec!["todo", "data.csv", "Untitled"]);
        assert_eq!(
            items.iter().map(|n| n.state.selected).collect::<Vec<_>>(),
            vec![false, true, false]
        );
        assert_eq!(items[1].description, "Unsaved changes");
        assert!(items[0].description.is_empty());
        assert!(
            tree.children(tabs.id())
                .any(|node| node.role == Role::Button && node.name == "New tab")
        );
    }

    #[test]
    fn selecting_a_tab_item_through_access_reports_the_activation() {
        let (mut harness, mut tabs) = setup();
        let tree = harness.tree(&tabs);
        let target = tree
            .children(tabs.id())
            .find(|node| node.name == "data.csv")
            .expect("tab item")
            .id;
        let mut cx = harness.event_cx(tabs.id());
        assert!(tabs.perform_item(&mut cx, target, &AccessAction::Select));
        assert_eq!(tabs.take_events(), vec![TabsEvent::Activated(20)]);
        let mut cx = harness.event_cx(tabs.id());
        assert!(!tabs.perform_item(&mut cx, WidgetId::new(u64::MAX), &AccessAction::Select));
        assert!(harness.perform(&mut tabs, AccessAction::Focus));
    }

    #[test]
    fn painting_draws_every_title() {
        let (mut harness, tabs) = setup();
        let canvas = harness.paint(&tabs);
        let texts = canvas.texts();
        assert!(texts.contains(&"todo"));
        assert!(texts.contains(&"data.csv"));
        assert!(texts.contains(&"Untitled"));
    }

    #[test]
    fn measuring_follows_the_strip() {
        let (mut harness, mut tabs) = setup();
        let bounds = harness.place(&mut tabs, Point::ZERO);
        assert_eq!(bounds.height, TAB_HEIGHT);
    }

    #[test]
    fn a_custom_id_names_the_strip_in_the_access_tree() {
        let id = WidgetId::allocate();
        let tabs = DocumentTabs::new(&strings()).with_id(id);
        assert_eq!(tabs.id(), id);
        assert_eq!(Harness::new().node(&tabs).id, id);
    }
}
