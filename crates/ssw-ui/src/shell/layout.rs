//! Pane geometry of the window shell.
//!
//! [`ShellLayout`] turns the window size, the DPI scale, the display [`ShellMode`] and the pane
//! toggles into the rectangles of the title row, command row, sidebar, editor, inspector and
//! status bar ([`ShellRects`]), reports which of them are transparent (Mica) and which are
//! solid ([`ShellLayout::surfaces`]), exposes the window-chrome hit regions
//! ([`ShellLayout::chrome_hit`]: caption, caption buttons, resize borders), the two resizable
//! splitters and the F6 pane cycling order.
//!
//! All geometry is in DIPs; heights and pane widths are snapped to device pixels for the scale.
//!
//! ```text
//! ┌ title row (Mica):  [sidebar toggle] [tabs…] [+]              [─][□][×] ┐
//! │ command row (Mica): File Edit Format View        [B I U] [inspector]    │
//! ├ sidebar (Mica) ┬ editor (solid, top-left rounded) ┬ inspector (Mica) ───┤
//! └ status bar (Mica) ──────────────────────────────────────────────────────┘
//! ```

use crate::geometry::{CornerRadii, Point, Rect, Size, snap_to_pixel};
use crate::input::Cursor;
use crate::widgets::tabs::TAB_HEIGHT;

/// Height of the title row (tabs live here); equals the tab strip height.
pub const TITLE_ROW_HEIGHT: f32 = TAB_HEIGHT;
/// Height of the command row (menus, format pill, inspector toggle).
pub const COMMAND_ROW_HEIGHT: f32 = 40.0;
/// Height of the status bar.
pub const STATUS_BAR_HEIGHT: f32 = 24.0;
/// Default sidebar width.
pub const SIDEBAR_DEFAULT_WIDTH: f32 = 260.0;
/// Narrowest sidebar the user can drag it to.
pub const SIDEBAR_MIN_WIDTH: f32 = 180.0;
/// Widest sidebar the user can drag it to.
pub const SIDEBAR_MAX_WIDTH: f32 = 480.0;
/// Default inspector width.
pub const INSPECTOR_DEFAULT_WIDTH: f32 = 280.0;
/// Narrowest inspector.
pub const INSPECTOR_MIN_WIDTH: f32 = 220.0;
/// Widest inspector.
pub const INSPECTOR_MAX_WIDTH: f32 = 480.0;
/// The editor never gets narrower than this; side panes shrink and finally hide instead.
pub const EDITOR_MIN_WIDTH: f32 = 320.0;
/// Width of the grab zone of a splitter, centered on the pane edge.
pub const SPLITTER_WIDTH: f32 = 6.0;
/// Width of one caption button (minimize, maximize, close).
pub const CAPTION_BUTTON_WIDTH: f32 = 46.0;
/// Thickness of the resize border.
pub const RESIZE_BORDER: f32 = 6.0;
/// Radius of the editor surface's rounded top-left corner.
pub const EDITOR_CORNER_RADIUS: f32 = 8.0;
/// Nudge of a splitter by one arrow-key press.
pub const SPLITTER_KEY_STEP: f32 = 16.0;

/// How much of the shell is shown.
///
/// - `Normal`: title row, command row, panes as toggled, status bar as toggled.
/// - `EditorOnly`: like `Normal` but both side panes are hidden (a single file opened from
///   Explorer). Toggling a pane returns to `Normal` with only that pane shown.
/// - `FullScreen` (F11): the window covers the monitor; no caption buttons and no resize
///   border, everything else as in `Normal`.
/// - `Zen` (Shift+F11): full screen with only the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ShellMode {
    #[default]
    Normal,
    EditorOnly,
    FullScreen,
    Zen,
}

impl ShellMode {
    /// Whether the title row and the command row are shown.
    #[must_use]
    pub const fn shows_chrome(self) -> bool {
        !matches!(self, ShellMode::Zen)
    }

    /// Whether the sidebar and inspector may be shown.
    #[must_use]
    pub const fn allows_side_panes(self) -> bool {
        matches!(self, ShellMode::Normal | ShellMode::FullScreen)
    }

    /// Whether the app draws caption buttons and the window has a resize border.
    #[must_use]
    pub const fn has_window_frame(self) -> bool {
        matches!(self, ShellMode::Normal | ShellMode::EditorOnly)
    }
}

/// A region of the window that receives keyboard focus as a unit (F6 cycles through them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Pane {
    TitleRow,
    CommandRow,
    Sidebar,
    Editor,
    Inspector,
    StatusBar,
}

impl Pane {
    /// Every pane in F6 cycling order: top to bottom, left to right.
    pub const ALL: [Pane; 6] = [
        Pane::TitleRow,
        Pane::CommandRow,
        Pane::Sidebar,
        Pane::Editor,
        Pane::Inspector,
        Pane::StatusBar,
    ];
}

/// One of the two draggable pane borders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Splitter {
    /// Between the sidebar and the editor.
    Sidebar,
    /// Between the editor and the inspector.
    Inspector,
}

/// The three caption buttons, left to right: the discriminant is the slot from the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptionButton {
    Minimize,
    Maximize,
    Close,
}

