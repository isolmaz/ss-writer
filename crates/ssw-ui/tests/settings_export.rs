//! The export dialog (spec §9): formats and options defaulting from the Export settings, validation
//! and the resulting `ExportRequest`.

mod settings_support;

use std::cell::RefCell;
use std::rc::Rc;

use settings_support::{Host, localizer};
use ssw_core::settings::{ExportFormat, ExportSettings, PageSize};
use ssw_ui::access::{AccessAction, Role};
use ssw_ui::canvas::DrawOp;
use ssw_ui::geometry::{Color, Point, Rect};
use ssw_ui::input::{
    Event, Modifiers, PointerButton, PointerEvent, PointerEventKind, TextInput, vk,
};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::shell::export_dialog::{ExportDialog, ExportEvent, ExportOptions, ExportRequest};
use ssw_ui::shell::settings::FieldId;
use ssw_ui::widgets::anim::Motion;

const WINDOW: Rect = Rect::new(0.0, 0.0, 900.0, 700.0);
/// A window too short for the options: the panel (window minus 24 DIPs margin on each side) leaves
/// the scrolling options column 204 DIPs.
const SHORT: Rect = Rect::new(0.0, 0.0, 900.0, 400.0);
/// Height of the panel above the options column (title and captions) and below it (buttons).
const BODY_TOP: f32 = 84.0;
const FOOTER: f32 = 64.0;
/// Where the options column starts inside the panel and how wide it is.
const COLUMN_X: f32 = 240.0;
const COLUMN_WIDTH: f32 = 520.0;

fn dialog_in(settings: &ExportSettings, window: Rect) -> (ExportDialog, Host) {
    let mut dialog = ExportDialog::new(
        settings,
        localizer(),
        Rc::new(RefCell::new(MemoryClipboard::default())),
        Motion::reduced(),
    );
    let mut host = Host::new(window);
    host.layout(&mut dialog);
    (dialog, host)
}

fn dialog(settings: &ExportSettings) -> (ExportDialog, Host) {
    dialog_in(settings, WINDOW)
}

fn defaults() -> (ExportDialog, Host) {
    dialog(&ExportSettings::default())
}

fn press(dialog: &mut ExportDialog, host: &mut Host, name: &str) {
    let button = host.node(dialog, Role::Button, name);
    host.click(dialog, button.bounds.center());
}

const FORMAT_NAMES: [&str; 11] = [
    "PDF",
    "PNG image",
    "JPG image",
    "HTML (single file)",
    "ePub",
    "Word (DOCX)",
    "OpenDocument (ODT)",
    "Rich Text (RTF)",
    "Plain text",
    "Markdown with attachments",
    "TextBundle",
];

#[test]
fn the_dialog_starts_from_the_export_settings() {
    let settings = ExportSettings {
        page_size: PageSize::Letter,
        margin_mm: 15.0,
        base_font_size_pt: 12.0,
        page_numbers: false,
        black_text: true,
        light_theme: false,
        include_title: false,
        default_format: ExportFormat::Docx,
        ..ExportSettings::default()
    };
    let (mut dialog, mut host) = self::dialog(&settings);
    assert_eq!(dialog.format(), ExportFormat::Docx);
    assert_eq!(dialog.options(), ExportOptions::from(&settings));
    assert!(dialog.is_open() && dialog.can_export());
    // Every format is listed and the default one is selected.
    let tree = host.tree(&dialog);
    let rows: Vec<_> = tree
        .nodes()
        .iter()
        .filter(|n| n.role == Role::ListItem)
        .collect();
    assert_eq!(
        rows.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(),
        FORMAT_NAMES
    );
    let selected: Vec<&str> = rows
        .iter()
        .filter(|n| n.state.selected)
        .map(|n| n.name.as_str())
        .collect();
    assert_eq!(selected, ["Word (DOCX)"]);
    // The options show their values.
    assert_eq!(
        host.control(&dialog, "Page size").value.as_deref(),
        Some("Letter")
    );
    assert_eq!(
        host.control(&dialog, "Margins").value.as_deref(),
        Some("15")
    );
    assert_eq!(
        host.control(&dialog, "Base font size").value.as_deref(),
        Some("12")
    );
    let toggles = [
        ("Page numbers", false),
        ("Black text", true),
        ("Light theme", false),
        ("Include the title", false),
    ];
    for (name, on) in toggles {
        let state = host.control(&dialog, name).state.toggle;
        assert_eq!(
            state.map(|s| s == ssw_ui::access::ToggleState::On),
            Some(on),
            "{name}"
        );
    }
    // Exporting right away requests exactly those choices.
    press(&mut dialog, &mut host, "Export");
    assert_eq!(
        dialog.take_events(),
        [ExportEvent::Export(ExportRequest {
            format: ExportFormat::Docx,
            options: ExportOptions::from(&settings),
        })]
    );
    assert!(!dialog.is_open());
}

