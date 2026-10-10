//! The export dialog (spec §9): the format list and the export options, defaulting from the
//! Export settings, ending in an [`ExportRequest`].
//!
//! The dialog is a modal overlay like `ContentDialog`: it covers the window it is shown in, takes
//! every input while open and closes itself with [`ExportEvent::Export`] or
//! [`ExportEvent::Cancel`]. The app asks for the destination file and runs the export.

use std::fmt;
use std::rc::Rc;

use ssw_core::i18n::Localizer;
use ssw_core::settings::{ExportFormat, ExportSettings, PageSize};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash};
use crate::geometry::{Color, Point, Rect, Size};
use crate::input::{Event, vk};
use crate::text::FontWeight;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::button::{Button, ButtonKind};
use crate::widgets::combo::ComboBox;
use crate::widgets::list::{ListEvent, ListView, SelectionMode};
use crate::widgets::textbox::TextBox;
use crate::widgets::toggle::ToggleSwitch;
use crate::widgets::{
    CONTROL_HEIGHT, Control, FONT_SIZE, SharedClipboard, paint_overlay, ui_style,
};

use super::settings::fields::{FieldId, display_number};
use super::settings::form::{Change, Form, FormScroll, Input, Row, forward};

/// Widest panel.
const PANEL_WIDTH: f32 = 760.0;
/// Narrowest panel.
const MIN_WIDTH: f32 = 420.0;
/// Shortest panel.
const MIN_HEIGHT: f32 = 300.0;
/// Space kept between the panel and the window edges.
const PANEL_MARGIN: f32 = 24.0;
/// Height of the title row.
const TITLE_HEIGHT: f32 = 56.0;
/// Height of the captions above the two columns.
const CAPTION_HEIGHT: f32 = 28.0;
/// Height of the button row.
const FOOTER_HEIGHT: f32 = 64.0;
/// Height of everything in the panel but the two columns' bodies.
const CHROME_HEIGHT: f32 = TITLE_HEIGHT + CAPTION_HEIGHT + FOOTER_HEIGHT;
/// Padding at the panel's edges.
const PAD: f32 = 16.0;
/// Width of the format column.
const LIST_WIDTH: f32 = 240.0;
/// Gap between the buttons.
const BUTTON_GAP: f32 = 8.0;
/// Em size of the title.
const TITLE_SIZE: f32 = 20.0;
/// Opacity of the scrim behind the panel.
const SCRIM_OPACITY: f32 = 0.4;

/// The formats in list order with the Fluent id of their names.
static FORMATS: [(ExportFormat, &str); 11] = [
    (ExportFormat::Pdf, "settings-opt-format-pdf"),
    (ExportFormat::Png, "settings-opt-format-png"),
    (ExportFormat::Jpg, "settings-opt-format-jpg"),
    (ExportFormat::Html, "settings-opt-format-html"),
    (ExportFormat::Epub, "settings-opt-format-epub"),
    (ExportFormat::Docx, "settings-opt-format-docx"),
    (ExportFormat::Odt, "settings-opt-format-odt"),
    (ExportFormat::Rtf, "settings-opt-format-rtf"),
    (ExportFormat::Txt, "settings-opt-format-txt"),
    (
        ExportFormat::MarkdownWithAttachments,
        "settings-opt-format-markdown",
    ),
    (ExportFormat::Textbundle, "settings-opt-format-textbundle"),
];

/// The page sizes in drop-down order with the Fluent id of their names.
static PAGE_SIZES: [(PageSize, &str); 2] = [
    (PageSize::A4, "settings-opt-page-a4"),
    (PageSize::Letter, "settings-opt-page-letter"),
];

/// The options of one export (spec §9).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportOptions {
    /// A4 or Letter.
    pub page_size: PageSize,
    /// Page margins in millimeters.
    pub margin_mm: f32,
    /// Base font size in points.
    pub base_font_size_pt: f32,
    /// Print page numbers.
    pub page_numbers: bool,
    /// Draw all text black.
    pub black_text: bool,
    /// Use the light theme colors.
    pub light_theme: bool,
    /// Put the document title at the top.
    pub include_title: bool,
}