impl CaptionButton {
    /// Left-to-right order.
    pub const ALL: [CaptionButton; 3] = [
        CaptionButton::Minimize,
        CaptionButton::Maximize,
        CaptionButton::Close,
    ];
}

/// A border or corner of the window that starts a resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResizeEdge {
    Left,
    Right,
    Top,
    Bottom,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

/// What `WM_NCHITTEST` should answer for a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeHit {
    /// Ordinary client area: the shell widgets get the input.
    Client,
    /// Draggable title area (`HTCAPTION`).
    Caption,
    /// A caption button; `Maximize` is `HTMAXBUTTON` so Snap Layouts appear.
    CaptionButton(CaptionButton),
    /// A resize border or corner.
    Resize(ResizeEdge),
}

/// How a region is composed with the backdrop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    /// Transparent: the Mica Alt backdrop shows through (solid theme color when transparency is
    /// off, which is the platform layer's decision).
    Mica,
    /// Opaque theme surface.
    Solid,
}

/// A painted region of the window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Surface {
    pub pane: Pane,
    pub rect: Rect,
    pub backdrop: Backdrop,
    /// Corner radii of the region (only the editor surface is rounded, at the top left).
    pub radii: CornerRadii,
}

/// The pane rectangles. Hidden panes are `None`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShellRects {
    pub title_row: Option<Rect>,
    pub command_row: Option<Rect>,
    pub sidebar: Option<Rect>,
    /// The editor slot; always present.
    pub editor: Rect,
    pub inspector: Option<Rect>,
    pub status_bar: Option<Rect>,
    /// The area reserved at the right end of the title row for the three caption buttons.
    pub caption_buttons: Option<Rect>,
}

impl ShellRects {
    /// The rectangle of `pane`, if it is shown.
    #[must_use]
    pub fn get(&self, pane: Pane) -> Option<Rect> {
        match pane {
            Pane::TitleRow => self.title_row,
            Pane::CommandRow => self.command_row,
            Pane::Sidebar => self.sidebar,
            Pane::Editor => Some(self.editor),
            Pane::Inspector => self.inspector,
            Pane::StatusBar => self.status_bar,
        }
    }
}

/// Window shell geometry and pane state.
#[derive(Debug, Clone, PartialEq)]
pub struct ShellLayout {
    size: Size,
    scale: f32,
    mode: ShellMode,
    maximized: bool,
    sidebar_visible: bool,
    inspector_visible: bool,
    status_bar_visible: bool,
    sidebar_width: f32,
    inspector_width: f32,
    drag: Option<Splitter>,
}

impl ShellLayout {
    /// A normal-mode layout for a window of `size` DIPs at dpi `scale` (`dpi / 96`): sidebar and
    /// status bar shown, inspector hidden.
    #[must_use]
    pub fn new(size: Size, scale: f32) -> Self {
        Self {
            size,
            scale,
            mode: ShellMode::Normal,
            maximized: false,
            sidebar_visible: true,
            inspector_visible: false,
            status_bar_visible: true,
            sidebar_width: SIDEBAR_DEFAULT_WIDTH,
            inspector_width: INSPECTOR_DEFAULT_WIDTH,
            drag: None,
        }
    }

    #[must_use]
    pub fn size(&self) -> Size {
        self.size
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    #[must_use]
    pub fn scale(&self) -> f32 {
        self.scale
    }

    pub fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    #[must_use]
    pub fn mode(&self) -> ShellMode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: ShellMode) {
        self.mode = mode;
        self.drag = None;
    }

    /// A maximized window has no resize border.
    pub fn set_maximized(&mut self, maximized: bool) {
        self.maximized = maximized;
    }

    #[must_use]
    pub fn is_maximized(&self) -> bool {
        self.maximized
    }

    /// The sidebar toggle as the user set it (it may still be hidden by the mode or a narrow
    /// window).
    #[must_use]
    pub fn sidebar_visible(&self) -> bool {
        self.sidebar_visible
    }

    pub fn set_sidebar_visible(&mut self, visible: bool) {
        self.sidebar_visible = visible;
    }

    #[must_use]
    pub fn inspector_visible(&self) -> bool {
        self.inspector_visible
    }

    pub fn set_inspector_visible(&mut self, visible: bool) {
        self.inspector_visible = visible;
    }

    #[must_use]
    pub fn status_bar_visible(&self) -> bool {
        self.status_bar_visible
    }

    pub fn set_status_bar_visible(&mut self, visible: bool) {
        self.status_bar_visible = visible;
    }

    /// Ctrl+Shift+E. In editor-only mode this returns to normal mode with only the sidebar shown.
    pub fn toggle_sidebar(&mut self) {
        if self.mode == ShellMode::EditorOnly {
            self.mode = ShellMode::Normal;
            self.sidebar_visible = true;
            self.inspector_visible = false;
        } else {
            self.sidebar_visible = !self.sidebar_visible;
        }
    }

