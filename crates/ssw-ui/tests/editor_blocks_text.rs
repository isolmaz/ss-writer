//! Lists, tasks, quotes, callouts, dividers, page breaks, front matter, raw HTML and footnote
//! definitions: what the renderers draw (recorded canvas operations), how carets, hits and
//! selections map through nested containers, and what the pointer can press.

mod editor_common;

use std::rc::Rc;

use ssw_core::document::Document;
use ssw_core::i18n::Localizer;
use ssw_core::markdown::ast::{Block, CalloutKind};
use ssw_core::text::TextRange;
use ssw_ui::canvas::{Canvas, Dash, DrawOp, IconFont, RecordingCanvas};
use ssw_ui::editor::block::{
    BlockClass, BlockCx, BlockEdit, BlockLayout, BlockPaintCx, BlockPress, BlockRegistry,
    BlockRenderer, GapLayout, HeadingRenderer,
};
use ssw_ui::editor::blocks_text::{self, BlockStrings};
use ssw_ui::editor::config::LinkTarget;
use ssw_ui::editor::display::Reveal;
use ssw_ui::editor::object::NoObjects;
use ssw_ui::editor::{Composition, EditorConfig, EditorEvent, EditorStrings};
use ssw_ui::geometry::{Color, CornerRadii, Point, Rect, Size};
use ssw_ui::input::{Cursor, Modifiers, PointerEventKind};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{CalloutKind as ThemeKind, DEFAULT_ACCENT, Theme};

use editor_common::{Harness, TOP, close};

/// The text column of every layout in these tests.
const WIDTH: f32 = 352.0;
/// Advance of one grapheme of the test engine.
const ADVANCE: f32 = 8.0;

/// A document, the services of a layout pass and the registry with this module's renderers.
struct Fixture {
    doc: Document,
    theme: Theme,
    config: EditorConfig,
    engine: MonoTextEngine,
    strings: EditorStrings,
    registry: BlockRegistry,
}

impl Fixture {
    fn new(text: &str) -> Self {
        let mut registry = BlockRegistry::with_defaults();
        blocks_text::register(&mut registry);
        Self {
            doc: Document::new(text),
            theme: Theme::light(DEFAULT_ACCENT),
            config: EditorConfig::default(),
            engine: MonoTextEngine::default(),
            strings: EditorStrings::default(),
            registry,
        }
    }

    fn body(&self) -> f32 {
        self.config.body_size()
    }

    fn line(&self) -> f32 {
        self.config.line_px()
    }

    fn block(&self, index: usize) -> &Block {
        &self.doc.ast().blocks[index]
    }

    /// Measures top-level block `index` with a caret at each of `carets`.
    fn measure(&self, index: usize, carets: &[usize]) -> Box<dyn BlockLayout> {
        self.measure_with(index, carets, None, false)
    }

    fn measure_with(
        &self,
        index: usize,
        carets: &[usize],
        composition: Option<&Composition>,
        reveal_all: bool,
    ) -> Box<dyn BlockLayout> {
        let reveal = Reveal::new(
            reveal_all,
            carets.iter().map(|at| TextRange::at(*at)).collect(),
        );
        let cx = BlockCx {
            text: &self.engine,
            theme: &self.theme,
            config: &self.config,
            buffer: self.doc.buffer(),
            ast: self.doc.ast(),
            reveal: &reveal,
            objects: &NoObjects,
            composition,
            width: WIDTH,
            folded: false,
            strings: &self.strings,
            registry: &self.registry,
        };
        self.registry.measure(&cx, self.block(index))
    }

    fn paint(&self, layout: &dyn BlockLayout) -> RecordingCanvas {
        let cx = BlockPaintCx {
            theme: &self.theme,
            config: &self.config,
            strings: &self.strings,
            text: &self.engine,
            hovered: false,
        };
        let mut canvas = RecordingCanvas::new();
        layout.paint_backdrop(&cx, &mut canvas);
        layout.paint(&cx, &mut canvas);
        assert!(canvas.is_balanced());
        canvas
    }

    fn painted(&self, index: usize, carets: &[usize]) -> RecordingCanvas {
        self.paint(self.measure(index, carets).as_ref())
    }
}

/// Filled rounded rectangles with their root-space bounds.
fn rounded_fills(canvas: &RecordingCanvas) -> Vec<(Rect, CornerRadii, Color)> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match (&entry.op, entry.bounds) {
            (DrawOp::FillRoundedRect { radii, color, .. }, Some(bounds)) => {
                Some((bounds, *radii, *color))
            }
            _ => None,
        })
        .collect()
}

/// Stroked rounded rectangles with their root-space bounds.
fn rounded_strokes(canvas: &RecordingCanvas) -> Vec<(Rect, Color)> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match (&entry.op, entry.bounds) {
            (DrawOp::StrokeRoundedRect { color, .. }, Some(bounds)) => Some((bounds, *color)),
            _ => None,
        })
        .collect()
}

fn rect_fills(canvas: &RecordingCanvas) -> Vec<(Rect, Color)> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match (&entry.op, entry.bounds) {
            (DrawOp::FillRect { color, .. }, Some(bounds)) => Some((bounds, *color)),
            _ => None,
        })
        .collect()
}

/// Drawn text layouts: text, root-space bounds, color.
fn texts(canvas: &RecordingCanvas) -> Vec<(String, Rect, Color)> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match (&entry.op, entry.bounds) {
            (DrawOp::Text { text, color, .. }, Some(bounds)) => {
                Some((text.clone(), bounds, *color))
            }
            _ => None,
        })
        .collect()
}

fn text_named(canvas: &RecordingCanvas, wanted: &str) -> (Rect, Color) {
    texts(canvas)
        .into_iter()
        .find(|(text, _, _)| text == wanted)
        .map(|(_, bounds, color)| (bounds, color))
        .unwrap_or_else(|| panic!("no text {wanted:?} drawn"))
}

fn icons(canvas: &RecordingCanvas) -> Vec<(char, Color, Point)> {
    canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::Icon {
                glyph,
                font: IconFont::SegoeFluent,
                color,
                center,
                ..
            } => Some((*glyph, *color, *center)),
            _ => None,
        })
        .collect()
}

fn lines(canvas: &RecordingCanvas) -> Vec<(Point, Point, Color, Dash)> {
    canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::StrokeLine {
                from,
                to,
                color,
                dash,
                ..
            } => Some((*from, *to, *color, *dash)),
            _ => None,
        })
        .collect()
}

fn near(a: Point, x: f32, y: f32) -> bool {
    close(a.x, x) && close(a.y, y)
}

// ----- lists ----------------------------------------------------------------------------------

#[test]
fn bullets_are_dot_ring_square_and_the_cycle_repeats() {
    let f = Fixture::new("- a\n  - b\n    - c\n      - d\n");
    let canvas = f.painted(0, &[]);
    let accent = f.theme.tokens.accent;
    let (col, line) = (1.5 * f.body(), f.line());
    // One marker column per nesting level; the marker is centred on the first line.
    let center = |level: f32| Point::new(col * level + col / 2.0, line * level + line / 2.0);

    let dots = rounded_fills(&canvas);
    assert_eq!(dots.len(), 2, "levels 0 and 3 are filled dots");
    for ((bounds, radii, color), level) in dots.iter().zip([0.0, 3.0]) {
        assert_eq!(*color, accent);
        assert!(close(bounds.width, bounds.height));
        assert!(radii.is_uniform() && close(radii.top_left, bounds.width / 2.0));
        assert!(near(bounds.center(), center(level).x, center(level).y));
    }
    let rings = rounded_strokes(&canvas);
    assert_eq!(rings.len(), 1, "level 1 is a ring");
    assert_eq!(rings[0].1, accent);
    assert!(near(rings[0].0.center(), center(1.0).x, center(1.0).y));
    let squares = rect_fills(&canvas);
    assert_eq!(squares.len(), 1, "level 2 is a filled square");
    assert_eq!(squares[0].1, accent);
    assert!(near(squares[0].0.center(), center(2.0).x, center(2.0).y));

    let drawn: Vec<String> = texts(&canvas).into_iter().map(|(t, _, _)| t).collect();
    assert_eq!(drawn, ["a", "b", "c", "d"]);
    let (b, _) = text_named(&canvas, "b");
    assert!(
        close(b.x, 2.0 * col),
        "the nested text starts two columns in"
    );
}