#[test]
fn the_options_convert_from_the_settings() {
    let options = ExportOptions::from(&ExportSettings::default());
    assert_eq!(options.page_size, PageSize::A4);
    assert_eq!(options.margin_mm, 20.0);
    assert_eq!(options.base_font_size_pt, 11.0);
    assert!(options.page_numbers && !options.black_text && options.light_theme);
    assert!(options.include_title);
}

#[test]
fn the_request_carries_every_choice() {
    let (mut dialog, mut host) = defaults();
    // Format: keyboard in the list.
    let list = host.node(&dialog, Role::List, "Format");
    assert!(host.perform(&mut dialog, list.id, AccessAction::Focus));
    host.key(&mut dialog, vk::DOWN, Modifiers::NONE);
    assert_eq!(dialog.format(), ExportFormat::Png);
    // Page size: drop-down.
    host.focus_control(&mut dialog, "Page size");
    host.key(&mut dialog, vk::DOWN, Modifiers::NONE);
    assert_eq!(dialog.options().page_size, PageSize::Letter);
    // Switches.
    for name in [
        "Page numbers",
        "Black text",
        "Light theme",
        "Include the title",
    ] {
        let node = host.control(&dialog, name);
        assert!(host.perform(&mut dialog, node.id, AccessAction::Toggle));
    }
    // Typed numbers.
    let margin = host.control(&dialog, "Margins");
    host.perform(&mut dialog, margin.id, AccessAction::SetValue("30".into()));
    let size = host.control(&dialog, "Base font size");
    host.perform(&mut dialog, size.id, AccessAction::SetValue("12.5".into()));
    let expected = ExportOptions {
        page_size: PageSize::Letter,
        margin_mm: 30.0,
        base_font_size_pt: 12.5,
        page_numbers: false,
        black_text: true,
        light_theme: false,
        include_title: false,
    };
    assert_eq!(dialog.options(), expected);
    press(&mut dialog, &mut host, "Export");
    assert_eq!(
        dialog.take_events(),
        [ExportEvent::Export(ExportRequest {
            format: ExportFormat::Png,
            options: expected,
        })]
    );
}

