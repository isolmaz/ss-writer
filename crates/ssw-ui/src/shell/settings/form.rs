//! Rows of labelled controls: the layout, painting, input routing and accessibility shared by
//! the Settings pages and the export dialog.
//!
//! A [`Form`] is laid out in *content coordinates*: the owner passes the rectangle the form
//! starts at and receives the height it needs, then paints and routes events with the same
//! coordinates (a scrolling owner translates by its offset, as `ScrollView` does). Row heights
//! depend only on the static texts, never on a validation error: a text row always reserves the
//! line under its control where the error appears, so showing or clearing an error never moves
//! anything.

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash};
use crate::geometry::{Point, Rect, Size};
use crate::input::Event;
use crate::widget::{
    AccessCx, EventCx, EventResult, FocusManager, LayoutCx, PaintCx, Widget, WidgetId,
};
use crate::widgets::button::Button;
use crate::widgets::combo::ComboBox;
use crate::widgets::slider::Slider;
use crate::widgets::textbox::TextBox;
use crate::widgets::toggle::ToggleSwitch;
use crate::widgets::{CONTROL_HEIGHT, Control, FONT_SIZE, ui_style};

/// Horizontal padding of the form.
pub(crate) const PAD: f32 = 24.0;
/// Gap between the label column and the control column.
const COLUMN_GAP: f32 = 16.0;
/// Width of the control column.
const CONTROL_WIDTH: f32 = 260.0;
/// Shortest row.
const ROW_MIN_HEIGHT: f32 = 52.0;
/// Vertical padding inside a row.
const ROW_PAD: f32 = 10.0;
/// Em size of the explanation under a label.
const HINT_SIZE: f32 = 12.0;
/// Em size of a validation message.
const ERROR_SIZE: f32 = 12.0;
/// Gap between a text box and its validation message.
const ERROR_GAP: f32 = 2.0;
/// Height reserved for the validation message.
const ERROR_LINE: f32 = 16.0;
/// Gap between a text box and its Browse button.
const ACCESSORY_GAP: f32 = 8.0;
/// Width reserved for the value shown beside a slider.
const SLIDER_LABEL_WIDTH: f32 = 56.0;

/// The control of a row. Rows are few and built once per page, so the size difference between
/// the variants (a combo box carries its popup list) costs nothing worth a heap allocation.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum Input {
    Toggle(ToggleSwitch),
    Choice(ComboBox),
    Text(TextBox),
    /// A slider whose value is shown as `labels[value]` (one entry per integer value).
    Slider {
        slider: Slider,
        labels: Vec<String>,
    },
    Button(Button),
}

impl Input {
    fn control(&self) -> &dyn Control {
        match self {
            Input::Toggle(control) => control,
            Input::Choice(control) => control,
            Input::Text(control) => control,
            Input::Slider { slider, .. } => slider,
            Input::Button(control) => control,
        }
    }

    fn control_mut(&mut self) -> &mut dyn Control {
        match self {
            Input::Toggle(control) => control,
            Input::Choice(control) => control,
            Input::Text(control) => control,
            Input::Slider { slider, .. } => slider,
            Input::Button(control) => control,
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        match self {
            Input::Toggle(control) => control.set_enabled(enabled),
            Input::Choice(control) => control.set_enabled(enabled),
            Input::Text(control) => control.set_enabled(enabled),
            Input::Slider { slider, .. } => slider.set_enabled(enabled),
            Input::Button(control) => control.set_enabled(enabled),
        }
    }
}

/// What the user did to a row's control.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Change {
    Toggled(bool),
    Selected(usize),
    /// The text of a text box after an edit.
    Edited(String),
    Slid(f64),
    /// Enter in a text box.
    Submitted,
    /// A button row was activated.
    Pressed,
    /// The Browse button beside a text box was activated.
    Accessory,
}

/// A labelled control.
#[derive(Debug)]
pub(crate) struct Row<K> {
    key: K,
    label: String,
    hint: Option<String>,
    input: Input,
    accessory: Option<Button>,
    error: Option<String>,
    error_id: WidgetId,
    // Geometry in content coordinates, set by `Form::layout`.
    rect: Rect,
    control_rect: Rect,
    label_origin: Point,
    hint_origin: Point,
    label_width: f32,
}

impl<K> Row<K> {
    /// A row labelled `label` (drawn by the form, except for toggles which draw their own).
    pub(crate) fn new(key: K, label: String, hint: Option<String>, input: Input) -> Self {
        Self {
            key,
            label,
            hint,
            input,
            accessory: None,
            error: None,
            error_id: WidgetId::allocate(),
            rect: Rect::ZERO,
            control_rect: Rect::ZERO,
            label_origin: Point::ZERO,
            hint_origin: Point::ZERO,
            label_width: 0.0,
        }
    }