impl From<&ExportSettings> for ExportOptions {
    fn from(settings: &ExportSettings) -> Self {
        Self {
            page_size: settings.page_size,
            margin_mm: settings.margin_mm,
            base_font_size_pt: settings.base_font_size_pt,
            page_numbers: settings.page_numbers,
            black_text: settings.black_text,
            light_theme: settings.light_theme,
            include_title: settings.include_title,
        }
    }
}

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportRequest {
    /// The chosen format.
    pub format: ExportFormat,
    /// The chosen options.
    pub options: ExportOptions,
}

/// How the dialog ended; drained with [`ExportDialog::take_events`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExportEvent {
    /// Export with these settings.
    Export(ExportRequest),
    /// The dialog was dismissed.
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionKey {
    PageSize,
    Margin,
    FontSize,
    PageNumbers,
    BlackText,
    LightTheme,
    IncludeTitle,
}

/// The export dialog.
pub struct ExportDialog {
    id: WidgetId,
    loc: Rc<Localizer>,
    formats: ListView,
    form: Form<OptionKey>,
    /// The options column: scrolls when the window is shorter than the options.
    scroll: FormScroll,
    export: Button,
    cancel: Button,
    format: ExportFormat,
    options: ExportOptions,
    open: bool,
    bounds: Rect,
    panel: Rect,
    events: Vec<ExportEvent>,
}

impl fmt::Debug for ExportDialog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportDialog")
            .field("id", &self.id)
            .field("format", &self.format)
            .field("options", &self.options)
            .field("open", &self.open)
            .finish_non_exhaustive()
    }
}

impl ExportDialog {
    /// A dialog preset from the Export settings: their default format and options.
    #[must_use]
    pub fn new(
        defaults: &ExportSettings,
        loc: Rc<Localizer>,
        clipboard: SharedClipboard,
        motion: Motion,
    ) -> Self {
        let options = ExportOptions::from(defaults);
        let formats = ListView::new(
            loc.text("export-dialog-formats"),
            FORMATS.iter().map(|(_, id)| loc.text(id)).collect(),
            SelectionMode::Single,
        )
        .with_motion(motion);
        let row = |key: OptionKey, label: &str, hint: Option<&str>, input: Input| {
            Row::new(key, loc.text(label), hint.map(|id| loc.text(id)), input)
        };
        let toggle = |label: &str, on: bool| {
            Input::Toggle(ToggleSwitch::new(loc.text(label), on).with_motion(motion))
        };
        let text = |label: &str, value: f32| {
            Input::Text(
                TextBox::new(
                    loc.text(label),
                    display_number(f64::from(value)),
                    Rc::clone(&clipboard),
                )
                .with_motion(motion),
            )
        };
        let page_size = Input::Choice(Box::new(
            ComboBox::new(
                loc.text("settings-export-page-size"),
                PAGE_SIZES.iter().map(|(_, id)| loc.text(id)).collect(),
                PAGE_SIZES
                    .iter()
                    .position(|(size, _)| *size == options.page_size),
            )
            .with_motion(motion),
        ));
        let form = Form::new(vec![
            row(
                OptionKey::PageSize,
                "settings-export-page-size",
                None,
                page_size,
            ),
            row(
                OptionKey::Margin,
                "settings-export-margin",
                Some("settings-hint-mm"),
                text("settings-export-margin", options.margin_mm),
            ),
            row(
                OptionKey::FontSize,
                "settings-export-font-size",
                Some("settings-hint-pt"),
                text("settings-export-font-size", options.base_font_size_pt),
            ),
            row(
                OptionKey::PageNumbers,
                "settings-export-page-numbers",
                None,
                toggle("settings-export-page-numbers", options.page_numbers),
            ),
            row(
                OptionKey::BlackText,
                "settings-export-black-text",
                None,
                toggle("settings-export-black-text", options.black_text),
            ),
            row(
                OptionKey::LightTheme,
                "settings-export-light-theme",
                None,
                toggle("settings-export-light-theme", options.light_theme),
            ),
            row(
                OptionKey::IncludeTitle,
                "settings-export-include-title",
                None,
                toggle("settings-export-include-title", options.include_title),
            ),
        ]);
        let export =
            Button::new(ButtonKind::Accent, loc.text("export-dialog-export")).with_motion(motion);
        let cancel =
            Button::new(ButtonKind::Standard, loc.text("export-dialog-cancel")).with_motion(motion);
        let scroll = FormScroll::new(loc.text("export-dialog-scroll"), motion);
        let mut dialog = Self {
            id: WidgetId::allocate(),
            loc,
            formats,
            form,
            scroll,
            export,
            cancel,
            format: defaults.default_format,
            options,
            open: true,
            bounds: Rect::ZERO,
            panel: Rect::ZERO,
            events: Vec::new(),
        };
        dialog.select_format(defaults.default_format);
        dialog
    }