    /// Ctrl+Shift+I. In editor-only mode this returns to normal mode with only the inspector
    /// shown.
    pub fn toggle_inspector(&mut self) {
        if self.mode == ShellMode::EditorOnly {
            self.mode = ShellMode::Normal;
            self.sidebar_visible = false;
            self.inspector_visible = true;
        } else {
            self.inspector_visible = !self.inspector_visible;
        }
    }

    /// The preferred sidebar width (what is persisted); the shown width may be smaller.
    #[must_use]
    pub fn sidebar_width(&self) -> f32 {
        self.sidebar_width
    }

    /// Sets the preferred sidebar width, clamped to its limits. Returns whether it changed.
    pub fn set_sidebar_width(&mut self, width: f32) -> bool {
        let width = width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        let changed = width != self.sidebar_width;
        self.sidebar_width = width;
        changed
    }

    /// The preferred inspector width.
    #[must_use]
    pub fn inspector_width(&self) -> f32 {
        self.inspector_width
    }

    /// Sets the preferred inspector width, clamped to its limits. Returns whether it changed.
    pub fn set_inspector_width(&mut self, width: f32) -> bool {
        let width = width.clamp(INSPECTOR_MIN_WIDTH, INSPECTOR_MAX_WIDTH);
        let changed = width != self.inspector_width;
        self.inspector_width = width;
        changed
    }

    fn snap(&self, value: f32) -> f32 {
        snap_to_pixel(value, self.scale)
    }

    /// The widths the side panes get: preferred widths, shrunk to their minimum and finally
    /// hidden (inspector first) so the editor keeps [`EDITOR_MIN_WIDTH`].
    fn pane_widths(&self) -> (f32, f32) {
        let side = self.mode.allows_side_panes();
        let wanted_sidebar = if side && self.sidebar_visible {
            self.sidebar_width
        } else {
            0.0
        };
        let wanted_inspector = if side && self.inspector_visible {
            self.inspector_width
        } else {
            0.0
        };
        let room = (self.size.width - EDITOR_MIN_WIDTH).max(0.0);
        let fit = |value: f32, other: f32, min: f32| {
            if value > 0.0 {
                value.min((room - other).max(min))
            } else {
                0.0
            }
        };
        let mut sidebar = wanted_sidebar;
        let mut inspector = wanted_inspector;
        if sidebar + inspector > room {
            inspector = fit(wanted_inspector, sidebar, INSPECTOR_MIN_WIDTH);
            sidebar = fit(wanted_sidebar, inspector, SIDEBAR_MIN_WIDTH);
        }
        if sidebar + inspector > room {
            inspector = 0.0;
            sidebar = fit(wanted_sidebar, 0.0, SIDEBAR_MIN_WIDTH);
        }
        if sidebar > room {
            sidebar = 0.0;
        }
        (self.snap(sidebar), self.snap(inspector))
    }

    /// The rectangles of every shown pane.
    #[must_use]
    pub fn rects(&self) -> ShellRects {
        let width = self.size.width;
        let chrome = self.mode.shows_chrome();
        let title_height = if chrome {
            self.snap(TITLE_ROW_HEIGHT)
        } else {
            0.0
        };
        let command_height = if chrome {
            self.snap(COMMAND_ROW_HEIGHT)
        } else {
            0.0
        };
        let status_height = if chrome && self.status_bar_visible {
            self.snap(STATUS_BAR_HEIGHT)
        } else {
            0.0
        };
        let body_top = title_height + command_height;
        let body_height = (self.size.height - body_top - status_height).max(0.0);
        let (sidebar, inspector) = self.pane_widths();
        let caption = self.snap(CAPTION_BUTTON_WIDTH) * 3.0;
        ShellRects {
            title_row: chrome.then(|| Rect::new(0.0, 0.0, width, title_height)),
            command_row: chrome.then(|| Rect::new(0.0, title_height, width, command_height)),
            sidebar: (sidebar > 0.0).then(|| Rect::new(0.0, body_top, sidebar, body_height)),
            editor: Rect::new(
                sidebar,
                body_top,
                (width - sidebar - inspector).max(0.0),
                body_height,
            ),
            inspector: (inspector > 0.0)
                .then(|| Rect::new(width - inspector, body_top, inspector, body_height)),
            status_bar: (status_height > 0.0)
                .then(|| Rect::new(0.0, self.size.height - status_height, width, status_height)),
            caption_buttons: self
                .mode
                .has_window_frame()
                .then(|| Rect::new(width - caption, 0.0, caption, title_height)),
        }
    }

    /// The rectangle of one caption button, when the window has a frame.
    #[must_use]
    pub fn caption_button_rect(&self, button: CaptionButton) -> Option<Rect> {
        let area = self.rects().caption_buttons?;
        let each = area.width / 3.0;
        let index = button as usize;
        Some(Rect::new(
            area.x + index as f32 * each,
            area.y,
            each,
            area.height,
        ))
    }