#[test]
fn ordered_numbers_are_right_aligned_in_a_column_as_wide_as_the_widest() {
    let f = Fixture::new("9. a\n10. b\n");
    let canvas = f.painted(0, &[]);
    let widest = 3.0 * ADVANCE;
    let col = (1.5 * f.body()).max(widest + 0.9 * f.body());
    let right = col - 0.4 * f.body();
    let (nine, color) = text_named(&canvas, "9.");
    let (ten, _) = text_named(&canvas, "10.");
    assert_eq!(color, f.theme.tokens.text_muted);
    assert!(close(nine.right(), right) && close(ten.right(), right));
    assert!(ten.x < nine.x, "the wider number grows to the left");
    assert!(close(text_named(&canvas, "a").0.x, col));
}

#[test]
fn ordered_lists_keep_their_delimiter_and_count_from_the_start_number() {
    let f = Fixture::new("3) a\n4) b\n");
    let canvas = f.painted(0, &[]);
    let drawn: Vec<String> = texts(&canvas).into_iter().map(|(t, _, _)| t).collect();
    assert_eq!(drawn, ["3)", "a", "4)", "b"]);
    assert_eq!(
        f.measure(0, &[]).access_label(&f.strings),
        BlockStrings::default().numbered_list
    );
}

#[test]
fn tasks_draw_rounded_checkboxes_and_mute_checked_text() {
    let f = Fixture::new("- [ ] open\n- [x] done\n");
    let canvas = f.painted(0, &[]);
    let tokens = &f.theme.tokens;
    let (body, line) = (f.body(), f.line());
    let side = 0.95 * body;
    let box_x = 1.5 * body - 0.4 * body - side;

    let open = rounded_strokes(&canvas);
    assert_eq!(open.len(), 1, "an unchecked box is an outline");
    assert_eq!(open[0].1, tokens.text_muted);
    assert!(near(open[0].0.center(), box_x + side / 2.0, line / 2.0));

    let done = rounded_fills(&canvas);
    assert_eq!(
        done.len(),
        1,
        "a checked box is filled; there are no bullets"
    );
    assert_eq!(done[0].2, tokens.accent);
    assert_eq!(done[0].1, CornerRadii::uniform(4.0));
    assert!(near(done[0].0.center(), box_x + side / 2.0, line * 1.5));
    let marks = icons(&canvas);
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].0, '\u{E73E}');
    assert_eq!(marks[0].1, tokens.accent_text_on);

    assert_eq!(text_named(&canvas, "open").1, tokens.text);
    assert_eq!(text_named(&canvas, "done").1, tokens.text_muted);
    assert_eq!(
        f.measure(0, &[]).access_label(&f.strings),
        BlockStrings::default().task_list
    );
}

#[test]
fn the_checkbox_rect_is_reported_for_toggling() {
    let f = Fixture::new("- [ ] open\n- [x] done\n");
    let layout = f.measure(0, &[]);
    let (body, line) = (f.body(), f.line());
    let box_center = Point::new(1.5 * body - 0.4 * body - 0.95 * body / 2.0, line / 2.0);

    let first = layout.task_box_at(box_center).expect("the first checkbox");
    assert_eq!(first.offset, 2, "the `[ ]` starts at byte 2");
    assert!(first.rect.contains(box_center));
    let second = layout
        .task_box_at(Point::new(box_center.x, box_center.y + line))
        .expect("the second checkbox");
    assert_eq!(second.offset, 13);
    assert!(
        layout
            .task_box_at(Point::new(100.0, box_center.y))
            .is_none()
    );
    assert!(
        layout
            .task_box_at(Point::new(box_center.x, 3.0 * line))
            .is_none()
    );
}

#[test]
fn a_task_item_with_the_caret_shows_its_source_and_still_reports_the_checkbox() {
    let f = Fixture::new("- [ ] open\n");
    let layout = f.measure(0, &[8]);
    let canvas = f.paint(layout.as_ref());
    assert_eq!(
        canvas.texts(),
        ["- [ ] ", "open"],
        "marker zone source, then the text"
    );
    assert!(rounded_strokes(&canvas).is_empty());
    let (open, _) = text_named(&canvas, "open");
    assert!(close(open.x, 6.0 * ADVANCE + 0.3 * f.body()));
    // The `[ ]` bytes (2..5) are the box.
    let found = layout
        .task_box_at(Point::new(3.5 * ADVANCE, f.line() / 2.0))
        .expect("the source checkbox");
    assert_eq!(found.offset, 2);
    assert!(
        layout
            .task_box_at(Point::new(0.5 * ADVANCE, f.line() / 2.0))
            .is_none(),
        "the dash is not the checkbox"
    );
}

#[test]
fn only_task_items_report_a_checkbox() {
    let f = Fixture::new("- plain\n- [ ] task\n");
    let layout = f.measure(0, &[]);
    let (body, line) = (f.body(), f.line());
    let box_center = Point::new(1.5 * body - 0.4 * body - 0.95 * body / 2.0, line / 2.0);
    assert!(
        layout.task_box_at(box_center).is_none(),
        "a bullet is not a checkbox"
    );
    let task = layout
        .task_box_at(Point::new(box_center.x, box_center.y + line))
        .expect("the task's checkbox");
    assert_eq!(task.offset, 10, "the `[ ]` of the second item");
}

#[test]
fn list_markers_show_their_source_while_the_caret_is_in_the_item() {
    let f = Fixture::new("para\n\n- a\n- b\n");
    let hidden = f.painted(1, &[0]);
    assert_eq!(rounded_fills(&hidden).len(), 2, "two dots");
    assert!(!hidden.texts().contains(&"- "));

    let shown = f.painted(1, &[8]);
    assert!(shown.texts().contains(&"- "), "the first item shows `- `");
    assert_eq!(
        rounded_fills(&shown).len(),
        1,
        "only the second item is a dot"
    );

    let all = f.paint(f.measure_with(1, &[], None, true).as_ref());
    assert!(
        all.texts().contains(&"- "),
        "Auto-hide Markdown off shows markers"
    );
    assert!(rounded_fills(&all).is_empty());
}

#[test]
fn carets_hits_and_selections_map_through_nested_lists() {
    let f = Fixture::new("- a\n  - bc\n");
    let layout = f.measure(0, &[]);
    let (col, line) = (1.5 * f.body(), f.line());

    assert_eq!(layout.height(), 2.0 * line);
    let a = layout.caret_rect(2);
    assert!(close(a.x, col) && close(a.y, 0.0) && close(a.height, line));
    let b = layout.caret_rect(8);
    assert!(close(b.x, 2.0 * col) && close(b.y, line));
    assert!(close(layout.caret_rect(9).x, 2.0 * col + ADVANCE));
    // A caret inside a hidden marker goes to the content.
    assert!(close(layout.caret_rect(0).x, col));

    assert_eq!(layout.hit_test(Point::new(col + 1.0, 5.0)), 2);
    assert_eq!(
        layout.hit_test(Point::new(2.0 * col + ADVANCE + 1.0, line + 5.0)),
        9
    );
    assert_eq!(layout.hit_test(Point::new(300.0, line + 5.0)), 10);
    assert_eq!(
        layout.hit_test(Point::new(5.0, 5.0)),
        2,
        "the marker column"
    );
    assert_eq!(
        layout.hit_test(Point::new(300.0, 400.0)),
        10,
        "below the list"
    );

    let rects = layout.selection_rects(TextRange::new(2, 10));
    assert!(
        rects
            .iter()
            .any(|r| close(r.x, col) && close(r.y, 0.0) && close(r.width, ADVANCE))
    );
    assert!(
        rects
            .iter()
            .any(|r| close(r.x, 2.0 * col) && close(r.y, line) && close(r.width, 2.0 * ADVANCE))
    );
    let marker = layout.selection_rects(TextRange::new(0, 2));
    assert_eq!(
        marker.len(),
        1,
        "a selected marker is one block over the column"
    );
    assert!(close(marker[0].width, col));

    assert_eq!(layout.line_range(2), TextRange::new(0, 3));
    assert_eq!(layout.line_range(8), TextRange::new(6, 10));
}