#[test]
fn invalid_numbers_block_the_export_until_fixed() {
    let (mut dialog, mut host) = defaults();
    let margin = host.control(&dialog, "Margins");
    host.perform(&mut dialog, margin.id, AccessAction::SetValue("500".into()));
    assert_eq!(
        dialog.error(FieldId::ExportMargin),
        Some("Enter a value from 0 to 100.")
    );
    assert!(host.has_node(&dialog, Role::Text, "Enter a value from 0 to 100."));
    assert!(!dialog.can_export());
    assert_eq!(dialog.options().margin_mm, 20.0);
    let export = host.node(&dialog, Role::Button, "Export");
    assert!(export.state.disabled);
    host.click(&mut dialog, export.bounds.center());
    assert!(dialog.take_events().is_empty());
    assert!(dialog.is_open());
    // Enter in a text box does not export an invalid dialog either.
    host.focus_control(&mut dialog, "Margins");
    host.key(&mut dialog, vk::RETURN, Modifiers::NONE);
    assert!(dialog.take_events().is_empty());
    // The message is drawn in the error color.
    let canvas = host.paint(&dialog);
    let error = host.theme.tokens.error;
    assert!(canvas.ops().any(|op| matches!(
        op,
        DrawOp::Text { text, color, .. } if text == "Enter a value from 0 to 100." && *color == error
    )));
    // A second field fails independently.
    let size = host.control(&dialog, "Base font size");
    host.perform(&mut dialog, size.id, AccessAction::SetValue("abc".into()));
    assert_eq!(
        dialog.error(FieldId::ExportFontSize),
        Some("Enter a number.")
    );
    host.perform(&mut dialog, margin.id, AccessAction::SetValue("25".into()));
    assert_eq!(dialog.error(FieldId::ExportMargin), None);
    assert_eq!(dialog.options().margin_mm, 25.0);
    assert!(!dialog.can_export());
    host.perform(&mut dialog, size.id, AccessAction::SetValue("14".into()));
    assert!(dialog.can_export());
    assert_eq!(dialog.error(FieldId::ExportFontSize), None);
    assert_eq!(dialog.error(FieldId::Autosave), None);
    assert!(!host.node(&dialog, Role::Button, "Export").state.disabled);
}

#[test]
fn enter_exports_from_the_list_and_from_a_text_box() {
    let (mut dialog, mut host) = defaults();
    let list = host.node(&dialog, Role::List, "Format");
    host.perform(&mut dialog, list.id, AccessAction::Focus);
    host.key(&mut dialog, vk::RETURN, Modifiers::NONE);
    let events = dialog.take_events();
    assert!(matches!(
        events.as_slice(),
        [ExportEvent::Export(ExportRequest {
            format: ExportFormat::Pdf,
            ..
        })]
    ));
    assert!(!dialog.is_open());
    dialog.open();
    host.layout(&mut dialog);
    host.focus_control(&mut dialog, "Margins");
    host.key(&mut dialog, vk::RETURN, Modifiers::NONE);
    assert_eq!(dialog.take_events().len(), 1);
}

#[test]
fn cancel_and_escape_dismiss_the_dialog() {
    let (mut dialog, mut host) = defaults();
    press(&mut dialog, &mut host, "Cancel");
    assert_eq!(dialog.take_events(), [ExportEvent::Cancel]);
    assert!(!dialog.is_open());
    // A closed dialog ignores input and describes nothing.
    assert!(!host.key(&mut dialog, vk::ESCAPE, Modifiers::NONE).handled);
    let id = dialog_id(&dialog);
    assert!(!host.perform(&mut dialog, id, AccessAction::Focus));
    let tree = host.tree(&dialog);
    let root = tree.root().unwrap_or_else(|| panic!("no root"));
    assert!(root.state.offscreen && root.children.is_empty());
    assert!(host.paint(&dialog).recorded().is_empty());
    dialog.open();
    host.layout(&mut dialog);
    host.key(&mut dialog, vk::ESCAPE, Modifiers::NONE);
    assert_eq!(dialog.take_events(), [ExportEvent::Cancel]);
}

fn dialog_id(dialog: &ExportDialog) -> ssw_ui::widget::WidgetId {
    use ssw_ui::widgets::Control;
    dialog.id()
}