    /// Which regions are transparent (Mica) and which are solid. The editor surface is solid with
    /// a rounded top-left corner (square in Zen, where nothing is above or beside it).
    #[must_use]
    pub fn surfaces(&self) -> Vec<Surface> {
        let rects = self.rects();
        let radii = if self.mode.shows_chrome() {
            CornerRadii::new(EDITOR_CORNER_RADIUS, 0.0, 0.0, 0.0)
        } else {
            CornerRadii::ZERO
        };
        Pane::ALL
            .iter()
            .filter_map(|&pane| {
                let rect = rects.get(pane)?;
                let (backdrop, radii) = if pane == Pane::Editor {
                    (Backdrop::Solid, radii)
                } else {
                    (Backdrop::Mica, CornerRadii::ZERO)
                };
                Some(Surface {
                    pane,
                    rect,
                    backdrop,
                    radii,
                })
            })
            .collect()
    }

    /// The shown panes in F6 cycling order.
    #[must_use]
    pub fn panes(&self) -> Vec<Pane> {
        let rects = self.rects();
        Pane::ALL
            .into_iter()
            .filter(|&pane| rects.get(pane).is_some())
            .collect()
    }

    /// The pane F6 (`forward`) or Shift+F6 moves to from `current`; the first (or last) shown
    /// pane when `current` is `None` or hidden. The editor is always shown, so there is always
    /// an answer.
    #[must_use]
    pub fn next_pane(&self, current: Option<Pane>, forward: bool) -> Pane {
        let panes = self.panes();
        let position = current.and_then(|pane| panes.iter().position(|&p| p == pane));
        let index = match (position, forward) {
            (Some(index), true) => (index + 1) % panes.len(),
            (Some(index), false) => (index + panes.len() - 1) % panes.len(),
            (None, true) => 0,
            (None, false) => panes.len() - 1,
        };
        panes[index]
    }

    /// The grab zone of `splitter`, when its pane is shown.
    #[must_use]
    pub fn splitter_rect(&self, splitter: Splitter) -> Option<Rect> {
        let rects = self.rects();
        match splitter {
            Splitter::Sidebar => rects.sidebar.map(|pane| {
                Rect::new(
                    pane.right() - SPLITTER_WIDTH / 2.0,
                    pane.y,
                    SPLITTER_WIDTH,
                    pane.height,
                )
            }),
            Splitter::Inspector => rects.inspector.map(|pane| {
                Rect::new(
                    pane.x - SPLITTER_WIDTH / 2.0,
                    pane.y,
                    SPLITTER_WIDTH,
                    pane.height,
                )
            }),
        }
    }

    /// The splitter whose grab zone contains `point`.
    #[must_use]
    pub fn splitter_at(&self, point: Point) -> Option<Splitter> {
        [Splitter::Sidebar, Splitter::Inspector]
            .into_iter()
            .find(|&splitter| {
                self.splitter_rect(splitter)
                    .is_some_and(|rect| rect.contains(point))
            })
    }

    /// The cursor to show at `point` for the splitters: the horizontal resize cursor over a
    /// grab zone or while dragging.
    #[must_use]
    pub fn cursor_at(&self, point: Point) -> Option<Cursor> {
        (self.drag.is_some() || self.splitter_at(point).is_some()).then_some(Cursor::SizeWE)
    }

    /// Starts dragging `splitter`.
    pub fn begin_drag(&mut self, splitter: Splitter) {
        self.drag = Some(splitter);
    }

    /// The splitter being dragged.
    #[must_use]
    pub fn dragging(&self) -> Option<Splitter> {
        self.drag
    }

    /// Moves the dragged splitter to window x `x`. Returns whether a pane width changed.
    pub fn drag_to(&mut self, x: f32) -> bool {
        match self.drag {
            Some(Splitter::Sidebar) => self.set_sidebar_width(x),
            Some(Splitter::Inspector) => self.set_inspector_width(self.size.width - x),
            None => false,
        }
    }

    /// Ends the drag.
    pub fn end_drag(&mut self) {
        self.drag = None;
    }

    /// Moves `splitter` by `delta` DIPs to the right (keyboard resize). Returns whether the
    /// width changed.
    pub fn nudge(&mut self, splitter: Splitter, delta: f32) -> bool {
        match splitter {
            Splitter::Sidebar => self.set_sidebar_width(self.sidebar_width + delta),
            Splitter::Inspector => self.set_inspector_width(self.inspector_width - delta),
        }
    }

    fn resize_edge(&self, point: Point) -> Option<ResizeEdge> {
        let border = RESIZE_BORDER;
        let corner = 2.0 * RESIZE_BORDER;
        let (width, height) = (self.size.width, self.size.height);
        let on_left = point.x < border;
        let on_right = point.x >= width - border;
        let on_top = point.y < border;
        let on_bottom = point.y >= height - border;
        let near_left = point.x < corner;
        let near_right = point.x >= width - corner;
        let near_top = point.y < corner;
        let near_bottom = point.y >= height - corner;
        let left = on_left || (near_left && (on_top || on_bottom));
        let right = on_right || (near_right && (on_top || on_bottom));
        let top = on_top || (near_top && (on_left || on_right));
        let bottom = on_bottom || (near_bottom && (on_left || on_right));
        match (left, right, top, bottom) {
            (true, _, true, _) => Some(ResizeEdge::TopLeft),
            (_, true, true, _) => Some(ResizeEdge::TopRight),
            (true, _, _, true) => Some(ResizeEdge::BottomLeft),
            (_, true, _, true) => Some(ResizeEdge::BottomRight),
            (true, ..) => Some(ResizeEdge::Left),
            (_, true, ..) => Some(ResizeEdge::Right),
            (_, _, true, _) => Some(ResizeEdge::Top),
            (_, _, _, true) => Some(ResizeEdge::Bottom),
            _ => None,
        }
    }