    /// The control that should have the keyboard focus when the dialog opens: the format list.
    #[must_use]
    pub fn focus_id(&self) -> WidgetId {
        self.formats.id()
    }

    /// The format that will be exported.
    #[must_use]
    pub fn format(&self) -> ExportFormat {
        self.format
    }

    /// The options that will be used.
    #[must_use]
    pub fn options(&self) -> ExportOptions {
        self.options
    }

    /// Chooses `format` in the list.
    pub fn select_format(&mut self, format: ExportFormat) {
        self.format = format;
        if let Some(index) = FORMATS
            .iter()
            .position(|(candidate, _)| *candidate == format)
        {
            self.formats.select(index);
        }
    }

    /// True while the dialog is shown.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Shows the dialog again with the choices it had.
    pub fn open(&mut self) {
        self.open = true;
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<ExportEvent> {
        std::mem::take(&mut self.events)
    }

    /// True when every typed option is valid.
    #[must_use]
    pub fn can_export(&self) -> bool {
        !self.form.has_error()
    }

    /// The validation message under the option `field` (`ExportMargin` or `ExportFontSize`).
    #[must_use]
    pub fn error(&self, field: FieldId) -> Option<&str> {
        let key = match field {
            FieldId::ExportMargin => OptionKey::Margin,
            FieldId::ExportFontSize => OptionKey::FontSize,
            _ => return None,
        };
        self.form.error(key)
    }

    fn finish(&mut self, cx: &mut EventCx, event: ExportEvent) {
        self.events.push(event);
        self.open = false;
        cx.invalidate(self.bounds);
    }

    fn request(&mut self, cx: &mut EventCx) {
        if self.can_export() {
            let request = ExportRequest {
                format: self.format,
                options: self.options,
            };
            self.finish(cx, ExportEvent::Export(request));
        }
    }

    /// Applies what the controls reported.
    fn harvest(&mut self, cx: &mut EventCx) {
        let list_events = self.formats.take_events();
        if let Some(format) = self
            .formats
            .selected_index()
            .and_then(|index| FORMATS.get(index))
            .map(|(format, _)| *format)
        {
            self.format = format;
        }
        let activated = list_events
            .iter()
            .any(|event| matches!(event, ListEvent::Activated(_)));
        let mut submit = activated;
        for (key, change) in self.form.take_changes() {
            match (key, change) {
                (OptionKey::PageSize, Change::Selected(index)) => {
                    if let Some((size, _)) = PAGE_SIZES.get(index) {
                        self.options.page_size = *size;
                    }
                }
                (OptionKey::Margin, Change::Edited(text)) => {
                    let outcome = self.parse(FieldId::ExportMargin, OptionKey::Margin, &text);
                    if let Some(value) = outcome {
                        self.options.margin_mm = value;
                    }
                }
                (OptionKey::FontSize, Change::Edited(text)) => {
                    let outcome = self.parse(FieldId::ExportFontSize, OptionKey::FontSize, &text);
                    if let Some(value) = outcome {
                        self.options.base_font_size_pt = value;
                    }
                }
                (OptionKey::PageNumbers, Change::Toggled(on)) => self.options.page_numbers = on,
                (OptionKey::BlackText, Change::Toggled(on)) => self.options.black_text = on,
                (OptionKey::LightTheme, Change::Toggled(on)) => self.options.light_theme = on,
                (OptionKey::IncludeTitle, Change::Toggled(on)) => self.options.include_title = on,
                // Only Enter in a text box is left: no other control reports anything else.
                (_, change) => submit |= change == Change::Submitted,
            }
        }
        self.export.set_enabled(self.can_export());
        let pressed = self.export.take_activated();
        if self.cancel.take_activated() {
            self.finish(cx, ExportEvent::Cancel);
        } else if submit || pressed {
            self.request(cx);
        }
        self.scroll.reveal(cx, &self.form);
    }

    /// Validates a typed number with the range of the matching Export setting.
    fn parse(&mut self, field: FieldId, key: OptionKey, text: &str) -> Option<f32> {
        match field.def().parse_input(text) {
            Ok(value) => {
                self.form.set_error(key, None);
                Some(value.number() as f32)
            }
            Err(problem) => {
                self.form.set_error(key, Some(problem.message(&self.loc)));
                None
            }
        }
    }

    fn buttons_mut(&mut self) -> [&mut Button; 2] {
        [&mut self.cancel, &mut self.export]
    }
}

impl Control for ExportDialog {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for ExportDialog {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        // The dialog covers the window it is shown in.
        Size::new(
            if available.width.is_finite() {
                available.width
            } else {
                PANEL_WIDTH + 2.0 * PANEL_MARGIN
            },
            if available.height.is_finite() {
                available.height
            } else {
                MIN_HEIGHT + 2.0 * PANEL_MARGIN
            },
        )
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        if !self.open {
            return;
        }
        let width = (bounds.width - 2.0 * PANEL_MARGIN).clamp(MIN_WIDTH, PANEL_WIDTH);
        let form_width = width - LIST_WIDTH;
        let wanted = self.form.height(cx, form_width);
        // The columns' bodies are as tall as the options need, or as the window leaves room
        // for: the options scroll when they need more.
        let body_height = wanted
            .min(bounds.height - 2.0 * PANEL_MARGIN - CHROME_HEIGHT)
            .max(MIN_HEIGHT - CHROME_HEIGHT);
        let height = CHROME_HEIGHT + body_height;
        self.panel = Rect::new(
            bounds.x + (bounds.width - width) / 2.0,
            bounds.y + (bounds.height - height) / 2.0,
            width,
            height,
        );
        let body_top = self.panel.y + TITLE_HEIGHT + CAPTION_HEIGHT;
        let list_id = self.formats.id();
        self.formats.arrange(
            &mut cx.for_widget(list_id),
            Rect::new(
                self.panel.x + PAD,
                body_top,
                LIST_WIDTH - 2.0 * PAD,
                body_height,
            ),
        );
        let viewport = Rect::new(self.panel.x + LIST_WIDTH, body_top, form_width, body_height);
        let content = self.form.layout(cx, viewport);
        self.scroll.arrange(cx, viewport, content);
        let mut right = self.panel.right() - PAD;
        let top = self.panel.bottom() - (FOOTER_HEIGHT + CONTROL_HEIGHT) / 2.0;
        // Laid out from the right edge: the accent button ends up rightmost.
        for button in [&mut self.export, &mut self.cancel] {
            let id = button.id();
            let size = button.measure(&mut cx.for_widget(id), Size::UNBOUNDED);
            right -= size.width;
            button.arrange(
                &mut cx.for_widget(id),
                Rect::new(right, top, size.width, CONTROL_HEIGHT),
            );
            right -= BUTTON_GAP;
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !self.open || !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        canvas.fill_rect(self.bounds, Color::BLACK.with_alpha(SCRIM_OPACITY));
        paint_overlay(canvas, self.panel, tokens.surface, cx.theme);
        let title = cx.text.layout(
            &self.loc.text("export-dialog-title"),
            &ui_style(cx.theme, TITLE_SIZE).with_weight(FontWeight::SEMIBOLD),
            &[],
            &[],
            f32::INFINITY,
        );
        canvas.draw_text_layout(
            title.as_ref(),
            Point::new(
                self.panel.x + PAD,
                self.panel.y + (TITLE_HEIGHT - title.size().height) / 2.0,
            ),
            tokens.text,
        );
        for (caption, x) in [
            ("export-dialog-formats", self.panel.x + PAD),
            ("export-dialog-options", self.panel.x + LIST_WIDTH + PAD),
        ] {
            let layout = cx.text.layout(
                &self.loc.text(caption),
                &ui_style(cx.theme, FONT_SIZE).with_weight(FontWeight::SEMIBOLD),
                &[],
                &[],
                f32::INFINITY,
            );
            canvas.draw_text_layout(
                layout.as_ref(),
                Point::new(
                    x,
                    self.panel.y + TITLE_HEIGHT + (CAPTION_HEIGHT - layout.size().height) / 2.0,
                ),
                tokens.text_muted,
            );
        }
        let split_x = self.panel.x + LIST_WIDTH;
        canvas.stroke_line(
            Point::new(split_x, self.panel.y + TITLE_HEIGHT),
            Point::new(split_x, self.panel.bottom() - FOOTER_HEIGHT),
            tokens.hairline,
            1.0,
            Dash::Solid,
        );
        self.formats
            .paint(&cx.for_widget(self.formats.id()), canvas);
        self.scroll.paint(cx, canvas, &self.form);
        for button in [&self.cancel, &self.export] {
            button.paint(&cx.for_widget(button.id()), canvas);
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        if !self.open {
            return EventResult::ignored();
        }
        let mut result = forward(&mut self.formats, cx, e);
        result = result.or(self.scroll.event(cx, &mut self.form, e));
        for button in self.buttons_mut() {
            result = result.or(forward(button, cx, e));
        }
        let dismiss = matches!(e, Event::Key(key)
            if key.is_down && key.vk == vk::ESCAPE && key.mods.is_empty())
            && !result.handled;
        self.harvest(cx);
        if dismiss && self.open {
            self.finish(cx, ExportEvent::Cancel);
        }
        // Modal: nothing behind the dialog sees pointer, wheel or text input.
        if matches!(
            e,
            Event::Pointer(_) | Event::Wheel(_) | Event::Text(_) | Event::Ime(_)
        ) {
            result = result.or(EventResult::handled());
        }
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::new();
        if self.open {
            self.formats.access(&cx.for_widget(self.formats.id()), out);
            children.push(self.formats.id());
            children.extend(self.scroll.access(cx, out, &self.form));
            for button in [&self.cancel, &self.export] {
                button.access(&cx.for_widget(button.id()), out);
                children.push(button.id());
            }
        }
        let bounds = if self.open { self.panel } else { Rect::ZERO };
        let mut node = AccessNode::new(
            self.id,
            Role::Window,
            self.loc.text("export-dialog-title"),
            bounds,
        )
        .with_children(children);
        node.state.offscreen = !self.open;
        out.insert(node);
    }

    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        if !self.open {
            return false;
        }
        let target = cx.id;
        let handled = if target == self.formats.id() {
            self.formats.perform(&mut cx.for_widget(target), a)
        } else if target == self.export.id() {
            self.export.perform(&mut cx.for_widget(target), a)
        } else if target == self.cancel.id() {
            self.cancel.perform(&mut cx.for_widget(target), a)
        } else if self.scroll.perform(cx, &mut self.form, a) {
            true
        } else {
            self.formats
                .perform_item(&mut cx.for_widget(target), target, a)
        };
        self.harvest(cx);
        handled
    }
}