#[test]
fn a_caret_in_the_marker_source_maps_through_the_dimmed_text() {
    let f = Fixture::new("1. abc\n");
    let layout = f.measure(0, &[0]);
    // "1." is two graphemes wide, so the column is wider than 1.5 em.
    let col = (1.5 * f.body()).max(2.0 * ADVANCE + 0.9 * f.body());
    assert!(close(layout.caret_rect(1).x, ADVANCE));
    assert!(
        close(layout.caret_rect(3).x, col),
        "the content starts after the marker column"
    );
    assert_eq!(layout.hit_test(Point::new(ADVANCE + 1.0, 5.0)), 1);
    let rects = layout.selection_rects(TextRange::new(0, 4));
    assert!(
        rects
            .iter()
            .any(|r| close(r.x, 0.0) && close(r.width, 3.0 * ADVANCE))
    );
}

#[test]
fn loose_lists_get_blank_rows_with_paragraph_spacing() {
    let f = Fixture::new("- a\n\n- b\n");
    let (line, spacing) = (f.line(), f.config.spacing_px());
    assert!(spacing > 0.0);
    assert!(close(f.measure(0, &[]).height(), 2.0 * line + spacing));

    // With the caret on the blank line the row is a full line the caret sits on.
    let layout = f.measure(0, &[4]);
    assert!(close(layout.height(), 3.0 * line));
    let caret = layout.caret_rect(4);
    assert!(close(caret.y, line) && close(caret.height, line));
    assert_eq!(layout.hit_test(Point::new(50.0, line + 5.0)), 4);
    assert_eq!(layout.line_range(4), TextRange::new(4, 4));
    // A selection that covers the blank line's break marks it.
    assert!(!layout.selection_rects(TextRange::new(3, 6)).is_empty());
}

#[test]
fn headings_and_formatting_inside_items_use_the_editor_display_map() {
    let f = Fixture::new("- ## Title\n- **bold** text\n");
    let hidden = f.painted(0, &[]);
    let drawn: Vec<String> = texts(&hidden).into_iter().map(|(t, _, _)| t).collect();
    assert_eq!(drawn, ["Title", "bold text"]);
    let (title, _) = text_named(&hidden, "Title");
    assert!(close(
        title.height,
        f.config
            .line_height_for(1.45 * f.body(), 1.3)
            .resolve(1.45 * f.body())
    ));

    let shown = f.painted(0, &[5]);
    assert!(
        shown.texts().contains(&"## Title"),
        "heading markers reveal"
    );
    let shown = f.painted(0, &[16]);
    assert!(
        shown.texts().contains(&"**bold** text"),
        "emphasis markers reveal"
    );
}

#[test]
fn composition_text_is_displayed_inside_a_list_item() {
    let f = Fixture::new("- ab\n");
    let composition = Composition {
        at: 4,
        text: "XY".into(),
        caret: 1,
        attrs: Vec::new(),
    };
    let layout = f.measure_with(0, &[4], Some(&composition), false);
    let canvas = f.paint(layout.as_ref());
    assert!(canvas.texts().contains(&"abXY"));
    let caret = layout.caret_rect(4);
    assert!(close(caret.x, 1.5 * f.body() + 3.0 * ADVANCE));
}

// ----- quotes ---------------------------------------------------------------------------------

#[test]
fn a_quote_has_a_left_bar_and_hides_its_markers() {
    let f = Fixture::new("> a\n> b\n");
    let layout = f.measure(0, &[]);
    let canvas = f.paint(layout.as_ref());
    let line = f.line();
    let bars = rounded_fills(&canvas);
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].2, f.theme.tokens.table_border);
    assert!(close(bars[0].0.x, 0.0) && close(bars[0].0.width, 3.0));
    assert!(close(bars[0].0.height, 2.0 * line));
    assert_eq!(layout.height(), 2.0 * line);
    assert_eq!(canvas.texts(), ["a\nb"]);
    let indent = 3.0 + 0.7 * f.body();
    assert!(close(text_named(&canvas, "a\nb").0.x, indent));
    assert_eq!(
        layout.access_label(&f.strings),
        BlockStrings::default().quote
    );
}

#[test]
fn a_quote_with_the_caret_shows_the_marker_of_every_line() {
    let f = Fixture::new("> a\n> b\n");
    let layout = f.measure(0, &[2]);
    let canvas = f.paint(layout.as_ref());
    assert_eq!(canvas.texts(), ["> ", "a\n> b"]);
    let indent = 3.0 + 0.7 * f.body();
    let content = indent + 2.0 * ADVANCE + 0.3 * f.body();
    let (prefix, color) = text_named(&canvas, "> ");
    assert!(close(prefix.x, indent));
    assert_eq!(color, f.theme.tokens.marker);
    assert!(close(text_named(&canvas, "a\n> b").0.x, content));

    // Carets in the first line's marker, the second line's marker and the text.
    assert!(close(layout.caret_rect(0).x, indent));
    assert!(close(layout.caret_rect(1).x, indent + ADVANCE));
    assert!(close(layout.caret_rect(2).x, content));
    let second = layout.caret_rect(4);
    assert!(
        close(second.x, content) && close(second.y, f.line()),
        "before the second `>`"
    );
    assert!(
        close(layout.caret_rect(6).x, content + 2.0 * ADVANCE),
        "after the second `> `"
    );

    // Hits in the marker column go to the marker, hits in the text to the text.
    assert_eq!(layout.hit_test(Point::new(indent + ADVANCE + 1.0, 5.0)), 1);
    assert_eq!(layout.hit_test(Point::new(content + 1.0, 5.0)), 2);
    let marker = layout.selection_rects(TextRange::new(0, 2));
    assert!(
        marker
            .iter()
            .any(|r| close(r.x, indent) && close(r.width, 2.0 * ADVANCE))
    );
}

#[test]
fn blank_quote_lines_are_rows_the_caret_can_sit_on() {
    let f = Fixture::new("> a\n>\n> b\n");
    let (line, spacing) = (f.line(), f.config.spacing_px());
    assert!(close(f.measure(0, &[]).height(), 2.0 * line + spacing));

    let layout = f.measure(0, &[4]);
    assert!(close(layout.height(), 3.0 * line));
    let canvas = f.paint(layout.as_ref());
    assert_eq!(canvas.texts(), ["> ", "a", ">", "> ", "b"]);
    let indent = 3.0 + 0.7 * f.body();
    let caret = layout.caret_rect(5);
    assert!(close(caret.x, indent + ADVANCE) && close(caret.y, line));
    assert_eq!(layout.hit_test(Point::new(indent + 30.0, line + 5.0)), 5);
    assert_eq!(layout.line_range(5), TextRange::new(4, 5));
}

#[test]
fn a_blank_quote_line_after_the_last_child_is_a_separator_row() {
    let f = Fixture::new("> a\n>\n");
    let (line, spacing) = (f.line(), f.config.spacing_px());
    assert!(close(f.measure(0, &[]).height(), line + spacing));
    assert!(close(f.measure(0, &[4]).height(), 2.0 * line));
}

