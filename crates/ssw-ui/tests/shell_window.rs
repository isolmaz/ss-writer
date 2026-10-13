//! Shell-level integration: the window layout, title bar, sidebar, inspector, status bar and
//! banner host laid out together by a minimal host that routes input by pane, like the real window
//! does. Uses only the public API.

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::i18n::Localizer;
use ssw_core::markdown::parse_document;
use ssw_core::toc::Toc;
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::editor::highlight::Language;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, PointerButton, PointerEvent, PointerEventKind, PointerType};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::shell::banners::{BannerHost, BannerStrings};
use ssw_ui::shell::inspector::{Inspector, InspectorStrings};
use ssw_ui::shell::layout::{Backdrop, ChromeHit, Pane, ShellLayout, ShellMode};
use ssw_ui::shell::sidebar::{
    EntryKind, NodeKey, SidebarEntry, SidebarEvent, SidebarOptions, SidebarStrings, SidebarView,
    TreeSource,
};
use ssw_ui::shell::statusbar::{
    Eol, StatusBar, StatusEvent, StatusInfo, StatusLanguage, StatusSegment, StatusStrings,
};
use ssw_ui::shell::titlebar::{TitleBar, TitleEvent, TitleStrings};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{EventCx, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests, Widget};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

const MAIN_FTL: &str = include_str!("../../../assets/i18n/en/main.ftl");
const SHELL_FTL: &str = include_str!("../../../assets/i18n/en/shell.ftl");

/// Delivers `$event` to the view `$field` of window `$window`.
macro_rules! route {
    ($window:ident, $field:ident, $event:ident) => {{
        let id = $window.$field.id();
        let mut cx = EventCx::new(
            id,
            &$window.theme,
            $window.layout.scale(),
            &$window.engine,
            &mut $window.focus,
            &mut $window.capture,
            &mut $window.requests,
        );
        $window.$field.event(&mut cx, $event);
    }};
}

struct Workspace;

impl TreeSource for Workspace {
    fn root_name(&self) -> String {
        "Notes".to_owned()
    }

    fn root_key(&self) -> NodeKey {
        0
    }

    fn children(&mut self, folder: NodeKey, _: &SidebarOptions) -> Vec<SidebarEntry> {
        match folder {
            0 => vec![
                SidebarEntry::folder(1, "docs"),
                SidebarEntry::file(2, "todo.md", EntryKind::Markdown),
            ],
            1 => vec![SidebarEntry::file(3, "guide.md", EntryKind::Markdown)],
            _ => Vec::new(),
        }
    }
}

struct Window {
    theme: Theme,
    engine: MonoTextEngine,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
    layout: ShellLayout,
    title: TitleBar,
    sidebar: SidebarView,
    inspector: Inspector,
    status: StatusBar,
    banners: BannerHost,
}

impl Window {
    fn new(width: f32, height: f32, scale: f32) -> Self {
        let localizer = Localizer::new("en", &[("en", MAIN_FTL), ("en", SHELL_FTL)]);
        assert!(
            localizer.diagnostics().is_empty(),
            "{:?}",
            localizer.diagnostics()
        );
        let motion = Motion::reduced();
        let mut title = TitleBar::new(TitleStrings::from_localizer(&localizer)).with_motion(motion);
        title.tabs_mut().add_file(1, "C:\\notes\\todo.md");
        title.tabs_mut().add_file(2, "C:\\notes\\data.csv");
        let sidebar = SidebarView::new(
            SidebarStrings::from_localizer(&localizer),
            Box::new(Workspace),
            Rc::new(RefCell::new(MemoryClipboard::default())),
            SidebarOptions::default(),
        )
        .with_motion(motion);
        let mut inspector =
            Inspector::new(InspectorStrings::from_localizer(&localizer)).with_motion(motion);
        let src = "# Intro\n## Setup\n";
        inspector.set_toc(&Toc::build(&parse_document(src), src));
        let mut status =
            StatusBar::new(StatusStrings::from_localizer(&localizer)).with_motion(motion);
        status.set_info(
            &StatusInfo {
                line: 3,
                column: 9,
                words: 120,
                selected_words: None,
                zoom_percent: 100,
                encoding: "UTF-8".to_owned(),
                eol: Eol::Lf,
                language: StatusLanguage::Hidden,
            },
            &localizer,
        );
        let mut banners = BannerHost::new(BannerStrings::from_localizer(&localizer));
        banners.show_read_only();
        let mut window = Self {
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
            layout: ShellLayout::new(Size::new(width, height), scale),
            title,
            sidebar,
            inspector,
            status,
            banners,
        };
        window.layout_all();
        window
    }