#[test]
fn escape_closes_an_open_drop_down_before_the_dialog() {
    let (mut dialog, mut host) = defaults();
    let size = host.focus_control(&mut dialog, "Page size");
    assert!(host.perform(&mut dialog, size.id, AccessAction::Expand));
    assert_eq!(
        host.control(&dialog, "Page size").state.expanded,
        Some(true)
    );
    host.key(&mut dialog, vk::ESCAPE, Modifiers::NONE);
    assert!(dialog.is_open());
    assert_eq!(
        host.control(&dialog, "Page size").state.expanded,
        Some(false)
    );
    assert!(dialog.take_events().is_empty());
    host.key(&mut dialog, vk::ESCAPE, Modifiers::NONE);
    assert_eq!(dialog.take_events(), [ExportEvent::Cancel]);
}

#[test]
fn the_dialog_is_modal() {
    let (mut dialog, mut host) = defaults();
    let outside = Point::new(3.0, 3.0);
    host.click(&mut dialog, outside);
    assert!(dialog.is_open());
    assert!(host.wheel(&mut dialog, outside, -10.0).handled);
    let typed = host.event(&mut dialog, Event::Text(TextInput("x".into())));
    assert!(typed.handled);
    assert!(dialog.take_events().is_empty());
}

#[test]
fn the_dialog_is_one_access_tree_inside_the_window() {
    let (dialog, host) = defaults();
    let tree = host.tree(&dialog);
    let root = tree.root().unwrap_or_else(|| panic!("no root"));
    assert_eq!(root.role, Role::Window);
    assert_eq!(root.name, "Export");
    assert!(!root.state.offscreen);
    assert!(WINDOW.contains_rect(root.bounds), "{:?}", root.bounds);
    assert_eq!(tree.walk().len(), tree.len());
    for name in [
        "Page size",
        "Margins",
        "Base font size",
        "Page numbers",
        "Black text",
        "Light theme",
        "Include the title",
    ] {
        let node = host.control(&dialog, name);
        assert!(
            root.bounds.contains_rect(node.bounds),
            "{name} is inside the panel"
        );
    }
    assert_eq!(host.control(&dialog, "Margins").description, "Millimeters.");
    assert_eq!(
        host.control(&dialog, "Base font size").description,
        "Points."
    );
}

#[test]
fn the_dialog_paints_a_scrim_the_title_and_both_columns() {
    let (dialog, host) = defaults();
    let canvas = host.paint(&dialog);
    let scrim = Color::BLACK.with_alpha(0.4);
    assert!(canvas.ops().any(|op| matches!(
        op,
        DrawOp::FillRect { rect, color } if *rect == WINDOW && *color == scrim
    )));
    let texts = canvas.texts();
    for expected in ["Export", "Format", "Options", "PDF", "Cancel", "Page size"] {
        assert!(texts.contains(&expected), "{expected:?} is drawn");
    }
}

const OPTIONS: [&str; 7] = [
    "Page size",
    "Margins",
    "Base font size",
    "Page numbers",
    "Black text",
    "Light theme",
    "Include the title",
];

/// The panel and the column the options show in.
fn panel_and_column(dialog: &ExportDialog, host: &Host) -> (Rect, Rect) {
    let tree = host.tree(dialog);
    let panel = tree
        .root()
        .map(|root| root.bounds)
        .unwrap_or_else(|| panic!("no root"));
    let column = Rect::new(
        panel.x + COLUMN_X,
        panel.y + BODY_TOP,
        COLUMN_WIDTH,
        panel.height - BODY_TOP - FOOTER,
    );
    (panel, column)
}

/// How far the options are scrolled and how far they can be.
fn scrolled(dialog: &ExportDialog, host: &Host) -> (f64, f64) {
    let bar = host.node(dialog, Role::ScrollBar, "Options scroll bar");
    let range = bar
        .patterns
        .range_value
        .unwrap_or_else(|| panic!("the scroll bar has no range"));
    (range.value, range.max)
}