#[test]
fn a_selection_marks_the_line_break_of_a_blank_quote_line_only_when_it_includes_it() {
    let f = Fixture::new("> a\n>\n> b\n");
    let layout = f.measure(0, &[]);
    // `2..5` ends at the blank line's own break (offset 5), `2..6` takes it along.
    let short = layout.selection_rects(TextRange::new(2, 5));
    let through = layout.selection_rects(TextRange::new(2, 6));
    assert_eq!(through.len(), short.len() + 1);
    let mark = through[through.len() - 1];
    assert!(close(mark.width, 6.0) && close(mark.y, f.line()));
}

#[test]
fn lists_nest_inside_quotes_and_markers_follow_the_reveal() {
    let f = Fixture::new("> - a\n> - b\n");
    let indent = 3.0 + 0.7 * f.body();
    let hidden = f.painted(0, &[]);
    assert_eq!(rounded_fills(&hidden).len(), 3, "the bar and two dots");
    let (a, _) = text_named(&hidden, "a");
    assert!(close(a.x, indent + 1.5 * f.body()));

    let layout = f.measure(0, &[4]);
    let shown = f.paint(layout.as_ref());
    let prefixes = shown.texts().iter().filter(|t| **t == "> ").count();
    assert_eq!(
        prefixes, 2,
        "the quote's first line and the list's second line"
    );
    assert!(
        shown.texts().contains(&"- "),
        "the item with the caret shows its marker"
    );
    let caret = layout.caret_rect(10);
    assert!(
        caret.y > f.line() - 0.01,
        "the second item is on the second line"
    );
}

#[test]
fn headings_inside_quotes_have_no_gutter_widgets() {
    let f = Fixture::new("> # Title\n");
    let layout = f.measure(0, &[]);
    let canvas = f.paint(layout.as_ref());
    assert_eq!(canvas.texts(), ["Title"]);
    let size = 1.75 * f.body();
    assert!(close(
        layout.height(),
        f.config.line_height_for(size, 1.3).resolve(size)
    ));
    let shown = f.painted(0, &[4]);
    assert!(shown.texts().contains(&"# Title"));
}

// ----- callouts -------------------------------------------------------------------------------

#[test]
fn every_callout_kind_paints_its_palette_icon_and_default_title() {
    let strings = BlockStrings::default();
    for (index, kind) in CalloutKind::ALL.into_iter().enumerate() {
        let f = Fixture::new(&format!("> [!{}]\n> body\n", kind.keyword()));
        let theme_kind = ThemeKind::ALL[index];
        let style = f.theme.tokens.callout.get(theme_kind);
        let layout = f.measure(0, &[]);
        let canvas = f.paint(layout.as_ref());
        let (body, line) = (f.body(), f.line());
        let height = 0.5 * body + line + line + 0.5 * body;
        assert!(close(layout.height(), height), "{kind:?}");

        let fills = rounded_fills(&canvas);
        assert_eq!(fills.len(), 2, "the tinted card and the bar");
        assert_eq!(fills[0].2, style.background);
        assert!(close(fills[0].0.width, WIDTH) && close(fills[0].0.height, height));
        assert_eq!(fills[1].2, style.bar);
        assert!(close(fills[1].0.width, 3.0));
        let glyphs = icons(&canvas);
        assert_eq!(glyphs.len(), 1);
        assert_eq!(glyphs[0].1, style.icon);
        let (title, color) = text_named(&canvas, &strings.callout_titles[index]);
        assert_eq!(color, style.icon, "the title takes the icon color");
        assert!(close(title.x, 3.0 + 0.75 * body + body + 0.5 * body));
        assert!(close(text_named(&canvas, "body").0.x, 3.0 + 0.75 * body));
        assert_eq!(
            layout.access_label(&f.strings),
            strings.callout_access[index]
        );
    }
    // Five different icons.
    let mut all: Vec<char> = CalloutKind::ALL
        .into_iter()
        .map(|kind| {
            let f = Fixture::new(&format!("> [!{}]\n", kind.keyword()));
            icons(&f.painted(0, &[]))[0].0
        })
        .collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 5);
}

#[test]
fn a_callout_title_is_its_source_text_until_the_caret_enters() {
    let f = Fixture::new("> [!TIP] Remember\n> body\n");
    let (body, line) = (f.body(), f.line());
    let title_x = 3.0 + 0.75 * body + body + 0.5 * body;
    let hidden = f.measure(0, &[]);
    let canvas = f.paint(hidden.as_ref());
    assert!(close(text_named(&canvas, "Remember").0.x, title_x));
    assert_eq!(
        hidden.hit_test(Point::new(title_x + ADVANCE + 1.0, 12.0)),
        10
    );
    assert!(close(hidden.caret_rect(10).x, title_x + ADVANCE));
    assert_eq!(hidden.line_range(12), TextRange::new(0, 17));
    let hits = hidden.selection_rects(TextRange::new(9, 12));
    assert!(
        hits.iter()
            .any(|r| close(r.x, title_x) && close(r.width, 3.0 * ADVANCE))
    );

    let shown = f.measure(0, &[12]);
    let canvas = f.paint(shown.as_ref());
    assert!(canvas.texts().contains(&"> [!TIP] Remember"));
    assert!(canvas.texts().contains(&"> "), "the body line's marker");
    assert_eq!(
        shown.hit_test(Point::new(title_x + 3.0 * ADVANCE + 1.0, 12.0)),
        3
    );
    assert!(close(shown.caret_rect(3).x, title_x + 3.0 * ADVANCE));
    let body_caret = shown.caret_rect(24);
    assert!(
        body_caret.y > 0.5 * body + line - 0.01,
        "the body is below the header"
    );
    assert!(shown.hit_test(Point::new(title_x, 0.5 * body + line + 5.0)) > 17);
}

#[test]
fn a_callout_without_a_title_maps_its_header_to_the_tag_end() {
    let f = Fixture::new("> [!NOTE]\n> body\n");
    let layout = f.measure(0, &[]);
    assert_eq!(layout.hit_test(Point::new(100.0, 10.0)), 9);
    assert!(close(
        layout.caret_rect(3).x,
        3.0 + 0.75 * f.body() + 1.5 * f.body()
    ));
    let rects = layout.selection_rects(TextRange::new(0, 9));
    assert_eq!(rects.len(), 1, "the default title as one block");
    assert_eq!(layout.line_range(5), TextRange::new(0, 9));
}

#[test]
fn the_callout_icon_raises_the_type_menu_and_shows_a_hand() {
    let f = Fixture::new("> [!WARNING] Careful\n");
    let layout = f.measure(0, &[]);
    let (body, line) = (f.body(), f.line());
    let icon = Rect::new(
        3.0 + 0.75 * body,
        0.5 * body + (line - body) / 2.0,
        body,
        body,
    );
    let origin = Point::new(100.0, 200.0);

    let press = layout
        .press(icon.center(), origin, Modifiers::NONE)
        .expect("a press on the icon");
    let (range, kind, anchor) = match press {
        BlockPress::Event(EditorEvent::CalloutMenu {
            range,
            kind,
            anchor,
        }) => (range, kind, anchor),
        other => panic!("expected the callout menu, got {other:?}"),
    };
    assert_eq!(range, f.block(0).range);
    assert_eq!(kind, CalloutKind::Warning);
    assert!(close(anchor.x, origin.x + icon.x) && close(anchor.y, origin.y + icon.y));
    assert!(close(anchor.width, body) && close(anchor.height, body));
    assert_eq!(layout.cursor_at(icon.center()), Some(Cursor::Hand));
    assert!(
        layout
            .press(Point::new(200.0, icon.center().y), origin, Modifiers::NONE)
            .is_none()
    );
    assert_eq!(layout.cursor_at(Point::new(200.0, icon.center().y)), None);
}

