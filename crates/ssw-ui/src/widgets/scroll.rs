//! Scrolling: the offset arithmetic shared by the scrolling controls ([`ScrollModel`]), the thin
//! overlay [`ScrollBar`] and the [`ScrollView`] container.
//!
//! The bar follows Windows 11: no arrows, a 2 px thumb at the edge that grows to 6 px with a
//! track behind it while the pointer is over it.

use std::fmt;
use std::time::Duration;

use crate::access::{AccessAction, AccessNode, AccessTree, RangeValue, Role, ScrollInfo};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Point, Rect, Size};
use crate::input::{Event, PointerButton, PointerEvent, PointerEventKind, WheelEvent};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::Control;
use super::anim::{Easing, HOVER_DURATION, Motion, Transition};

/// Offset change of one wheel notch, in DIPs.
pub const WHEEL_STEP: f32 = 48.0;
/// Duration of an eased scroll.
pub const SMOOTH_DURATION: Duration = Duration::from_millis(167);
/// Thickness of the hit zone of a bar.
pub const BAR_WIDTH: f32 = 12.0;
/// Thumb thickness at rest.
pub const THIN_WIDTH: f32 = 2.0;
/// Thumb thickness while hovered.
pub const WIDE_WIDTH: f32 = 6.0;
/// Gap between the thumb and the edge of the bar.
const EDGE: f32 = 2.0;
/// Shortest thumb.
const MIN_THUMB: f32 = 24.0;
/// Part of a page that stays visible after a page scroll.
const PAGE_OVERLAP: f32 = 16.0;

/// A scrolling direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// The scroll offset of one axis: viewport and content extents, the current offset and an eased
/// target. All values are DIPs; the offset is always within `0..=max_offset`.
#[derive(Debug, Clone, Copy)]
pub struct ScrollModel {
    viewport: f32,
    content: f32,
    offset: Transition,
}

impl Default for ScrollModel {
    fn default() -> Self {
        Self::new(Motion::full())
    }
}

impl ScrollModel {
    #[must_use]
    pub fn new(motion: Motion) -> Self {
        Self {
            viewport: 0.0,
            content: 0.0,
            offset: Transition::new(0.0, motion),
        }
    }

    pub fn set_motion(&mut self, motion: Motion) {
        self.offset.set_motion(motion);
    }

    /// Sets the visible and total extents and keeps the offset in range.
    pub fn set_extent(&mut self, viewport: f32, content: f32) {
        self.viewport = viewport.max(0.0);
        self.content = content.max(0.0);
        let max = self.max_offset();
        if self.offset.value() > max || self.offset.target() > max {
            self.jump(max);
        }
    }

    #[must_use]
    pub fn viewport(&self) -> f32 {
        self.viewport
    }

    #[must_use]
    pub fn content(&self) -> f32 {
        self.content
    }

    /// The offset being displayed (it trails [`target`](Self::target) while easing).
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset.value()
    }

    #[must_use]
    pub fn target(&self) -> f32 {
        self.offset.target()
    }

    #[must_use]
    pub fn max_offset(&self) -> f32 {
        (self.content - self.viewport).max(0.0)
    }

    #[must_use]
    pub fn is_scrollable(&self) -> bool {
        self.max_offset() > 0.0
    }

    #[must_use]
    pub fn is_animating(&self) -> bool {
        self.offset.is_running()
    }

    /// The offset change of a page scroll.
    #[must_use]
    pub fn page(&self) -> f32 {
        (self.viewport - PAGE_OVERLAP).max(self.viewport / 2.0)
    }

    fn clamp(&self, offset: f32) -> f32 {
        offset.clamp(0.0, self.max_offset())
    }

    /// Jumps to `offset` (clamped). Returns whether the displayed offset changed.
    pub fn jump(&mut self, offset: f32) -> bool {
        let target = self.clamp(offset);
        let changed = target != self.offset.value();
        self.offset
            .set_target(target, Duration::ZERO, Easing::Linear);
        self.offset.snap();
        changed
    }

    /// Jumps by `delta`.
    pub fn scroll_by(&mut self, delta: f32) -> bool {
        self.jump(self.target() + delta)
    }

    /// Eases toward `offset` (clamped). Returns whether animation frames are needed; with
    /// reduced motion the jump happens at once and none are.
    pub fn animate_to(&mut self, offset: f32) -> bool {
        let target = self.clamp(offset);
        self.offset
            .set_target(target, SMOOTH_DURATION, Easing::Decelerate)
    }

    /// Eases by `delta` from the current target, so repeated wheel notches add up.
    pub fn animate_by(&mut self, delta: f32) -> bool {
        self.animate_to(self.target() + delta)
    }

    /// Advances the easing to the frame time. Returns `(changed, more)`: whether the displayed
    /// offset may have changed and whether another frame is needed.
    pub fn tick(&mut self, now: Duration) -> (bool, bool) {
        let was_running = self.offset.is_running();
        let more = self.offset.tick(now);
        (was_running, more)
    }

    /// Scrolls the least amount that shows `start..end` (content coordinates). Returns whether
    /// the offset changed.
    pub fn ensure_visible(&mut self, start: f32, end: f32) -> bool {
        let top = self.target();
        if start < top {
            self.jump(start)
        } else if end > top + self.viewport {
            self.jump(end - self.viewport)
        } else {
            false
        }
    }

    /// The scrolled share `0..=100` (0 when nothing scrolls).
    #[must_use]
    pub fn percent(&self) -> f32 {
        let max = self.max_offset();
        if max <= 0.0 {
            0.0
        } else {
            self.offset() / max * 100.0
        }
    }

    /// The visible share `0..=100` of the content.
    #[must_use]
    pub fn view_percent(&self) -> f32 {
        if self.content <= self.viewport || self.content <= 0.0 {
            100.0
        } else {
            self.viewport / self.content * 100.0
        }
    }

    /// Jumps to a scrolled share `0..=100`.
    pub fn jump_percent(&mut self, percent: f32) -> bool {
        self.jump(self.max_offset() * percent.clamp(0.0, 100.0) / 100.0)
    }

    /// The thumb's start and length along a track of `track` DIPs.
    #[must_use]
    pub fn thumb(&self, track: f32) -> (f32, f32) {
        let track = track.max(0.0);
        if !self.is_scrollable() || track <= 0.0 {
            return (0.0, track);
        }
        let length = (track * self.viewport / self.content).clamp(MIN_THUMB.min(track), track);
        let start = (track - length) * (self.offset() / self.max_offset());
        (start, length)
    }

    /// The offset whose thumb starts at `thumb_start` along a track of `track` DIPs.
    #[must_use]
    pub fn offset_for_thumb(&self, track: f32, thumb_start: f32) -> f32 {
        let (_, length) = self.thumb(track);
        let free = track - length;
        if free <= 0.0 {
            0.0
        } else {
            thumb_start.clamp(0.0, free) / free * self.max_offset()
        }
    }
}

/// The UIA scroll data of a view scrolling on `horizontal` and `vertical`.
#[must_use]
pub fn scroll_info(horizontal: &ScrollModel, vertical: &ScrollModel) -> ScrollInfo {
    ScrollInfo {
        horizontal_percent: horizontal.percent(),
        vertical_percent: vertical.percent(),
        horizontal_view_percent: horizontal.view_percent(),
        vertical_view_percent: vertical.view_percent(),
        horizontally_scrollable: horizontal.is_scrollable(),
        vertically_scrollable: vertical.is_scrollable(),
    }
}

