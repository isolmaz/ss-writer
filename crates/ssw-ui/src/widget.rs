//! The widget contract: [`Widget`], the per-pass contexts and the host-owned state they borrow.
//!
//! A frame runs in passes, each with its own context type so a widget can only do what the pass
//! allows:
//!
//! | pass | method | context | may |
//! |---|---|---|---|
//! | layout | [`Widget::measure`], [`Widget::arrange`] | [`LayoutCx`] | lay out text, register tab stops, invalidate |
//! | paint | [`Widget::paint`] | [`PaintCx`] | read theme and focus; draw on the [`Canvas`] |
//! | input | [`Widget::event`], [`Widget::perform`] | [`EventCx`] | move focus, capture the pointer, start timers, set the cursor, invalidate |
//! | access | [`Widget::access`] | [`AccessCx`] | read focus; describe itself in the [`AccessTree`] |
//!
//! State that outlives a pass is owned by the host and lent to the contexts: the
//! [`FocusManager`] (focused widget and tab order), the [`PointerCapture`] (which widget receives
//! pointer events regardless of position) and [`Requests`] (invalidation, timers, animation
//! frames, cursor). The host drains [`Requests`] after each pass and forwards them to the OS
//! ([`Timers`], `InvalidateRect`, `SetCursor`).
//!
//! All geometry is in window DIPs; `scale` is `dpi / 96` and is only used to snap to device
//! pixels ([`snap_to_pixel`]).

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::access::{AccessAction, AccessTree};
use crate::canvas::Canvas;
use crate::geometry::{Rect, Size, snap_to_pixel};
use crate::input::{Cursor, Event};
use crate::services::{TimerId, Timers};
use crate::text::TextLayoutEngine;
use crate::theme::Theme;

/// Identifies a widget (and its [`AccessNode`](crate::access::AccessNode)) within a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WidgetId(u64);

impl WidgetId {
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// A process-unique id.
    #[must_use]
    pub fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// What a widget did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventResult {
    /// The event was consumed; the host stops routing it.
    pub handled: bool,
    /// The widget wants keyboard focus (the host calls [`FocusManager::focus`]).
    pub request_focus: bool,
}

impl EventResult {
    /// The event is not for this widget.
    #[must_use]
    pub const fn ignored() -> Self {
        Self {
            handled: false,
            request_focus: false,
        }
    }

    /// The event was consumed.
    #[must_use]
    pub const fn handled() -> Self {
        Self {
            handled: true,
            request_focus: false,
        }
    }

    /// The event was consumed and the widget wants keyboard focus (a click on a focusable widget).
    #[must_use]
    pub const fn handled_with_focus() -> Self {
        Self {
            handled: true,
            request_focus: true,
        }
    }

    /// Combines two results: handled if either was, focus requested if either requested it.
    #[must_use]
    pub const fn or(self, other: EventResult) -> Self {
        Self {
            handled: self.handled || other.handled,
            request_focus: self.request_focus || other.request_focus,
        }
    }
}

/// A node of the widget tree.
///
/// Passes are driven top-down by the host or by container widgets: `measure` then `arrange` for
/// layout, then `paint`, `event`, `access` and `perform` as needed. Containers forward to their
/// children with `cx.for_widget(child_id)`.
pub trait Widget {
    /// The size this widget wants within `available` (`f32::INFINITY` means unconstrained). The
    /// result may exceed `available`; the parent decides what to do about it.
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size;

    /// Assigns the final `bounds` (window DIPs). Focusable widgets register themselves with
    /// [`LayoutCx::add_tab_stop`]; containers arrange their children in tab order.
    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect);

    /// Draws the widget within the bounds given to `arrange`.
    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas);

    /// Handles an input event. Pointer events carry window coordinates.
    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult;

    /// Describes this widget (and, for containers, its children) in `out`.
    fn access(&self, cx: &AccessCx, out: &mut AccessTree);

    /// Carries out a request from assistive technology. Returns whether the widget supports
    /// the action and performed it.
    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool;
}

/// A change of the focused widget, for delivering `Event::Focus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FocusChange {
    /// The widget that had focus and lost it.
    pub lost: Option<WidgetId>,
    /// The widget that now has focus.
    pub gained: Option<WidgetId>,
}

/// The focused widget and the tab order.
///
/// Only widgets in the tab order can hold focus. The order is rebuilt after every layout:
/// [`begin_tab_order`](Self::begin_tab_order), widgets call [`add_tab_stop`](Self::add_tab_stop)
/// while arranging, then [`commit_tab_order`](Self::commit_tab_order). Every change of the
/// focused widget is queued; the host drains it with [`take_changes`](Self::take_changes) and
/// sends `Event::Focus` to the widgets involved.
#[derive(Debug, Clone, Default)]
pub struct FocusManager {
    focused: Option<WidgetId>,
    order: Vec<WidgetId>,
    building: Vec<WidgetId>,
    changes: Vec<FocusChange>,
}

impl FocusManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn focused(&self) -> Option<WidgetId> {
        self.focused
    }

    #[must_use]
    pub fn is_focused(&self, id: WidgetId) -> bool {
        self.focused == Some(id)
    }

    /// The committed tab order.
    #[must_use]
    pub fn tab_order(&self) -> &[WidgetId] {
        &self.order
    }

    /// True when `id` is in the committed tab order.
    #[must_use]
    pub fn can_focus(&self, id: WidgetId) -> bool {
        self.order.contains(&id)
    }

    /// Starts collecting a new tab order.
    pub fn begin_tab_order(&mut self) {
        self.building.clear();
    }

    /// Appends `id` to the order being collected; repeated ids keep their first position.
    pub fn add_tab_stop(&mut self, id: WidgetId) {
        if !self.building.contains(&id) {
            self.building.push(id);
        }
    }

    /// Makes the collected order current. Focus is dropped when the focused widget left the
    /// order. Returns whether the focused widget changed.
    pub fn commit_tab_order(&mut self) -> bool {
        self.order = std::mem::take(&mut self.building);
        match self.focused {
            Some(id) if !self.order.contains(&id) => self.set_focused(None),
            _ => false,
        }
    }

    /// Replaces the tab order in one step (see [`commit_tab_order`](Self::commit_tab_order)).
    pub fn set_tab_order(&mut self, order: impl IntoIterator<Item = WidgetId>) -> bool {
        self.begin_tab_order();
        for id in order {
            self.add_tab_stop(id);
        }
        self.commit_tab_order()
    }

    /// Focuses `id`. Returns whether focus changed: `false` when `id` is already focused or is
    /// not in the tab order.
    pub fn focus(&mut self, id: WidgetId) -> bool {
        self.can_focus(id) && self.set_focused(Some(id))
    }

    /// Removes focus. Returns whether anything had focus.
    pub fn blur(&mut self) -> bool {
        self.set_focused(None)
    }

    /// Moves focus to the next tab stop, wrapping around; from no focus, to the first. Returns
    /// whether focus changed.
    pub fn focus_next(&mut self) -> bool {
        self.traverse(true)
    }

    /// Moves focus to the previous tab stop, wrapping around; from no focus, to the last.
    /// Returns whether focus changed.
    pub fn focus_previous(&mut self) -> bool {
        self.traverse(false)
    }

    /// Focus changes since the last call, oldest first.
    pub fn take_changes(&mut self) -> Vec<FocusChange> {
        std::mem::take(&mut self.changes)
    }

    fn traverse(&mut self, forward: bool) -> bool {
        let count = self.order.len();
        if count == 0 {
            return false;
        }
        let current = self
            .focused
            .and_then(|id| self.order.iter().position(|stop| *stop == id));
        let next = match (current, forward) {
            (Some(index), true) => (index + 1) % count,
            (Some(index), false) => (index + count - 1) % count,
            (None, true) => 0,
            (None, false) => count - 1,
        };
        let target = self.order[next];
        self.set_focused(Some(target))
    }

    fn set_focused(&mut self, to: Option<WidgetId>) -> bool {
        if self.focused == to {
            return false;
        }
        self.changes.push(FocusChange {
            lost: self.focused,
            gained: to,
        });
        self.focused = to;
        true
    }
}