    fn arrange(
        theme: &Theme,
        engine: &MonoTextEngine,
        focus: &mut FocusManager,
        requests: &mut Requests,
        scale: f32,
        widget: &mut dyn Control,
        bounds: Rect,
    ) {
        let id = widget.id();
        let mut cx = LayoutCx::new(id, theme, scale, engine, focus, requests);
        widget.arrange(&mut cx, bounds);
    }

    /// Lays every view out in its pane and commits the tab order (visual order).
    fn layout_all(&mut self) {
        let rects = self.layout.rects();
        let scale = self.layout.scale();
        self.title
            .set_caption_width(rects.caption_buttons.map_or(0.0, |r| r.width));
        self.focus.begin_tab_order();
        let (theme, engine) = (&self.theme, &self.engine);
        if let (Some(title), Some(command)) = (rects.title_row, rects.command_row) {
            let bounds = title.union(command);
            Self::arrange(
                theme,
                engine,
                &mut self.focus,
                &mut self.requests,
                scale,
                &mut self.title,
                bounds,
            );
        }
        if let Some(sidebar) = rects.sidebar {
            Self::arrange(
                theme,
                engine,
                &mut self.focus,
                &mut self.requests,
                scale,
                &mut self.sidebar,
                sidebar,
            );
        }
        let banner_height = self.banners.content_height();
        let banner_area = Rect::new(
            rects.editor.x,
            rects.editor.y,
            rects.editor.width,
            banner_height,
        );
        Self::arrange(
            theme,
            engine,
            &mut self.focus,
            &mut self.requests,
            scale,
            &mut self.banners,
            banner_area,
        );
        if let Some(inspector) = rects.inspector {
            Self::arrange(
                theme,
                engine,
                &mut self.focus,
                &mut self.requests,
                scale,
                &mut self.inspector,
                inspector,
            );
        }
        if let Some(status) = rects.status_bar {
            Self::arrange(
                theme,
                engine,
                &mut self.focus,
                &mut self.requests,
                scale,
                &mut self.status,
                status,
            );
        }
        self.focus.commit_tab_order();
        self.title.set_panes_open(
            self.layout.rects().sidebar.is_some(),
            self.layout.rects().inspector.is_some(),
        );
    }

    fn send(&mut self, event: &Event) {
        route!(self, title, event);
        route!(self, sidebar, event);
        route!(self, banners, event);
        route!(self, inspector, event);
        route!(self, status, event);
    }

    fn click(&mut self, at: Point, button: PointerButton) {
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            self.send(&Event::Pointer(PointerEvent {
                kind,
                button: Some(button),
                position: at,
                pointer_type: PointerType::Mouse,
                click_count: 1,
                mods: ssw_ui::input::Modifiers::NONE,
            }));
        }
    }

    fn paint(&self) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        let widgets: [&dyn Control; 5] = [
            &self.title,
            &self.sidebar,
            &self.banners,
            &self.inspector,
            &self.status,
        ];
        for widget in widgets {
            let cx = PaintCx::new(
                widget.id(),
                &self.theme,
                self.layout.scale(),
                &self.engine,
                &self.focus,
                &self.capture,
                None,
            );
            widget.paint(&cx, &mut canvas);
        }
        assert!(canvas.is_balanced());
        canvas
    }
}