#[test]
fn a_tall_window_shows_every_option_without_a_scroll_bar() {
    let (mut dialog, mut host) = defaults();
    let (_, column) = panel_and_column(&dialog, &host);
    for name in OPTIONS {
        let node = host.control(&dialog, name);
        assert!(!node.state.offscreen, "{name}");
        assert!(
            column.contains_rect(node.bounds),
            "{name}: {:?}",
            node.bounds
        );
    }
    assert!(!host.has_node(&dialog, Role::ScrollBar, "Options scroll bar"));
    // A wheel over the options has nothing to scroll.
    let before = host.control(&dialog, "Margins").bounds;
    let over = Point::new(column.x + 100.0, column.y + 50.0);
    assert!(host.wheel(&mut dialog, over, -100.0).handled);
    assert_eq!(host.control(&dialog, "Margins").bounds, before);
}

#[test]
fn a_short_window_scrolls_the_options_and_keyboard_focus_scrolls_into_view() {
    let (mut dialog, mut host) = dialog_in(&ExportSettings::default(), SHORT);
    let (panel, column) = panel_and_column(&dialog, &host);
    // The panel fits the window; the title, captions, formats and buttons keep their place.
    assert!(SHORT.contains_rect(panel), "{panel:?}");
    assert_eq!(panel.height, SHORT.height - 48.0);
    let export = host.node(&dialog, Role::Button, "Export");
    assert!(panel.contains_rect(export.bounds), "{:?}", export.bounds);
    // At the top the first option is shown and the last one is below the column.
    let first = host.control(&dialog, "Page size");
    assert!(column.contains_rect(first.bounds), "{:?}", first.bounds);
    let last = host.control(&dialog, "Include the title");
    assert!(last.state.offscreen && last.bounds.y >= column.bottom());
    let (value, max) = scrolled(&dialog, &host);
    assert_eq!(value, 0.0);
    assert!(max > 0.0);
    let bar = host.node(&dialog, Role::ScrollBar, "Options scroll bar");
    assert!(!bar.state.offscreen);
    let tree = host.tree(&dialog);
    assert!(
        tree.root()
            .is_some_and(|root| root.children.contains(&bar.id))
    );
    assert_eq!(tree.walk().len(), tree.len());

    // Moving keyboard focus to the last option scrolls it into view.
    let last = host.focus_control(&mut dialog, "Include the title");
    assert!(last.state.focused && !last.state.offscreen);
    assert!(column.contains_rect(last.bounds), "{:?}", last.bounds);
    assert!(scrolled(&dialog, &host).0 > 0.0);
    // And back to the first one.
    let first = host.focus_control(&mut dialog, "Page size");
    assert!(column.contains_rect(first.bounds), "{:?}", first.bounds);
    assert_eq!(scrolled(&dialog, &host).0, 0.0);
    // Every option can be reached.
    for name in OPTIONS {
        let node = host.focus_control(&mut dialog, name);
        assert!(
            column.contains_rect(node.bounds),
            "{name}: {:?}",
            node.bounds
        );
    }
}

#[test]
fn the_wheel_scrolls_the_options_and_clicks_land_on_what_is_drawn() {
    let (mut dialog, mut host) = dialog_in(&ExportSettings::default(), SHORT);
    let (panel, column) = panel_and_column(&dialog, &host);
    let over = Point::new(column.x + 100.0, column.y + 50.0);
    let before = host.control(&dialog, "Black text").bounds;
    assert!(host.wheel(&mut dialog, over, -100.0).handled);
    assert_eq!(
        host.control(&dialog, "Black text").bounds.y,
        before.y - 100.0
    );
    assert_eq!(scrolled(&dialog, &host).0, 100.0);
    // A wheel over the title row leaves the options alone; one over them goes back and forth
    // and stops at both ends.
    host.wheel(&mut dialog, Point::new(over.x, panel.y + 20.0), -50.0);
    assert_eq!(scrolled(&dialog, &host).0, 100.0);
    host.wheel(&mut dialog, over, 30.0);
    assert_eq!(scrolled(&dialog, &host).0, 70.0);
    host.wheel(&mut dialog, over, 10_000.0);
    assert_eq!(scrolled(&dialog, &host).0, 0.0);
    host.wheel(&mut dialog, over, -10_000.0);
    let (value, max) = scrolled(&dialog, &host);
    assert_eq!(value, max);

    // Clicking where the access tree puts a control toggles that control.
    let toggle = host.control(&dialog, "Include the title");
    assert!(column.contains_rect(toggle.bounds), "{:?}", toggle.bounds);
    host.click(&mut dialog, toggle.bounds.center());
    assert!(!dialog.options().include_title);
    // A control half under the caption takes clicks on its visible half only.
    let numbers = host.control(&dialog, "Page numbers").bounds;
    assert!(
        numbers.y < column.y && numbers.bottom() > column.y,
        "{numbers:?} straddles the top of {column:?}"
    );
    let x = numbers.center().x;
    host.click(&mut dialog, Point::new(x, column.y - 1.0));
    assert!(dialog.options().page_numbers);
    host.click(&mut dialog, Point::new(x, column.y + 1.0));
    assert!(!dialog.options().page_numbers);
}