#[test]
fn clicking_the_icon_in_the_editor_reports_the_menu_without_moving_the_caret() {
    let mut h = Harness::new("> [!NOTE] Title\n");
    blocks_text::register(h.view.registry_mut());
    h.arrange();
    let before = h.selection();
    h.events();
    let body = h.view.config().body_size();
    let line = h.view.config().line_px();
    let icon_y = TOP + 0.5 * body + line / 2.0;
    h.click(
        Point::new(h.left + 3.0 + 0.75 * body + body / 2.0, icon_y),
        1,
        Modifiers::NONE,
    );
    let events = h.events();
    let menu = events
        .iter()
        .find_map(|event| match event {
            EditorEvent::CalloutMenu { kind, anchor, .. } => Some((*kind, *anchor)),
            _ => None,
        })
        .expect("a callout menu event");
    assert_eq!(menu.0, CalloutKind::Note);
    assert!(close(menu.1.x, h.left + 3.0 + 0.75 * body));
    assert!(close(menu.1.y, TOP + 0.5 * body + (line - body) / 2.0));
    assert_eq!(h.selection(), before);
}

#[test]
fn callout_tint_is_drawn_behind_the_selection() {
    let f = Fixture::new("> [!NOTE]\n");
    let layout = f.measure(0, &[]);
    let cx = BlockPaintCx {
        theme: &f.theme,
        config: &f.config,
        strings: &f.strings,
        text: &f.engine,
        hovered: false,
    };
    let mut backdrop = RecordingCanvas::new();
    layout.paint_backdrop(&cx, &mut backdrop);
    assert_eq!(rounded_fills(&backdrop).len(), 2);
    let mut front = RecordingCanvas::new();
    layout.paint(&cx, &mut front);
    assert!(rounded_fills(&front).is_empty());
}

// ----- dividers, page breaks, front matter, raw HTML, footnotes -------------------------------

#[test]
fn a_divider_is_a_hairline_and_shows_its_source_with_the_caret() {
    let f = Fixture::new("a\n\n---\n\nb\n");
    let layout = f.measure(1, &[]);
    let canvas = f.paint(layout.as_ref());
    let drawn = lines(&canvas);
    assert_eq!(drawn.len(), 1);
    assert_eq!(drawn[0].2, f.theme.tokens.hairline);
    assert_eq!(drawn[0].3, Dash::Solid);
    let y = f.line() / 2.0;
    assert!(near(drawn[0].0, 0.0, y) && near(drawn[0].1, WIDTH, y));
    assert!(canvas.texts().is_empty());
    assert_eq!(layout.height(), f.line());

    assert_eq!(layout.hit_test(Point::new(10.0, 5.0)), 3);
    assert_eq!(layout.hit_test(Point::new(300.0, 5.0)), 6);
    assert!(close(layout.caret_rect(3).x, 0.0) && close(layout.caret_rect(6).x, WIDTH));
    assert_eq!(
        layout.selection_rects(TextRange::new(3, 6)),
        [Rect::new(0.0, 0.0, WIDTH, f.line())]
    );
    assert!(layout.selection_rects(TextRange::new(0, 1)).is_empty());
    assert_eq!(layout.line_range(4), TextRange::new(3, 6));
    assert_eq!(
        layout.access_label(&f.strings),
        BlockStrings::default().divider
    );

    let shown = f.measure(1, &[4]);
    let canvas = f.paint(shown.as_ref());
    assert_eq!(canvas.texts(), ["---"]);
    assert!(lines(&canvas).is_empty());
    assert!(close(shown.caret_rect(4).x, ADVANCE));
}

#[test]
fn a_page_break_is_a_labelled_dashed_hairline() {
    let f = Fixture::new("a\n\n<!-- pagebreak -->\n\nb\n");
    let layout = f.measure(1, &[]);
    let canvas = f.paint(layout.as_ref());
    let (label, color) = text_named(&canvas, "Page break");
    assert_eq!(color, f.theme.tokens.text_muted);
    let width = 10.0 * ADVANCE;
    assert!(close(label.x, (WIDTH - width) / 2.0) && close(label.width, width));
    let gap = 0.6 * f.body();
    let drawn = lines(&canvas);
    assert_eq!(drawn.len(), 2);
    assert!(
        drawn
            .iter()
            .all(|l| l.2 == f.theme.tokens.hairline && l.3 == Dash::Dashed)
    );
    assert!(near(drawn[0].0, 0.0, f.line() / 2.0) && close(drawn[0].1.x, label.x - gap));
    assert!(close(drawn[1].0.x, label.right() + gap) && close(drawn[1].1.x, WIDTH));
    assert_eq!(
        layout.access_label(&f.strings),
        BlockStrings::default().page_break
    );

    let shown = f.painted(1, &[4]);
    assert_eq!(shown.texts(), ["<!-- pagebreak -->"]);
    assert!(lines(&shown).is_empty());
}

#[test]
fn a_page_break_label_wider_than_the_column_leaves_no_room_for_hairlines() {
    let label = "W".repeat(60);
    let strings = BlockStrings {
        page_break: label.clone(),
        ..BlockStrings::default()
    };
    let mut f = Fixture::new("a\n\n<!-- pagebreak -->\n\nb\n");
    f.registry = BlockRegistry::with_defaults();
    blocks_text::register_with(&mut f.registry, strings);
    let canvas = f.painted(1, &[]);
    assert_eq!(canvas.texts(), [label.as_str()]);
    assert!(lines(&canvas).is_empty());
}

#[test]
fn front_matter_is_a_muted_monospace_card_the_setting_can_hide() {
    let mut f = Fixture::new("---\ntitle: x\n---\n\nbody\n");
    let (body, line) = (f.body(), f.line());
    let pad = 0.75 * body;
    let layout = f.measure(0, &[]);
    let canvas = f.paint(layout.as_ref());
    assert!(close(layout.height(), 3.0 * line + 2.0 * pad));
    let card = rounded_fills(&canvas);
    assert_eq!(card.len(), 1);
    assert_eq!(card[0].2, f.theme.tokens.code_bg);
    assert_eq!(card[0].1, CornerRadii::uniform(6.0));
    assert!(close(card[0].0.width, WIDTH));
    let (text, color) = text_named(&canvas, "---\ntitle: x\n---");
    assert_eq!(color, f.theme.tokens.text_muted);
    assert!(close(text.x, pad) && close(text.y, pad));
    assert!(close(layout.caret_rect(7).y, pad + line));
    assert_eq!(layout.hit_test(Point::new(pad + 1.0, pad + line + 5.0)), 4);
    assert_eq!(
        layout.access_label(&f.strings),
        BlockStrings::default().front_matter
    );

    f.config.hide_front_matter = true;
    let hidden = f.measure(0, &[20]);
    assert_eq!(hidden.height(), 0.0);
    let canvas = f.paint(hidden.as_ref());
    assert!(canvas.recorded().is_empty());
    assert_eq!(hidden.hit_test(Point::new(5.0, 0.0)), 16);
    assert_eq!(hidden.caret_rect(4), Rect::ZERO);
    assert!(hidden.selection_rects(TextRange::new(0, 16)).is_empty());
    assert_eq!(hidden.line_range(4), f.block(0).range);
    assert_eq!(
        hidden.access_label(&f.strings),
        BlockStrings::default().front_matter
    );

    let editing = f.measure(0, &[6]);
    assert!(editing.height() > 0.0, "the caret inside shows the card");
}

#[test]
fn raw_html_is_dimmed_source_text() {
    let f = Fixture::new("<div>\nhi\n</div>\n");
    let layout = f.measure(0, &[]);
    let canvas = f.paint(layout.as_ref());
    let (text, color) = text_named(&canvas, "<div>\nhi\n</div>");
    assert_eq!(color, f.theme.tokens.text_muted);
    assert!(close(text.height, 3.0 * f.line()));
    assert!(close(layout.caret_rect(6).y, f.line()));
    assert_eq!(
        layout.hit_test(Point::new(ADVANCE + 1.0, f.line() + 5.0)),
        7
    );
    assert_eq!(layout.line_range(7), TextRange::new(6, 8));
    assert_eq!(layout.selection_rects(TextRange::new(0, 5)).len(), 1);
    assert_eq!(
        layout.access_label(&f.strings),
        BlockStrings::default().html
    );
    assert!(layout.link_at(Point::new(1.0, 1.0)).is_none());
}