/// Which widget receives pointer events regardless of where the pointer is (a pressed button,
/// a scroll-bar thumb, a text selection drag).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerCapture {
    owner: Option<WidgetId>,
}

impl PointerCapture {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn owner(&self) -> Option<WidgetId> {
        self.owner
    }

    #[must_use]
    pub fn is_captured_by(&self, id: WidgetId) -> bool {
        self.owner == Some(id)
    }

    /// Gives capture to `id`, taking it from any other owner (like `SetCapture`).
    pub fn capture(&mut self, id: WidgetId) {
        self.owner = Some(id);
    }

    /// Releases capture if `id` owns it. Returns whether it did; other widgets cannot release
    /// someone else's capture.
    pub fn release(&mut self, id: WidgetId) -> bool {
        if self.owner == Some(id) {
            self.owner = None;
            true
        } else {
            false
        }
    }

    /// Releases capture whoever owns it (the OS took it away). Returns the previous owner.
    pub fn force_release(&mut self) -> Option<WidgetId> {
        self.owner.take()
    }

    /// The widget a pointer event goes to: the capture owner if there is one, otherwise `hit`,
    /// the widget under the pointer.
    #[must_use]
    pub fn target(&self, hit: Option<WidgetId>) -> Option<WidgetId> {
        self.owner.or(hit)
    }
}

/// The part of the window that needs repainting.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Invalidation {
    #[default]
    Clean,
    /// The bounding box of everything invalidated.
    Rect(Rect),
    /// The whole window.
    All,
}

impl Invalidation {
    /// Adds `rect`; empty rectangles change nothing.
    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        *self = match *self {
            Invalidation::Clean => Invalidation::Rect(rect),
            Invalidation::Rect(existing) => Invalidation::Rect(existing.union(rect)),
            Invalidation::All => Invalidation::All,
        };
    }

    /// Marks the whole window.
    pub fn add_all(&mut self) {
        *self = Invalidation::All;
    }

    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self, Invalidation::Clean)
    }

    /// The area to repaint within `window`; `None` when nothing (or nothing inside the window)
    /// needs repainting.
    #[must_use]
    pub fn region(&self, window: Rect) -> Option<Rect> {
        match *self {
            Invalidation::Clean => None,
            Invalidation::Rect(rect) => rect.intersect(window),
            Invalidation::All => (!window.is_empty()).then_some(window),
        }
    }
}

/// A timer operation requested by a widget, applied to [`Timers`] by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerRequest {
    /// Start (or restart) the timer.
    Set {
        id: TimerId,
        interval: Duration,
        repeat: bool,
    },
    /// Stop the timer.
    Kill(TimerId),
}

impl TimerRequest {
    fn id(&self) -> TimerId {
        match *self {
            TimerRequest::Set { id, .. } | TimerRequest::Kill(id) => id,
        }
    }
}

/// What widgets ask the host to do after a pass.
#[derive(Debug, Clone, Default)]
pub struct Requests {
    invalidation: Invalidation,
    timers: Vec<TimerRequest>,
    animation_frame: bool,
    cursor: Option<Cursor>,
}

impl Requests {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Repaints `rect` (window DIPs).
    pub fn invalidate(&mut self, rect: Rect) {
        self.invalidation.add(rect);
    }

    /// Repaints the whole window.
    pub fn invalidate_all(&mut self) {
        self.invalidation.add_all();
    }

    #[must_use]
    pub fn invalidation(&self) -> Invalidation {
        self.invalidation
    }

    /// The accumulated invalidation, resetting it.
    pub fn take_invalidation(&mut self) -> Invalidation {
        std::mem::take(&mut self.invalidation)
    }

    /// Starts or restarts timer `id`. Earlier requests for the same id in this batch are
    /// superseded.
    pub fn set_timer(&mut self, id: TimerId, interval: Duration, repeat: bool) {
        self.timers.retain(|request| request.id() != id);
        self.timers.push(TimerRequest::Set {
            id,
            interval,
            repeat,
        });
    }

    /// Stops timer `id`. A start requested earlier in this batch is dropped; the kill is still
    /// sent because the timer may already be running.
    pub fn kill_timer(&mut self, id: TimerId) {
        self.timers.retain(|request| request.id() != id);
        self.timers.push(TimerRequest::Kill(id));
    }

    #[must_use]
    pub fn timers(&self) -> &[TimerRequest] {
        &self.timers
    }

    /// Asks for one `Event::AnimationFrame`.
    pub fn request_animation_frame(&mut self) {
        self.animation_frame = true;
    }

    #[must_use]
    pub fn animation_frame_requested(&self) -> bool {
        self.animation_frame
    }