    /// The `WM_NCHITTEST` answer for `point`: resize borders first (unless maximized or without a
    /// frame), then the caption buttons, then the title row as a draggable caption. The title bar
    /// refines `Caption` to `Client` where tabs and buttons sit
    /// ([`TitleBar::chrome_hit`](super::titlebar::TitleBar::chrome_hit)).
    #[must_use]
    pub fn chrome_hit(&self, point: Point) -> ChromeHit {
        let window = Rect::from_origin_size(Point::ZERO, self.size);
        if !window.contains(point) {
            return ChromeHit::Client;
        }
        if self.mode.has_window_frame()
            && !self.maximized
            && let Some(edge) = self.resize_edge(point)
        {
            return ChromeHit::Resize(edge);
        }
        let rects = self.rects();
        if let Some(buttons) = rects.caption_buttons.filter(|area| area.contains(point)) {
            let each = buttons.width / 3.0;
            let index = (((point.x - buttons.x) / each) as usize).min(2);
            return ChromeHit::CaptionButton(CaptionButton::ALL[index]);
        }
        if rects.title_row.is_some_and(|row| row.contains(point)) {
            ChromeHit::Caption
        } else {
            ChromeHit::Client
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: f32, height: f32, scale: f32) -> ShellLayout {
        ShellLayout::new(Size::new(width, height), scale)
    }

    #[test]
    fn normal_layout_at_100_percent() {
        let layout = layout(1280.0, 800.0, 1.0);
        let rects = layout.rects();
        assert_eq!(rects.title_row, Some(Rect::new(0.0, 0.0, 1280.0, 40.0)));
        assert_eq!(rects.command_row, Some(Rect::new(0.0, 40.0, 1280.0, 40.0)));
        assert_eq!(rects.sidebar, Some(Rect::new(0.0, 80.0, 260.0, 696.0)));
        assert_eq!(rects.editor, Rect::new(260.0, 80.0, 1020.0, 696.0));
        assert_eq!(rects.inspector, None);
        assert_eq!(rects.status_bar, Some(Rect::new(0.0, 776.0, 1280.0, 24.0)));
        assert_eq!(
            rects.caption_buttons,
            Some(Rect::new(1142.0, 0.0, 138.0, 40.0))
        );
    }

    #[test]
    fn inspector_takes_the_right_edge() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        layout.set_inspector_visible(true);
        let rects = layout.rects();
        assert_eq!(rects.inspector, Some(Rect::new(1000.0, 80.0, 280.0, 696.0)));
        assert_eq!(rects.editor, Rect::new(260.0, 80.0, 740.0, 696.0));
        layout.set_sidebar_visible(false);
        layout.set_status_bar_visible(false);
        let rects = layout.rects();
        assert_eq!(rects.sidebar, None);
        assert_eq!(rects.status_bar, None);
        assert_eq!(rects.editor, Rect::new(0.0, 80.0, 1000.0, 720.0));
    }

    #[test]
    fn heights_and_widths_snap_to_device_pixels() {
        for scale in [1.25_f32, 1.5, 1.75, 2.0] {
            let mut layout = layout(1000.0, 700.0, scale);
            layout.set_sidebar_width(200.3);
            let rects = layout.rects();
            let sidebar = rects.sidebar.expect("sidebar is shown");
            let device = sidebar.width * scale;
            assert!(
                (device - device.round()).abs() < 1e-3,
                "width {} not on the pixel grid at {scale}",
                sidebar.width
            );
            for height in [
                rects.title_row.expect("title row").height,
                rects.command_row.expect("command row").height,
                rects.status_bar.expect("status bar").height,
            ] {
                let device = height * scale;
                assert!((device - device.round()).abs() < 1e-3);
            }
            assert_eq!(rects.editor.x, sidebar.right());
        }
        let at_125 = self::layout(1000.0, 700.0, 1.25);
        assert_eq!(at_125.rects().status_bar.map(|r| r.height), Some(24.0));
        let at_150 = self::layout(1000.0, 700.0, 1.5);
        assert_eq!(at_150.rects().caption_buttons.map(|r| r.width), Some(138.0));
    }