#[test]
fn a_footnote_definition_shows_its_label_beside_the_text() {
    let f = Fixture::new("text[^1]\n\n[^1]: The note\n");
    let layout = f.measure(1, &[]);
    let canvas = f.paint(layout.as_ref());
    let (body, line) = (f.body(), f.line());
    let (label, color) = text_named(&canvas, "1");
    assert_eq!(color, f.theme.tokens.link);
    assert!(close(label.right(), 1.5 * body - 0.4 * body));
    assert!(close(text_named(&canvas, "The note").0.x, 1.5 * body));
    assert!(close(layout.height(), line));
    assert_eq!(
        layout.access_label(&f.strings),
        BlockStrings::default().footnote
    );

    let shown = f.measure(1, &[12]);
    let canvas = f.paint(shown.as_ref());
    assert!(canvas.texts().contains(&"[^1]: "));
    assert!(close(
        text_named(&canvas, "The note").0.x,
        6.0 * ADVANCE + 0.3 * body
    ));
    assert!(
        close(shown.caret_rect(12).x, 2.0 * ADVANCE),
        "inside the source marker"
    );
}

// ----- registry, strings, containers around other kinds ------------------------------------

/// A block of fixed height that records where it is asked about.
struct ProbeLayout {
    range: TextRange,
    link: LinkTarget,
    press: bool,
}

impl BlockLayout for ProbeLayout {
    fn height(&self) -> f32 {
        40.0
    }

    fn paint(&self, _cx: &BlockPaintCx<'_>, canvas: &mut dyn Canvas) {
        canvas.fill_rect(
            Rect::new(0.0, 0.0, 10.0, 40.0),
            Color::new(1.0, 0.0, 0.0, 1.0),
        );
    }

    fn hit_test(&self, _point: Point) -> usize {
        self.range.start
    }

    fn caret_rect(&self, _offset: usize) -> Rect {
        Rect::new(0.0, 0.0, 0.0, 40.0)
    }

    fn selection_rects(&self, _range: TextRange) -> Vec<Rect> {
        vec![Rect::new(0.0, 0.0, 10.0, 40.0)]
    }

    fn line_range(&self, _offset: usize) -> TextRange {
        self.range
    }

    fn link_at(&self, _point: Point) -> Option<LinkTarget> {
        Some(self.link.clone())
    }

    fn context_menu(&self, point: Point, origin: Point) -> Option<EditorEvent> {
        Some(EditorEvent::HeadingMenu {
            range: self.range,
            level: 1,
            anchor: Rect::new(origin.x + point.x, origin.y + point.y, 1.0, 1.0),
        })
    }

    fn press(&self, point: Point, origin: Point, _mods: Modifiers) -> Option<BlockPress> {
        self.press.then(|| {
            BlockPress::Edit(BlockEdit {
                range: self.range,
                text: format!("{} {}", origin.x + point.x, origin.y + point.y),
            })
        })
    }

    fn cursor_at(&self, _point: Point) -> Option<Cursor> {
        Some(Cursor::Hand)
    }

    fn access_label(&self, _strings: &EditorStrings) -> String {
        "probe".into()
    }
}

/// Lays every block out as a [`ProbeLayout`] with `link` under the pointer; `press` makes a
/// press replace the block with the position it was made at.
struct Probe {
    link: LinkTarget,
    press: bool,
}

impl Probe {
    fn url() -> Rc<Self> {
        Rc::new(Self {
            link: LinkTarget::Url("probe".into()),
            press: true,
        })
    }
}

impl BlockRenderer for Probe {
    fn measure(&self, _cx: &BlockCx<'_>, block: &Block) -> Box<dyn BlockLayout> {
        Box::new(ProbeLayout {
            range: block.range,
            link: self.link.clone(),
            press: self.press,
        })
    }
}

#[test]
fn other_block_kinds_inside_containers_are_laid_out_through_the_registry() {
    let mut f = Fixture::new("- item\n\n  ```\n  x\n  ```\n");
    f.registry.register(BlockClass::CodeBlock, Probe::url());
    let layout = f.measure(0, &[]);
    let (col, line, spacing) = (1.5 * f.body(), f.line(), f.config.spacing_px());
    let top = line + spacing;
    assert!(close(layout.height(), top + 40.0));

    let canvas = f.paint(layout.as_ref());
    let probe = rect_fills(&canvas);
    assert_eq!(probe.len(), 1);
    assert!(close(probe[0].0.x, col) && close(probe[0].0.y, top));

    let code_start = f.block(0).children[0].children[1].range.start;
    let inside = Point::new(col + 5.0, top + 5.0);
    assert_eq!(layout.hit_test(inside), code_start);
    let caret = layout.caret_rect(code_start);
    assert!(close(caret.x, col) && close(caret.y, top) && close(caret.height, 40.0));
    assert!(
        layout
            .selection_rects(f.block(0).children[0].children[1].range)
            .iter()
            .any(|r| close(r.y, top))
    );
    assert_eq!(
        layout.link_at(inside),
        Some(LinkTarget::Url("probe".into()))
    );
    assert_eq!(layout.cursor_at(inside), Some(Cursor::Hand));
    assert!(layout.link_at(Point::new(col + 5.0, 5.0)).is_none());
    assert!(layout.link_at(Point::new(col + 5.0, 500.0)).is_none());

    // Presses and menus see the child's coordinates and window origin.
    let origin = Point::new(7.0, 11.0);
    let Some(BlockPress::Edit(edit)) = layout.press(inside, origin, Modifiers::NONE) else {
        panic!("the probe edits");
    };
    assert_eq!(
        edit.text,
        format!("{} {}", origin.x + inside.x, origin.y + inside.y)
    );
    let Some(EditorEvent::HeadingMenu { anchor, .. }) = layout.context_menu(inside, origin) else {
        panic!("the probe opens a menu");
    };
    assert!(close(anchor.x, origin.x + inside.x) && close(anchor.y, origin.y + inside.y));
}

#[test]
fn strings_come_from_the_localizer() {
    let ftl = "blocks-page-break = Seitenumbruch\nblocks-callout-note = Hinweis\n";
    let localizer = Localizer::new("de", &[("de", ftl), ("en", blocks_text::EN_FTL)]);
    let strings = BlockStrings::new(&localizer);
    assert_eq!(strings.page_break, "Seitenumbruch");
    assert_eq!(strings.callout_titles[0], "Hinweis");
    assert_eq!(
        strings.callout_titles[1], "Tip",
        "other ids fall back to English"
    );

    let mut f = Fixture::new("a\n\n<!-- pagebreak -->\n\n> [!NOTE]\n");
    f.registry = BlockRegistry::with_defaults();
    blocks_text::register_with(&mut f.registry, strings);
    assert!(f.painted(1, &[]).texts().contains(&"Seitenumbruch"));
    assert!(f.painted(2, &[]).texts().contains(&"Hinweis"));
    assert_eq!(
        BlockStrings::default(),
        BlockStrings::new(&Localizer::new("en", &[("en", blocks_text::EN_FTL)]))
    );
}

#[test]
fn a_stray_list_item_is_laid_out_as_a_one_item_list() {
    let f = Fixture::new("- a\n- b\n");
    // Hand the item itself to the registry, as a renderer for a kind of its own would.
    let item = f.block(0).children[1].clone();
    let reveal = Reveal::new(false, Vec::new());
    let cx = BlockCx {
        text: &f.engine,
        theme: &f.theme,
        config: &f.config,
        buffer: f.doc.buffer(),
        ast: f.doc.ast(),
        reveal: &reveal,
        objects: &NoObjects,
        composition: None,
        width: WIDTH,
        folded: false,
        strings: &f.strings,
        registry: &f.registry,
    };
    let layout = f.registry.measure(&cx, &item);
    assert!(close(layout.height(), f.line()));
    let canvas = f.paint(layout.as_ref());
    assert_eq!(canvas.texts(), ["b"]);
}