    /// Sets the cursor for the current pointer position; the last request wins.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.cursor = Some(cursor);
    }

    #[must_use]
    pub fn cursor(&self) -> Option<Cursor> {
        self.cursor
    }

    /// The requested cursor, resetting it (the host falls back to [`Cursor::Arrow`]).
    pub fn take_cursor(&mut self) -> Option<Cursor> {
        self.cursor.take()
    }

    /// Applies and clears the timer and animation-frame requests.
    pub fn flush_timers(&mut self, timers: &mut dyn Timers) {
        for request in self.timers.drain(..) {
            match request {
                TimerRequest::Set {
                    id,
                    interval,
                    repeat,
                } => timers.set(id, interval, repeat),
                TimerRequest::Kill(id) => timers.kill(id),
            }
        }
        if std::mem::take(&mut self.animation_frame) {
            timers.request_animation_frame();
        }
    }
}

/// Context of the layout pass.
pub struct LayoutCx<'a> {
    /// The widget being laid out.
    pub id: WidgetId,
    pub theme: &'a Theme,
    /// `dpi / 96`.
    pub scale: f32,
    pub text: &'a dyn TextLayoutEngine,
    pub focus: &'a mut FocusManager,
    pub requests: &'a mut Requests,
}

impl<'a> LayoutCx<'a> {
    #[must_use]
    pub fn new(
        id: WidgetId,
        theme: &'a Theme,
        scale: f32,
        text: &'a dyn TextLayoutEngine,
        focus: &'a mut FocusManager,
        requests: &'a mut Requests,
    ) -> Self {
        Self {
            id,
            theme,
            scale,
            text,
            focus,
            requests,
        }
    }

    /// A context for laying out child `id`, sharing this context's state.
    pub fn for_widget(&mut self, id: WidgetId) -> LayoutCx<'_> {
        LayoutCx {
            id,
            theme: self.theme,
            scale: self.scale,
            text: self.text,
            focus: &mut *self.focus,
            requests: &mut *self.requests,
        }
    }

    /// Registers this widget as the next tab stop.
    pub fn add_tab_stop(&mut self) {
        self.focus.add_tab_stop(self.id);
    }

    pub fn invalidate(&mut self, rect: Rect) {
        self.requests.invalidate(rect);
    }

    /// Rounds `value` (DIPs) to a device pixel.
    #[must_use]
    pub fn snap(&self, value: f32) -> f32 {
        snap_to_pixel(value, self.scale)
    }
}

impl fmt::Debug for LayoutCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LayoutCx")
            .field("id", &self.id)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

/// Context of the paint pass.
#[derive(Clone, Copy)]
pub struct PaintCx<'a> {
    /// The widget being painted.
    pub id: WidgetId,
    pub theme: &'a Theme,
    /// `dpi / 96`.
    pub scale: f32,
    pub text: &'a dyn TextLayoutEngine,
    pub focus: &'a FocusManager,
    pub capture: &'a PointerCapture,
    /// The area being repainted, or `None` for the whole window.
    pub damage: Option<Rect>,
}

impl<'a> PaintCx<'a> {
    #[must_use]
    pub fn new(
        id: WidgetId,
        theme: &'a Theme,
        scale: f32,
        text: &'a dyn TextLayoutEngine,
        focus: &'a FocusManager,
        capture: &'a PointerCapture,
        damage: Option<Rect>,
    ) -> Self {
        Self {
            id,
            theme,
            scale,
            text,
            focus,
            capture,
            damage,
        }
    }

    /// A context for painting child `id`.
    #[must_use]
    pub fn for_widget(&self, id: WidgetId) -> PaintCx<'a> {
        PaintCx { id, ..*self }
    }

    /// True when this widget has keyboard focus.
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focus.is_focused(self.id)
    }

    /// True when this widget holds the pointer capture.
    #[must_use]
    pub fn has_pointer_capture(&self) -> bool {
        self.capture.is_captured_by(self.id)
    }

    /// True when `bounds` overlaps the area being repainted; widgets skip painting otherwise.
    #[must_use]
    pub fn needs_paint(&self, bounds: Rect) -> bool {
        self.damage.is_none_or(|damage| damage.intersects(bounds))
    }

    /// Rounds `value` (DIPs) to a device pixel.
    #[must_use]
    pub fn snap(&self, value: f32) -> f32 {
        snap_to_pixel(value, self.scale)
    }
}

impl fmt::Debug for PaintCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaintCx")
            .field("id", &self.id)
            .field("scale", &self.scale)
            .field("damage", &self.damage)
            .finish_non_exhaustive()
    }
}

/// Context of input handling ([`Widget::event`] and [`Widget::perform`]).
pub struct EventCx<'a> {
    /// The widget receiving the event.
    pub id: WidgetId,
    pub theme: &'a Theme,
    /// `dpi / 96`.
    pub scale: f32,
    pub text: &'a dyn TextLayoutEngine,
    pub focus: &'a mut FocusManager,
    pub capture: &'a mut PointerCapture,
    pub requests: &'a mut Requests,
}

impl<'a> EventCx<'a> {
    #[must_use]
    pub fn new(
        id: WidgetId,
        theme: &'a Theme,
        scale: f32,
        text: &'a dyn TextLayoutEngine,
        focus: &'a mut FocusManager,
        capture: &'a mut PointerCapture,
        requests: &'a mut Requests,
    ) -> Self {
        Self {
            id,
            theme,
            scale,
            text,
            focus,
            capture,
            requests,
        }
    }

    /// A context for delivering an event to child `id`, sharing this context's state.
    pub fn for_widget(&mut self, id: WidgetId) -> EventCx<'_> {
        EventCx {
            id,
            theme: self.theme,
            scale: self.scale,
            text: self.text,
            focus: &mut *self.focus,
            capture: &mut *self.capture,
            requests: &mut *self.requests,
        }
    }

    /// True when this widget has keyboard focus.
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focus.is_focused(self.id)
    }

    /// Moves keyboard focus to this widget now (instead of through
    /// [`EventResult::request_focus`]). Returns whether focus changed.
    pub fn focus_self(&mut self) -> bool {
        self.focus.focus(self.id)
    }

    /// Routes all pointer events to this widget until [`release_pointer`](Self::release_pointer).
    pub fn capture_pointer(&mut self) {
        self.capture.capture(self.id);
    }

    /// Ends this widget's pointer capture. Returns whether it held the capture.
    pub fn release_pointer(&mut self) -> bool {
        self.capture.release(self.id)
    }

    /// True when this widget holds the pointer capture.
    #[must_use]
    pub fn has_pointer_capture(&self) -> bool {
        self.capture.is_captured_by(self.id)
    }

    pub fn invalidate(&mut self, rect: Rect) {
        self.requests.invalidate(rect);
    }

    pub fn invalidate_all(&mut self) {
        self.requests.invalidate_all();
    }

    /// Starts a timer with a fresh id; the widget receives `Event::Timer(id)`.
    pub fn start_timer(&mut self, interval: Duration, repeat: bool) -> TimerId {
        let id = TimerId::allocate();
        self.requests.set_timer(id, interval, repeat);
        id
    }

    pub fn set_timer(&mut self, id: TimerId, interval: Duration, repeat: bool) {
        self.requests.set_timer(id, interval, repeat);
    }

    pub fn kill_timer(&mut self, id: TimerId) {
        self.requests.kill_timer(id);
    }

    pub fn request_animation_frame(&mut self) {
        self.requests.request_animation_frame();
    }

    /// Sets the cursor shown at the current pointer position.
    pub fn set_cursor(&mut self, cursor: Cursor) {
        self.requests.set_cursor(cursor);
    }

    /// Rounds `value` (DIPs) to a device pixel.
    #[must_use]
    pub fn snap(&self, value: f32) -> f32 {
        snap_to_pixel(value, self.scale)
    }
}

