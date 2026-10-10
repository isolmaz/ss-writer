//! Commands that do not apply in the current context (spec §7, manual test SRCH-06): the
//! palette and Quick Open's `>` mode list them drawn disabled and refuse to run them, by
//! keyboard, pointer and assistive technology alike.

mod search_common;

use search_common::{Host, Shortcuts, clipboard, localizer, node_named};
use ssw_core::command::CommandId;
use ssw_core::toc::Toc;
use ssw_ui::access::AccessAction;
use ssw_ui::canvas::{DrawOp, RecordingCanvas};
use ssw_ui::geometry::{Color, Rect};
use ssw_ui::input::vk;
use ssw_ui::shell::palette::CommandPalette;
use ssw_ui::shell::quick_open::{QuickOpen, QuickOpenEvent, QuickOpenMode};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;

const WINDOW: Rect = Rect::new(0.0, 0.0, 800.0, 600.0);

/// The palette filtered to "bold" with every command but Bold available.
fn bold_is_unavailable() -> (Host, QuickOpen) {
    let mut host = Host::new();
    let mut quick =
        QuickOpen::new(localizer(), &Shortcuts, &clipboard()).with_motion(Motion::reduced());
    quick.set_files(Vec::new(), Vec::new());
    quick.set_outline(&Toc::default());
    quick.open(QuickOpenMode::Commands);
    host.layout(&mut quick, WINDOW);
    let focus = quick.take_focus_request().expect("opening asks for focus");
    host.focus_on(focus);
    quick.set_command_availability(&|command| command != CommandId::FormatBold);
    host.text(&mut quick, "bold");
    (host, quick)
}

/// The color the first text of the canvas reading `text` was drawn in.
fn color_of(canvas: &RecordingCanvas, text: &str) -> Color {
    canvas
        .ops()
        .find_map(|op| match op {
            DrawOp::Text {
                text: drawn, color, ..
            } if drawn == text => Some(*color),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{text:?} was not drawn"))
}

#[test]
fn the_palette_model_marks_the_commands_that_do_not_apply() {
    let localizer = localizer();
    let mut palette = CommandPalette::new(&localizer, &Shortcuts);
    assert!(palette.is_enabled(CommandId::FormatBold));
    assert!(palette.rank("bold", 1)[0].enabled);
    palette.set_availability(&|command| command != CommandId::FormatBold);
    assert!(!palette.is_enabled(CommandId::FormatBold));
    assert!(palette.is_enabled(CommandId::FormatItalic));
    let bold = &palette.rank("bold", 1)[0];
    assert!(!bold.enabled && bold.to_row().disabled);
    let italic = &palette.rank("italic", 1)[0];
    assert!(italic.enabled && !italic.to_row().disabled);
}

#[test]
fn a_disabled_command_is_drawn_in_the_disabled_color_and_the_others_are_not() {
    let (host, quick) = bold_is_unavailable();
    let tokens = &host.theme.tokens;
    let row = &quick.list().rows()[0];
    assert_eq!((row.label.as_str(), row.disabled), ("Bold", true));
    let canvas = host.paint(&quick);
    assert_eq!(color_of(&canvas, "Bold"), tokens.text_disabled);
    assert_eq!(color_of(&canvas, "Format"), tokens.text_disabled);
    assert_eq!(color_of(&canvas, "Ctrl+B"), tokens.text_disabled);

    // The same row of a palette where everything applies uses the normal colors.
    let (mut host, mut quick) = bold_is_unavailable();
    quick.set_command_availability(&|_| true);
    assert!(!quick.list().rows()[0].disabled);
    let canvas = host.paint(&quick);
    let tokens = &host.theme.tokens;
    assert_eq!(color_of(&canvas, "Bold"), tokens.text);
    assert_eq!(color_of(&canvas, "Format"), tokens.text_muted);
    assert_eq!(color_of(&canvas, "Ctrl+B"), tokens.text_muted);
    host.key(&mut quick, vk::RETURN);
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::RunCommand(CommandId::FormatBold)]
    );
}

#[test]
fn enter_and_a_click_on_a_disabled_command_do_nothing() {
    let (mut host, mut quick) = bold_is_unavailable();
    assert!(host.key(&mut quick, vk::RETURN).handled);
    assert!(quick.is_visible(), "the palette stays open");
    assert!(quick.take_events().is_empty());
    let at = quick.list().row_rect(0).expect("row").center();
    assert!(host.click(&mut quick, at).handled);
    assert!(quick.is_visible());
    assert!(quick.take_events().is_empty());
    // Once the command applies, the same Enter runs it.
    quick.set_command_availability(&|_| true);
    host.key(&mut quick, vk::RETURN);
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::RunCommand(CommandId::FormatBold)]
    );
    assert!(!quick.is_visible());
}

#[test]
fn assistive_technology_sees_a_disabled_command_and_cannot_invoke_it() {
    let (mut host, mut quick) = bold_is_unavailable();
    let tree = host.tree(&quick);
    let list_id = node_named(&tree, "Results").id;
    let first = tree.children(list_id).next().expect("a row").clone();
    assert!(first.state.disabled);
    assert!(!first.patterns.invoke);
    let mut cx = host.cx(quick.id());
    assert!(!quick.perform_child(&mut cx, first.id, &AccessAction::Invoke));
    assert!(quick.is_visible());
    assert!(quick.take_events().is_empty());

    // Once the command applies the row is invokable.
    quick.set_command_availability(&|_| true);
    let tree = host.tree(&quick);
    let list_id = node_named(&tree, "Results").id;
    let first = tree.children(list_id).next().expect("a row").clone();
    assert!(!first.state.disabled && first.patterns.invoke);
    let mut cx = host.cx(quick.id());
    assert!(quick.perform_child(&mut cx, first.id, &AccessAction::Invoke));
    assert_eq!(
        quick.take_events(),
        vec![QuickOpenEvent::RunCommand(CommandId::FormatBold)]
    );
}