#[test]
fn every_view_lives_in_its_pane_without_overlap() {
    for (width, height, scale) in [
        (1280.0, 800.0, 1.0),
        (1000.0, 640.0, 1.5),
        (900.0, 600.0, 2.0),
    ] {
        let mut window = Window::new(width, height, scale);
        window.layout.set_inspector_visible(true);
        window.layout_all();
        let rects = window.layout.rects();
        let panes = [
            rects.title_row,
            rects.command_row,
            rects.sidebar,
            Some(rects.editor),
            rects.inspector,
            rects.status_bar,
        ];
        let shown: Vec<Rect> = panes.into_iter().flatten().collect();
        for (index, a) in shown.iter().enumerate() {
            for b in &shown[index + 1..] {
                assert!(
                    !a.intersects(*b),
                    "{a:?} overlaps {b:?} at {width}x{height}@{scale}"
                );
            }
        }
        // The views were arranged into exactly their panes.
        assert_eq!(window.sidebar.bounds(), rects.sidebar.expect("sidebar"));
        assert_eq!(
            window.inspector.bounds(),
            rects.inspector.expect("inspector")
        );
        assert_eq!(
            window.status.bounds(),
            rects.status_bar.expect("status bar")
        );
        assert_eq!(
            window.title.title_rect(),
            rects.title_row.expect("title row")
        );
        assert_eq!(
            window.title.command_rect(),
            rects.command_row.expect("command row")
        );
        assert_eq!(window.banners.bounds().y, rects.editor.y);
        // Tabs stop before the draggable gap and the caption buttons.
        let strip = window.title.tabs().strip().bounds();
        let caption = rects.caption_buttons.expect("caption buttons");
        assert!(strip.right() <= caption.x - 32.0 + 0.001);
        assert!(window.focus.tab_order().len() > 10);
        window.paint();
    }
}

#[test]
fn the_title_bar_toggles_reflow_the_window() {
    let mut window = Window::new(1280.0, 800.0, 1.0);
    assert!(window.layout.rects().sidebar.is_some());
    let toggle = window.title.sidebar_toggle_rect().center();
    window.click(toggle, PointerButton::Left);
    let events = window.title.take_events();
    assert_eq!(events, vec![TitleEvent::ToggleSidebar]);
    window.layout.toggle_sidebar();
    window.layout_all();
    let rects = window.layout.rects();
    assert!(rects.sidebar.is_none());
    assert_eq!(rects.editor.x, 0.0);

    let toggle = window.title.inspector_toggle_rect().center();
    window.click(toggle, PointerButton::Left);
    assert_eq!(
        window.title.take_events(),
        vec![TitleEvent::ToggleInspector]
    );
    window.layout.toggle_inspector();
    window.layout_all();
    let rects = window.layout.rects();
    assert_eq!(rects.inspector.map(|r| r.width), Some(280.0));
    assert_eq!(rects.editor.right(), rects.inspector.expect("inspector").x);
}

#[test]
fn clicks_reach_the_view_under_the_pointer() {
    let mut window = Window::new(1280.0, 800.0, 1.0);
    // The sidebar's first row is the "docs" folder; its second click-free selection is reported.
    let row = window.sidebar.row_rect(0).center();
    window.click(row, PointerButton::Left);
    assert_eq!(
        window.sidebar.take_events(),
        vec![SidebarEvent::Selected(1)]
    );
    // Nothing else heard it.
    assert!(window.title.take_events().is_empty());
    assert!(window.status.take_events().is_empty());
    // Right click in the tree requests a native context menu.
    let at = window.sidebar.row_rect(1).center();
    window.click(at, PointerButton::Right);
    let events = window.sidebar.take_events();
    assert!(events.contains(&SidebarEvent::ContextMenu { key: Some(2), at }));
    // The status bar's encoding segment is clickable.
    let encoding = window
        .status
        .segment_rect(ssw_ui::shell::statusbar::StatusSegment::Encoding)
        .center();
    window.click(encoding, PointerButton::Left);
    assert_eq!(window.status.take_events().len(), 1);
}