    /// Adds a button beside the control (the Browse button of a folder).
    pub(crate) fn with_accessory(mut self, button: Button) -> Self {
        self.accessory = Some(button);
        self
    }

    fn is_toggle(&self) -> bool {
        matches!(self.input, Input::Toggle(_))
    }
}

/// A column of rows keyed by `K`.
#[derive(Debug)]
pub(crate) struct Form<K> {
    rows: Vec<Row<K>>,
    changes: Vec<(K, Change)>,
}

fn text_size(cx: &LayoutCx, text: &str, size: f32, width: f32) -> Size {
    cx.text
        .layout(text, &ui_style(cx.theme, size), &[], &[], width)
        .size()
}

/// Delivers `e` to `control` with a context for its id.
pub(crate) fn forward(control: &mut dyn Control, cx: &mut EventCx, e: &Event) -> EventResult {
    let id = control.id();
    control.event(&mut cx.for_widget(id), e)
}

impl<K: Copy + PartialEq> Form<K> {
    pub(crate) fn new(rows: Vec<Row<K>>) -> Self {
        Self {
            rows,
            changes: Vec::new(),
        }
    }

    /// Arranges every row below `bounds.origin()` over `bounds.width` and returns the height
    /// the form needs.
    pub(crate) fn layout(&mut self, cx: &mut LayoutCx, bounds: Rect) -> f32 {
        let inner_x = bounds.x + PAD;
        let inner_width = (bounds.width - 2.0 * PAD).max(0.0);
        let column_width = CONTROL_WIDTH.min(inner_width / 2.0);
        let label_width = (inner_width - column_width - COLUMN_GAP).max(0.0);
        let column_x = inner_x + inner_width - column_width;
        let mut y = bounds.y;
        for row in &mut self.rows {
            let toggle = row.is_toggle();
            let text_width = if toggle { inner_width } else { label_width };
            let label_height = if toggle {
                CONTROL_HEIGHT
            } else {
                text_size(cx, &row.label, FONT_SIZE, text_width).height
            };
            let hint_height = row.hint.as_ref().map_or(0.0, |hint| {
                text_size(cx, hint, HINT_SIZE, text_width).height
            });
            let text_height = label_height + hint_height;
            let can_error = matches!(row.input, Input::Text(_));
            let control_block = if can_error {
                CONTROL_HEIGHT + ERROR_GAP + ERROR_LINE
            } else {
                CONTROL_HEIGHT
            };
            let height = (2.0 * ROW_PAD + text_height.max(control_block)).max(ROW_MIN_HEIGHT);
            let text_top = y + (height - text_height) / 2.0;
            row.label_origin = Point::new(inner_x, text_top);
            row.hint_origin = Point::new(inner_x, text_top + label_height);
            row.label_width = text_width;
            let id = row.input.control().id();
            if toggle {
                let size = row.input.control_mut().measure(
                    &mut cx.for_widget(id),
                    Size::new(inner_width, f32::INFINITY),
                );
                let rect = Rect::new(
                    inner_x,
                    text_top,
                    size.width.min(inner_width),
                    CONTROL_HEIGHT,
                );
                row.input
                    .control_mut()
                    .arrange(&mut cx.for_widget(id), rect);
                row.control_rect = rect;
            } else {
                let control_top = if can_error {
                    y + ROW_PAD
                } else {
                    y + (height - CONTROL_HEIGHT) / 2.0
                };
                let mut rect = Rect::new(column_x, control_top, column_width, CONTROL_HEIGHT);
                if let Some(button) = row.accessory.as_mut() {
                    let button_id = button.id();
                    let size = button.measure(&mut cx.for_widget(button_id), Size::UNBOUNDED);
                    let width = size.width.min(column_width / 2.0);
                    let button_rect =
                        Rect::new(rect.right() - width, control_top, width, CONTROL_HEIGHT);
                    button.arrange(&mut cx.for_widget(button_id), button_rect);
                    rect.width = (rect.width - width - ACCESSORY_GAP).max(0.0);
                }
                if matches!(row.input, Input::Slider { .. }) {
                    rect.width = (rect.width - SLIDER_LABEL_WIDTH).max(0.0);
                }
                row.input
                    .control_mut()
                    .arrange(&mut cx.for_widget(id), rect);
                row.control_rect = rect;
            }
            row.rect = Rect::new(bounds.x, y, bounds.width, height);
            y += height;
        }
        y - bounds.y + PAD
    }