/// The offset change a wheel event asks for on `axis`. Wheel-up and wheel-left scroll toward
/// the start; Shift turns a vertical wheel into a horizontal one.
#[must_use]
pub fn wheel_offset_delta(event: &WheelEvent, axis: Axis) -> f32 {
    let unit = if event.precise { 1.0 } else { WHEEL_STEP };
    let (x, y) = if event.mods.shift() && event.delta.x == 0.0 {
        (-event.delta.y, 0.0)
    } else {
        (event.delta.x, event.delta.y)
    };
    match axis {
        Axis::Vertical => -y * unit,
        Axis::Horizontal => x * unit,
    }
}

/// A thin overlay scroll bar. It is not a tab stop; assistive technology drives it through
/// RangeValue.
#[derive(Debug, Clone)]
pub struct ScrollBar {
    id: WidgetId,
    axis: Axis,
    name: String,
    bounds: Rect,
    model: ScrollModel,
    hover: Transition,
    hovered: bool,
    /// Distance from the thumb start to where it was grabbed.
    drag: Option<f32>,
    changed: bool,
}

impl ScrollBar {
    /// A bar scrolling along `axis`; `name` is the accessible name.
    #[must_use]
    pub fn new(axis: Axis, name: impl Into<String>) -> Self {
        Self {
            id: WidgetId::allocate(),
            axis,
            name: name.into(),
            bounds: Rect::ZERO,
            model: ScrollModel::default(),
            hover: Transition::new(0.0, Motion::full()),
            hovered: false,
            drag: None,
            changed: false,
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.model.set_motion(motion);
        self.hover.set_motion(motion);
        self
    }

    #[must_use]
    pub fn axis(&self) -> Axis {
        self.axis
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// Places the bar (what `arrange` does, for owners that lay it out themselves).
    pub fn set_bounds(&mut self, bounds: Rect) {
        self.bounds = bounds;
    }

    #[must_use]
    pub fn model(&self) -> &ScrollModel {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut ScrollModel {
        &mut self.model
    }

    #[must_use]
    pub fn offset(&self) -> f32 {
        self.model.offset()
    }

    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    #[must_use]
    pub fn is_hovered(&self) -> bool {
        self.hovered
    }

    /// True once after the user (pointer or assistive technology) moved the offset.
    pub fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }

    fn track_len(&self) -> f32 {
        match self.axis {
            Axis::Vertical => self.bounds.height,
            Axis::Horizontal => self.bounds.width,
        }
    }

    fn along(&self, point: Point) -> f32 {
        match self.axis {
            Axis::Vertical => point.y - self.bounds.y,
            Axis::Horizontal => point.x - self.bounds.x,
        }
    }

    fn thickness(&self) -> f32 {
        THIN_WIDTH + (WIDE_WIDTH - THIN_WIDTH) * self.hover.value()
    }

    /// The visible thumb.
    #[must_use]
    pub fn thumb_rect(&self) -> Rect {
        let (start, length) = self.model.thumb(self.track_len());
        let thickness = self.thickness();
        match self.axis {
            Axis::Vertical => Rect::new(
                self.bounds.right() - thickness - EDGE,
                self.bounds.y + start,
                thickness,
                length,
            ),
            Axis::Horizontal => Rect::new(
                self.bounds.x + start,
                self.bounds.bottom() - thickness - EDGE,
                length,
                thickness,
            ),
        }
    }

    /// The thumb's hit zone: its extent along the bar, the full thickness across it.
    fn thumb_zone(&self) -> Rect {
        let thumb = self.thumb_rect();
        match self.axis {
            Axis::Vertical => Rect::new(self.bounds.x, thumb.y, self.bounds.width, thumb.height),
            Axis::Horizontal => Rect::new(thumb.x, self.bounds.y, thumb.width, self.bounds.height),
        }
    }

    fn set_hovered(&mut self, cx: &mut EventCx, hovered: bool) -> bool {
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        let target = if hovered { 1.0 } else { 0.0 };
        if self
            .hover
            .set_target(target, HOVER_DURATION, Easing::Standard)
        {
            cx.request_animation_frame();
        }
        cx.invalidate(self.bounds);
        true
    }

    /// Scrolls for a wheel event over the bar's view. Returns whether the target moved.
    pub fn wheel(&mut self, cx: &mut EventCx, event: &WheelEvent) -> bool {
        let delta = wheel_offset_delta(event, self.axis);
        if delta == 0.0 || !self.model.is_scrollable() {
            return false;
        }
        let before = self.model.target();
        if event.precise {
            self.model.scroll_by(delta);
        } else if self.model.animate_by(delta) {
            cx.request_animation_frame();
        }
        let moved = self.model.target() != before;
        if moved {
            self.changed = true;
            cx.invalidate(self.bounds);
        }
        moved
    }

    /// Advances the easing of the offset and of the hover expansion. Returns whether the
    /// scrolled offset moved (the owner repaints the content).
    pub fn animate(&mut self, cx: &mut EventCx, now: Duration) -> bool {
        let (moved, more) = self.model.tick(now);
        let hover_was_running = self.hover.is_running();
        let hover_more = self.hover.tick(now);
        if more || hover_more {
            cx.request_animation_frame();
        }
        if moved || hover_was_running {
            cx.invalidate(self.bounds);
        }
        moved
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let scrollable = self.model.is_scrollable();
        let left = event.button == Some(PointerButton::Left);
        match event.kind {
            PointerEventKind::Down
                if left && scrollable && self.bounds.contains(event.position) =>
            {
                let (start, _) = self.model.thumb(self.track_len());
                if self.thumb_zone().contains(event.position) {
                    self.drag = Some(self.along(event.position) - start);
                    cx.capture_pointer();
                } else {
                    let page = self.model.page();
                    let delta = if self.along(event.position) < start {
                        -page
                    } else {
                        page
                    };
                    if self.model.animate_by(delta) {
                        cx.request_animation_frame();
                    }
                    self.changed = true;
                }
                self.set_hovered(cx, true);
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            PointerEventKind::Move if self.drag.is_some() => {
                let grab = self.drag.unwrap_or(0.0);
                let start = self.along(event.position) - grab;
                let offset = self.model.offset_for_thumb(self.track_len(), start);
                if self.model.jump(offset) {
                    self.changed = true;
                }
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            PointerEventKind::Up if left && self.drag.is_some() => {
                self.drag = None;
                cx.release_pointer();
                self.set_hovered(cx, self.bounds.contains(event.position) && scrollable);
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            PointerEventKind::Move | PointerEventKind::Enter => {
                let inside = scrollable && self.bounds.contains(event.position);
                if self.set_hovered(cx, inside) {
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            PointerEventKind::Leave if self.drag.is_none() => {
                if self.set_hovered(cx, false) {
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            _ => EventResult::ignored(),
        }
    }

    fn access_node(&self) -> AccessNode {
        let mut node = AccessNode::new(self.id, Role::ScrollBar, self.name.clone(), self.bounds);
        node.state.offscreen = !self.model.is_scrollable();
        node.patterns.range_value = Some(RangeValue {
            value: f64::from(self.model.offset()),
            min: 0.0,
            max: f64::from(self.model.max_offset()),
            step: f64::from(WHEEL_STEP),
        });
        node
    }
}

impl Control for ScrollBar {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ScrollBar {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let along = |extent: f32| if extent.is_finite() { extent } else { 0.0 };
        match self.axis {
            Axis::Vertical => Size::new(BAR_WIDTH, along(available.height)),
            Axis::Horizontal => Size::new(along(available.width), BAR_WIDTH),
        }
    }

    fn arrange(&mut self, _cx: &mut LayoutCx, bounds: Rect) {
        self.set_bounds(bounds);
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.model.is_scrollable() || !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let hover = self.hover.value();
        if hover > 0.0 {
            canvas.fill_rect(
                self.bounds,
                tokens.hover_bg.with_alpha(tokens.hover_bg.a * hover),
            );
        }
        let thumb = self.thumb_rect();
        canvas.fill_rounded_rect(
            thumb,
            CornerRadii::uniform(thumb.width.min(thumb.height) / 2.0),
            tokens.scrollbar,
        );
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) if self.bounds.contains(event.position) => {
                if self.wheel(cx, event) {
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::AnimationFrame(now) => {
                self.animate(cx, *now);
                EventResult::ignored()
            }
            _ => EventResult::ignored(),
        }
    }

    fn access(&self, _cx: &AccessCx, out: &mut AccessTree) {
        out.insert(self.access_node());
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        let target = match a {
            AccessAction::SetRangeValue(value) => Some(*value as f32),
            AccessAction::ScrollTo {
                horizontal,
                vertical,
            } => {
                let percent = match self.axis {
                    Axis::Horizontal => *horizontal,
                    Axis::Vertical => *vertical,
                };
                percent.map(|percent| self.model.max_offset() * percent.clamp(0.0, 100.0) / 100.0)
            }
            _ => None,
        };
        let Some(offset) = target else {
            return false;
        };
        self.model.jump(offset);
        self.changed = true;
        cx.invalidate(self.bounds);
        true
    }
}

/// A container that scrolls one child larger than its bounds, with overlay bars.
///
/// The child is laid out in content coordinates (origin at the view's origin); painting and
/// pointer input translate by the scroll offset, and its access nodes are reported in window
/// coordinates with `offscreen` set when they are scrolled out of view.
pub struct ScrollView {
    id: WidgetId,
    name: String,
    child: Box<dyn Control>,
    horizontal: bool,
    bounds: Rect,
    content: Size,
    vertical_bar: ScrollBar,
    horizontal_bar: ScrollBar,
    scrolled: bool,
}

impl fmt::Debug for ScrollView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScrollView")
            .field("id", &self.id)
            .field("bounds", &self.bounds)
            .field("content", &self.content)
            .field("horizontal", &self.horizontal)
            .finish_non_exhaustive()
    }
}

impl ScrollView {
    /// A view scrolling `child` vertically; `name` is the accessible name.
    #[must_use]
    pub fn new(child: Box<dyn Control>, name: impl Into<String>) -> Self {
        Self {
            id: WidgetId::allocate(),
            name: name.into(),
            child,
            horizontal: false,
            bounds: Rect::ZERO,
            content: Size::ZERO,
            vertical_bar: ScrollBar::new(Axis::Vertical, ""),
            horizontal_bar: ScrollBar::new(Axis::Horizontal, ""),
            scrolled: false,
        }
    }

    /// Also scrolls horizontally instead of fitting the child to the width.
    #[must_use]
    pub fn horizontal(mut self) -> Self {
        self.horizontal = true;
        self
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        self.vertical_bar = self.vertical_bar.with_motion(motion);
        self.horizontal_bar = self.horizontal_bar.with_motion(motion);
        self
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    #[must_use]
    pub fn content_size(&self) -> Size {
        self.content
    }

    /// The scroll offset (content coordinates of the view's top-left).
    #[must_use]
    pub fn offset(&self) -> Point {
        Point::new(self.horizontal_bar.offset(), self.vertical_bar.offset())
    }

    #[must_use]
    pub fn child(&self) -> &dyn Control {
        self.child.as_ref()
    }

    pub fn child_mut(&mut self) -> &mut dyn Control {
        self.child.as_mut()
    }

    #[must_use]
    pub fn vertical_bar(&self) -> &ScrollBar {
        &self.vertical_bar
    }

    #[must_use]
    pub fn horizontal_bar(&self) -> &ScrollBar {
        &self.horizontal_bar
    }

    /// True once after the offset changed through input or assistive technology.
    pub fn take_scrolled(&mut self) -> bool {
        std::mem::take(&mut self.scrolled)
    }

    /// Jumps to `offset` (clamped per axis).
    pub fn scroll_to(&mut self, offset: Point) {
        self.horizontal_bar.model_mut().jump(offset.x);
        self.vertical_bar.model_mut().jump(offset.y);
    }

    /// Eases to `offset`. Returns whether animation frames are needed.
    pub fn scroll_to_smooth(&mut self, offset: Point) -> bool {
        let horizontal = self.horizontal_bar.model_mut().animate_to(offset.x);
        let vertical = self.vertical_bar.model_mut().animate_to(offset.y);
        horizontal || vertical
    }

    /// Scrolls the least amount that shows `rect` (content coordinates, relative to the view's
    /// origin). Returns whether the offset changed.
    pub fn ensure_visible(&mut self, rect: Rect) -> bool {
        let horizontal = self
            .horizontal_bar
            .model_mut()
            .ensure_visible(rect.left(), rect.right());
        let vertical = self
            .vertical_bar
            .model_mut()
            .ensure_visible(rect.top(), rect.bottom());
        horizontal || vertical
    }

    fn translated(&self, position: Point) -> Point {
        let offset = self.offset();
        Point::new(position.x + offset.x, position.y + offset.y)
    }

    fn bar_contains(&self, position: Point) -> bool {
        (self.vertical_bar.model().is_scrollable() && self.vertical_bar.bounds().contains(position))
            || (self.horizontal_bar.model().is_scrollable()
                && self.horizontal_bar.bounds().contains(position))
    }

    fn forward_to_child(&mut self, cx: &mut EventCx, event: &Event) -> EventResult {
        let child_id = self.child.id();
        let mut child_cx = cx.for_widget(child_id);
        self.child.event(&mut child_cx, event)
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        let vertical = self.vertical_bar.event(cx, &Event::Pointer(*event));
        let horizontal = self.horizontal_bar.event(cx, &Event::Pointer(*event));
        let scrolled = self.vertical_bar.take_changed() | self.horizontal_bar.take_changed();
        if scrolled {
            self.scrolled = true;
            cx.invalidate(self.bounds);
        }
        if vertical.handled || horizontal.handled {
            return vertical.or(horizontal);
        }
        let child_id = self.child.id();
        let captured = cx.capture.is_captured_by(child_id);
        let inside = self.bounds.contains(event.position) && !self.bar_contains(event.position);
        if !(inside || captured || event.kind == PointerEventKind::Leave) {
            return EventResult::ignored();
        }
        let moved = PointerEvent {
            position: self.translated(event.position),
            ..*event
        };
        self.forward_to_child(cx, &Event::Pointer(moved))
    }

    fn wheel(&mut self, cx: &mut EventCx, event: &WheelEvent) -> EventResult {
        if !self.bounds.contains(event.position) {
            return EventResult::ignored();
        }
        let moved = WheelEvent {
            position: self.translated(event.position),
            ..*event
        };
        let inner = self.forward_to_child(cx, &Event::Wheel(moved));
        if inner.handled {
            return inner;
        }
        let vertical = self.vertical_bar.wheel(cx, event);
        let horizontal = self.horizontal_bar.wheel(cx, event);
        if vertical || horizontal {
            self.scrolled = true;
            self.vertical_bar.take_changed();
            self.horizontal_bar.take_changed();
            cx.invalidate(self.bounds);
            EventResult::handled()
        } else {
            EventResult::ignored()
        }
    }

    fn child_nodes(&self, cx: &AccessCx, out: &mut AccessTree) {
        let child_id = self.child.id();
        let mut sub = AccessTree::new(child_id);
        self.child.access(&cx.for_widget(child_id), &mut sub);
        let offset = self.offset();
        for node in sub.nodes() {
            let mut node = node.clone();
            node.bounds = node.bounds.translate(-offset.x, -offset.y);
            node.state.offscreen |= !node.bounds.intersects(self.bounds);
            out.insert(node);
        }
    }
}

impl Control for ScrollView {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ScrollView {
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size {
        let child_id = self.child.id();
        let width = if self.horizontal {
            f32::INFINITY
        } else {
            available.width
        };
        let size = self.child.measure(
            &mut cx.for_widget(child_id),
            Size::new(width, f32::INFINITY),
        );
        Size::new(
            size.width.min(available.width),
            size.height.min(available.height),
        )
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let child_id = self.child.id();
        let width = if self.horizontal {
            f32::INFINITY
        } else {
            bounds.width
        };
        let size = self.child.measure(
            &mut cx.for_widget(child_id),
            Size::new(width, f32::INFINITY),
        );
        let content_width = if self.horizontal {
            size.width
        } else {
            bounds.width
        };
        self.content = Size::new(content_width, size.height);
        self.horizontal_bar
            .model_mut()
            .set_extent(bounds.width, content_width);
        self.vertical_bar
            .model_mut()
            .set_extent(bounds.height, size.height);
        let child_bounds = Rect::from_origin_size(
            bounds.origin(),
            Size::new(
                content_width.max(bounds.width),
                size.height.max(bounds.height),
            ),
        );
        self.child
            .arrange(&mut cx.for_widget(child_id), child_bounds);
        let corner = if self.horizontal_bar.model().is_scrollable() {
            BAR_WIDTH
        } else {
            0.0
        };
        let vertical_id = self.vertical_bar.id();
        let horizontal_id = self.horizontal_bar.id();
        self.vertical_bar.arrange(
            &mut cx.for_widget(vertical_id),
            Rect::new(
                bounds.right() - BAR_WIDTH,
                bounds.y,
                BAR_WIDTH,
                (bounds.height - corner).max(0.0),
            ),
        );
        self.horizontal_bar.arrange(
            &mut cx.for_widget(horizontal_id),
            Rect::new(
                bounds.x,
                bounds.bottom() - BAR_WIDTH,
                (bounds.width - BAR_WIDTH).max(0.0),
                BAR_WIDTH,
            ),
        );
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let offset = self.offset();
        canvas.push_clip_rect(self.bounds);
        canvas.save();
        canvas.translate(-offset.x, -offset.y);
        let mut child_cx = cx.for_widget(self.child.id());
        child_cx.damage = cx.damage.map(|damage| damage.translate(offset.x, offset.y));
        self.child.paint(&child_cx, canvas);
        canvas.restore();
        canvas.pop_clip();
        self.vertical_bar
            .paint(&cx.for_widget(self.vertical_bar.id()), canvas);
        self.horizontal_bar
            .paint(&cx.for_widget(self.horizontal_bar.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) => self.wheel(cx, event),
            Event::AnimationFrame(now) => {
                let vertical = self.vertical_bar.animate(cx, *now);
                let horizontal = self.horizontal_bar.animate(cx, *now);
                if vertical || horizontal {
                    cx.invalidate(self.bounds);
                }
                self.forward_to_child(cx, e)
            }
            _ => self.forward_to_child(cx, e),
        }
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        self.child_nodes(cx, out);
        let mut children = vec![self.child.id()];
        for bar in [&self.vertical_bar, &self.horizontal_bar] {
            if bar.model().is_scrollable() {
                children.push(bar.id());
                out.insert(bar.access_node());
            }
        }
        let mut node = AccessNode::new(self.id, Role::Pane, self.name.clone(), self.bounds)
            .with_children(children);
        node.patterns.scroll = Some(scroll_info(
            self.horizontal_bar.model(),
            self.vertical_bar.model(),
        ));
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::ScrollTo {
                horizontal,
                vertical,
            } => {
                if let Some(percent) = horizontal {
                    self.horizontal_bar.model_mut().jump_percent(*percent);
                }
                if let Some(percent) = vertical {
                    self.vertical_bar.model_mut().jump_percent(*percent);
                }
                self.scrolled = true;
                cx.invalidate(self.bounds);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::Modifiers;
    use crate::widgets::testkit::Harness;

    fn model(viewport: f32, content: f32) -> ScrollModel {
        let mut model = ScrollModel::new(Motion::reduced());
        model.set_extent(viewport, content);
        model
    }

    fn wheel(dy: f32, precise: bool, x: f32, y: f32) -> WheelEvent {
        WheelEvent {
            delta: Point::new(0.0, dy),
            precise,
            position: Point::new(x, y),
            mods: Modifiers::NONE,
        }
    }

    #[test]
    fn kit_scroll_model_clamps_and_reports_ratios() {
        let mut m = model(100.0, 400.0);
        assert_eq!(m.max_offset(), 300.0);
        assert!(m.is_scrollable());
        assert!(m.jump(1000.0));
        assert_eq!(m.offset(), 300.0);
        assert!(!m.jump(300.0));
        assert!(m.scroll_by(-50.0));
        assert_eq!(m.offset(), 250.0);
        assert!(m.jump(-5.0));
        assert_eq!(m.offset(), 0.0);
        assert_eq!(m.page(), 84.0);
        assert_eq!(model(10.0, 100.0).page(), 5.0);
        m.jump(150.0);
        assert_eq!(m.percent(), 50.0);
        assert_eq!(m.view_percent(), 25.0);
        assert!(m.jump_percent(100.0));
        assert_eq!(m.offset(), 300.0);
        m.jump_percent(250.0);
        assert_eq!(m.offset(), 300.0);
        // Shrinking the content pulls the offset back in range.
        m.set_extent(100.0, 150.0);
        assert_eq!(m.offset(), 50.0);
        let flat = model(100.0, 50.0);
        assert!(!flat.is_scrollable());
        assert_eq!((flat.percent(), flat.view_percent()), (0.0, 100.0));
        assert_eq!(model(0.0, 0.0).view_percent(), 100.0);
        assert_eq!((m.viewport(), m.content()), (100.0, 150.0));
    }

    #[test]
    fn kit_scroll_model_ensure_visible_scrolls_minimally() {
        let mut m = model(100.0, 1000.0);
        m.jump(200.0);
        assert!(!m.ensure_visible(220.0, 260.0));
        assert!(m.ensure_visible(150.0, 180.0));
        assert_eq!(m.offset(), 150.0);
        assert!(m.ensure_visible(400.0, 430.0));
        assert_eq!(m.offset(), 330.0);
    }

    #[test]
    fn kit_scroll_model_thumb_geometry() {
        let mut m = model(100.0, 400.0);
        assert_eq!(m.thumb(200.0), (0.0, 50.0));
        m.jump(300.0);
        assert_eq!(m.thumb(200.0), (150.0, 50.0));
        // Never shorter than the minimum.
        let tiny = model(10.0, 10_000.0);
        assert_eq!(tiny.thumb(200.0).1, 24.0);
        assert_eq!(tiny.thumb(10.0).1, 10.0);
        assert_eq!(model(100.0, 50.0).thumb(80.0), (0.0, 80.0));
        assert_eq!(m.thumb(-5.0), (0.0, 0.0));
        assert_eq!(m.offset_for_thumb(200.0, 75.0), 150.0);
        assert_eq!(m.offset_for_thumb(200.0, 5000.0), 300.0);
        assert_eq!(m.offset_for_thumb(200.0, -5.0), 0.0);
        // A track as short as the minimum thumb leaves 26 DIPs of travel for 300 of offset.
        assert!((model(100.0, 400.0).offset_for_thumb(50.0, 13.0) - 150.0).abs() < 1e-3);
        assert_eq!(model(100.0, 100.0).offset_for_thumb(50.0, 13.0), 0.0);
    }

    #[test]
    fn kit_scroll_model_eases_with_full_motion() {
        let mut m = ScrollModel::new(Motion::full());
        m.set_extent(100.0, 500.0);
        assert!(m.animate_by(100.0));
        assert!(m.is_animating());
        assert_eq!(m.target(), 100.0);
        assert_eq!(m.offset(), 0.0);
        // Repeated notches accumulate on the target.
        assert!(m.animate_by(100.0));
        assert_eq!(m.target(), 200.0);
        let (changed, more) = m.tick(Duration::from_millis(1000));
        assert!(changed && more);
        let (changed, more) = m.tick(Duration::from_millis(1300));
        assert!(changed && !more);
        assert_eq!(m.offset(), 200.0);
        let (changed, more) = m.tick(Duration::from_millis(1400));
        assert!(!changed && !more);
        // Reduced motion jumps at once and needs no frames.
        m.set_motion(Motion::reduced());
        assert!(!m.animate_to(50.0));
        assert_eq!(m.offset(), 50.0);
    }

    #[test]
    fn kit_wheel_deltas_by_axis_and_precision() {
        let notch = wheel(-1.0, false, 0.0, 0.0);
        assert_eq!(wheel_offset_delta(&notch, Axis::Vertical), 48.0);
        assert_eq!(wheel_offset_delta(&notch, Axis::Horizontal), 0.0);
        assert_eq!(
            wheel_offset_delta(&wheel(30.0, true, 0.0, 0.0), Axis::Vertical),
            -30.0
        );
        let sideways = WheelEvent {
            delta: Point::new(2.0, 0.0),
            ..notch
        };
        assert_eq!(wheel_offset_delta(&sideways, Axis::Horizontal), 96.0);
        let shifted = WheelEvent {
            mods: Modifiers::SHIFT,
            ..wheel(-1.0, false, 0.0, 0.0)
        };
        assert_eq!(wheel_offset_delta(&shifted, Axis::Horizontal), 48.0);
        assert_eq!(wheel_offset_delta(&shifted, Axis::Vertical), 0.0);
        let shifted_up = WheelEvent {
            delta: Point::new(0.0, 1.0),
            ..shifted
        };
        assert_eq!(wheel_offset_delta(&shifted_up, Axis::Horizontal), -48.0);
    }

    fn bar(axis: Axis, viewport: f32, content: f32) -> (Harness, ScrollBar, Rect) {
        let mut harness = Harness::new();
        let mut bar = ScrollBar::new(axis, "Vertical").with_motion(Motion::reduced());
        let bounds = match axis {
            Axis::Vertical => Rect::new(100.0, 0.0, BAR_WIDTH, viewport),
            Axis::Horizontal => Rect::new(0.0, 100.0, viewport, BAR_WIDTH),
        };
        bar.model_mut().set_extent(viewport, content);
        harness.arrange(&mut bar, bounds);
        (harness, bar, bounds)
    }

    fn down(x: f32, y: f32) -> (PointerEventKind, Option<PointerButton>, Point) {
        (
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(x, y),
        )
    }

    #[test]
    fn kit_scrollbar_measures_and_is_not_a_tab_stop() {
        let mut harness = Harness::new();
        let mut v = ScrollBar::new(Axis::Vertical, "v");
        let mut h = ScrollBar::new(Axis::Horizontal, "h");
        let id = v.id();
        let mut cx = LayoutCx::new(
            id,
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            v.measure(&mut cx, Size::new(500.0, 300.0)),
            Size::new(12.0, 300.0)
        );
        assert_eq!(
            h.measure(&mut cx, Size::new(500.0, 300.0)),
            Size::new(500.0, 12.0)
        );
        assert_eq!(v.measure(&mut cx, Size::UNBOUNDED), Size::new(12.0, 0.0));
        harness.arrange(&mut v, Rect::new(0.0, 0.0, 12.0, 100.0));
        assert!(harness.focus.tab_order().is_empty());
        assert_eq!(v.axis(), Axis::Vertical);
    }

    #[test]
    fn kit_scrollbar_thumb_drag_moves_offset_and_releases() {
        let (mut harness, mut bar, bounds) = bar(Axis::Vertical, 200.0, 800.0);
        assert_eq!(bar.thumb_rect().height, 50.0);
        let (kind, button, at) = down(bounds.x + 6.0, 20.0);
        assert!(harness.pointer(&mut bar, kind, button, at).handled);
        assert!(bar.is_dragging() && harness.capture.is_captured_by(bar.id()));
        // Drag 75 DIPs down: the free track is 150, the max offset 600.
        harness.pointer(
            &mut bar,
            PointerEventKind::Move,
            None,
            Point::new(bounds.x + 6.0, 95.0),
        );
        assert_eq!(bar.offset(), 300.0);
        assert!(bar.take_changed());
        assert!(!bar.take_changed());
        // Dragging past the end clamps.
        harness.pointer(
            &mut bar,
            PointerEventKind::Move,
            None,
            Point::new(bounds.x + 6.0, 900.0),
        );
        assert_eq!(bar.offset(), 600.0);
        assert!(bar.take_changed());
        // Dragging further past the end changes nothing.
        assert!(
            harness
                .pointer(
                    &mut bar,
                    PointerEventKind::Move,
                    None,
                    Point::new(bounds.x + 6.0, 950.0),
                )
                .handled
        );
        assert_eq!(bar.offset(), 600.0);
        assert!(!bar.take_changed());
        harness.pointer(
            &mut bar,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(bounds.x + 6.0, 900.0),
        );
        assert!(!bar.is_dragging());
        assert!(!harness.capture.is_captured_by(bar.id()));
        assert!(!bar.is_hovered());
    }

    #[test]
    fn kit_scrollbar_track_click_pages_toward_the_click() {
        let (mut harness, mut bar, bounds) = bar(Axis::Vertical, 200.0, 800.0);
        let (kind, button, _) = down(0.0, 0.0);
        harness.pointer(&mut bar, kind, button, Point::new(bounds.x + 6.0, 150.0));
        assert_eq!(bar.offset(), 184.0);
        assert!(bar.take_changed());
        harness.pointer(
            &mut bar,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(bounds.x + 6.0, 150.0),
        );
        // Thumb now starts at 184 / 600 * 150 = 46; clicking above it pages back.
        harness.pointer(&mut bar, kind, button, Point::new(bounds.x + 6.0, 5.0));
        assert_eq!(bar.offset(), 0.0);
        assert!(!bar.is_dragging());
    }

    #[test]
    fn kit_scrollbar_horizontal_drag_and_hover_expansion() {
        let mut harness = Harness::new();
        let mut bar = ScrollBar::new(Axis::Horizontal, "h");
        bar.model_mut().set_extent(100.0, 400.0);
        harness.arrange(&mut bar, Rect::new(0.0, 50.0, 100.0, BAR_WIDTH));
        assert_eq!(
            bar.thumb_rect(),
            Rect::new(0.0, 50.0 + 12.0 - 2.0 - 2.0, 25.0, 2.0)
        );
        harness.hover(&mut bar, Point::new(10.0, 55.0));
        assert!(bar.is_hovered());
        harness.run_animations(&mut bar);
        assert_eq!(bar.thumb_rect().height, 6.0);
        let canvas = harness.paint(&bar);
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRect { rect, .. } if *rect == Rect::new(0.0, 50.0, 100.0, 12.0))));
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { color, radii, .. }
            if *color == tokens.scrollbar && *radii == CornerRadii::uniform(3.0))
        ));
        let (kind, button, _) = down(0.0, 0.0);
        harness.pointer(&mut bar, kind, button, Point::new(10.0, 55.0));
        harness.pointer(
            &mut bar,
            PointerEventKind::Move,
            None,
            Point::new(40.0, 55.0),
        );
        assert_eq!(bar.offset(), 120.0);
        harness.pointer(&mut bar, PointerEventKind::Leave, None, Point::ZERO);
        assert!(bar.is_hovered(), "leave is ignored during a drag");
        harness.pointer(
            &mut bar,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            Point::new(500.0, 500.0),
        );
        assert!(!bar.is_hovered());
        // Hover ends on leave.
        harness.hover(&mut bar, Point::new(10.0, 55.0));
        assert!(
            harness
                .pointer(&mut bar, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        assert!(
            !harness
                .pointer(&mut bar, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
    }

    #[test]
    fn kit_scrollbar_ignores_input_when_nothing_scrolls_or_wrong_button() {
        let (mut harness, mut bar, bounds) = bar(Axis::Vertical, 200.0, 100.0);
        let (kind, button, at) = down(bounds.x + 2.0, 10.0);
        assert!(!harness.pointer(&mut bar, kind, button, at).handled);
        assert!(!harness.hover(&mut bar, at).handled);
        assert!(
            !harness
                .send(
                    &mut bar,
                    Event::Wheel(wheel(-1.0, false, bounds.x + 2.0, 10.0))
                )
                .handled
        );
        assert!(harness.paint(&bar).recorded().is_empty());
        assert!(harness.node(&bar).state.offscreen);
        let (mut harness, mut bar, bounds) = self::bar(Axis::Vertical, 200.0, 800.0);
        assert!(
            !harness
                .pointer(
                    &mut bar,
                    PointerEventKind::Down,
                    Some(PointerButton::Right),
                    Point::new(bounds.x + 2.0, 10.0)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut bar,
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    Point::new(bounds.x + 2.0, 10.0)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut bar,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(5.0, 10.0)
                )
                .handled
        );
        assert!(!harness.key(&mut bar, 0x28).handled);
    }

    #[test]
    fn kit_scrollbar_wheel_notches_ease_and_precise_jumps() {
        let mut harness = Harness::new();
        let mut bar = ScrollBar::new(Axis::Vertical, "v");
        bar.model_mut().set_extent(200.0, 800.0);
        harness.arrange(&mut bar, Rect::new(100.0, 0.0, 12.0, 200.0));
        let over = Point::new(105.0, 50.0);
        let notch = |dy| {
            Event::Wheel(WheelEvent {
                position: over,
                ..wheel(dy, false, 0.0, 0.0)
            })
        };
        assert!(harness.send(&mut bar, notch(-1.0)).handled);
        assert_eq!(bar.model().target(), 48.0);
        assert_eq!(bar.offset(), 0.0);
        assert!(harness.requests.animation_frame_requested());
        harness.run_animations(&mut bar);
        assert_eq!(bar.offset(), 48.0);
        assert!(bar.take_changed());
        let precise = Event::Wheel(WheelEvent {
            position: over,
            ..wheel(10.0, true, 0.0, 0.0)
        });
        assert!(harness.send(&mut bar, precise).handled);
        assert_eq!(bar.offset(), 38.0);
        // At the start, scrolling further up does nothing.
        bar.model_mut().jump(0.0);
        assert!(!harness.send(&mut bar, notch(1.0)).handled);
        // Wheel events away from the bar are not its business.
        let away = Event::Wheel(WheelEvent {
            position: Point::new(5.0, 5.0),
            ..wheel(-1.0, false, 0.0, 0.0)
        });
        assert!(!harness.send(&mut bar, away).handled);
    }

    #[test]
    fn kit_scrollbar_access_node_and_actions() {
        let (mut harness, mut bar, bounds) = bar(Axis::Vertical, 200.0, 800.0);
        let node = harness.node(&bar);
        assert_eq!(node.role, Role::ScrollBar);
        assert_eq!(node.name, "Vertical");
        assert_eq!(node.bounds, bounds);
        assert!(!node.state.focusable);
        let range = node.patterns.range_value.expect("range value");
        assert_eq!(
            (range.value, range.min, range.max, range.step),
            (0.0, 0.0, 600.0, 48.0)
        );
        assert!(harness.perform(&mut bar, AccessAction::SetRangeValue(250.0)));
        assert_eq!(bar.offset(), 250.0);
        assert!(bar.take_changed());
        assert!(harness.perform(
            &mut bar,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(100.0)
            }
        ));
        assert_eq!(bar.offset(), 600.0);
        // The other axis' scroll request is not for this bar.
        assert!(!harness.perform(
            &mut bar,
            AccessAction::ScrollTo {
                horizontal: Some(0.0),
                vertical: None
            }
        ));
        assert!(!harness.perform(&mut bar, AccessAction::Invoke));
        let (mut harness, mut h, _) = self::bar(Axis::Horizontal, 200.0, 800.0);
        assert!(harness.perform(
            &mut h,
            AccessAction::ScrollTo {
                horizontal: Some(50.0),
                vertical: None
            }
        ));
        assert_eq!(h.offset(), 300.0);
        assert!(!harness.perform(
            &mut h,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(5.0)
            }
        ));
    }

    /// What the content block saw, shared with the test that owns the view.
    #[derive(Debug, Default)]
    struct BlockState {
        bounds: Rect,
        /// The damage rectangle of the last paint, in the block's own coordinates.
        damage: Option<Rect>,
        clicks: Vec<Point>,
        eats_wheel: bool,
        keys: u32,
    }

    type Shared = Rc<RefCell<BlockState>>;

    /// A fixed-size content block that records the input it receives. Its access node has one
    /// child, an item 450 DIPs below the block's top.
    struct Block {
        id: WidgetId,
        item: WidgetId,
        size: Size,
        state: Shared,
    }

    impl Block {
        fn new(width: f32, height: f32) -> (Self, Shared) {
            let state = Shared::default();
            let block = Self {
                id: WidgetId::allocate(),
                item: WidgetId::allocate(),
                size: Size::new(width, height),
                state: Rc::clone(&state),
            };
            (block, state)
        }
    }

    impl Control for Block {
        fn id(&self) -> WidgetId {
            self.id
        }
    }

    impl Widget for Block {
        fn measure(&mut self, _cx: &mut LayoutCx, _available: Size) -> Size {
            self.size
        }
        fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
            self.state.borrow_mut().bounds = bounds;
            cx.add_tab_stop();
        }
        fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
            let mut state = self.state.borrow_mut();
            state.damage = cx.damage;
            canvas.fill_rect(state.bounds, cx.theme.tokens.accent);
        }
        fn event(&mut self, _cx: &mut EventCx, e: &Event) -> EventResult {
            let mut state = self.state.borrow_mut();
            match e {
                Event::Pointer(p)
                    if p.kind == PointerEventKind::Down && state.bounds.contains(p.position) =>
                {
                    state.clicks.push(p.position);
                    EventResult::handled()
                }
                Event::Wheel(_) if state.eats_wheel => EventResult::handled(),
                Event::Key(_) => {
                    state.keys += 1;
                    EventResult::handled()
                }
                _ => EventResult::ignored(),
            }
        }
        fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
            let bounds = self.state.borrow().bounds;
            out.insert(
                AccessNode::new(self.id, Role::Group, "block", bounds)
                    .with_state(crate::widgets::control_state(cx, true))
                    .with_children([self.item]),
            );
            out.insert(AccessNode::new(
                self.item,
                Role::Button,
                "far",
                Rect::new(bounds.x, bounds.y + 450.0, bounds.width, 20.0),
            ));
        }
        fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
            false
        }
    }

    fn view(width: f32, height: f32, viewport: Rect) -> (Harness, ScrollView, Shared) {
        let mut harness = Harness::new();
        let (block, state) = Block::new(width, height);
        let mut view = ScrollView::new(Box::new(block), "Content").with_motion(Motion::reduced());
        harness.arrange(&mut view, viewport);
        (harness, view, state)
    }

    #[test]
    fn kit_scrollview_sizes_content_and_bars() {
        let viewport = Rect::new(10.0, 20.0, 200.0, 100.0);
        let (harness, view, state) = view(300.0, 500.0, viewport);
        // Without horizontal scrolling the child is fitted to the width.
        assert_eq!(view.content_size(), Size::new(200.0, 500.0));
        assert!(view.vertical_bar().model().is_scrollable());
        assert!(!view.horizontal_bar().model().is_scrollable());
        assert_eq!(
            view.vertical_bar().bounds(),
            Rect::new(198.0, 20.0, 12.0, 100.0)
        );
        assert_eq!(state.borrow().bounds, Rect::new(10.0, 20.0, 200.0, 500.0));
        assert_eq!(harness.focus.tab_order().len(), 1);
        assert_eq!(view.offset(), Point::ZERO);
        assert_eq!(view.bounds(), viewport);
    }

    #[test]
    fn kit_scrollview_horizontal_view_has_both_bars() {
        let mut harness = Harness::new();
        let (block, _) = Block::new(300.0, 500.0);
        let mut view = ScrollView::new(Box::new(block), "c")
            .horizontal()
            .with_motion(Motion::reduced());
        let viewport = Rect::new(0.0, 0.0, 200.0, 100.0);
        let measured = {
            let id = view.id();
            let mut cx = LayoutCx::new(
                id,
                &harness.theme,
                1.0,
                &harness.engine,
                &mut harness.focus,
                &mut harness.requests,
            );
            view.measure(&mut cx, Size::new(250.0, 400.0))
        };
        assert_eq!(measured, Size::new(250.0, 400.0));
        harness.arrange(&mut view, viewport);
        assert_eq!(view.content_size(), Size::new(300.0, 500.0));
        assert!(view.horizontal_bar().model().is_scrollable());
        // The vertical bar stops above the corner shared with the horizontal bar.
        assert_eq!(view.vertical_bar().bounds().height, 88.0);
        assert_eq!(
            view.horizontal_bar().bounds(),
            Rect::new(0.0, 88.0, 188.0, 12.0)
        );
        view.scroll_to(Point::new(40.0, 70.0));
        assert_eq!(view.offset(), Point::new(40.0, 70.0));
        view.scroll_to(Point::new(1000.0, 1000.0));
        assert_eq!(view.offset(), Point::new(100.0, 400.0));
    }

    #[test]
    fn kit_scrollview_translates_pointer_input_by_the_offset() {
        let viewport = Rect::new(10.0, 20.0, 200.0, 100.0);
        let (mut harness, mut view, state) = view(200.0, 500.0, viewport);
        view.scroll_to(Point::new(0.0, 150.0));
        let (kind, button, _) = down(0.0, 0.0);
        harness.pointer(&mut view, kind, button, Point::new(50.0, 40.0));
        assert_eq!(state.borrow().clicks, vec![Point::new(50.0, 190.0)]);
        // Presses outside the viewport never reach hidden content.
        harness.pointer(&mut view, kind, button, Point::new(50.0, 500.0));
        assert_eq!(state.borrow().clicks.len(), 1);
        // Presses on the vertical bar belong to the bar.
        let on_bar = Point::new(205.0, 30.0);
        assert!(harness.pointer(&mut view, kind, button, on_bar).handled);
        assert_eq!(state.borrow().clicks.len(), 1);
        assert!(view.take_scrolled());
        assert!(!view.take_scrolled());
    }

    #[test]
    fn kit_scrollview_wheel_scrolls_unless_the_child_consumes_it() {
        let viewport = Rect::new(0.0, 0.0, 200.0, 100.0);
        let (mut harness, mut view, state) = view(200.0, 500.0, viewport);
        let over = Point::new(50.0, 50.0);
        let notch = Event::Wheel(WheelEvent {
            position: over,
            ..wheel(-2.0, true, 0.0, 0.0)
        });
        assert!(harness.send(&mut view, notch.clone()).handled);
        assert_eq!(view.offset().y, 2.0);
        assert!(view.take_scrolled());
        // Outside the view nothing happens.
        let away = Event::Wheel(WheelEvent {
            position: Point::new(500.0, 50.0),
            ..wheel(-2.0, true, 0.0, 0.0)
        });
        assert!(!harness.send(&mut view, away).handled);
        // An inner scroller that handles the wheel keeps the outer view still.
        state.borrow_mut().eats_wheel = true;
        assert!(harness.send(&mut view, notch).handled);
        assert_eq!(view.offset().y, 2.0);
        // Nothing scrolls when the content fits.
        let (mut harness, mut fit, _) = self::view(200.0, 50.0, viewport);
        assert!(
            !harness
                .send(
                    &mut fit,
                    Event::Wheel(WheelEvent {
                        position: over,
                        ..wheel(-1.0, true, 0.0, 0.0)
                    })
                )
                .handled
        );
    }

    #[test]
    fn kit_scrollview_forwards_keys_and_animates() {
        let viewport = Rect::new(0.0, 0.0, 200.0, 100.0);
        let mut harness = Harness::new();
        let (block, state) = Block::new(200.0, 500.0);
        let mut view = ScrollView::new(Box::new(block), "c");
        harness.arrange(&mut view, viewport);
        assert!(harness.key(&mut view, 0x41).handled);
        assert_eq!(state.borrow().keys, 1);
        assert!(view.scroll_to_smooth(Point::new(0.0, 100.0)));
        // The owner asks for the frames an eased scroll needs.
        harness.requests.request_animation_frame();
        harness.requests.take_invalidation();
        harness.run_animations(&mut view);
        assert_eq!(view.offset().y, 100.0);
        assert!(!harness.requests.invalidation().is_clean());
        // A frame while neither bar moves repaints nothing.
        harness.requests.take_invalidation();
        harness.frame(&mut view, 16);
        assert!(harness.requests.invalidation().is_clean());
        // Showing a rectangle scrolls just enough.
        assert!(view.ensure_visible(Rect::new(0.0, 250.0, 50.0, 20.0)));
        assert_eq!(view.offset().y, 170.0);
        assert!(!view.ensure_visible(Rect::new(0.0, 200.0, 50.0, 20.0)));
        view.scroll_to(Point::ZERO);
        assert!(!view.scroll_to_smooth(Point::ZERO));
    }

    #[test]
    fn kit_scrollview_paints_clipped_and_translated() {
        let viewport = Rect::new(10.0, 20.0, 200.0, 100.0);
        let (mut harness, mut view, state) = view(200.0, 500.0, viewport);
        view.scroll_to(Point::new(0.0, 100.0));
        let canvas = harness.paint(&view);
        assert_eq!(state.borrow().damage, None, "a full paint has no damage");
        let ops: Vec<&DrawOp> = canvas.ops().collect();
        assert_eq!(*ops[0], DrawOp::PushClipRect(viewport));
        assert_eq!(*ops[1], DrawOp::Save);
        assert!(matches!(ops[2], DrawOp::Transform(t) if t.dy == -100.0 && t.dx == 0.0));
        assert!(
            matches!(ops[3], DrawOp::FillRect { rect, .. } if *rect == Rect::new(10.0, 20.0, 200.0, 500.0))
        );
        // The thumb sits at the end of the track after scrolling.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == harness.theme.tokens.scrollbar)));
        // Damage outside the view paints nothing; damage inside is translated for the child.
        let mut empty = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            view.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(500.0, 500.0, 5.0, 5.0)),
        );
        view.paint(&cx, &mut empty);
        assert!(empty.recorded().is_empty());
        let mut partial = crate::canvas::RecordingCanvas::new();
        let cx = PaintCx::new(
            view.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &harness.focus,
            &harness.capture,
            Some(Rect::new(20.0, 30.0, 5.0, 5.0)),
        );
        view.paint(&cx, &mut partial);
        assert!(
            partial
                .ops()
                .any(|op| matches!(op, DrawOp::FillRect { .. }))
        );
        // The child sees the damage in its own coordinates: scrolled by the offset.
        assert_eq!(
            state.borrow().damage,
            Some(Rect::new(20.0, 130.0, 5.0, 5.0))
        );
    }

    #[test]
    fn kit_scrollview_access_reports_window_coordinates_and_scroll_pattern() {
        let viewport = Rect::new(10.0, 20.0, 200.0, 100.0);
        let (mut harness, mut view, _) = view(200.0, 500.0, viewport);
        view.scroll_to(Point::new(0.0, 200.0));
        let tree = harness.tree(&view);
        let root = tree.root().expect("root");
        assert_eq!(root.role, Role::Pane);
        assert_eq!(root.name, "Content");
        let info = root.patterns.scroll.expect("scroll pattern");
        assert!(info.vertically_scrollable && !info.horizontally_scrollable);
        assert_eq!(info.vertical_percent, 50.0);
        assert!((info.vertical_view_percent - 20.0).abs() < 1e-3);
        assert_eq!(root.children.len(), 2);
        let content = tree
            .children(view.id())
            .find(|n| n.role == Role::Group)
            .expect("content");
        assert_eq!(content.bounds, Rect::new(10.0, -180.0, 200.0, 500.0));
        assert!(!content.state.offscreen);
        // The item 450 DIPs down is below the 100 DIP viewport until the view scrolls to it.
        let item_id = content.children[0];
        let item = tree.node(item_id).expect("item");
        assert_eq!(item.bounds.y, 270.0);
        assert!(item.state.offscreen);
        let bar = tree
            .children(view.id())
            .find(|n| n.role == Role::ScrollBar)
            .expect("bar");
        assert_eq!(bar.patterns.range_value.map(|r| r.value), Some(200.0));
        // Scrolling through assistive technology.
        assert!(harness.perform(
            &mut view,
            AccessAction::ScrollTo {
                horizontal: Some(10.0),
                vertical: Some(0.0)
            }
        ));
        assert_eq!(view.offset(), Point::ZERO);
        assert!(view.take_scrolled());
        // Each axis can be addressed on its own; the content is as wide as the view, so the
        // horizontal share has nothing to scroll.
        assert!(harness.perform(
            &mut view,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(50.0)
            }
        ));
        assert_eq!(view.offset(), Point::new(0.0, 200.0));
        assert!(harness.perform(
            &mut view,
            AccessAction::ScrollTo {
                horizontal: Some(50.0),
                vertical: None
            }
        ));
        assert_eq!(view.offset(), Point::new(0.0, 200.0));
        assert!(!harness.perform(&mut view, AccessAction::Invoke));
        view.scroll_to(Point::new(0.0, 400.0));
        let tree = harness.tree(&view);
        let content = tree
            .children(view.id())
            .find(|n| n.role == Role::Group)
            .expect("content");
        let item = tree.node(content.children[0]).expect("item");
        assert_eq!(item.bounds.y, 70.0);
        assert!(!item.state.offscreen);
    }