impl fmt::Debug for EventCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCx")
            .field("id", &self.id)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

/// Context of the accessibility pass.
#[derive(Clone, Copy)]
pub struct AccessCx<'a> {
    /// The widget describing itself.
    pub id: WidgetId,
    pub theme: &'a Theme,
    /// `dpi / 96`.
    pub scale: f32,
    pub text: &'a dyn TextLayoutEngine,
    pub focus: &'a FocusManager,
}

impl<'a> AccessCx<'a> {
    #[must_use]
    pub fn new(
        id: WidgetId,
        theme: &'a Theme,
        scale: f32,
        text: &'a dyn TextLayoutEngine,
        focus: &'a FocusManager,
    ) -> Self {
        Self {
            id,
            theme,
            scale,
            text,
            focus,
        }
    }

    /// A context for describing child `id`.
    #[must_use]
    pub fn for_widget(&self, id: WidgetId) -> AccessCx<'a> {
        AccessCx { id, ..*self }
    }

    /// True when this widget has keyboard focus (for `AccessState::focused`).
    #[must_use]
    pub fn is_focused(&self) -> bool {
        self.focus.is_focused(self.id)
    }

    /// True when this widget is in the tab order (for `AccessState::focusable`).
    #[must_use]
    pub fn is_focusable(&self) -> bool {
        self.focus.can_focus(self.id)
    }
}