#[test]
fn the_language_segment_of_a_plain_document_is_a_button_of_the_status_bar() {
    let mut window = Window::new(1280.0, 800.0, 1.0);
    let localizer = Localizer::new("en", &[("en", MAIN_FTL), ("en", SHELL_FTL)]);
    let info = |language| StatusInfo {
        line: 1,
        column: 1,
        words: 0,
        selected_words: None,
        zoom_percent: 100,
        encoding: "UTF-8".to_owned(),
        eol: Eol::Lf,
        language,
    };
    // A Markdown document has no language segment.
    assert_eq!(window.status.text(StatusSegment::Language), "");
    assert_eq!(
        window.status.segment_rect(StatusSegment::Language),
        Rect::ZERO
    );
    let tab_stops = window.focus.tab_order().len();

    // A plain document shows its language, and the segment takes the rightmost place.
    window
        .status
        .set_info(&info(StatusLanguage::Language(Language::Json)), &localizer);
    window.layout_all();
    assert_eq!(window.status.text(StatusSegment::Language), "JSON");
    let language = window.status.segment_rect(StatusSegment::Language);
    let line_ending = window.status.segment_rect(StatusSegment::LineEnding);
    assert!(language.x > line_ending.right());
    assert_eq!(window.focus.tab_order().len(), tab_stops + 1);
    window.click(language.center(), PointerButton::Left);
    assert_eq!(
        window.status.take_events(),
        vec![StatusEvent::LanguageClicked { anchor: language }]
    );

    // A plain document without a language says so.
    window
        .status
        .set_info(&info(StatusLanguage::PlainText), &localizer);
    window.layout_all();
    assert_eq!(window.status.text(StatusSegment::Language), "Plain text");

    // Back to Markdown: the segment is gone.
    window
        .status
        .set_info(&info(StatusLanguage::Hidden), &localizer);
    window.layout_all();
    assert_eq!(window.status.text(StatusSegment::Language), "");
    assert_eq!(window.focus.tab_order().len(), tab_stops);
}

#[test]
fn f6_visits_every_shown_pane_once_per_cycle() {
    let mut window = Window::new(1280.0, 800.0, 1.0);
    window.layout.set_inspector_visible(true);
    let mut seen = Vec::new();
    let mut current = None;
    for _ in 0..window.layout.panes().len() {
        let next = window.layout.next_pane(current, true);
        seen.push(next);
        current = Some(next);
    }
    assert_eq!(seen, window.layout.panes());
    assert_eq!(window.layout.next_pane(current, true), seen[0]);
    assert_eq!(seen.last(), Some(&Pane::StatusBar));
}

#[test]
fn zen_mode_leaves_only_the_solid_editor() {
    let mut window = Window::new(1280.0, 800.0, 1.0);
    window.layout.set_mode(ShellMode::Zen);
    window.layout_all();
    let surfaces = window.layout.surfaces();
    assert_eq!(surfaces.len(), 1);
    assert_eq!(surfaces[0].pane, Pane::Editor);
    assert_eq!(surfaces[0].backdrop, Backdrop::Solid);
    assert_eq!(surfaces[0].rect, Rect::new(0.0, 0.0, 1280.0, 800.0));
    assert_eq!(
        window.layout.chrome_hit(Point::new(640.0, 10.0)),
        ChromeHit::Client
    );

    window.layout.set_mode(ShellMode::Normal);
    window.layout_all();
    let surfaces = window.layout.surfaces();
    let mica = surfaces
        .iter()
        .filter(|s| s.backdrop == Backdrop::Mica)
        .count();
    assert_eq!(mica, 4, "title row, command row, sidebar and status bar");
}

#[test]
fn chrome_hit_testing_follows_the_title_bar_widgets() {
    let window = Window::new(1280.0, 800.0, 1.0);
    let hit = |x: f32, y: f32| window.title.chrome_hit(&window.layout, Point::new(x, y));
    // Tabs and the sidebar toggle take clicks; the empty title row drags the window.
    assert_eq!(hit(20.0, 20.0), ChromeHit::Client);
    assert_eq!(hit(150.0, 20.0), ChromeHit::Client);
    assert_eq!(hit(900.0, 20.0), ChromeHit::Caption);
    assert!(matches!(hit(1250.0, 20.0), ChromeHit::CaptionButton(_)));
    assert!(matches!(hit(600.0, 2.0), ChromeHit::Resize(_)));
    assert_eq!(hit(600.0, 60.0), ChromeHit::Client);
}

#[test]
fn the_banner_stack_pushes_nothing_when_empty_and_resizes_when_shown() {
    let mut window = Window::new(1280.0, 800.0, 1.0);
    assert_eq!(window.banners.content_height(), 48.0);
    window
        .banners
        .dismiss(ssw_ui::shell::banners::BannerKind::ReadOnly);
    window.layout_all();
    assert_eq!(window.banners.content_height(), 0.0);
    assert_eq!(window.banners.bounds().height, 0.0);
    window.banners.show_conflict();
    window.layout_all();
    assert_eq!(window.banners.bounds().height, 48.0);
    let canvas = window.paint();
    assert!(canvas.texts().contains(&"The file changed on disk"));
}
