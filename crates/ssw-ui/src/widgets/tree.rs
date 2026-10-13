//! A tree view: lazy expansion, chevrons, 16 DIP indentation, keyboard expand/collapse, a rename
//! hook (F2) and drag source / drop target regions.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, IconFont};
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{
    Cursor, Event, FocusEvent, KeyEvent, PointerButton, PointerEvent, PointerEventKind, vk,
};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};

use super::anim::Motion;
use super::scroll::{Axis, BAR_WIDTH, ScrollBar, scroll_info};
use super::{
    CONTROL_HEIGHT, Control, FONT_SIZE, TypeAhead, control_state, draw_left_centered, focus_ring,
    glyph, layout_line, perform_focus,
};

/// Height of one row.
pub const ROW_HEIGHT: f32 = CONTROL_HEIGHT;
/// Indentation per depth level.
pub const INDENT: f32 = 16.0;
/// Width of the chevron zone.
const CHEVRON: f32 = 16.0;
/// Em size of the chevron glyph.
const CHEVRON_SIZE: f32 = 12.0;
/// Gap between the chevron zone and the label.
const LABEL_GAP: f32 = 8.0;
/// Movement with the button held that turns a press into a drag.
pub const DRAG_THRESHOLD: f32 = 4.0;
/// Tallest tree that asks for its natural height when the parent does not constrain it.
const MAX_NATURAL_HEIGHT: f32 = 320.0;

/// A node of the tree. `key` identifies it to the owner.
#[derive(Debug, Clone)]
pub struct TreeNode {
    key: u64,
    id: WidgetId,
    label: String,
    expandable: bool,
    /// `None` for an expandable node whose children have not been supplied yet.
    children: Option<Vec<TreeNode>>,
    expanded: bool,
    dimmed: bool,
}

impl TreeNode {
    /// A node without children.
    #[must_use]
    pub fn leaf(key: u64, label: impl Into<String>) -> Self {
        Self {
            key,
            id: WidgetId::allocate(),
            label: label.into(),
            expandable: false,
            children: Some(Vec::new()),
            expanded: false,
            dimmed: false,
        }
    }

    /// A node whose `children` are already known.
    #[must_use]
    pub fn branch(key: u64, label: impl Into<String>, children: Vec<TreeNode>) -> Self {
        Self {
            expandable: true,
            children: Some(children),
            ..Self::leaf(key, label)
        }
    }

    /// An expandable node whose children are supplied on demand: expanding it queues
    /// [`TreeEvent::ExpandRequested`] and the owner answers with [`TreeView::set_children`].
    #[must_use]
    pub fn lazy(key: u64, label: impl Into<String>) -> Self {
        Self {
            expandable: true,
            children: None,
            ..Self::leaf(key, label)
        }
    }

    /// Starts expanded.
    #[must_use]
    pub fn expanded(mut self) -> Self {
        self.expanded = self.expandable;
        self
    }

    /// Draws the label dimmed (a hidden or cut item).
    #[must_use]
    pub fn dimmed(mut self, dimmed: bool) -> Self {
        self.dimmed = dimmed;
        self
    }

    #[must_use]
    pub fn key(&self) -> u64 {
        self.key
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    fn contains(&self, key: u64) -> bool {
        self.key == key
            || self
                .children
                .as_ref()
                .is_some_and(|children| children.iter().any(|child| child.contains(key)))
    }
}

fn find(nodes: &[TreeNode], key: u64) -> Option<&TreeNode> {
    nodes.iter().find_map(|node| {
        if node.key == key {
            Some(node)
        } else {
            node.children
                .as_deref()
                .and_then(|children| find(children, key))
        }
    })
}

fn find_mut(nodes: &mut [TreeNode], key: u64) -> Option<&mut TreeNode> {
    nodes.iter_mut().find_map(|node| {
        if node.key == key {
            Some(node)
        } else {
            node.children
                .as_deref_mut()
                .and_then(|children| find_mut(children, key))
        }
    })
}

/// The key of the parent of `key`.
fn parent_of(nodes: &[TreeNode], key: u64) -> Option<u64> {
    nodes.iter().find_map(|node| {
        let children = node.children.as_deref()?;
        if children.iter().any(|child| child.key == key) {
            Some(node.key)
        } else {
            parent_of(children, key)
        }
    })
}

/// Where a dragged node would land relative to the node under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropPosition {
    Before,
    Inside,
    After,
}

/// What happened to the tree since the owner last looked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEvent {
    /// The user selected this node.
    Selected(u64),
    /// Enter or a double click.
    Activated(u64),
    Expanded(u64),
    Collapsed(u64),
    /// An expandable node without children was expanded; answer with `set_children`.
    ExpandRequested(u64),
    /// F2 on the selected node.
    RenameRequested(u64),
    /// A press moved past [`DRAG_THRESHOLD`]: this node is being dragged.
    DragStarted(u64),
    /// The drag ended over a valid target.
    Dropped {
        source: u64,
        target: u64,
        position: DropPosition,
    },
    /// The drag ended without a valid target (or with Esc).
    DragCancelled(u64),
}

#[derive(Debug, Clone)]
struct Row {
    key: u64,
    id: WidgetId,
    depth: usize,
    label: String,
    expandable: bool,
    expanded: bool,
    dimmed: bool,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    source: u64,
    target: Option<(u64, DropPosition)>,
}

/// A tree of text nodes. Only the rows in view are painted and described to accessibility.
#[derive(Debug, Clone)]
pub struct TreeView {
    id: WidgetId,
    label: String,
    roots: Vec<TreeNode>,
    rows: Vec<Row>,
    selected: Option<u64>,
    hover: Option<usize>,
    enabled: bool,
    bounds: Rect,
    bar: ScrollBar,
    type_ahead: TypeAhead,
    events: Vec<TreeEvent>,
    /// The node pressed with the left button and where, until it is released.
    press: Option<(Point, u64)>,
    drag: Option<Drag>,
}