impl fmt::Debug for AccessCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessCx")
            .field("id", &self.id)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{AccessNode, AccessState, Patterns, Role};
    use crate::canvas::{DrawOp, RecordingCanvas};
    use crate::geometry::{Color, CornerRadii, Point};
    use crate::input::{
        FocusEvent, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind, vk,
    };
    use crate::text::{MonoTextEngine, TextLayout, TextStyle};

    fn id(raw: u64) -> WidgetId {
        WidgetId::new(raw)
    }

    /// A push button with real behavior: press captures the pointer and asks for focus, release
    /// over the button clicks, Space/Enter click when focused.
    struct TestButton {
        id: WidgetId,
        label: &'static str,
        bounds: Rect,
        pressed: bool,
        hovered: bool,
        clicks: u32,
        layout: Option<Box<dyn TextLayout>>,
    }

    impl TestButton {
        fn new(raw: u64, label: &'static str) -> Self {
            Self {
                id: id(raw),
                label,
                bounds: Rect::ZERO,
                pressed: false,
                hovered: false,
                clicks: 0,
                layout: None,
            }
        }

        fn click(&mut self, cx: &mut EventCx) {
            self.clicks += 1;
            cx.invalidate(self.bounds);
        }
    }

    impl Widget for TestButton {
        fn measure(&mut self, cx: &mut LayoutCx, _available: Size) -> Size {
            let metrics = &cx.theme.tokens.metrics;
            let style = TextStyle::new(metrics.ui_font, metrics.base_font_size);
            let layout = cx.text.layout(self.label, &style, &[], &[], f32::INFINITY);
            let size = layout.size();
            self.layout = Some(layout);
            Size::new(size.width + 16.0, size.height + 8.0)
        }

        fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
            self.bounds = bounds;
            cx.add_tab_stop();
        }

        fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
            if !cx.needs_paint(self.bounds) {
                return;
            }
            let tokens = &cx.theme.tokens;
            let radii = CornerRadii::uniform(tokens.metrics.control_radius);
            let background = if self.pressed {
                tokens.pressed_bg
            } else if self.hovered {
                tokens.hover_bg
            } else {
                tokens.surface
            };
            canvas.fill_rounded_rect(self.bounds, radii, background);
            if let Some(layout) = &self.layout {
                canvas.draw_text_layout(
                    &**layout,
                    Point::new(self.bounds.x + 8.0, self.bounds.y + 4.0),
                    tokens.text,
                );
            }
            if cx.is_focused() {
                canvas.stroke_rounded_rect(self.bounds, radii, tokens.focus_ring, 2.0);
            }
        }

        fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
            match e {
                Event::Pointer(pointer) => {
                    let inside = self.bounds.contains(pointer.position);
                    match pointer.kind {
                        PointerEventKind::Down
                            if pointer.button == Some(PointerButton::Left) && inside =>
                        {
                            self.pressed = true;
                            cx.capture_pointer();
                            cx.invalidate(self.bounds);
                            EventResult::handled_with_focus()
                        }
                        PointerEventKind::Up
                            if pointer.button == Some(PointerButton::Left) && self.pressed =>
                        {
                            self.pressed = false;
                            cx.release_pointer();
                            if inside {
                                self.click(cx);
                            } else {
                                cx.invalidate(self.bounds);
                            }
                            EventResult::handled()
                        }
                        PointerEventKind::Move => {
                            if inside != self.hovered {
                                self.hovered = inside;
                                cx.invalidate(self.bounds);
                            }
                            if inside || self.pressed {
                                cx.set_cursor(Cursor::Hand);
                                EventResult::handled()
                            } else {
                                EventResult::ignored()
                            }
                        }
                        PointerEventKind::Leave => {
                            if self.hovered {
                                self.hovered = false;
                                cx.invalidate(self.bounds);
                            }
                            EventResult::ignored()
                        }
                        _ => EventResult::ignored(),
                    }
                }
                Event::Key(key)
                    if key.is_down
                        && cx.is_focused()
                        && (key.vk == vk::SPACE || key.vk == vk::RETURN) =>
                {
                    self.click(cx);
                    EventResult::handled()
                }
                Event::Focus(_) => {
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                }
                _ => EventResult::ignored(),
            }
        }

        fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
            let state = AccessState {
                focusable: cx.is_focusable(),
                focused: cx.is_focused(),
                ..AccessState::default()
            };
            out.insert(
                AccessNode::new(self.id, Role::Button, self.label, self.bounds)
                    .with_state(state)
                    .with_patterns(Patterns {
                        invoke: true,
                        ..Patterns::default()
                    }),
            );
        }

        fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
            match a {
                AccessAction::Invoke => {
                    self.click(cx);
                    true
                }
                AccessAction::Focus => {
                    cx.focus_self();
                    true
                }
                _ => false,
            }
        }
    }

    /// Host-owned state for driving widgets in tests.
    struct Host {
        theme: Theme,
        text: MonoTextEngine,
        focus: FocusManager,
        capture: PointerCapture,
        requests: Requests,
    }

    impl Host {
        fn new() -> Self {
            Self {
                theme: Theme::light(Color::from_rgb_u32(0x1874D3)),
                text: MonoTextEngine::default(),
                focus: FocusManager::new(),
                capture: PointerCapture::new(),
                requests: Requests::new(),
            }
        }

        /// Measures and arranges `buttons` side by side, rebuilding the tab order.
        fn layout(&mut self, buttons: &mut [TestButton]) {
            self.focus.begin_tab_order();
            let mut x = 0.0;
            for button in buttons.iter_mut() {
                let mut cx = LayoutCx::new(
                    button.id,
                    &self.theme,
                    1.0,
                    &self.text,
                    &mut self.focus,
                    &mut self.requests,
                );
                let size = button.measure(&mut cx, Size::new(f32::INFINITY, f32::INFINITY));
                button.arrange(&mut cx, Rect::from_origin_size(Point::new(x, 0.0), size));
                x += size.width + 10.0;
            }
            self.focus.commit_tab_order();
        }

        /// Routes a pointer event like the real host: to the capture owner, else to the button
        /// under the pointer; grants focus when the widget asks for it.
        fn pointer(
            &mut self,
            buttons: &mut [TestButton],
            kind: PointerEventKind,
            button: Option<PointerButton>,
            at: Point,
        ) -> EventResult {
            let hit = buttons.iter().find(|b| b.bounds.contains(at)).map(|b| b.id);
            let Some(target) = self.capture.target(hit) else {
                return EventResult::ignored();
            };
            let Some(widget) = buttons.iter_mut().find(|b| b.id == target) else {
                return EventResult::ignored();
            };
            let event = Event::Pointer(PointerEvent::mouse(kind, button, at));
            let mut cx = EventCx::new(
                target,
                &self.theme,
                1.0,
                &self.text,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            let result = widget.event(&mut cx, &event);
            if result.request_focus {
                self.focus.focus(target);
            }
            result
        }

        fn key(&mut self, buttons: &mut [TestButton], code: u16) -> EventResult {
            let Some(target) = self.focus.focused() else {
                return EventResult::ignored();
            };
            let Some(widget) = buttons.iter_mut().find(|b| b.id == target) else {
                return EventResult::ignored();
            };
            let event = Event::Key(KeyEvent::press(code, Modifiers::NONE));
            let mut cx = EventCx::new(
                target,
                &self.theme,
                1.0,
                &self.text,
                &mut self.focus,
                &mut self.capture,
                &mut self.requests,
            );
            widget.event(&mut cx, &event)
        }
    }

    fn three_buttons() -> Vec<TestButton> {
        vec![
            TestButton::new(10, "One"),
            TestButton::new(20, "Two"),
            TestButton::new(30, "Three"),
        ]
    }

    #[test]
    fn widget_ids_round_trip_and_allocate_uniquely() {
        assert_eq!(WidgetId::new(7).get(), 7);
        let a = WidgetId::allocate();
        let b = WidgetId::allocate();
        assert_ne!(a, b);
    }

    #[test]
    fn event_results_combine() {
        assert_eq!(EventResult::default(), EventResult::ignored());
        let both = EventResult::handled().or(EventResult {
            handled: false,
            request_focus: true,
        });
        assert_eq!(both, EventResult::handled_with_focus());
        assert_eq!(
            EventResult::ignored().or(EventResult::ignored()),
            EventResult::ignored()
        );
    }

    #[test]
    fn tab_traversal_follows_the_order_and_wraps() {
        let mut focus = FocusManager::new();
        assert!(!focus.focus_next(), "empty order has nowhere to go");
        assert!(!focus.focus_previous());

        focus.set_tab_order([id(3), id(1), id(2), id(1)]);
        assert_eq!(
            focus.tab_order(),
            &[id(3), id(1), id(2)],
            "duplicates keep the first position"
        );
        assert_eq!(focus.focused(), None);

        let mut visited = Vec::new();
        for _ in 0..4 {
            assert!(focus.focus_next());
            visited.push(focus.focused());
        }
        assert_eq!(
            visited,
            [Some(id(3)), Some(id(1)), Some(id(2)), Some(id(3))]
        );

        assert!(focus.focus_previous());
        assert_eq!(
            focus.focused(),
            Some(id(2)),
            "shift-tab from the first stop wraps to the last"
        );
        assert!(focus.focus_previous());
        assert_eq!(focus.focused(), Some(id(1)));

        focus.blur();
        assert!(focus.focus_previous());
        assert_eq!(
            focus.focused(),
            Some(id(2)),
            "backwards from nothing starts at the end"
        );
    }

    #[test]
    fn a_single_tab_stop_keeps_focus() {
        let mut focus = FocusManager::new();
        focus.set_tab_order([id(5)]);
        assert!(focus.focus_next());
        assert!(!focus.focus_next());
        assert!(!focus.focus_previous());
        assert_eq!(focus.focused(), Some(id(5)));
    }

    #[test]
    fn only_tab_stops_take_focus_and_changes_are_queued() {
        let mut focus = FocusManager::new();
        focus.set_tab_order([id(1), id(2)]);
        assert!(!focus.focus(id(99)), "unregistered ids are refused");
        assert_eq!(focus.focused(), None);

        assert!(focus.focus(id(1)));
        assert!(!focus.focus(id(1)), "refocusing is not a change");
        assert!(focus.focus(id(2)));
        assert!(focus.blur());
        assert!(!focus.blur());

        assert_eq!(
            focus.take_changes(),
            [
                FocusChange {
                    lost: None,
                    gained: Some(id(1))
                },
                FocusChange {
                    lost: Some(id(1)),
                    gained: Some(id(2))
                },
                FocusChange {
                    lost: Some(id(2)),
                    gained: None
                },
            ]
        );
        assert!(focus.take_changes().is_empty());
    }

    #[test]
    fn committing_a_tab_order_without_the_focused_widget_drops_focus() {
        let mut focus = FocusManager::new();
        focus.set_tab_order([id(1), id(2)]);
        focus.focus(id(2));
        focus.take_changes();

        focus.begin_tab_order();
        focus.add_tab_stop(id(1));
        assert_eq!(
            focus.tab_order(),
            &[id(1), id(2)],
            "the old order stays until committed"
        );
        assert!(focus.commit_tab_order());
        assert_eq!(focus.focused(), None);
        assert_eq!(
            focus.take_changes(),
            [FocusChange {
                lost: Some(id(2)),
                gained: None
            }]
        );

        focus.focus(id(1));
        assert!(
            !focus.set_tab_order([id(2), id(1)]),
            "focus survives a reorder"
        );
        assert_eq!(focus.focused(), Some(id(1)));
    }

    #[test]
    fn layout_registers_tab_stops_in_arrange_order() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        assert_eq!(host.focus.tab_order(), &[id(10), id(20), id(30)]);
        assert!(buttons[0].bounds.width > 0.0);
        assert!(
            buttons[1].bounds.x > buttons[0].bounds.right(),
            "buttons do not overlap"
        );

        buttons.remove(1);
        host.layout(&mut buttons);
        assert_eq!(host.focus.tab_order(), &[id(10), id(30)]);
    }

    #[test]
    fn invalidation_accumulates_a_bounding_box() {
        let mut requests = Requests::new();
        assert!(requests.invalidation().is_clean());

        requests.invalidate(Rect::new(10.0, 10.0, 10.0, 10.0));
        requests.invalidate(Rect::ZERO);
        requests.invalidate(Rect::new(0.0, 5.0, 4.0, 4.0));
        assert_eq!(
            requests.invalidation(),
            Invalidation::Rect(Rect::from_ltrb(0.0, 5.0, 20.0, 20.0))
        );

        let window = Rect::new(0.0, 0.0, 15.0, 100.0);
        assert_eq!(
            requests.invalidation().region(window),
            Some(Rect::from_ltrb(0.0, 5.0, 15.0, 20.0)),
            "the region is clipped to the window"
        );

        requests.invalidate_all();
        requests.invalidate(Rect::new(0.0, 0.0, 1.0, 1.0));
        assert_eq!(requests.invalidation(), Invalidation::All);
        assert_eq!(requests.invalidation().region(window), Some(window));

        assert_eq!(requests.take_invalidation(), Invalidation::All);
        assert!(requests.invalidation().is_clean());
        assert_eq!(requests.invalidation().region(window), None);
    }

    #[test]
    fn invalidation_outside_the_window_yields_no_region() {
        let mut invalidation = Invalidation::Clean;
        invalidation.add(Rect::new(200.0, 200.0, 5.0, 5.0));
        assert_eq!(invalidation.region(Rect::new(0.0, 0.0, 100.0, 100.0)), None);
        assert_eq!(Invalidation::All.region(Rect::ZERO), None);
    }

    #[derive(Default)]
    struct RecordingTimers {
        calls: Vec<String>,
    }

    impl Timers for RecordingTimers {
        fn set(&mut self, id: TimerId, interval: Duration, repeat: bool) {
            self.calls.push(format!(
                "set {} {}ms {repeat}",
                id.get(),
                interval.as_millis()
            ));
        }

        fn kill(&mut self, id: TimerId) {
            self.calls.push(format!("kill {}", id.get()));
        }

        fn request_animation_frame(&mut self) {
            self.calls.push("frame".to_owned());
        }
    }

    #[test]
    fn timer_requests_supersede_each_other_and_flush_in_order() {
        let mut requests = Requests::new();
        let a = TimerId::new(1);
        let b = TimerId::new(2);
        requests.set_timer(a, Duration::from_millis(500), true);
        requests.set_timer(b, Duration::from_millis(10), false);
        requests.set_timer(a, Duration::from_millis(250), true);
        requests.kill_timer(b);
        requests.request_animation_frame();
        requests.request_animation_frame();
        assert_eq!(requests.timers().len(), 2);

        let mut timers = RecordingTimers::default();
        requests.flush_timers(&mut timers);
        assert_eq!(timers.calls, ["set 1 250ms true", "kill 2", "frame"]);

        assert!(requests.timers().is_empty());
        assert!(!requests.animation_frame_requested());
        requests.flush_timers(&mut timers);
        assert_eq!(timers.calls.len(), 3, "a flushed batch is not sent twice");
    }

    #[test]
    fn cursor_requests_keep_the_last_and_reset_on_take() {
        let mut requests = Requests::new();
        assert_eq!(requests.cursor(), None);
        requests.set_cursor(Cursor::IBeam);
        requests.set_cursor(Cursor::Hand);
        assert_eq!(requests.take_cursor(), Some(Cursor::Hand));
        assert_eq!(requests.take_cursor(), None);
    }

    #[test]
    fn capture_ownership_rules() {
        let mut capture = PointerCapture::new();
        assert_eq!(capture.target(Some(id(1))), Some(id(1)));
        assert_eq!(capture.target(None), None);

        capture.capture(id(2));
        assert_eq!(
            capture.target(Some(id(1))),
            Some(id(2)),
            "capture beats hit testing"
        );
        assert_eq!(
            capture.target(None),
            Some(id(2)),
            "capture works outside every widget"
        );

        assert!(!capture.release(id(1)), "only the owner releases");
        assert!(capture.is_captured_by(id(2)));

        capture.capture(id(3));
        assert!(
            !capture.is_captured_by(id(2)),
            "a new capture steals the old one"
        );
        assert_eq!(capture.force_release(), Some(id(3)));
        assert_eq!(capture.owner(), None);
        assert_eq!(capture.force_release(), None);
    }

    #[test]
    fn press_captures_and_release_over_the_button_clicks() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let inside = buttons[1].bounds.center();

        let down = host.pointer(
            &mut buttons,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            inside,
        );
        assert_eq!(down, EventResult::handled_with_focus());
        assert_eq!(host.capture.owner(), Some(id(20)));
        assert_eq!(
            host.focus.focused(),
            Some(id(20)),
            "the host granted the requested focus"
        );
        assert_eq!(
            host.requests.take_invalidation(),
            Invalidation::Rect(buttons[1].bounds)
        );

        let up = host.pointer(
            &mut buttons,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            inside,
        );
        assert_eq!(up, EventResult::handled());
        assert_eq!(host.capture.owner(), None);
        assert_eq!(buttons[1].clicks, 1);
        assert!(!buttons[1].pressed);
    }

    #[test]
    fn captured_pointer_events_reach_the_widget_outside_its_bounds() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let inside = buttons[0].bounds.center();
        let far_away = Point::new(buttons[2].bounds.right() + 500.0, 300.0);
        let over_other = buttons[2].bounds.center();

        host.pointer(
            &mut buttons,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            inside,
        );
        host.requests.take_cursor();

        let moved = host.pointer(&mut buttons, PointerEventKind::Move, None, far_away);
        assert_eq!(
            moved,
            EventResult::handled(),
            "still delivered to the pressed button"
        );
        assert_eq!(host.requests.cursor(), Some(Cursor::Hand));

        let moved_over_other = host.pointer(&mut buttons, PointerEventKind::Move, None, over_other);
        assert_eq!(moved_over_other, EventResult::handled());
        assert!(
            !buttons[2].hovered,
            "the widget under the pointer sees nothing while captured"
        );

        host.pointer(
            &mut buttons,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            far_away,
        );
        assert_eq!(
            buttons[0].clicks, 0,
            "releasing outside the button is not a click"
        );
        assert_eq!(host.capture.owner(), None, "capture ends on release");

        let after = host.pointer(&mut buttons, PointerEventKind::Move, None, far_away);
        assert_eq!(
            after,
            EventResult::ignored(),
            "without capture nothing is under the pointer"
        );
    }

    #[test]
    fn hover_and_cursor_without_capture() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let center = buttons[2].bounds.center();

        host.pointer(&mut buttons, PointerEventKind::Move, None, center);
        assert!(buttons[2].hovered);
        assert_eq!(host.requests.take_cursor(), Some(Cursor::Hand));
        assert_eq!(
            host.requests.take_invalidation(),
            Invalidation::Rect(buttons[2].bounds)
        );

        host.pointer(&mut buttons, PointerEventKind::Move, None, center);
        assert!(
            host.requests.invalidation().is_clean(),
            "moving within the button changes nothing"
        );
        assert_eq!(host.capture.owner(), None);
    }

    #[test]
    fn keyboard_activation_needs_focus() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);

        assert_eq!(host.key(&mut buttons, vk::SPACE), EventResult::ignored());

        host.focus.focus_next();
        host.focus.focus_next();
        assert_eq!(host.key(&mut buttons, vk::SPACE), EventResult::handled());
        assert_eq!(host.key(&mut buttons, vk::RETURN), EventResult::handled());
        assert_eq!(
            buttons.iter().map(|b| b.clicks).collect::<Vec<_>>(),
            [0, 2, 0]
        );
        assert_eq!(host.key(&mut buttons, vk::TAB), EventResult::ignored());
    }

    #[test]
    fn child_contexts_share_host_state() {
        let mut host = Host::new();
        let mut cx = EventCx::new(
            id(1),
            &host.theme,
            1.5,
            &host.text,
            &mut host.focus,
            &mut host.capture,
            &mut host.requests,
        );
        {
            let mut child = cx.for_widget(id(2));
            assert_eq!(child.id, id(2));
            assert!((child.scale - 1.5).abs() < f32::EPSILON);
            child.capture_pointer();
            child.invalidate(Rect::new(0.0, 0.0, 4.0, 4.0));
            child.set_cursor(Cursor::SizeWE);
        }
        assert_eq!(cx.id, id(1));
        assert!(!cx.has_pointer_capture(), "the parent is not the owner");
        assert!(!cx.release_pointer());
        assert!(
            (cx.snap(1.0) - 2.0 / 1.5).abs() < 1e-6,
            "1 DIP at 1.5x rounds to 2 device pixels"
        );
        assert_eq!(host.capture.owner(), Some(id(2)));
        assert_eq!(host.requests.cursor(), Some(Cursor::SizeWE));
        assert!(!host.requests.invalidation().is_clean());
    }

    #[test]
    fn timers_started_through_the_context_get_fresh_ids() {
        let mut host = Host::new();
        let mut cx = EventCx::new(
            id(1),
            &host.theme,
            1.0,
            &host.text,
            &mut host.focus,
            &mut host.capture,
            &mut host.requests,
        );
        let first = cx.start_timer(Duration::from_millis(530), true);
        let second = cx.start_timer(Duration::from_millis(100), false);
        cx.kill_timer(second);
        assert_ne!(first, second);
        assert_eq!(
            host.requests.timers(),
            [
                TimerRequest::Set {
                    id: first,
                    interval: Duration::from_millis(530),
                    repeat: true
                },
                TimerRequest::Kill(second),
            ]
        );
    }

    #[test]
    fn paint_draws_the_focus_ring_only_when_focused() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        host.focus.focus(id(30));

        let paint = |host: &Host, button: &TestButton, damage: Option<Rect>| {
            let cx = PaintCx::new(
                button.id,
                &host.theme,
                1.0,
                &host.text,
                &host.focus,
                &host.capture,
                damage,
            );
            let mut canvas = RecordingCanvas::new();
            button.paint(&cx, &mut canvas);
            canvas
        };
        let has_ring = |canvas: &RecordingCanvas| {
            canvas
                .ops()
                .any(|op| matches!(op, DrawOp::StrokeRoundedRect { .. }))
        };

        assert!(!has_ring(&paint(&host, &buttons[0], None)));
        assert!(has_ring(&paint(&host, &buttons[2], None)));
        assert_eq!(paint(&host, &buttons[0], None).texts(), ["One"]);

        let elsewhere = Some(Rect::new(0.0, 500.0, 10.0, 10.0));
        assert_eq!(
            paint(&host, &buttons[2], elsewhere).ops().count(),
            0,
            "outside the damage nothing is drawn"
        );
    }

    #[test]
    fn access_reflects_focus_and_actions_are_performed() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        host.focus.focus(id(20));

        let mut tree = AccessTree::new(id(10));
        for button in &buttons {
            let cx = AccessCx::new(button.id, &host.theme, 1.0, &host.text, &host.focus);
            button.access(&cx, &mut tree);
        }
        assert_eq!(tree.len(), 3);
        let focused = tree.focused().map(|node| node.name.clone());
        assert_eq!(focused.as_deref(), Some("Two"));
        let first = tree
            .node(id(10))
            .map(|node| (node.role, node.state.focusable, node.state.focused));
        assert_eq!(first, Some((Role::Button, true, false)));

        host.focus.blur();
        let mut cx = EventCx::new(
            id(30),
            &host.theme,
            1.0,
            &host.text,
            &mut host.focus,
            &mut host.capture,
            &mut host.requests,
        );
        assert!(buttons[2].perform(&mut cx, &AccessAction::Invoke));
        assert!(buttons[2].perform(&mut cx, &AccessAction::Focus));
        assert!(
            !buttons[2].perform(&mut cx, &AccessAction::Expand),
            "unsupported actions report false"
        );
        assert_eq!(buttons[2].clicks, 1);
        assert_eq!(host.focus.focused(), Some(id(30)));
    }

    #[test]
    fn focus_events_are_plain_input() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let mut cx = EventCx::new(
            id(10),
            &host.theme,
            1.0,
            &host.text,
            &mut host.focus,
            &mut host.capture,
            &mut host.requests,
        );
        let result = buttons[0].event(&mut cx, &Event::Focus(FocusEvent::Gained));
        assert!(result.handled);
        assert_eq!(
            host.requests.take_invalidation(),
            Invalidation::Rect(buttons[0].bounds)
        );
    }

    #[test]
    fn contexts_describe_themselves_and_snap_to_device_pixels() {
        let mut host = Host::new();
        let widget = id(1);
        let before = format!("{:?}", host.requests.invalidation());
        {
            let mut layout = LayoutCx::new(
                widget,
                &host.theme,
                1.5,
                &host.text,
                &mut host.focus,
                &mut host.requests,
            );
            let printed = format!("{layout:?}");
            assert!(
                printed.contains("LayoutCx") && printed.contains("scale: 1.5"),
                "{printed}"
            );
            assert_eq!(layout.snap(10.2), 10.0, "15.3 device pixels round to 15");
            layout.invalidate(Rect::new(0.0, 0.0, 5.0, 5.0));
        }
        assert_ne!(format!("{:?}", host.requests.invalidation()), before);

        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let at = buttons[0].bounds.center();
        host.pointer(
            &mut buttons,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            at,
        );
        {
            let paint = PaintCx::new(
                buttons[0].id,
                &host.theme,
                1.0,
                &host.text,
                &host.focus,
                &host.capture,
                None,
            );
            assert!(
                paint.has_pointer_capture(),
                "the pressed button holds the pointer"
            );
            assert_eq!(paint.snap(2.4), 2.0);
            let printed = format!("{paint:?}");
            assert!(
                printed.contains("PaintCx") && printed.contains("damage: None"),
                "{printed}"
            );
        }
        {
            let event = EventCx::new(
                widget,
                &host.theme,
                2.0,
                &host.text,
                &mut host.focus,
                &mut host.capture,
                &mut host.requests,
            );
            let printed = format!("{event:?}");
            assert!(
                printed.contains("EventCx") && printed.contains("scale: 2.0"),
                "{printed}"
            );
        }
        let access = AccessCx::new(widget, &host.theme, 2.0, &host.text, &host.focus);
        let printed = format!("{access:?}");
        assert!(
            printed.contains("AccessCx") && printed.contains("scale: 2.0"),
            "{printed}"
        );
    }

    fn background(host: &Host, button: &TestButton) -> Color {
        let mut canvas = RecordingCanvas::new();
        let cx = PaintCx::new(
            button.id,
            &host.theme,
            1.0,
            &host.text,
            &host.focus,
            &host.capture,
            None,
        );
        button.paint(&cx, &mut canvas);
        let fills: Vec<Color> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::FillRoundedRect { color, .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert_eq!(fills.len(), 1, "one background per paint");
        fills[0]
    }

    #[test]
    fn a_test_button_that_was_never_measured_paints_its_background_without_a_label() {
        let host = Host::new();
        let mut button = TestButton::new(30, "Lazy");
        button.bounds = Rect::new(0.0, 0.0, 80.0, 24.0);
        let cx = PaintCx::new(
            button.id,
            &host.theme,
            1.0,
            &host.text,
            &host.focus,
            &host.capture,
            None,
        );
        let mut canvas = RecordingCanvas::new();
        button.paint(&cx, &mut canvas);
        assert!(canvas.texts().is_empty(), "no layout, no label");
        assert_eq!(canvas.ops().count(), 1, "just the background");
        assert_eq!(background(&host, &button), host.theme.tokens.surface);
    }

    #[test]
    fn the_test_button_paints_hover_and_pressed_backgrounds_and_ignores_strays() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let center = buttons[0].bounds.center();
        assert_eq!(background(&host, &buttons[0]), host.theme.tokens.surface);

        host.pointer(&mut buttons, PointerEventKind::Move, None, center);
        assert_eq!(background(&host, &buttons[0]), host.theme.tokens.hover_bg);
        host.pointer(
            &mut buttons,
            PointerEventKind::Down,
            Some(PointerButton::Left),
            center,
        );
        assert_eq!(background(&host, &buttons[0]), host.theme.tokens.pressed_bg);
        host.pointer(
            &mut buttons,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            center,
        );
        assert_eq!(buttons[0].clicks, 1);

        let deliver = |host: &mut Host, button: &mut TestButton, kind, at| {
            let event = Event::Pointer(PointerEvent::mouse(kind, None, at));
            let mut cx = EventCx::new(
                button.id,
                &host.theme,
                1.0,
                &host.text,
                &mut host.focus,
                &mut host.capture,
                &mut host.requests,
            );
            button.event(&mut cx, &event)
        };
        let far = Point::new(-100.0, -100.0);
        assert!(buttons[0].hovered);
        assert!(!deliver(&mut host, &mut buttons[0], PointerEventKind::Leave, far).handled);
        assert!(!buttons[0].hovered, "leaving ends the hover");
        assert!(!deliver(&mut host, &mut buttons[0], PointerEventKind::Leave, far).handled);
        let moved = deliver(&mut host, &mut buttons[0], PointerEventKind::Move, far);
        assert!(!moved.handled, "a move elsewhere is not the button's");
        let entered = deliver(&mut host, &mut buttons[0], PointerEventKind::Enter, far);
        assert!(!entered.handled, "enter alone is not a hover");
        assert!(!buttons[0].hovered);
    }

    #[test]
    fn the_test_host_ignores_events_for_widgets_it_does_not_know() {
        let mut host = Host::new();
        let mut buttons = three_buttons();
        host.layout(&mut buttons);
        let center = buttons[0].bounds.center();
        host.capture.capture(id(999));
        assert!(
            !host
                .pointer(&mut buttons, PointerEventKind::Move, None, center)
                .handled
        );

        host.focus.begin_tab_order();
        host.focus.add_tab_stop(id(999));
        host.focus.commit_tab_order();
        assert!(host.focus.focus(id(999)));
        assert!(!host.key(&mut buttons, vk::SPACE).handled);
    }
}