    #[test]
    fn side_panes_shrink_then_hide_to_keep_the_editor_usable() {
        let mut layout = layout(900.0, 600.0, 1.0);
        layout.set_inspector_visible(true);
        // 900 - 320 = 580 room: 260 + 280 fits.
        assert_eq!(layout.rects().editor.width, 360.0);
        layout.set_size(Size::new(800.0, 600.0));
        // 480 room: inspector shrinks to 220 (260 + 220).
        let rects = layout.rects();
        assert_eq!(rects.inspector.map(|r| r.width), Some(220.0));
        assert_eq!(rects.sidebar.map(|r| r.width), Some(260.0));
        assert_eq!(rects.editor.width, 320.0);
        layout.set_size(Size::new(650.0, 600.0));
        // 330 room: inspector would need 220 + 180 = 400 > 330, so it hides; sidebar shrinks.
        let rects = layout.rects();
        assert_eq!(rects.inspector, None);
        assert_eq!(rects.sidebar.map(|r| r.width), Some(260.0));
        layout.set_size(Size::new(550.0, 600.0));
        // 230 room: the sidebar shrinks to fit.
        assert_eq!(layout.rects().sidebar.map(|r| r.width), Some(230.0));
        layout.set_size(Size::new(480.0, 600.0));
        // 160 room: even the minimum sidebar does not fit, only the editor remains.
        let rects = layout.rects();
        assert_eq!(rects.sidebar, None);
        assert_eq!(rects.editor, Rect::new(0.0, 80.0, 480.0, 496.0));
        // The preferred widths survive the squeeze.
        assert_eq!(layout.sidebar_width(), 260.0);
        assert_eq!(layout.inspector_width(), 280.0);
    }

    #[test]
    fn a_lone_side_pane_shrinks_to_the_room_the_editor_leaves() {
        // 550 - 320 = 230 room: the sidebar alone shrinks to it.
        let mut layout = layout(550.0, 600.0, 1.0);
        let rects = layout.rects();
        assert_eq!(rects.inspector, None);
        assert_eq!(rects.sidebar.map(|r| r.width), Some(230.0));
        assert_eq!(rects.editor.width, 320.0);
        // The inspector alone does the same.
        layout.set_sidebar_visible(false);
        layout.set_inspector_visible(true);
        let rects = layout.rects();
        assert_eq!(rects.sidebar, None);
        assert_eq!(rects.inspector.map(|r| r.width), Some(230.0));
        assert_eq!(rects.editor.width, 320.0);
    }

    #[test]
    fn tiny_windows_do_not_produce_negative_rects() {
        let mut layout = layout(100.0, 50.0, 1.0);
        layout.set_inspector_visible(true);
        let rects = layout.rects();
        assert_eq!(rects.editor.width, 100.0);
        assert_eq!(rects.editor.height, 0.0);
        assert!(rects.sidebar.is_none() && rects.inspector.is_none());
    }

    #[test]
    fn modes_hide_chrome_and_panes() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        layout.set_inspector_visible(true);

        layout.set_mode(ShellMode::EditorOnly);
        let rects = layout.rects();
        assert!(rects.title_row.is_some() && rects.command_row.is_some());
        assert!(rects.sidebar.is_none() && rects.inspector.is_none());
        assert!(rects.caption_buttons.is_some());
        assert_eq!(rects.editor.x, 0.0);
        assert_eq!(rects.editor.width, 1280.0);

        layout.set_mode(ShellMode::FullScreen);
        let rects = layout.rects();
        assert!(rects.title_row.is_some() && rects.status_bar.is_some());
        assert!(rects.sidebar.is_some() && rects.inspector.is_some());
        assert_eq!(
            rects.caption_buttons, None,
            "no caption buttons in full screen"
        );