    /// Draws the rows (content coordinates).
    pub(crate) fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        let tokens = &cx.theme.tokens;
        for row in &self.rows {
            if !cx.needs_paint(row.rect) {
                continue;
            }
            canvas.stroke_line(
                Point::new(row.rect.x + PAD, row.rect.bottom() - 0.5),
                Point::new(row.rect.right() - PAD, row.rect.bottom() - 0.5),
                tokens.hairline,
                1.0,
                Dash::Solid,
            );
            let enabled = !is_disabled(&row.input);
            if !row.is_toggle() {
                let layout = cx.text.layout(
                    &row.label,
                    &ui_style(cx.theme, FONT_SIZE),
                    &[],
                    &[],
                    row.label_width,
                );
                let color = if enabled {
                    tokens.text
                } else {
                    tokens.text_disabled
                };
                canvas.draw_text_layout(layout.as_ref(), row.label_origin, color);
            }
            if let Some(hint) = &row.hint {
                let layout = cx.text.layout(
                    hint,
                    &ui_style(cx.theme, HINT_SIZE),
                    &[],
                    &[],
                    row.label_width,
                );
                canvas.draw_text_layout(layout.as_ref(), row.hint_origin, tokens.text_muted);
            }
            if let Some(error) = &row.error {
                let layout = cx.text.layout(
                    error,
                    &ui_style(cx.theme, ERROR_SIZE),
                    &[],
                    &[],
                    f32::INFINITY,
                );
                let x = (row.control_rect.right() - layout.size().width).max(row.rect.x + PAD);
                let origin = Point::new(x, row.control_rect.bottom() + ERROR_GAP);
                canvas.draw_text_layout(layout.as_ref(), origin, tokens.error);
            }
            if let Input::Slider { slider, labels } = &row.input {
                let index = slider.value().round().max(0.0) as usize;
                if let Some(text) = labels.get(index) {
                    let layout = cx.text.layout(
                        text,
                        &ui_style(cx.theme, FONT_SIZE),
                        &[],
                        &[],
                        f32::INFINITY,
                    );
                    let origin = Point::new(
                        row.control_rect.right() + ACCESSORY_GAP,
                        row.control_rect.y + (CONTROL_HEIGHT - layout.size().height) / 2.0,
                    );
                    canvas.draw_text_layout(layout.as_ref(), origin, tokens.text_muted);
                }
            }
            let control = row.input.control();
            control.paint(&cx.for_widget(control.id()), canvas);
            if let Some(button) = &row.accessory {
                button.paint(&cx.for_widget(button.id()), canvas);
            }
        }
    }

    /// The row whose combo box is open: while a popup is open it gets every pointer and wheel
    /// event so a click that dismisses it does not also reach the control below.
    fn open_combo(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| matches!(&row.input, Input::Choice(combo) if combo.is_open()))
    }

    /// Delivers `e` to the controls and collects what they report.
    pub(crate) fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let exclusive = if matches!(e, Event::Pointer(_) | Event::Wheel(_)) {
            self.open_combo()
        } else {
            None
        };
        let mut result = EventResult::ignored();
        for (index, row) in self.rows.iter_mut().enumerate() {
            if exclusive.is_some_and(|open| open != index) {
                continue;
            }
            result = result.or(forward(row.input.control_mut(), cx, e));
            if let Some(button) = row.accessory.as_mut() {
                result = result.or(forward(button, cx, e));
            }
        }
        self.collect();
        result
    }

    fn collect(&mut self) {
        for row in &mut self.rows {
            let key = row.key;
            match &mut row.input {
                Input::Toggle(toggle) => {
                    if let Some(on) = toggle.take_changed() {
                        self.changes.push((key, Change::Toggled(on)));
                    }
                }
                Input::Choice(combo) => {
                    if let Some(index) = combo.take_changed() {
                        self.changes.push((key, Change::Selected(index)));
                    }
                }
                Input::Text(text) => {
                    if text.take_changed() {
                        self.changes
                            .push((key, Change::Edited(text.text().to_owned())));
                    }
                    if text.take_submitted() {
                        self.changes.push((key, Change::Submitted));
                    }
                }
                Input::Slider { slider, .. } => {
                    if let Some(value) = slider.take_changed() {
                        self.changes.push((key, Change::Slid(value)));
                    }
                }
                Input::Button(button) => {
                    if button.take_activated() {
                        self.changes.push((key, Change::Pressed));
                    }
                }
            }
            if let Some(button) = row.accessory.as_mut()
                && button.take_activated()
            {
                self.changes.push((key, Change::Accessory));
            }
        }
    }

    /// The changes since the last call, oldest first.
    pub(crate) fn take_changes(&mut self) -> Vec<(K, Change)> {
        std::mem::take(&mut self.changes)
    }

    /// Shows (or clears) the validation message of the row `key`.
    pub(crate) fn set_error(&mut self, key: K, message: Option<String>) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.key == key) {
            row.error = message;
        }
    }

    /// The validation message of the row `key`.
    pub(crate) fn error(&self, key: K) -> Option<&str> {
        self.rows
            .iter()
            .find(|row| row.key == key)
            .and_then(|row| row.error.as_deref())
    }

    /// True when any row shows a validation message.
    pub(crate) fn has_error(&self) -> bool {
        self.rows.iter().any(|row| row.error.is_some())
    }

    /// Replaces the text of the text box of the row `key` without reporting a change.
    pub(crate) fn set_text(&mut self, key: K, text: &str) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.key == key)
            && let Input::Text(text_box) = &mut row.input
        {
            text_box.set_text(text);
        }
    }

    /// Enables or disables the controls of the row `key`.
    pub(crate) fn set_enabled(&mut self, key: K, enabled: bool) {
        if let Some(row) = self.rows.iter_mut().find(|row| row.key == key) {
            row.input.set_enabled(enabled);
            if let Some(button) = row.accessory.as_mut() {
                button.set_enabled(enabled);
            }
        }
    }

    /// The bounds (content coordinates) of the row holding the focused control.
    pub(crate) fn focused_row(&self, focus: &FocusManager) -> Option<Rect> {
        self.rows
            .iter()
            .find(|row| {
                focus.is_focused(row.input.control().id())
                    || row
                        .accessory
                        .as_ref()
                        .is_some_and(|button| focus.is_focused(button.id()))
            })
            .map(|row| row.rect)
    }

    /// The bounds of the open combo box with its popup (content coordinates).
    pub(crate) fn open_popup(&self) -> Option<Rect> {
        self.rows.iter().find_map(|row| match &row.input {
            Input::Choice(combo) => combo
                .popup_bounds()
                .map(|popup| combo.bounds().union(popup)),
            _ => None,
        })
    }

    /// Describes the controls and validation messages in `out`, translated by `-offset` and
    /// flagged off-screen when outside `viewport`. Returns the ids to list as children of the
    /// page node.
    pub(crate) fn access(
        &self,
        cx: &AccessCx,
        out: &mut AccessTree,
        offset: Point,
        viewport: Rect,
    ) -> Vec<WidgetId> {
        let mut ids = Vec::new();
        for row in &self.rows {
            let accessory = row.accessory.as_ref().map(|button| button as &dyn Control);
            let controls =
                std::iter::once((row.input.control(), true)).chain(accessory.map(|b| (b, false)));
            for (control, described) in controls {
                let id = control.id();
                let mut sub = AccessTree::new(id);
                control.access(&cx.for_widget(id), &mut sub);
                for node in sub.nodes() {
                    let mut node = node.clone();
                    node.bounds = node.bounds.translate(-offset.x, -offset.y);
                    node.state.offscreen |= !node.bounds.intersects(viewport);
                    if node.id == id
                        && described
                        && let Some(hint) = &row.hint
                    {
                        node.description.clone_from(hint);
                    }
                    out.insert(node);
                }
                ids.push(id);
            }
            if let Some(error) = &row.error {
                let bounds = Rect::new(
                    row.control_rect.x,
                    row.control_rect.bottom() + ERROR_GAP,
                    row.control_rect.width,
                    ERROR_LINE,
                )
                .translate(-offset.x, -offset.y);
                let mut node = AccessNode::new(row.error_id, Role::Text, error.clone(), bounds);
                node.state.offscreen = !bounds.intersects(viewport);
                out.insert(node);
                ids.push(row.error_id);
            }
        }
        ids
    }

    /// Carries out `a` on the control with id `cx.id` and collects the resulting changes.
    pub(crate) fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        let target = cx.id;
        let mut handled = false;
        for row in &mut self.rows {
            let control: Option<&mut dyn Control> = if row.input.control().id() == target {
                Some(row.input.control_mut())
            } else {
                row.accessory
                    .as_mut()
                    .filter(|button| button.id() == target)
                    .map(|button| button as &mut dyn Control)
            };
            if let Some(control) = control {
                handled = control.perform(&mut cx.for_widget(target), a);
                break;
            }
        }
        self.collect();
        handled
    }
}

/// True when the control of an input is disabled.
fn is_disabled(input: &Input) -> bool {
    match input {
        Input::Toggle(control) => !control.is_enabled(),
        Input::Choice(control) => !control.is_enabled(),
        Input::Text(control) => !control.is_enabled(),
        Input::Slider { slider, .. } => !slider.is_enabled(),
        Input::Button(control) => !control.is_enabled(),
    }
}