impl TreeView {
    /// A tree of `roots`; `label` is the accessible name.
    #[must_use]
    pub fn new(label: impl Into<String>, roots: Vec<TreeNode>) -> Self {
        let mut tree = Self {
            id: WidgetId::allocate(),
            label: label.into(),
            roots,
            rows: Vec::new(),
            selected: None,
            hover: None,
            enabled: true,
            bounds: Rect::ZERO,
            bar: ScrollBar::new(Axis::Vertical, ""),
            type_ahead: TypeAhead::new(),
            events: Vec::new(),
            press: None,
            drag: None,
        };
        tree.rebuild();
        tree
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
    pub fn selected_key(&self) -> Option<u64> {
        self.selected
    }

    /// Selects `key` (expanding nothing) without notifying the owner. Unknown or hidden keys
    /// clear the selection.
    pub fn select_key(&mut self, key: u64) {
        self.selected = self.rows.iter().any(|row| row.key == key).then_some(key);
        self.reveal_selection();
    }

    /// Whether `key` is expanded; `None` when it is not an expandable node.
    #[must_use]
    pub fn is_expanded(&self, key: u64) -> Option<bool> {
        find(&self.roots, key)
            .filter(|node| node.expandable)
            .map(|node| node.expanded)
    }

    /// Expands or collapses `key` without notifying the owner.
    pub fn set_expanded(&mut self, key: u64, expanded: bool) {
        self.apply_expanded(key, expanded);
    }

    /// Supplies the children of a lazy node.
    pub fn set_children(&mut self, key: u64, children: Vec<TreeNode>) {
        if let Some(node) = find_mut(&mut self.roots, key) {
            node.children = Some(children);
            node.expandable = true;
        }
        self.rebuild();
    }

    /// Marks a node dimmed or not.
    pub fn set_dimmed(&mut self, key: u64, dimmed: bool) {
        if let Some(node) = find_mut(&mut self.roots, key) {
            node.dimmed = dimmed;
        }
        self.rebuild();
    }

    /// Renames a node (the owner's answer to [`TreeEvent::RenameRequested`]).
    pub fn set_label(&mut self, key: u64, label: impl Into<String>) {
        if let Some(node) = find_mut(&mut self.roots, key) {
            node.label = label.into();
        }
        self.rebuild();
    }

    /// Number of visible rows.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The key of visible row `index`.
    #[must_use]
    pub fn row_key(&self, index: usize) -> Option<u64> {
        self.rows.get(index).map(|row| row.key)
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    #[must_use]
    pub fn scroll_offset(&self) -> f32 {
        self.bar.offset()
    }

    /// The node being dragged, if a drag is in progress.
    #[must_use]
    pub fn dragging(&self) -> Option<u64> {
        self.drag.map(|drag| drag.source)
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<TreeEvent> {
        std::mem::take(&mut self.events)
    }

    /// Bounds of visible row `index` in window coordinates.
    #[must_use]
    pub fn row_rect(&self, index: usize) -> Rect {
        Rect::new(
            self.bounds.x,
            self.bounds.y + index as f32 * ROW_HEIGHT - self.bar.offset(),
            self.bounds.width,
            ROW_HEIGHT,
        )
    }

    fn chevron_rect(&self, index: usize) -> Rect {
        let row = self.row_rect(index);
        let depth = self.rows[index].depth;
        Rect::new(
            row.x + 4.0 + depth as f32 * INDENT,
            row.y,
            CHEVRON,
            row.height,
        )
    }

    fn label_x(&self, index: usize) -> f32 {
        let chevron = self.chevron_rect(index);
        chevron.right() + LABEL_GAP
    }

    /// The rows intersecting the viewport as `first..end`.
    #[must_use]
    pub fn visible_range(&self) -> std::ops::Range<usize> {
        let offset = self.bar.offset();
        let first = (offset / ROW_HEIGHT).floor() as usize;
        let end = ((offset + self.bounds.height) / ROW_HEIGHT).ceil() as usize;
        first.min(self.rows.len())..end.min(self.rows.len())
    }

    fn row_at(&self, point: Point) -> Option<usize> {
        if !self.bounds.contains(point) {
            return None;
        }
        let y = point.y - self.bounds.y + self.bar.offset();
        let index = (y / ROW_HEIGHT) as usize;
        (index < self.rows.len()).then_some(index)
    }

    /// The drop target at `point`: the node under it and where a dropped node would land. This is
    /// also what an external drag (files from Explorer) uses.
    #[must_use]
    pub fn drop_target_at(&self, point: Point) -> Option<(u64, DropPosition)> {
        let index = self.row_at(point)?;
        let row = self.row_rect(index);
        let fraction = (point.y - row.y) / row.height;
        let position = if fraction < 0.25 {
            DropPosition::Before
        } else if fraction > 0.75 {
            DropPosition::After
        } else {
            DropPosition::Inside
        };
        Some((self.rows[index].key, position))
    }

    fn rebuild(&mut self) {
        fn walk(nodes: &[TreeNode], depth: usize, out: &mut Vec<Row>) {
            for node in nodes {
                out.push(Row {
                    key: node.key,
                    id: node.id,
                    depth,
                    label: node.label.clone(),
                    expandable: node.expandable,
                    expanded: node.expandable && node.expanded,
                    dimmed: node.dimmed,
                });
                if let Some(children) = node.children.as_deref().filter(|_| node.expanded) {
                    walk(children, depth + 1, out);
                }
            }
        }
        self.rows.clear();
        walk(&self.roots, 0, &mut self.rows);
        let content = self.rows.len() as f32 * ROW_HEIGHT;
        self.bar.model_mut().set_extent(self.bounds.height, content);
        self.hover = None;
    }

    fn row_of(&self, key: u64) -> Option<usize> {
        self.rows.iter().position(|row| row.key == key)
    }

    fn reveal_selection(&mut self) {
        if let Some(index) = self.selected.and_then(|key| self.row_of(key)) {
            let top = index as f32 * ROW_HEIGHT;
            self.bar.model_mut().ensure_visible(top, top + ROW_HEIGHT);
        }
    }

    /// Changes the expanded flag of `key`. Returns whether it changed.
    fn apply_expanded(&mut self, key: u64, expanded: bool) -> bool {
        let Some(node) = find_mut(&mut self.roots, key) else {
            return false;
        };
        if !node.expandable || node.expanded == expanded {
            return false;
        }
        node.expanded = expanded;
        self.rebuild();
        if !expanded
            && self
                .selected
                .is_some_and(|selected| self.row_of(selected).is_none())
        {
            // Collapsing hides the selection: it moves up to the collapsed node.
            self.selected = Some(key);
            self.events.push(TreeEvent::Selected(key));
        }
        true
    }

    /// User-initiated expand or collapse: also queues the events.
    fn user_expand(&mut self, key: u64, expanded: bool) {
        if !self.apply_expanded(key, expanded) {
            return;
        }
        if expanded {
            self.events.push(TreeEvent::Expanded(key));
            let loaded = find(&self.roots, key).is_some_and(|node| node.children.is_some());
            if !loaded {
                self.events.push(TreeEvent::ExpandRequested(key));
            }
        } else {
            self.events.push(TreeEvent::Collapsed(key));
        }
    }

    fn select_row(&mut self, index: usize) {
        let key = self.rows[index].key;
        if self.selected != Some(key) {
            self.selected = Some(key);
            self.events.push(TreeEvent::Selected(key));
        }
        let top = index as f32 * ROW_HEIGHT;
        self.bar.model_mut().ensure_visible(top, top + ROW_HEIGHT);
    }

    fn page_rows(&self) -> usize {
        ((self.bounds.height / ROW_HEIGHT) as usize)
            .saturating_sub(1)
            .max(1)
    }

    fn key(&mut self, key: &KeyEvent) -> bool {
        if let Some(drag) = self.drag
            && key.vk == vk::ESCAPE
        {
            self.drag = None;
            self.press = None;
            self.events.push(TreeEvent::DragCancelled(drag.source));
            return true;
        }
        if self.rows.is_empty() || !key.mods.is_empty() {
            return false;
        }
        let last = self.rows.len() - 1;
        let current = self.selected.and_then(|key| self.row_of(key));
        let step = |delta: isize| -> usize {
            current.map_or(0, |index| index.saturating_add_signed(delta).min(last))
        };
        let target = match key.vk {
            vk::UP => step(-1),
            vk::DOWN => step(1),
            vk::PAGE_UP => step(-(self.page_rows() as isize)),
            vk::PAGE_DOWN => step(self.page_rows() as isize),
            vk::HOME => 0,
            vk::END => last,
            vk::RIGHT => {
                let Some(index) = current else { return true };
                let row = &self.rows[index];
                if row.expandable && !row.expanded {
                    let key = row.key;
                    self.user_expand(key, true);
                    return true;
                }
                match self.rows.get(index + 1) {
                    Some(next) if row.expanded && next.depth > row.depth => index + 1,
                    _ => return true,
                }
            }
            vk::LEFT => {
                let Some(index) = current else { return true };
                let row = &self.rows[index];
                if row.expanded {
                    let key = row.key;
                    self.user_expand(key, false);
                    return true;
                }
                match parent_of(&self.roots, row.key).and_then(|parent| self.row_of(parent)) {
                    Some(parent) => parent,
                    None => return true,
                }
            }
            vk::RETURN => {
                if let Some(selected) = self.selected {
                    self.events.push(TreeEvent::Activated(selected));
                }
                return true;
            }
            vk::F2 => {
                if let Some(selected) = self.selected {
                    self.events.push(TreeEvent::RenameRequested(selected));
                }
                return true;
            }
            _ => return false,
        };
        self.select_row(target);
        true
    }

    fn valid_target(&self, source: u64, target: u64) -> bool {
        source != target && find(&self.roots, source).is_some_and(|node| !node.contains(target))
    }

    fn pointer(&mut self, cx: &mut EventCx, event: &PointerEvent) -> EventResult {
        if !self.enabled {
            return EventResult::ignored();
        }
        let bar = self.bar.event(cx, &Event::Pointer(*event));
        if self.bar.take_changed() {
            cx.invalidate(self.bounds);
        }
        if bar.handled {
            return bar;
        }
        let left = event.button == Some(PointerButton::Left);
        if event.kind == PointerEventKind::Move {
            let on_bar =
                self.bar.model().is_scrollable() && self.bar.bounds().contains(event.position);
            if self.press.is_some() || (self.row_at(event.position).is_some() && !on_bar) {
                cx.set_cursor(Cursor::Hand);
            }
        }
        match event.kind {
            PointerEventKind::Down if left => {
                let Some(index) = self.row_at(event.position) else {
                    return EventResult::ignored();
                };
                let key = self.rows[index].key;
                if self.rows[index].expandable && self.chevron_rect(index).contains(event.position)
                {
                    let expand = !self.rows[index].expanded;
                    self.user_expand(key, expand);
                } else {
                    self.select_row(index);
                    if event.click_count == 2 {
                        self.events.push(TreeEvent::Activated(key));
                        if self.rows[index].expandable {
                            let expand = !self.rows[index].expanded;
                            self.user_expand(key, expand);
                        }
                    }
                    self.press = Some((event.position, key));
                    cx.capture_pointer();
                }
                cx.invalidate(self.bounds);
                EventResult::handled_with_focus()
            }
            PointerEventKind::Move => {
                if let Some(drag) = self.drag {
                    let target = self
                        .drop_target_at(event.position)
                        .filter(|(target, _)| self.valid_target(drag.source, *target));
                    self.drag = Some(Drag { target, ..drag });
                    cx.invalidate(self.bounds);
                    return EventResult::handled();
                }
                if let Some((origin, source)) = self.press {
                    if origin.distance(event.position) >= DRAG_THRESHOLD {
                        self.drag = Some(Drag {
                            source,
                            target: None,
                        });
                        self.events.push(TreeEvent::DragStarted(source));
                        cx.invalidate(self.bounds);
                        return EventResult::handled();
                    }
                    return EventResult::handled();
                }
                self.hover_to(cx, self.row_at(event.position))
            }
            PointerEventKind::Enter => self.hover_to(cx, self.row_at(event.position)),
            PointerEventKind::Leave => {
                if self.press.is_some() {
                    return EventResult::ignored();
                }
                self.hover_to(cx, None)
            }
            PointerEventKind::Up if left && self.press.is_some() => {
                self.press = None;
                cx.release_pointer();
                if let Some(drag) = self.drag.take() {
                    match drag.target {
                        Some((target, position)) => self.events.push(TreeEvent::Dropped {
                            source: drag.source,
                            target,
                            position,
                        }),
                        None => self.events.push(TreeEvent::DragCancelled(drag.source)),
                    }
                }
                cx.invalidate(self.bounds);
                EventResult::handled()
            }
            _ => EventResult::ignored(),
        }
    }

    fn hover_to(&mut self, cx: &mut EventCx, hover: Option<usize>) -> EventResult {
        if hover == self.hover {
            return EventResult::ignored();
        }
        self.hover = hover;
        cx.invalidate(self.bounds);
        EventResult::handled()
    }

    fn row_node(&self, cx: &AccessCx, index: usize) -> AccessNode {
        let row = &self.rows[index];
        let mut node = AccessNode::new(
            row.id,
            Role::TreeItem,
            row.label.clone(),
            self.row_rect(index),
        );
        node.state.selected = self.selected == Some(row.key);
        node.state.focusable = true;
        node.state.focused = cx.is_focused() && node.state.selected;
        node.state.disabled = !self.enabled;
        node.state.expanded = row.expandable.then_some(row.expanded);
        node.patterns.selection_item = true;
        node.patterns.expand_collapse = row.expandable;
        node
    }

    /// Carries out `action` on the node whose access node has id `item`. Returns whether the
    /// node exists and supports the action.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let Some(index) = self.rows.iter().position(|row| row.id == item) else {
            return false;
        };
        if !self.enabled {
            return false;
        }
        let key = self.rows[index].key;
        let expandable = self.rows[index].expandable;
        let done = match action {
            AccessAction::Select => {
                self.select_row(index);
                cx.focus_self();
                true
            }
            AccessAction::Invoke => {
                self.events.push(TreeEvent::Activated(key));
                true
            }
            AccessAction::Expand if expandable => {
                self.user_expand(key, true);
                true
            }
            AccessAction::Collapse if expandable => {
                self.user_expand(key, false);
                true
            }
            AccessAction::ScrollIntoView => {
                let top = index as f32 * ROW_HEIGHT;
                self.bar.model_mut().ensure_visible(top, top + ROW_HEIGHT);
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

impl Control for TreeView {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for TreeView {
    fn measure(&mut self, cx: &mut LayoutCx, available: Size) -> Size {
        let widest = self
            .rows
            .iter()
            .map(|row| {
                layout_line(cx.text, cx.theme, &row.label, FONT_SIZE)
                    .size()
                    .width
                    + row.depth as f32 * INDENT
            })
            .fold(0.0_f32, f32::max);
        let width = if available.width.is_finite() {
            available.width
        } else {
            widest + 4.0 + CHEVRON + LABEL_GAP + 12.0 + BAR_WIDTH
        };
        let content = self.rows.len() as f32 * ROW_HEIGHT;
        let height = if available.height.is_finite() {
            available.height.min(content)
        } else {
            content.min(MAX_NATURAL_HEIGHT)
        };
        Size::new(width, height)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let content = self.rows.len() as f32 * ROW_HEIGHT;
        self.bar.model_mut().set_extent(bounds.height, content);
        let bar_id = self.bar.id();
        self.bar.arrange(
            &mut cx.for_widget(bar_id),
            Rect::new(
                bounds.right() - BAR_WIDTH,
                bounds.y,
                BAR_WIDTH,
                bounds.height,
            ),
        );
        if self.enabled {
            cx.add_tab_stop();
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        let radius = tokens.metrics.control_radius;
        canvas.push_clip_rect(self.bounds);
        for index in self.visible_range() {
            let row = &self.rows[index];
            let rect = self.row_rect(index);
            let pill = rect.inset(Insets::symmetric(4.0, 2.0));
            let selected = self.selected == Some(row.key);
            if selected {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.pressed_bg);
                canvas.fill_rounded_rect(
                    Rect::new(rect.x + 2.0, rect.center().y - 8.0, 3.0, 16.0),
                    CornerRadii::uniform(1.5),
                    tokens.accent,
                );
            } else if self.hover == Some(index) && self.enabled {
                canvas.fill_rounded_rect(pill, CornerRadii::uniform(radius), tokens.hover_bg);
            }
            let dragged = self.drag.is_some_and(|drag| drag.source == row.key);
            let color = if !self.enabled {
                tokens.text_disabled
            } else if row.dimmed || dragged {
                tokens.text_muted
            } else {
                tokens.text
            };
            if row.expandable {
                let glyph = if row.expanded {
                    glyph::CHEVRON_DOWN
                } else {
                    glyph::CHEVRON_RIGHT
                };
                canvas.draw_icon(
                    glyph,
                    IconFont::SegoeFluent,
                    CHEVRON_SIZE,
                    color,
                    self.chevron_rect(index).center(),
                );
            }
            let text = layout_line(cx.text, cx.theme, &row.label, FONT_SIZE);
            draw_left_centered(canvas, text.as_ref(), self.label_x(index), rect, color);
            let drop_here = self
                .drag
                .and_then(|drag| drag.target)
                .filter(|&(target, _)| target == row.key);
            if let Some((_, position)) = drop_here {
                Self::paint_drop_indicator(canvas, rect, position, cx);
            }
            if cx.is_focused() && selected {
                focus_ring::paint(canvas, pill, radius, cx.theme);
            }
        }
        canvas.pop_clip();
        self.bar.paint(&cx.for_widget(self.bar.id()), canvas);
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        match e {
            Event::Pointer(event) => self.pointer(cx, event),
            Event::Wheel(event) if self.enabled && self.bounds.contains(event.position) => {
                if self.bar.wheel(cx, event) {
                    self.bar.take_changed();
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Key(key) if key.is_down && self.enabled && cx.is_focused() => {
                if self.key(key) {
                    if self.press.is_none() && cx.has_pointer_capture() {
                        // Esc ended a drag that still held the pointer.
                        cx.release_pointer();
                    }
                    cx.invalidate(self.bounds);
                    EventResult::handled()
                } else {
                    EventResult::ignored()
                }
            }
            Event::Text(text) if self.enabled && cx.is_focused() => {
                if !self.type_ahead.push(cx, &text.0) {
                    return EventResult::ignored();
                }
                let rows = &self.rows;
                let current = self
                    .selected
                    .and_then(|key| rows.iter().position(|row| row.key == key));
                let found = self.type_ahead.find(rows.len(), current, |index, needle| {
                    rows[index].label.to_lowercase().starts_with(needle)
                });
                if let Some(index) = found {
                    self.select_row(index);
                    cx.invalidate(self.bounds);
                }
                EventResult::handled()
            }
            Event::Timer(id) => {
                self.type_ahead.on_timer(*id);
                EventResult::ignored()
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
        let range = self.visible_range();
        let mut nodes: Vec<AccessNode> = Vec::with_capacity(range.len());
        // Nodes nest below the nearest realized ancestor, the tree itself otherwise.
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut top_level = Vec::new();
        for index in range {
            let depth = self.rows[index].depth;
            while stack.last().is_some_and(|&(stacked, _)| stacked >= depth) {
                stack.pop();
            }
            let slot = nodes.len();
            nodes.push(self.row_node(cx, index));
            match stack.last() {
                Some(&(_, parent)) => {
                    let id = nodes[slot].id;
                    nodes[parent].children.push(id);
                }
                None => top_level.push(nodes[slot].id),
            }
            stack.push((depth, slot));
        }
        for node in nodes {
            out.insert(node);
        }
        let mut node = AccessNode::new(self.id, Role::Tree, self.label.clone(), self.bounds)
            .with_state(control_state(cx, self.enabled))
            .with_children(top_level);
        node.state.focused = cx.is_focused() && self.selected.is_none();
        node.patterns.scroll = Some(scroll_info(&Default::default(), self.bar.model()));
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        match a {
            AccessAction::Focus if self.enabled => perform_focus(cx),
            AccessAction::ScrollTo {
                vertical: Some(percent),
                ..
            } if self.enabled => {
                self.bar.model_mut().jump_percent(*percent);
                cx.invalidate(self.bounds);
                true
            }
            _ => false,
        }
    }
}

impl TreeView {
    fn paint_drop_indicator(
        canvas: &mut dyn Canvas,
        rect: Rect,
        position: DropPosition,
        cx: &PaintCx,
    ) {
        let tokens = &cx.theme.tokens;
        match position {
            DropPosition::Inside => canvas.stroke_rounded_rect(
                rect.inset(Insets::symmetric(4.0, 2.0)),
                CornerRadii::uniform(tokens.metrics.control_radius),
                tokens.accent,
                2.0,
            ),
            DropPosition::Before => {
                canvas.fill_rect(Rect::new(rect.x, rect.y, rect.width, 2.0), tokens.accent)
            }
            DropPosition::After => {
                canvas.fill_rect(
                    Rect::new(rect.x, rect.bottom() - 2.0, rect.width, 2.0),
                    tokens.accent,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{Modifiers, PointerType};
    use crate::widgets::testkit::Harness;

    /// root(1) > [docs(2) > [a(3), b(4)], src(5, lazy), readme(6)]
    fn nodes() -> Vec<TreeNode> {
        vec![
            TreeNode::branch(
                1,
                "root",
                vec![
                    TreeNode::branch(
                        2,
                        "docs",
                        vec![TreeNode::leaf(3, "alpha"), TreeNode::leaf(4, "beta")],
                    ),
                    TreeNode::lazy(5, "src"),
                    TreeNode::leaf(6, "readme").dimmed(true),
                ],
            )
            .expanded(),
        ]
    }

    fn setup() -> (Harness, TreeView) {
        let mut harness = Harness::new();
        let mut tree = TreeView::new("Explorer", nodes()).with_motion(Motion::reduced());
        harness.arrange(&mut tree, Rect::new(0.0, 0.0, 240.0, 128.0));
        (harness, tree)
    }

    fn keys(tree: &TreeView) -> Vec<u64> {
        (0..tree.row_count())
            .filter_map(|index| tree.row_key(index))
            .collect()
    }

    fn click(harness: &mut Harness, tree: &mut TreeView, at: Point, count: u8) {
        harness.send(
            tree,
            Event::Pointer(PointerEvent {
                kind: PointerEventKind::Down,
                button: Some(PointerButton::Left),
                position: at,
                pointer_type: PointerType::Mouse,
                click_count: count,
                mods: Modifiers::NONE,
            }),
        );
    }

    #[test]
    fn kit_tree_flattens_expanded_nodes_with_indentation() {
        let (_, tree) = setup();
        assert_eq!(keys(&tree), vec![1, 2, 5, 6]);
        assert_eq!(tree.is_expanded(1), Some(true));
        assert_eq!(tree.is_expanded(2), Some(false));
        assert_eq!(tree.is_expanded(3), None);
        assert_eq!(tree.is_expanded(99), None);
        // Chevrons sit one indent further per depth level.
        assert_eq!(tree.chevron_rect(0).x, 4.0);
        assert_eq!(tree.chevron_rect(1).x, 20.0);
        assert_eq!(tree.label_x(1), 20.0 + 16.0 + 8.0);
        assert_eq!(tree.row_rect(2), Rect::new(0.0, 64.0, 240.0, 32.0));
    }

    #[test]
    fn kit_tree_keyboard_expand_collapse_and_navigation() {
        let (mut harness, mut tree) = setup();
        assert!(!harness.key(&mut tree, vk::DOWN).handled);
        harness.focus_widget(&mut tree);
        // Right and Left without a selection are consumed and change nothing.
        assert!(harness.key(&mut tree, vk::RIGHT).handled);
        assert!(harness.key(&mut tree, vk::LEFT).handled);
        assert_eq!(tree.selected_key(), None);
        assert_eq!(keys(&tree), vec![1, 2, 5, 6]);
        harness.key(&mut tree, vk::DOWN);
        assert_eq!(tree.selected_key(), Some(1));
        harness.key(&mut tree, vk::DOWN);
        assert_eq!(tree.selected_key(), Some(2));
        // Right expands a collapsed node, then enters it.
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(tree.is_expanded(2), Some(true));
        assert_eq!(keys(&tree), vec![1, 2, 3, 4, 5, 6]);
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(tree.selected_key(), Some(3));
        // Right on a leaf does nothing; Left goes to the parent, then collapses it.
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(tree.selected_key(), Some(3));
        harness.key(&mut tree, vk::LEFT);
        assert_eq!(tree.selected_key(), Some(2));
        harness.key(&mut tree, vk::LEFT);
        assert_eq!(tree.is_expanded(2), Some(false));
        assert_eq!(keys(&tree), vec![1, 2, 5, 6]);
        // Left on a collapsed root has no parent to go to.
        harness.key(&mut tree, vk::HOME);
        harness.key(&mut tree, vk::LEFT);
        assert_eq!(tree.is_expanded(1), Some(false));
        assert!(harness.key(&mut tree, vk::LEFT).handled);
        assert_eq!(tree.selected_key(), Some(1));
        harness.key(&mut tree, vk::END);
        assert_eq!(tree.selected_key(), Some(1));
        // Right on a collapsed node expands it again.
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(tree.is_expanded(1), Some(true));
        harness.key(&mut tree, vk::END);
        assert_eq!(tree.selected_key(), Some(6));
        // A page is the visible rows minus one: three of the four.
        harness.key(&mut tree, vk::PAGE_UP);
        assert_eq!(tree.selected_key(), Some(1));
        harness.key(&mut tree, vk::PAGE_DOWN);
        assert_eq!(tree.selected_key(), Some(6));
        harness.key(&mut tree, vk::UP);
        assert_eq!(tree.selected_key(), Some(5));
        assert!(!harness.key(&mut tree, vk::TAB).handled);
        assert!(
            !harness
                .key_with(&mut tree, vk::DOWN, Modifiers::CTRL)
                .handled
        );
    }

    #[test]
    fn kit_tree_lazy_expand_requests_children_and_shows_them() {
        let (mut harness, mut tree) = setup();
        harness.focus_widget(&mut tree);
        tree.select_key(5);
        tree.take_events();
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(
            tree.take_events(),
            vec![TreeEvent::Expanded(5), TreeEvent::ExpandRequested(5)]
        );
        tree.set_children(
            5,
            vec![TreeNode::leaf(7, "main.rs"), TreeNode::leaf(8, "lib.rs")],
        );
        assert_eq!(keys(&tree), vec![1, 2, 5, 7, 8, 6]);
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(tree.selected_key(), Some(7));
        // Left on a child goes to its parent; Left on the expanded parent collapses it.
        harness.key(&mut tree, vk::LEFT);
        assert_eq!(tree.selected_key(), Some(5));
        harness.key(&mut tree, vk::LEFT);
        assert_eq!(tree.selected_key(), Some(5));
        assert_eq!(tree.take_events().last(), Some(&TreeEvent::Collapsed(5)));
        // A loaded node expands without asking.
        harness.key(&mut tree, vk::RIGHT);
        assert_eq!(tree.take_events(), vec![TreeEvent::Expanded(5)]);
        tree.set_children(99, Vec::new());
        assert_eq!(tree.row_count(), 6);
    }

    #[test]
    fn kit_tree_collapsing_an_ancestor_reselects_it() {
        let (mut harness, mut tree) = setup();
        tree.set_expanded(2, true);
        tree.select_key(4);
        harness.focus_widget(&mut tree);
        tree.take_events();
        // Collapse the grandparent with the pointer on its chevron.
        let chevron = tree.chevron_rect(0).center();
        click(&mut harness, &mut tree, chevron, 1);
        assert_eq!(tree.selected_key(), Some(1));
        assert_eq!(
            tree.take_events(),
            vec![TreeEvent::Selected(1), TreeEvent::Collapsed(1)]
        );
        assert_eq!(keys(&tree), vec![1]);
    }

    #[test]
    fn kit_tree_enter_f2_and_type_ahead() {
        let (mut harness, mut tree) = setup();
        harness.focus_widget(&mut tree);
        harness.key(&mut tree, vk::RETURN);
        harness.key(&mut tree, vk::F2);
        assert!(tree.take_events().is_empty(), "nothing selected yet");
        tree.set_expanded(2, true);
        assert!(harness.text(&mut tree, "b").handled);
        assert_eq!(tree.selected_key(), Some(4));
        assert!(harness.key(&mut tree, vk::F2).handled);
        assert!(harness.key(&mut tree, vk::RETURN).handled);
        assert_eq!(
            tree.take_events(),
            vec![
                TreeEvent::Selected(4),
                TreeEvent::RenameRequested(4),
                TreeEvent::Activated(4)
            ]
        );
        tree.set_label(4, "gamma");
        assert_eq!(tree.rows[3].label, "gamma");
        let timer = tree.type_ahead.timer;
        harness.send(&mut tree, Event::Timer(timer));
        assert!(harness.text(&mut tree, "g").handled);
        assert_eq!(tree.selected_key(), Some(4));
        harness.send(&mut tree, Event::Timer(timer));
        // A prefix that no label starts with is consumed and keeps the selection.
        assert!(harness.text(&mut tree, "z").handled);
        assert_eq!(tree.selected_key(), Some(4));
        harness.send(&mut tree, Event::Timer(timer));
        assert!(!harness.text(&mut tree, " ").handled);
        assert!(
            !harness
                .send(
                    &mut tree,
                    Event::Timer(crate::services::TimerId::allocate())
                )
                .handled
        );
        // A frame while nothing animates repaints nothing.
        harness.requests.take_invalidation();
        harness.frame(&mut tree, 16);
        assert!(harness.requests.take_invalidation().is_clean());
    }

    #[test]
    fn kit_tree_pointer_select_chevron_double_click_and_hover() {
        let (mut harness, mut tree) = setup();
        let docs = tree.row_rect(1).center();
        click(&mut harness, &mut tree, docs, 1);
        assert_eq!(tree.selected_key(), Some(2));
        assert_eq!(tree.is_expanded(2), Some(false));
        harness.pointer(
            &mut tree,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            docs,
        );
        assert!(!harness.capture.is_captured_by(tree.id()));
        click(&mut harness, &mut tree, docs, 2);
        harness.pointer(
            &mut tree,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            docs,
        );
        assert_eq!(tree.is_expanded(2), Some(true));
        let chevron = tree.chevron_rect(1).center();
        click(&mut harness, &mut tree, chevron, 1);
        harness.pointer(
            &mut tree,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            chevron,
        );
        assert_eq!(tree.is_expanded(2), Some(false));
        assert_eq!(
            tree.take_events(),
            vec![
                TreeEvent::Selected(2),
                TreeEvent::Activated(2),
                TreeEvent::Expanded(2),
                TreeEvent::Collapsed(2),
            ]
        );
        // Double-clicking a leaf activates it; there is nothing to expand.
        let readme = tree.row_rect(3).center();
        click(&mut harness, &mut tree, readme, 2);
        harness.pointer(
            &mut tree,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            readme,
        );
        assert_eq!(
            tree.take_events(),
            vec![TreeEvent::Selected(6), TreeEvent::Activated(6)]
        );
        // Hover and leave.
        assert!(harness.hover(&mut tree, readme).handled);
        assert!(!harness.hover(&mut tree, readme).handled);
        assert!(
            harness
                .pointer(&mut tree, PointerEventKind::Leave, None, Point::ZERO)
                .handled
        );
        assert!(
            harness
                .pointer(&mut tree, PointerEventKind::Enter, None, readme)
                .handled
        );
        // Empty space and other buttons are ignored.
        assert!(
            !harness
                .pointer(
                    &mut tree,
                    PointerEventKind::Down,
                    Some(PointerButton::Left),
                    Point::new(10.0, 200.0)
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut tree,
                    PointerEventKind::Down,
                    Some(PointerButton::Right),
                    docs
                )
                .handled
        );
        assert!(
            !harness
                .pointer(
                    &mut tree,
                    PointerEventKind::Up,
                    Some(PointerButton::Left),
                    docs
                )
                .handled
        );
    }

    #[test]
    fn kit_tree_drag_and_drop_events_and_validation() {
        let (mut harness, mut tree) = setup();
        tree.set_expanded(2, true);
        harness.arrange(&mut tree, Rect::new(0.0, 0.0, 240.0, 256.0));
        let readme_row = tree.row_count() - 1;
        let source = tree.row_rect(readme_row).center();
        click(&mut harness, &mut tree, source, 1);
        tree.take_events();
        // Small movements are still a click.
        assert!(
            harness
                .pointer(
                    &mut tree,
                    PointerEventKind::Move,
                    None,
                    Point::new(source.x + 2.0, source.y)
                )
                .handled
        );
        assert_eq!(tree.dragging(), None);
        // Past the threshold the drag starts; over docs' middle it targets "inside".
        let docs = tree.row_rect(1).center();
        harness.pointer(
            &mut tree,
            PointerEventKind::Move,
            None,
            Point::new(source.x + 10.0, source.y),
        );
        assert_eq!(tree.dragging(), Some(6));
        harness.pointer(&mut tree, PointerEventKind::Move, None, docs);
        let canvas = harness.paint(&tree);
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::StrokeRoundedRect { color, width, .. } if *color == tokens.accent && *width == 2.0)));
        // The dragged row is dimmed.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::Text { text, color, .. } if text == "readme" && *color == tokens.text_muted)));
        harness.pointer(
            &mut tree,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            docs,
        );
        assert_eq!(
            tree.take_events(),
            vec![
                TreeEvent::DragStarted(6),
                TreeEvent::Dropped {
                    source: 6,
                    target: 2,
                    position: DropPosition::Inside
                },
            ]
        );
        assert_eq!(tree.dragging(), None);
        // A node cannot be dropped onto itself or its own descendant: the drag cancels.
        let root = tree.row_rect(0).center();
        click(&mut harness, &mut tree, root, 1);
        tree.take_events();
        harness.pointer(
            &mut tree,
            PointerEventKind::Move,
            None,
            Point::new(root.x + 10.0, root.y),
        );
        harness.pointer(&mut tree, PointerEventKind::Move, None, docs);
        harness.pointer(
            &mut tree,
            PointerEventKind::Up,
            Some(PointerButton::Left),
            docs,
        );
        assert_eq!(
            tree.take_events(),
            vec![TreeEvent::DragStarted(1), TreeEvent::DragCancelled(1)]
        );
        // Esc cancels a drag in progress.
        let alpha = tree.row_rect(2).center();
        click(&mut harness, &mut tree, alpha, 1);
        harness.pointer(
            &mut tree,
            PointerEventKind::Move,
            None,
            Point::new(alpha.x + 10.0, alpha.y),
        );
        harness.focus_widget(&mut tree);
        tree.take_events();
        assert!(harness.key(&mut tree, vk::ESCAPE).handled);
        assert_eq!(tree.take_events(), vec![TreeEvent::DragCancelled(3)]);
        assert_eq!(tree.dragging(), None);
    }

    #[test]
    fn kit_tree_drop_regions_for_external_drags() {
        let (_, tree) = setup();
        let row = tree.row_rect(1);
        assert_eq!(
            tree.drop_target_at(Point::new(50.0, row.y + 2.0)),
            Some((2, DropPosition::Before))
        );
        assert_eq!(
            tree.drop_target_at(Point::new(50.0, row.y + 16.0)),
            Some((2, DropPosition::Inside))
        );
        assert_eq!(
            tree.drop_target_at(Point::new(50.0, row.y + 30.0)),
            Some((2, DropPosition::After))
        );
        assert_eq!(tree.drop_target_at(Point::new(500.0, 10.0)), None);
    }

    #[test]
    fn kit_tree_paints_chevrons_selection_dimming_and_drop_lines() {
        let (mut harness, mut tree) = setup();
        let tokens = harness.theme.tokens;
        tree.select_key(5);
        let canvas = harness.paint(&tree);
        assert_eq!(canvas.texts(), vec!["root", "docs", "src", "readme"]);
        let chevrons: Vec<char> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Icon { glyph, .. } => Some(*glyph),
                _ => None,
            })
            .collect();
        assert_eq!(
            chevrons,
            vec![
                glyph::CHEVRON_DOWN,
                glyph::CHEVRON_RIGHT,
                glyph::CHEVRON_RIGHT
            ]
        );
        assert!(canvas.ops().any(
            |op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == tokens.pressed_bg)
        ));
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::Text { text, color, .. } if text == "readme" && *color == tokens.text_muted)));
        // Indentation: "docs" starts one level in.
        assert!(canvas.ops().any(|op| matches!(op, DrawOp::Text { text, origin, .. } if text == "docs" && origin.x == 20.0 + 16.0 + 8.0)));
        harness.focus_widget(&mut tree);
        let canvas = harness.paint(&tree);
        assert_eq!(
            canvas
                .ops()
                .filter(|op| matches!(op, DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
                .count(),
            1
        );
        // Drop lines before and after a row.
        for (position, y) in [(DropPosition::Before, 32.0), (DropPosition::After, 62.0)] {
            tree.drag = Some(Drag {
                source: 6,
                target: Some((2, position)),
            });
            let canvas = harness.paint(&tree);
            assert!(canvas.ops().any(|op| matches!(op, DrawOp::FillRect { rect, color } if rect.y == y && rect.height == 2.0 && *color == tokens.accent)), "{position:?}");
        }
        tree.drag = Some(Drag {
            source: 6,
            target: None,
        });
        let canvas = harness.paint(&tree);
        assert!(!canvas.ops().any(|op| matches!(op, DrawOp::FillRect { .. })));
        tree.drag = None;
        tree.set_enabled(false);
        let canvas = harness.paint(&tree);
        let text_colors: Vec<crate::geometry::Color> = canvas
            .ops()
            .filter_map(|op| match op {
                DrawOp::Text { color, .. } => Some(*color),
                _ => None,
            })
            .collect();
        assert_eq!(text_colors.len(), 4);
        assert!(
            text_colors
                .iter()
                .all(|color| *color == tokens.text_disabled)
        );
    }

    #[test]
    fn kit_tree_access_nests_realized_rows_and_performs_item_actions() {
        let (mut harness, mut tree) = setup();
        tree.set_expanded(2, true);
        harness.arrange(&mut tree, Rect::new(0.0, 0.0, 240.0, 256.0));
        harness.focus_widget(&mut tree);
        tree.select_key(2);
        let tree_nodes = harness.tree(&tree);
        let root = tree_nodes.root().expect("root");
        assert_eq!((root.role, root.name.as_str()), (Role::Tree, "Explorer"));
        assert_eq!(root.children.len(), 1, "only the root item is top level");
        let item_root = tree_nodes.first_child(tree.id()).expect("root item");
        assert_eq!(
            (item_root.role, item_root.name.as_str()),
            (Role::TreeItem, "root")
        );
        assert_eq!(item_root.state.expanded, Some(true));
        assert!(item_root.patterns.expand_collapse && item_root.patterns.selection_item);
        let docs = tree_nodes.children(item_root.id).next().expect("docs");
        assert_eq!(docs.state.expanded, Some(true));
        assert!(docs.state.selected && docs.state.focused);
        assert_eq!(tree_nodes.children(docs.id).count(), 2);
        let leaf = tree_nodes.children(docs.id).next().expect("alpha");
        assert_eq!(leaf.state.expanded, None);
        assert!(!leaf.patterns.expand_collapse);
        assert_eq!(tree_nodes.children(item_root.id).count(), 3);
        // Items.
        let docs_id = docs.id;
        let leaf_id = leaf.id;
        let id = tree.id();
        {
            let mut cx = harness.event_cx(id);
            assert!(tree.perform_item(&mut cx, docs_id, &AccessAction::Collapse));
            assert!(tree.perform_item(&mut cx, docs_id, &AccessAction::Expand));
            assert!(!tree.perform_item(&mut cx, leaf_id, &AccessAction::Expand));
            assert!(!tree.perform_item(&mut cx, leaf_id, &AccessAction::Collapse));
            assert!(tree.perform_item(&mut cx, leaf_id, &AccessAction::Select));
            assert!(tree.perform_item(&mut cx, leaf_id, &AccessAction::Invoke));
            assert!(tree.perform_item(&mut cx, leaf_id, &AccessAction::ScrollIntoView));
            assert!(!tree.perform_item(&mut cx, leaf_id, &AccessAction::Toggle));
            assert!(!tree.perform_item(&mut cx, WidgetId::allocate(), &AccessAction::Select));
        }
        assert_eq!(tree.selected_key(), Some(3));
        // A tree scrolled so that a child is the first realized row attaches it to the tree.
        let mut tall = TreeView::new("t", nodes()).with_motion(Motion::reduced());
        harness.arrange(&mut tall, Rect::new(0.0, 0.0, 240.0, 64.0));
        tall.set_expanded(2, true);
        harness.arrange(&mut tall, Rect::new(0.0, 0.0, 240.0, 64.0));
        tall.bar.model_mut().jump(64.0);
        let scrolled = harness.tree(&tall);
        let first = scrolled.first_child(tall.id()).expect("first realized row");
        assert_eq!(first.name, "alpha");
        assert_eq!(scrolled.root().expect("root").children.len(), 2);
        // The tree's own actions.
        assert!(harness.perform(&mut tall, AccessAction::Focus));
        assert!(harness.perform(
            &mut tall,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(0.0)
            }
        ));
        assert_eq!(tall.scroll_offset(), 0.0);
        assert!(!harness.perform(&mut tall, AccessAction::Invoke));
        tall.set_enabled(false);
        harness.arrange(&mut tall, Rect::new(0.0, 0.0, 240.0, 64.0));
        assert!(!harness.perform(&mut tall, AccessAction::Focus));
        assert!(!harness.perform(
            &mut tall,
            AccessAction::ScrollTo {
                horizontal: None,
                vertical: Some(0.0)
            }
        ));
        let leaf_id = tall.rows[0].id;
        let tall_id = tall.id();
        let mut cx = harness.event_cx(tall_id);
        assert!(!tall.perform_item(&mut cx, leaf_id, &AccessAction::Select));
        assert!(!tall.is_enabled());
        assert!(harness.node(&tall).state.disabled);
        assert!(!harness.click(&mut tall, Point::new(10.0, 10.0)).handled);
        assert!(!harness.key(&mut tall, vk::DOWN).handled);
    }

    #[test]
    fn kit_tree_measure_dimming_and_misc_accessors() {
        let mut harness = Harness::new();
        let mut tree = TreeView::new("t", nodes());
        let size = harness.place(&mut tree, Point::ZERO).size();
        // Four rows of 32; the widest label is "readme"/"docs" nested one level.
        assert_eq!(size.height, 128.0);
        assert_eq!(
            size.width,
            6.0 * 8.0 + 16.0 + 4.0 + 16.0 + 8.0 + 12.0 + 12.0
        );
        let id = tree.id();
        let mut cx = LayoutCx::new(
            id,
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            tree.measure(&mut cx, Size::new(100.0, 50.0)),
            Size::new(100.0, 50.0)
        );
        assert_eq!(
            tree.measure(&mut cx, Size::new(100.0, 500.0)),
            Size::new(100.0, 128.0)
        );
        tree.set_dimmed(6, false);
        assert!(!tree.rows[3].dimmed);
        // Keys that are not in the tree change nothing.
        tree.set_dimmed(99, true);
        tree.set_label(99, "ghost");
        assert_eq!(tree.row_count(), 4);
        assert!(
            tree.rows
                .iter()
                .all(|row| !row.dimmed && row.label != "ghost")
        );
        assert_eq!(tree.row_key(99), None);
        tree.select_key(77);
        assert_eq!(tree.selected_key(), None);
        let leaf = TreeNode::leaf(1, "x");
        assert_eq!((leaf.key(), leaf.label()), (1, "x"));
        assert!(!leaf.clone().expanded().expanded);
        assert_eq!(tree.bounds(), Rect::new(0.0, 0.0, size.width, size.height));
    }

    fn long_nodes() -> Vec<TreeNode> {
        vec![
            TreeNode::branch(
                1,
                "root",
                (10..60)
                    .map(|key| TreeNode::leaf(key, format!("node {key}")))
                    .collect(),
            )
            .expanded(),
        ]
    }

    fn long_tree(motion: Motion) -> (Harness, TreeView) {
        let mut harness = Harness::new();
        let mut tree = TreeView::new("Explorer", long_nodes()).with_motion(motion);
        harness.arrange(&mut tree, Rect::new(0.0, 0.0, 240.0, 128.0));
        (harness, tree)
    }

    fn notch(dy: f32) -> Event {
        Event::Wheel(crate::input::WheelEvent {
            delta: Point::new(0.0, dy),
            precise: false,
            position: Point::new(100.0, 50.0),
            mods: Modifiers::NONE,
        })
    }

    #[test]
    fn kit_tree_requests_the_hand_over_rows_and_while_pressed() {
        let (mut harness, mut tree) = long_tree(Motion::reduced());
        let row = tree.row_rect(1).center();
        harness.hover(&mut tree, row);
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
        harness.hover(&mut tree, Point::new(237.0, 50.0));
        assert_eq!(harness.requests.take_cursor(), None, "scroll bar");
        harness.hover(&mut tree, Point::new(500.0, 50.0));
        assert_eq!(harness.requests.take_cursor(), None, "outside");
        let left = Some(PointerButton::Left);
        harness.pointer(&mut tree, PointerEventKind::Down, left, row);
        harness.requests.take_cursor();
        harness.hover(&mut tree, Point::new(500.0, 50.0));
        assert_eq!(harness.requests.take_cursor(), Some(Cursor::Hand));
        harness.pointer(&mut tree, PointerEventKind::Up, left, row);
        tree.set_enabled(false);
        harness.hover(&mut tree, row);
        assert_eq!(harness.requests.take_cursor(), None);
    }

    #[test]
    fn kit_tree_with_id_names_the_control() {
        let id = WidgetId::allocate();
        let tree = TreeView::new("Explorer", nodes()).with_id(id);
        assert_eq!(tree.id(), id);
        assert_eq!(Harness::new().node(&tree).id, id);
    }

    #[test]
    fn kit_tree_expanding_needs_a_known_expandable_node_that_changes() {
        let (_, mut tree) = setup();
        assert!(!tree.apply_expanded(99, true));
        // A leaf cannot expand and an expanded node cannot expand again.
        assert!(!tree.apply_expanded(3, true));
        assert!(!tree.apply_expanded(1, true));
        tree.user_expand(3, true);
        tree.user_expand(1, true);
        assert!(tree.take_events().is_empty());
    }

    #[test]
    fn kit_tree_paints_only_where_damaged_and_fills_the_hovered_row() {
        let (mut harness, mut tree) = setup();
        let away = Rect::new(500.0, 500.0, 5.0, 5.0);
        assert_eq!(harness.paint_damaged(&tree, Some(away)).ops().count(), 0);
        let hover = harness.theme.tokens.hover_bg;
        let fills = |harness: &mut Harness, tree: &TreeView| {
            harness
                .paint(tree)
                .ops()
                .filter(|op| matches!(op, DrawOp::FillRoundedRect { color, .. } if *color == hover))
                .count()
        };
        assert_eq!(fills(&mut harness, &tree), 0);
        let row = tree.row_rect(2).center();
        harness.hover(&mut tree, row);
        assert_eq!(fills(&mut harness, &tree), 1);
    }

    #[test]
    fn kit_tree_leaving_while_a_press_is_pending_keeps_the_state() {
        let (mut harness, mut tree) = setup();
        let row = tree.row_rect(1).center();
        click(&mut harness, &mut tree, row, 1);
        let left = harness.pointer(
            &mut tree,
            PointerEventKind::Leave,
            None,
            Point::new(500.0, 500.0),
        );
        assert!(!left.handled);
    }

    #[test]
    fn kit_tree_a_wheel_over_a_tree_that_fits_is_left_alone() {
        let (mut harness, mut tree) = setup();
        assert!(!harness.send(&mut tree, notch(-1.0)).handled);
    }

    #[test]
    fn kit_tree_a_wheel_scrolls_a_long_tree_and_frames_ease_it() {
        let (mut harness, mut tree) = long_tree(Motion::reduced());
        assert!(harness.send(&mut tree, notch(-1.0)).handled);
        assert!(tree.row_rect(0).y < 0.0);
        let (mut harness, mut tree) = long_tree(Motion::full());
        assert!(harness.send(&mut tree, notch(-3.0)).handled);
        assert!(harness.requests.animation_frame_requested());
        harness.frame(&mut tree, 16);
        harness.requests.take_invalidation();
        harness.frame(&mut tree, 100);
        assert!(!harness.requests.take_invalidation().is_clean());
        harness.run_animations(&mut tree);
        assert!(tree.row_rect(0).y < 0.0);
    }

    #[test]
    fn kit_tree_dragging_the_scroll_bar_scrolls_the_rows() {
        let (mut harness, mut tree) = long_tree(Motion::reduced());
        let grab = tree.bar.thumb_rect().center();
        let left = Some(PointerButton::Left);
        assert!(
            harness
                .pointer(&mut tree, PointerEventKind::Down, left, grab)
                .handled
        );
        harness.requests.take_invalidation();
        let to = Point::new(grab.x, grab.y + 40.0);
        harness.pointer(&mut tree, PointerEventKind::Move, None, to);
        assert!(tree.row_rect(0).y < 0.0);
        assert!(!harness.requests.take_invalidation().is_clean());
        harness.pointer(&mut tree, PointerEventKind::Up, left, to);
    }

    #[test]
    fn kit_tree_a_drag_without_a_target_draws_no_drop_indicator() {
        let (mut harness, mut tree) = setup();
        let accent = harness.theme.tokens.accent;
        let strokes = |harness: &mut Harness, tree: &TreeView| {
            harness
                .paint(tree)
                .ops()
                .filter(
                    |op| matches!(op, DrawOp::StrokeRoundedRect { color, .. } if *color == accent),
                )
                .count()
        };
        // Dropping inside a row outlines it; without a target nothing is outlined.
        tree.drag = Some(Drag {
            source: 6,
            target: Some((2, DropPosition::Inside)),
        });
        assert_eq!(strokes(&mut harness, &tree), 1);
        tree.drag = Some(Drag {
            source: 6,
            target: None,
        });
        assert_eq!(strokes(&mut harness, &tree), 0);
    }
}