        layout.set_mode(ShellMode::Zen);
        let rects = layout.rects();
        assert_eq!(rects.title_row, None);
        assert_eq!(rects.command_row, None);
        assert_eq!(rects.status_bar, None);
        assert_eq!(rects.sidebar, None);
        assert_eq!(rects.inspector, None);
        assert_eq!(rects.caption_buttons, None);
        assert_eq!(rects.editor, Rect::new(0.0, 0.0, 1280.0, 800.0));
        assert_eq!(layout.mode(), ShellMode::Zen);
    }

    #[test]
    fn toggling_a_pane_in_editor_only_mode_returns_to_normal() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        layout.set_mode(ShellMode::EditorOnly);
        layout.set_sidebar_visible(false);
        layout.toggle_sidebar();
        assert_eq!(layout.mode(), ShellMode::Normal);
        assert!(layout.sidebar_visible() && !layout.inspector_visible());

        layout.set_mode(ShellMode::EditorOnly);
        layout.toggle_inspector();
        assert_eq!(layout.mode(), ShellMode::Normal);
        assert!(layout.inspector_visible() && !layout.sidebar_visible());

        // In normal mode the toggles flip.
        layout.toggle_inspector();
        assert!(!layout.inspector_visible());
        layout.toggle_sidebar();
        assert!(layout.sidebar_visible());
    }

    #[test]
    fn surfaces_mark_mica_regions_and_the_solid_editor() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        layout.set_inspector_visible(true);
        let surfaces = layout.surfaces();
        let panes: Vec<Pane> = surfaces.iter().map(|s| s.pane).collect();
        assert_eq!(panes, Pane::ALL.to_vec());
        for surface in &surfaces {
            if surface.pane == Pane::Editor {
                assert_eq!(surface.backdrop, Backdrop::Solid);
                assert_eq!(
                    surface.radii,
                    CornerRadii::new(EDITOR_CORNER_RADIUS, 0.0, 0.0, 0.0)
                );
            } else {
                assert_eq!(surface.backdrop, Backdrop::Mica);
                assert!(surface.radii.is_zero());
            }
        }
        layout.set_mode(ShellMode::Zen);
        let surfaces = layout.surfaces();
        assert_eq!(surfaces.len(), 1);
        assert_eq!(surfaces[0].backdrop, Backdrop::Solid);
        assert!(surfaces[0].radii.is_zero());
    }

    #[test]
    fn f6_cycles_the_shown_panes_in_both_directions() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        assert_eq!(
            layout.panes(),
            vec![
                Pane::TitleRow,
                Pane::CommandRow,
                Pane::Sidebar,
                Pane::Editor,
                Pane::StatusBar
            ]
        );
        assert_eq!(layout.next_pane(Some(Pane::Editor), true), Pane::StatusBar);
        assert_eq!(
            layout.next_pane(Some(Pane::StatusBar), true),
            Pane::TitleRow
        );
        assert_eq!(
            layout.next_pane(Some(Pane::TitleRow), false),
            Pane::StatusBar
        );
        assert_eq!(
            layout.next_pane(Some(Pane::Sidebar), false),
            Pane::CommandRow
        );
        // The inspector is hidden: from it, the cycle starts over.
        assert_eq!(
            layout.next_pane(Some(Pane::Inspector), true),
            Pane::TitleRow
        );
        assert_eq!(layout.next_pane(None, true), Pane::TitleRow);
        assert_eq!(layout.next_pane(None, false), Pane::StatusBar);
        layout.set_inspector_visible(true);
        assert_eq!(layout.next_pane(Some(Pane::Editor), true), Pane::Inspector);
        layout.set_mode(ShellMode::Zen);
        assert_eq!(layout.next_pane(Some(Pane::Editor), true), Pane::Editor);
        assert_eq!(layout.next_pane(Some(Pane::Editor), false), Pane::Editor);
    }

    #[test]
    fn splitters_resize_the_panes_within_limits() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        layout.set_inspector_visible(true);
        let sidebar_edge = layout
            .splitter_rect(Splitter::Sidebar)
            .expect("sidebar splitter");
        assert_eq!(sidebar_edge, Rect::new(257.0, 80.0, 6.0, 696.0));
        let inspector_edge = layout
            .splitter_rect(Splitter::Inspector)
            .expect("inspector splitter");
        assert_eq!(inspector_edge, Rect::new(997.0, 80.0, 6.0, 696.0));
        assert_eq!(
            layout.splitter_at(Point::new(259.0, 300.0)),
            Some(Splitter::Sidebar)
        );
        assert_eq!(
            layout.splitter_at(Point::new(1001.0, 300.0)),
            Some(Splitter::Inspector)
        );
        assert_eq!(layout.splitter_at(Point::new(500.0, 300.0)), None);
        assert_eq!(
            layout.cursor_at(Point::new(259.0, 300.0)),
            Some(Cursor::SizeWE)
        );
        assert_eq!(layout.cursor_at(Point::new(500.0, 300.0)), None);

        assert!(!layout.drag_to(300.0), "no drag in progress");
        layout.begin_drag(Splitter::Sidebar);
        assert_eq!(layout.dragging(), Some(Splitter::Sidebar));
        assert_eq!(
            layout.cursor_at(Point::new(500.0, 300.0)),
            Some(Cursor::SizeWE)
        );
        assert!(layout.drag_to(320.0));
        assert_eq!(layout.rects().sidebar.map(|r| r.width), Some(320.0));
        assert!(!layout.drag_to(320.0));
        assert!(layout.drag_to(10.0));
        assert_eq!(layout.sidebar_width(), SIDEBAR_MIN_WIDTH);
        assert!(layout.drag_to(2000.0));
        assert_eq!(layout.sidebar_width(), SIDEBAR_MAX_WIDTH);
        layout.end_drag();
        assert_eq!(layout.dragging(), None);

        layout.begin_drag(Splitter::Inspector);
        assert!(layout.drag_to(900.0));
        assert_eq!(layout.inspector_width(), 380.0);
        assert!(layout.drag_to(1270.0));
        assert_eq!(layout.inspector_width(), INSPECTOR_MIN_WIDTH);
        layout.end_drag();

        assert!(!layout.nudge(Splitter::Sidebar, SPLITTER_KEY_STEP));
        assert_eq!(layout.sidebar_width(), SIDEBAR_MAX_WIDTH);
        layout.set_sidebar_width(300.0);
        assert!(layout.nudge(Splitter::Sidebar, -SPLITTER_KEY_STEP));
        assert_eq!(layout.sidebar_width(), 284.0);
        assert!(layout.nudge(Splitter::Inspector, -SPLITTER_KEY_STEP));
        assert_eq!(layout.inspector_width(), INSPECTOR_MIN_WIDTH + 16.0);
        assert!(!layout.set_inspector_width(INSPECTOR_MIN_WIDTH + 16.0));

        layout.set_mode(ShellMode::Zen);
        assert_eq!(layout.splitter_rect(Splitter::Sidebar), None);
        assert_eq!(layout.splitter_at(Point::new(259.0, 300.0)), None);
    }

    #[test]
    fn changing_the_mode_cancels_a_drag() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        layout.begin_drag(Splitter::Sidebar);
        layout.set_mode(ShellMode::FullScreen);
        assert_eq!(layout.dragging(), None);
    }

    #[test]
    fn chrome_hits_resize_caption_and_buttons() {
        let mut layout = layout(1280.0, 800.0, 1.0);
        let hit = |layout: &ShellLayout, x: f32, y: f32| layout.chrome_hit(Point::new(x, y));
        assert_eq!(hit(&layout, 640.0, 20.0), ChromeHit::Caption);
        assert_eq!(hit(&layout, 640.0, 300.0), ChromeHit::Client);
        assert_eq!(hit(&layout, -5.0, 300.0), ChromeHit::Client);
        // Edges and corners.
        assert_eq!(
            hit(&layout, 2.0, 300.0),
            ChromeHit::Resize(ResizeEdge::Left)
        );
        assert_eq!(
            hit(&layout, 1278.0, 300.0),
            ChromeHit::Resize(ResizeEdge::Right)
        );
        assert_eq!(hit(&layout, 640.0, 2.0), ChromeHit::Resize(ResizeEdge::Top));
        assert_eq!(
            hit(&layout, 640.0, 798.0),
            ChromeHit::Resize(ResizeEdge::Bottom)
        );
        assert_eq!(
            hit(&layout, 2.0, 2.0),
            ChromeHit::Resize(ResizeEdge::TopLeft)
        );
        assert_eq!(
            hit(&layout, 10.0, 2.0),
            ChromeHit::Resize(ResizeEdge::TopLeft)
        );
        assert_eq!(
            hit(&layout, 2.0, 10.0),
            ChromeHit::Resize(ResizeEdge::TopLeft)
        );
        assert_eq!(
            hit(&layout, 1270.0, 2.0),
            ChromeHit::Resize(ResizeEdge::TopRight)
        );
        assert_eq!(
            hit(&layout, 2.0, 790.0),
            ChromeHit::Resize(ResizeEdge::BottomLeft)
        );
        assert_eq!(
            hit(&layout, 1278.0, 798.0),
            ChromeHit::Resize(ResizeEdge::BottomRight)
        );
        // A corner also reaches along the bottom and right edges, up to twice the border.
        assert_eq!(
            hit(&layout, 10.0, 798.0),
            ChromeHit::Resize(ResizeEdge::BottomLeft)
        );
        assert_eq!(
            hit(&layout, 1270.0, 798.0),
            ChromeHit::Resize(ResizeEdge::BottomRight)
        );
        assert_eq!(
            hit(&layout, 1278.0, 10.0),
            ChromeHit::Resize(ResizeEdge::TopRight)
        );
        assert_eq!(
            hit(&layout, 1278.0, 790.0),
            ChromeHit::Resize(ResizeEdge::BottomRight)
        );
        // Beyond the corner reach the edge is a plain one.
        assert_eq!(
            hit(&layout, 1278.0, 14.0),
            ChromeHit::Resize(ResizeEdge::Right)
        );
        // Caption buttons: 46 DIPs each from x = 1142.
        assert_eq!(
            hit(&layout, 1150.0, 20.0),
            ChromeHit::CaptionButton(CaptionButton::Minimize)
        );
        assert_eq!(
            hit(&layout, 1200.0, 20.0),
            ChromeHit::CaptionButton(CaptionButton::Maximize)
        );
        assert_eq!(
            hit(&layout, 1260.0, 20.0),
            ChromeHit::CaptionButton(CaptionButton::Close)
        );
        // Below the title row the button column is ordinary client area.
        assert_eq!(hit(&layout, 1260.0, 60.0), ChromeHit::Client);
        assert_eq!(
            layout.caption_button_rect(CaptionButton::Maximize),
            Some(Rect::new(1188.0, 0.0, 46.0, 40.0))
        );

        layout.set_maximized(true);
        assert!(layout.is_maximized());
        assert_eq!(hit(&layout, 2.0, 300.0), ChromeHit::Client);
        assert_eq!(hit(&layout, 640.0, 2.0), ChromeHit::Caption);

        layout.set_maximized(false);
        layout.set_mode(ShellMode::FullScreen);
        assert_eq!(hit(&layout, 2.0, 300.0), ChromeHit::Client);
        assert_eq!(hit(&layout, 1260.0, 20.0), ChromeHit::Caption);
        assert_eq!(layout.caption_button_rect(CaptionButton::Close), None);
        layout.set_mode(ShellMode::Zen);
        assert_eq!(hit(&layout, 640.0, 20.0), ChromeHit::Client);
    }

    #[test]
    fn size_and_scale_accessors_round_trip() {
        let mut layout = layout(1000.0, 600.0, 1.0);
        layout.set_size(Size::new(1100.0, 650.0));
        layout.set_scale(1.5);
        assert_eq!(layout.size(), Size::new(1100.0, 650.0));
        assert_eq!(layout.scale(), 1.5);
        assert!(layout.status_bar_visible());
        assert!(!layout.is_maximized());
        assert_eq!(layout.mode(), ShellMode::Normal);
    }
}