#[test]
fn the_scroll_bar_follows_a_drag_and_assistive_technology() {
    let (mut dialog, mut host) = dialog_in(&ExportSettings::default(), SHORT);
    let (_, column) = panel_and_column(&dialog, &host);
    let bar = host.node(&dialog, Role::ScrollBar, "Options scroll bar");
    let x = bar.bounds.center().x;
    let pointer = |kind: PointerEventKind, y: f32| {
        Event::Pointer(PointerEvent {
            click_count: 1,
            ..PointerEvent::mouse(kind, Some(PointerButton::Left), Point::new(x, y))
        })
    };
    let grab = bar.bounds.y + 4.0;
    assert!(
        host.event(&mut dialog, pointer(PointerEventKind::Down, grab))
            .handled
    );
    host.event(&mut dialog, pointer(PointerEventKind::Move, grab + 56.0));
    host.event(&mut dialog, pointer(PointerEventKind::Up, grab + 56.0));
    let (value, max) = scrolled(&dialog, &host);
    assert!(value > 0.0 && value < max, "{value} of {max}");
    assert!(
        host.control(&dialog, "Page size").bounds.y < column.y,
        "the first option scrolled up with the drag"
    );

    assert!(host.perform(&mut dialog, bar.id, AccessAction::SetRangeValue(max)));
    assert_eq!(scrolled(&dialog, &host).0, max);
    let last = host.control(&dialog, "Include the title");
    assert!(column.contains_rect(last.bounds), "{:?}", last.bounds);
    assert!(host.perform(&mut dialog, bar.id, AccessAction::SetRangeValue(0.0)));
    let first = host.control(&dialog, "Page size");
    assert!(column.contains_rect(first.bounds), "{:?}", first.bounds);
    assert!(!host.perform(&mut dialog, bar.id, AccessAction::Invoke));
}

#[test]
fn the_options_are_clipped_to_their_column_and_the_bar_is_drawn_over_them() {
    let (mut dialog, mut host) = dialog_in(&ExportSettings::default(), SHORT);
    let (_, column) = panel_and_column(&dialog, &host);
    host.focus_control(&mut dialog, "Include the title");
    let canvas = host.paint(&dialog);
    assert!(
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::PushClipRect(rect) if *rect == column))
    );
    let drawn = canvas
        .recorded()
        .iter()
        .find_map(|recorded| match &recorded.op {
            DrawOp::Text { text, .. } if text == "Include the title" => recorded.bounds,
            _ => None,
        });
    assert!(
        drawn.is_some_and(|bounds| column.contains_rect(bounds)),
        "{drawn:?} is inside {column:?}"
    );
    let thumb = host.theme.tokens.scrollbar;
    assert!(canvas.ops().any(|op| matches!(
        op,
        DrawOp::FillRoundedRect { color, .. } if *color == thumb
    )));
}