#[test]
fn a_callout_body_forwards_geometry_and_interaction_to_its_children() {
    let mut f = Fixture::new("> [!NOTE]\n> ```\n> x\n> ```\n");
    f.registry.register(BlockClass::CodeBlock, Probe::url());
    let layout = f.measure(0, &[]);
    let code = f.block(0).children.last().expect("the code block").range;
    let inside = Point::new(100.0, layout.height() - 0.5 * f.body() - 10.0);

    assert_eq!(
        layout.link_at(inside),
        Some(LinkTarget::Url("probe".into()))
    );
    assert!(layout.task_box_at(inside).is_none());
    let origin = Point::new(7.0, 11.0);
    let Some(EditorEvent::HeadingMenu { anchor, .. }) = layout.context_menu(inside, origin) else {
        panic!("the probe opens a menu");
    };
    assert!(close(anchor.x, origin.x + inside.x) && close(anchor.y, origin.y + inside.y));

    // A line inside the body is the child's line, not the header's.
    assert_eq!(layout.line_range(code.start), code);
    // A selection inside the body has no rectangle in the header.
    let rects = layout.selection_rects(code);
    assert_eq!(rects.len(), 1);
    assert!(rects[0].y >= f.line(), "below the header: {:?}", rects[0]);
}

#[test]
fn a_callout_body_task_is_found_through_the_callout() {
    let src = "> [!NOTE]\n> see [link](http://x.test)\n> - [ ] task\n";
    let f = Fixture::new(src);
    let layout = f.measure(0, &[]);
    let line = f.line();

    let link = layout.caret_rect(src.find("link").expect("link"));
    let on_link = Point::new(link.x + 2.0, link.y + link.height / 2.0);
    assert_eq!(
        layout.link_at(on_link),
        Some(LinkTarget::Url("http://x.test".into()))
    );

    let task = layout.caret_rect(src.find("task").expect("task"));
    let y = task.y + line / 2.0;
    let found = (0..WIDTH as u32)
        .find_map(|x| layout.task_box_at(Point::new(x as f32, y)))
        .expect("the checkbox is reported through the callout");
    assert_eq!(found.offset, src.find("[ ]").expect("marker"));
    assert!(found.rect.y <= y && y <= found.rect.bottom());
}

#[test]
fn an_empty_list_item_answers_through_an_empty_stack() {
    let f = Fixture::new("-\n");
    let layout = f.measure(0, &[]);
    let item = f.block(0).children[0].range;
    let col = 1.5 * f.body();

    // Right of the marker column there is no content: the end of the item.
    assert_eq!(layout.hit_test(Point::new(100.0, 5.0)), item.end);
    assert_eq!(layout.line_range(item.start), item);
    let caret = layout.caret_rect(item.end);
    assert!(close(caret.x, col) && close(caret.y, 0.0), "{caret:?}");
    assert!(close(caret.height, f.line()));
}

#[test]
fn an_empty_quote_is_one_blank_row() {
    let f = Fixture::new(">\n");
    let layout = f.measure(0, &[]);
    let range = f.block(0).range;
    assert!(close(layout.height(), f.line()));
    assert_eq!(layout.hit_test(Point::new(100.0, 5.0)), range.end);
    assert_eq!(layout.line_range(range.start), range);
    let caret = layout.caret_rect(range.end);
    assert!(close(caret.x, 3.0 + 0.7 * f.body()) && close(caret.height, f.line()));
}

#[test]
fn blank_rows_of_a_container_are_not_interactive() {
    let f = Fixture::new("> a\n>\n> b\n");
    let layout = f.measure(0, &[]);
    let (line, spacing) = (f.line(), f.config.spacing_px());
    let blank = Point::new(50.0, line + spacing / 2.0);
    assert!(layout.link_at(blank).is_none());
    assert!(layout.task_box_at(blank).is_none());
    assert!(layout.context_menu(blank, Point::ZERO).is_none());
    assert!(layout.press(blank, Point::ZERO, Modifiers::NONE).is_none());
    assert!(layout.cursor_at(blank).is_none());
}

#[test]
fn links_in_a_container_need_the_point_on_the_text() {
    let src = "> see [l](http://x.test)\n";
    let f = Fixture::new(src);
    let layout = f.measure(0, &[]);
    let caret = layout.caret_rect(src.find("l](").expect("link text"));
    let y = caret.y + caret.height / 2.0;
    assert_eq!(
        layout.link_at(Point::new(caret.x + 2.0, y)),
        Some(LinkTarget::Url("http://x.test".into()))
    );
    assert!(layout.link_at(Point::new(WIDTH - 10.0, y)).is_none());
}

// ----- block.rs: classes, defaults, plain text blocks ------------------------------------------

#[test]
fn every_block_kind_maps_to_its_class() {
    let cases = [
        ("para\n", BlockClass::Paragraph),
        ("# h\n", BlockClass::Heading),
        ("- a\n", BlockClass::List),
        ("> q\n", BlockClass::BlockQuote),
        ("> [!NOTE]\n", BlockClass::Callout),
        ("```\nx\n```\n", BlockClass::CodeBlock),
        ("$$\nx\n$$\n", BlockClass::MathBlock),
        ("| a |\n|---|\n", BlockClass::Table),
        ("***\n", BlockClass::ThematicBreak),
        ("---\ntitle: x\n---\n", BlockClass::FrontMatter),
        ("<div>\nhi\n</div>\n", BlockClass::HtmlBlock),
        ("[^1]: note\n", BlockClass::FootnoteDefinition),
        ("[ref]: /url\n", BlockClass::LinkReferenceDefinition),
        ("<!-- pagebreak -->\n", BlockClass::PageBreak),
    ];
    for (src, class) in cases {
        let doc = Document::new(src);
        assert_eq!(BlockClass::of(&doc.ast().blocks[0].kind), class, "{src:?}");
    }
}

#[test]
fn a_blank_gap_uses_the_layout_defaults() {
    let strings = EditorStrings::default();
    let gap = GapLayout::new(TextRange::new(2, 2), 12.0);
    assert!(close(gap.height(), 12.0));
    assert!(close(gap.content_width(), 0.0));
    assert_eq!(gap.gutter_hit(Point::new(-10.0, 5.0)), None);
    assert_eq!(gap.cursor_at(Point::new(1.0, 1.0)), None);
    assert_eq!(gap.hit_test(Point::new(50.0, 5.0)), 2);
    assert_eq!(gap.line_range(0), TextRange::new(2, 2));
    assert_eq!(gap.access_label(&strings), strings.blank_line);
    assert!(gap.selection_rects(TextRange::new(2, 2)).is_empty());
    assert_eq!(gap.selection_rects(TextRange::new(1, 3)).len(), 1);
    assert_eq!(gap.caret_rect(2), Rect::new(0.0, 0.0, 0.0, 12.0));
}

#[test]
fn paragraphs_and_raw_blocks_describe_themselves() {
    let mut f = Fixture::new("see [l](http://x.test)\n\n> quote\n");
    f.registry = BlockRegistry::with_defaults();
    let paragraph = f.measure(0, &[]);
    assert_eq!(paragraph.access_label(&f.strings), f.strings.paragraph);
    // Only a heading has gutter widgets.
    assert_eq!(paragraph.gutter_hit(Point::new(-30.0, 5.0)), None);

    // A link answers over its text and not in the empty space after the line.
    let link = paragraph.caret_rect(5);
    let y = link.y + link.height / 2.0;
    assert_eq!(
        paragraph.link_at(Point::new(link.x + 2.0, y)),
        Some(LinkTarget::Url("http://x.test".into()))
    );
    assert!(paragraph.link_at(Point::new(WIDTH - 10.0, y)).is_none());

    // Without a renderer the quote is dimmed source.
    let raw = f.measure(1, &[]);
    assert_eq!(raw.access_label(&f.strings), f.strings.block);
}