    #[test]
    fn kit_scrollview_debug_names_the_view() {
        let (_, view, _) = view(10.0, 10.0, Rect::new(0.0, 0.0, 10.0, 10.0));
        assert!(format!("{view:?}").starts_with("ScrollView"));
    }

    #[test]
    fn kit_scrollbar_with_id_and_animated_paging() {
        let id = WidgetId::allocate();
        let mut harness = Harness::new();
        let mut bar = ScrollBar::new(Axis::Vertical, "Scroll")
            .with_id(id)
            .with_motion(Motion::full());
        assert_eq!(bar.id(), id);
        assert_eq!(harness.node(&bar).id, id);
        bar.model_mut().set_extent(100.0, 1000.0);
        harness.arrange(&mut bar, Rect::new(0.0, 0.0, BAR_WIDTH, 100.0));
        // A press on the track below the thumb pages down by easing.
        let below = Point::new(BAR_WIDTH / 2.0, 90.0);
        let left = Some(PointerButton::Left);
        assert!(
            harness
                .pointer(&mut bar, PointerEventKind::Down, left, below)
                .handled
        );
        assert!(harness.requests.animation_frame_requested());
    }

    #[test]
    fn kit_scrollview_with_id_exposes_its_child_and_measures_the_width() {
        let id = WidgetId::allocate();
        let viewport = Rect::new(10.0, 20.0, 200.0, 100.0);
        let (mut harness, view, _) = view(300.0, 500.0, viewport);
        let mut view = view.with_id(id);
        assert_eq!(view.id(), id);
        let child = view.child().id();
        let mut cx = harness.event_cx(child);
        assert_eq!(view.child_mut().id(), child);
        assert!(!view.child_mut().perform(&mut cx, &AccessAction::Invoke));
        let measured = harness.measure(&mut view, Size::new(200.0, 100.0));
        assert_eq!(measured.width, 200.0);
    }

    #[test]
    fn kit_scrollview_pointer_over_the_horizontal_bar_belongs_to_the_bar() {
        let mut harness = Harness::new();
        let (block, _) = Block::new(600.0, 500.0);
        let mut view = ScrollView::new(Box::new(block), "c")
            .horizontal()
            .with_motion(Motion::reduced());
        harness.arrange(&mut view, Rect::new(10.0, 20.0, 200.0, 100.0));
        assert!(view.horizontal_bar().model().is_scrollable());
        let on_bar = view.horizontal_bar().bounds().center();
        assert!(view.bar_contains(on_bar));
        assert!(!view.bar_contains(Point::new(11.0, 21.0)));
    }
}