#[test]
fn the_heading_renderer_sizes_a_block_that_is_not_a_heading_as_level_one() {
    let mut f = Fixture::new("# abc\n\nabc\n");
    f.registry
        .register(BlockClass::Paragraph, Rc::new(HeadingRenderer));
    let heading = f.measure(0, &[]);
    let lookalike = f.measure(1, &[]);
    assert!(close(lookalike.height(), heading.height()));
    assert!(lookalike.height() > f.line(), "taller than a body line");
}

#[test]
fn contexts_and_registries_show_what_identifies_them() {
    let f = Fixture::new("a\n");
    let reveal = Reveal::new(true, Vec::new());
    let cx = BlockCx {
        text: &f.engine,
        theme: &f.theme,
        config: &f.config,
        buffer: f.doc.buffer(),
        ast: f.doc.ast(),
        reveal: &reveal,
        objects: &NoObjects,
        composition: None,
        width: WIDTH,
        folded: true,
        strings: &f.strings,
        registry: &f.registry,
    };
    let printed = format!("{cx:?}");
    assert!(printed.contains("width: 352.0"), "{printed}");
    assert!(printed.contains("folded: true"), "{printed}");

    let paint = BlockPaintCx {
        theme: &f.theme,
        config: &f.config,
        strings: &f.strings,
        text: &f.engine,
        hovered: true,
    };
    assert!(format!("{paint:?}").contains("hovered: true"));

    // The default registry knows paragraphs and headings, listed in sorted order.
    let registry = format!("{:?}", BlockRegistry::default());
    assert!(
        registry.contains(r#"["Heading", "Paragraph"]"#),
        "{registry}"
    );
}

// ----- in the editor --------------------------------------------------------------------------

fn editor(text: &str, size: Size) -> Harness {
    let mut h = Harness::sized(text, size);
    blocks_text::register(h.view.registry_mut());
    h.arrange();
    h
}

#[test]
fn clicking_a_task_checkbox_toggles_it_in_one_undo_step() {
    let mut h = editor("- [ ] a\n\nb\n", Size::new(400.0, 200.0));
    let (body, line) = (h.view.config().body_size(), h.view.config().line_px());
    let box_center = Point::new(
        h.left + 1.5 * body - 0.4 * body - 0.95 * body / 2.0,
        TOP + line / 2.0,
    );
    // The caret in the item would show its source instead of the checkbox: put it elsewhere.
    h.set_caret(10);
    let before = h.selection();
    h.click(box_center, 1, Modifiers::NONE);
    assert_eq!(h.text(), "- [x] a\n\nb\n");
    assert_eq!(h.selection(), before, "the caret does not move");
    h.key(editor_common::KEY_Z, Modifiers::CTRL);
    assert_eq!(h.text(), "- [ ] a\n\nb\n", "one undo step");
}

#[test]
fn a_read_only_view_leaves_a_clicked_task_checkbox_alone() {
    let mut h = editor("- [ ] a\n\nb\n", Size::new(400.0, 200.0));
    let (body, line) = (h.view.config().body_size(), h.view.config().line_px());
    let box_center = Point::new(
        h.left + 1.5 * body - 0.4 * body - 0.95 * body / 2.0,
        TOP + line / 2.0,
    );
    h.set_caret(10);
    h.view.set_read_only(true);
    h.click(box_center, 1, Modifiers::NONE);
    assert_eq!(h.text(), "- [ ] a\n\nb\n");
    assert!(!h.doc.borrow().can_undo());
}

#[test]
fn the_pointer_is_a_hand_over_a_task_checkbox_and_a_text_cursor_beside_it() {
    let mut h = editor("- [ ] a\n\nb\n", Size::new(400.0, 200.0));
    let (body, line) = (h.view.config().body_size(), h.view.config().line_px());
    let box_center = Point::new(
        h.left + 1.5 * body - 0.4 * body - 0.95 * body / 2.0,
        TOP + line / 2.0,
    );
    h.set_caret(10);
    h.pointer(PointerEventKind::Move, box_center, 0, Modifiers::NONE);
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
    let beside = Point::new(h.left + 3.0 * body, TOP + line / 2.0);
    h.pointer(PointerEventKind::Move, beside, 0, Modifiers::NONE);
    assert_eq!(h.requests.take_cursor(), Some(Cursor::IBeam));
}

#[test]
fn the_pointer_is_an_arrow_over_a_hidden_divider() {
    let mut h = editor("a\n\n---\n\nb\n", Size::new(400.0, 200.0));
    let rule = h.view.rect_at(3).expect("on the divider");
    h.pointer(PointerEventKind::Move, rule.center(), 0, Modifiers::NONE);
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Arrow));
}

#[test]
fn the_pointer_over_a_heading_shows_its_open_fold_chevron() {
    let mut h = editor("# Title\n\nbody\n", Size::new(400.0, 200.0));
    h.set_caret(12);
    assert!(
        icons(&h.paint()).is_empty(),
        "no chevron until the pointer is over the heading"
    );
    let over = Point::new(h.left + 10.0, TOP + 10.0);
    h.pointer(PointerEventKind::Move, over, 0, Modifiers::NONE);
    let shown = icons(&h.paint());
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].0, '\u{E70D}', "the chevron of an open section");
    assert_eq!(shown[0].1, h.theme.tokens.text_muted);
}

#[test]
fn a_block_press_edit_replaces_the_block_as_one_undo_step() {
    let mut h = editor("```\nx\n```\n\npara\n", Size::new(400.0, 200.0));
    h.view
        .registry_mut()
        .register(BlockClass::CodeBlock, Probe::url());
    h.arrange();
    h.click(Point::new(h.left + 5.0, TOP + 5.0), 1, Modifiers::NONE);
    assert_eq!(
        h.text(),
        format!("{} {}\n\npara\n", h.left + 5.0, TOP + 5.0)
    );
    h.key(editor_common::KEY_Z, Modifiers::CTRL);
    assert_eq!(h.text(), "```\nx\n```\n\npara\n");
}

#[test]
fn a_footnote_link_without_a_definition_only_places_the_caret() {
    let mut h = editor("```\nx\n```\n\npara\n", Size::new(400.0, 200.0));
    h.view.registry_mut().register(
        BlockClass::CodeBlock,
        Rc::new(Probe {
            link: LinkTarget::Footnote("missing".into()),
            press: false,
        }),
    );
    h.arrange();
    h.set_caret(h.text().len() - 1);
    h.events();
    h.click(Point::new(h.left + 5.0, TOP + 5.0), 1, Modifiers::NONE);
    assert_eq!(h.selection().head, 0, "the caret went to the block");
    assert!(
        !h.events()
            .iter()
            .any(|event| matches!(event, EditorEvent::LinkActivated(_)))
    );
}

#[test]
fn a_document_with_every_text_block_paints() {
    let text = "---\ntitle: x\n---\n\n# H\n\n- a\n- [ ] t\n\n> q\n\n> [!NOTE] n\n> body\n\n---\n\n<!-- pagebreak -->\n\n<b>x</b>\n\n[^1]: note\n";
    let mut h = editor(text, Size::new(400.0, 1600.0));
    h.set_caret(text.len());
    let painted = h.painted_texts();
    for wanted in ["H", "a", "t", "q", "n", "body", "Page break", "note"] {
        assert!(
            painted.iter().any(|t| t == wanted),
            "{wanted:?} in {painted:?}"
        );
    }
    assert!(painted.iter().any(|t| t == "---\ntitle: x\n---"));
    assert!(painted.iter().any(|t| t == "<b>x</b>"));
}
