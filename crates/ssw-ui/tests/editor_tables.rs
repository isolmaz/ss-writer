//! Tables in the editor (spec §3.1): grid drawing, caret and hit mapping between the grid and
//! the source, Tab / Enter flows through the core table operations, the cell menu event, raw
//! mode, copy commands and re-alignment on leaving.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;

use editor_common::{COLUMN_LEFT, Harness, TOP, close};
use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::i18n::Localizer;
use ssw_core::ops::table::CellPos;
use ssw_core::text::TextRange;
use ssw_ui::canvas::{DrawOp, RecordingCanvas, SceneId};
use ssw_ui::editor::block::{BlockClass, BlockCx, BlockLayout, BlockPaintCx, BlockRegistry};
use ssw_ui::editor::blocks_table::{self, TableRenderer, TableStrings};
use ssw_ui::editor::display::Reveal;
use ssw_ui::editor::object::{
    InlineObjectProvider, NoObjects, ObjectInfo, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::editor::{Composition, EditorConfig, EditorEvent, EditorStrings, LinkTarget};
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{Event, Modifiers, PointerButton, PointerEvent, PointerEventKind, vk};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::text::{FontWeight, MonoTextEngine, TextLayout, TextLayoutEngine, TextStyle};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widgets::SharedClipboard;

/// Header, two body rows; the first column is left aligned, the second right aligned.
const SAMPLE: &str = "| Name | Qty |\n|:-----|----:|\n| Apple | 3 |\n| Pear | 12 |\n";

// At 16 DIP text: 12 DIP padding left and right, 4 DIP above and below, 24 DIP lines, so rows
// are 32 DIP tall. The columns of SAMPLE are 64 (Apple + padding) and 56 (the minimum) wide.
const ROW: f32 = 32.0;

#[derive(Debug, Clone, PartialEq)]
struct Call {
    text: String,
    weight: FontWeight,
    width: f32,
}

/// The test engine that remembers how it was asked to lay text out.
struct Spy {
    inner: MonoTextEngine,
    calls: RefCell<Vec<Call>>,
}

impl TextLayoutEngine for Spy {
    fn layout(
        &self,
        text: &str,
        base: &TextStyle,
        ranges: &[ssw_ui::text::StyledRange],
        objects: &[ssw_ui::text::InlineObject],
        max_width: f32,
    ) -> Box<dyn TextLayout> {
        self.calls.borrow_mut().push(Call {
            text: text.to_owned(),
            weight: base.weight,
            width: max_width,
        });
        self.inner.layout(text, base, ranges, objects, max_width)
    }
}

/// Measures the table of a document through the registry, the way the editor does.
struct Lab {
    doc: Document,
    engine: Spy,
    theme: Theme,
    config: EditorConfig,
    strings: EditorStrings,
    registry: BlockRegistry,
}

impl Lab {
    fn new(text: &str) -> Self {
        let mut registry = BlockRegistry::with_defaults();
        blocks_table::register(&mut registry);
        Self {
            doc: Document::new(text),
            engine: Spy {
                inner: MonoTextEngine::default(),
                calls: RefCell::new(Vec::new()),
            },
            theme: Theme::light(DEFAULT_ACCENT),
            config: EditorConfig::default(),
            strings: EditorStrings::default(),
            registry,
        }
    }

    fn measure_with(
        &self,
        width: f32,
        selections: Vec<TextRange>,
        objects: &dyn InlineObjectProvider,
        composition: Option<&Composition>,
    ) -> Box<dyn BlockLayout> {
        let reveal = Reveal::new(false, selections);
        let block = self
            .doc
            .ast()
            .blocks
            .iter()
            .find(|block| BlockClass::of(&block.kind) == BlockClass::Table)
            .expect("the document has a table");
        let cx = BlockCx {
            text: &self.engine,
            theme: &self.theme,
            config: &self.config,
            buffer: self.doc.buffer(),
            ast: self.doc.ast(),
            reveal: &reveal,
            objects,
            composition,
            width,
            folded: false,
            strings: &self.strings,
            registry: &self.registry,
        };
        self.registry.measure(&cx, block)
    }

    fn layout(&self) -> Box<dyn BlockLayout> {
        self.measure_with(352.0, Vec::new(), &NoObjects, None)
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
        layout.paint(&cx, &mut canvas);
        assert!(canvas.is_balanced());
        canvas
    }

    /// Offset of the first occurrence of `needle`.
    fn at(&self, needle: &str) -> usize {
        self.doc.text().find(needle).expect("needle in the text")
    }
}

fn near(actual: Rect, x: f32, y: f32, width: f32, height: f32) {
    assert!(
        close(actual.x, x)
            && close(actual.y, y)
            && close(actual.width, width)
            && close(actual.height, height),
        "{actual:?} is not ({x}, {y}, {width}, {height})"
    );
}

/// Every drawn text with the rectangle it covers.
fn texts(canvas: &RecordingCanvas) -> Vec<(String, Rect)> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match (&entry.op, entry.bounds) {
            (DrawOp::Text { text, .. }, Some(bounds)) => Some((text.clone(), bounds)),
            _ => None,
        })
        .collect()
}

fn text_at<'a>(drawn: &'a [(String, Rect)], text: &str) -> &'a Rect {
    &drawn
        .iter()
        .find(|(drawn, _)| drawn == text)
        .unwrap_or_else(|| panic!("{text:?} is drawn"))
        .1
}

fn right_click(h: &mut Harness, position: Point) {
    h.event(Event::Pointer(PointerEvent::mouse(
        PointerEventKind::Down,
        Some(PointerButton::Right),
        position,
    )));
}

// ----- the grid -------------------------------------------------------------------------------

#[test]
fn the_grid_has_a_rounded_border_cell_separators_and_the_text_of_every_cell() {
    let lab = Lab::new(SAMPLE);
    let layout = lab.layout();
    assert!(close(layout.height(), 3.0 * ROW));
    let canvas = lab.paint(layout.as_ref());
    let tokens = &lab.theme.tokens;

    let frames: Vec<_> = canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::StrokeRoundedRect {
                rect,
                radii,
                color,
                width,
            } => Some((*rect, *radii, *color, *width)),
            _ => None,
        })
        .collect();
    assert_eq!(frames.len(), 1);
    let (rect, radii, color, width) = frames[0];
    near(rect, 0.0, 0.0, 120.0, 3.0 * ROW);
    assert!(radii.is_uniform() && radii.top_left > 0.0);
    assert_eq!(color, tokens.table_border);
    assert!(close(width, 1.0));

    let lines: Vec<(Point, Point)> = canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::StrokeLine {
                from, to, color, ..
            } => {
                assert_eq!(*color, tokens.table_border);
                Some((*from, *to))
            }
            _ => None,
        })
        .collect();
    // Two row separators across, one column separator down.
    assert_eq!(lines.len(), 3);
    assert!(lines.contains(&(Point::new(0.0, ROW), Point::new(120.0, ROW))));
    assert!(lines.contains(&(Point::new(0.0, 2.0 * ROW), Point::new(120.0, 2.0 * ROW))));
    assert!(lines.contains(&(Point::new(64.0, 0.0), Point::new(64.0, 3.0 * ROW))));

    let order: Vec<String> = texts(&canvas).into_iter().map(|(text, _)| text).collect();
    assert_eq!(order, ["Name", "Qty", "Apple", "3", "Pear", "12"]);
}

#[test]
fn only_the_header_is_bold() {
    let lab = Lab::new(SAMPLE);
    let _layout = lab.layout();
    let calls = lab.engine.calls.borrow();
    for word in ["Name", "Qty"] {
        let call = calls.iter().find(|call| call.text == word).expect(word);
        assert_eq!(call.weight, FontWeight::BOLD, "{word}");
    }
    for word in ["Apple", "3", "Pear", "12"] {
        let call = calls.iter().find(|call| call.text == word).expect(word);
        assert_eq!(call.weight, FontWeight::NORMAL, "{word}");
    }
}

#[test]
fn every_second_body_row_is_tinted_and_the_last_one_follows_the_rounded_corners() {
    // Five rows: the third is tinted with a plain fill, the fifth (the last) with rounded
    // bottom corners.
    let lab = Lab::new("| h |\n|---|\n| a |\n| b |\n| c |\n| d |\n");
    let layout = lab.layout();
    let canvas = lab.paint(layout.as_ref());
    let plain: Vec<_> = canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::FillRect { rect, color } => Some((*rect, *color)),
            _ => None,
        })
        .collect();
    assert_eq!(plain.len(), 1, "the second body row");
    near(plain[0].0, 0.0, 2.0 * ROW, 56.0, ROW);
    let rounded: Vec<_> = canvas
        .ops()
        .filter_map(|op| match op {
            DrawOp::FillRoundedRect { rect, radii, color } => Some((*rect, *radii, *color)),
            _ => None,
        })
        .collect();
    assert_eq!(rounded.len(), 1, "the fourth and last body row");
    near(rounded[0].0, 0.0, 4.0 * ROW, 56.0, ROW);
    let radii = rounded[0].1;
    assert!(radii.top_left == 0.0 && radii.top_right == 0.0);
    assert!(radii.bottom_left > 0.0 && radii.bottom_left == radii.bottom_right);
    for color in [plain[0].1, rounded[0].2] {
        let zebra = lab.theme.tokens.table_zebra;
        assert!(close(color.r, zebra.r) && close(color.b, zebra.b));
        assert!(
            color.a < 1.0 && color.a > 0.0,
            "translucent, so the selection below shows through"
        );
    }
    assert!(
        canvas.ops().all(|op| !matches!(
            op,
            DrawOp::PushClipRoundedRect { .. } | DrawOp::PushLayer { .. }
        )),
        "no clip layer per table"
    );
    // With a body row less the last row is not tinted: every tinted row is a plain fill.
    let even = Lab::new("| h |\n|---|\n| a |\n| b |\n| c |\n");
    let canvas = even.paint(even.layout().as_ref());
    assert_eq!(
        canvas
            .ops()
            .filter(|op| matches!(op, DrawOp::FillRect { .. }))
            .count(),
        1
    );
    assert!(
        canvas
            .ops()
            .all(|op| !matches!(op, DrawOp::FillRoundedRect { .. }))
    );
}

#[test]
fn column_alignment_places_the_text_inside_the_free_space_of_the_cell() {
    let lab = Lab::new(SAMPLE);
    let canvas = lab.paint(lab.layout().as_ref());
    let drawn = texts(&canvas);
    // Left aligned column: after the 12 DIP padding.
    near(*text_at(&drawn, "Name"), 12.0, 4.0, 32.0, 24.0);
    near(*text_at(&drawn, "Apple"), 12.0, ROW + 4.0, 40.0, 24.0);
    near(*text_at(&drawn, "Pear"), 12.0, 2.0 * ROW + 4.0, 32.0, 24.0);
    // Right aligned column (64..120, 32 DIP of text area): the text ends at the padding.
    near(*text_at(&drawn, "Qty"), 84.0, 4.0, 24.0, 24.0);
    near(*text_at(&drawn, "3"), 100.0, ROW + 4.0, 8.0, 24.0);
    near(*text_at(&drawn, "12"), 92.0, 2.0 * ROW + 4.0, 16.0, 24.0);

    let centered = Lab::new("| a |\n|:-:|\n| bb |\n");
    let drawn = texts(&centered.paint(centered.layout().as_ref()));
    near(*text_at(&drawn, "a"), 24.0, 4.0, 8.0, 24.0);
    near(*text_at(&drawn, "bb"), 20.0, ROW + 4.0, 16.0, 24.0);
}

#[test]
fn a_column_that_is_too_wide_for_the_text_column_wraps_its_cells() {
    let lab = Lab::new("| a | long long long long |\n|---|---|\n| 1 | 2 |\n");
    let layout = lab.measure_with(120.0, Vec::new(), &NoObjects, None);
    // The long cell gets 64 DIP (40 of text area): four lines of 24 DIP, plus the padding.
    assert!(close(layout.height(), 4.0 * 24.0 + 8.0 + ROW));
    let canvas = lab.paint(layout.as_ref());
    let frame = canvas
        .ops()
        .find_map(|op| match op {
            DrawOp::StrokeRoundedRect { rect, .. } => Some(*rect),
            _ => None,
        })
        .expect("a frame");
    assert!(
        (frame.width - 120.0).abs() < 0.05,
        "the table fills the column: {}",
        frame.width
    );
    let wrapped = lab
        .engine
        .calls
        .borrow()
        .iter()
        .any(|call| call.text == "long long long long" && call.width.is_finite());
    assert!(wrapped);
    let start = lab.at("long long");
    // First line reaches to the top of the row, the lines in between are tight, the last one
    // reaches to the bottom of the row.
    let first = layout.caret_rect(start);
    near(first, 56.0 + 12.0, 0.0, 0.0, 28.0);
    let second = layout.caret_rect(start + 5);
    near(second, 56.0 + 12.0, 28.0, 0.0, 24.0);
    let last = layout.caret_rect(start + 15);
    near(last, 56.0 + 12.0, 76.0, 0.0, 28.0);
    // Home and End stay on the visual line inside the cell.
    let line = layout.line_range(start + 6);
    assert_eq!(line.start, start + 5);
    assert!(line.end > line.start && line.end <= start + 10);
}

#[test]
fn many_columns_share_a_narrow_text_column_equally() {
    let lab = Lab::new("| a | b | c |\n|---|---|---|\n| 1 | 2 | 3 |\n");
    let layout = lab.measure_with(100.0, Vec::new(), &NoObjects, None);
    let canvas = lab.paint(layout.as_ref());
    let frame = canvas
        .ops()
        .find_map(|op| match op {
            DrawOp::StrokeRoundedRect { rect, .. } => Some(*rect),
            _ => None,
        })
        .expect("a frame");
    assert!((frame.width - 100.0).abs() < 0.05, "{}", frame.width);
    assert!(close(layout.height(), 2.0 * ROW), "one line per cell");
}

#[test]
fn markers_of_inline_elements_show_only_while_the_caret_touches_them() {
    let lab = Lab::new("| x **b** y |\n|---|\n| 1 |\n");
    let _ = lab.layout();
    let shown = |lab: &Lab| {
        lab.engine
            .calls
            .borrow()
            .iter()
            .map(|call| call.text.clone())
            .collect::<Vec<_>>()
    };
    assert!(shown(&lab).contains(&"x b y".to_owned()));
    assert!(!shown(&lab).contains(&"x **b** y".to_owned()));
    let caret = lab.at("**b**") + 3;
    let _ = lab.measure_with(352.0, vec![TextRange::at(caret)], &NoObjects, None);
    assert!(shown(&lab).contains(&"x **b** y".to_owned()));
}

struct ReadyMath;

impl InlineObjectProvider for ReadyMath {
    fn object(&self, _request: &ObjectRequest<'_>) -> ObjectState {
        ObjectState::Ready(ObjectInfo {
            visual: ObjectVisual::Scene(SceneId(7)),
            size: Size::new(20.0, 16.0),
            baseline: 12.0,
        })
    }
}

#[test]
fn math_in_a_cell_is_drawn_through_the_object_provider_inside_the_cell() {
    let lab = Lab::new("| $x$ | b |\n|---|---|\n| 1 | 2 |\n");
    let layout = lab.measure_with(352.0, Vec::new(), &ReadyMath, None);
    let canvas = lab.paint(layout.as_ref());
    let dest = canvas
        .ops()
        .find_map(|op| match op {
            DrawOp::Scene { scene, dest } if *scene == SceneId(7) => Some(*dest),
            _ => None,
        })
        .expect("the scene is drawn");
    assert!(close(dest.width, 20.0) && close(dest.height, 16.0));
    assert!(
        Rect::new(0.0, 0.0, 56.0, ROW).contains_rect(dest),
        "inside the first header cell: {dest:?}"
    );
}

#[test]
fn links_in_cells_are_found_only_on_their_text() {
    let lab = Lab::new("| [site](http://x.example) and more | b |\n|---|---|\n| 1 | 2 |\n");
    let layout = lab.layout();
    // "site and more" is 104 DIP wide and starts at x = 12 (padding); the column is 128 wide.
    assert_eq!(
        layout.link_at(Point::new(14.0, 16.0)),
        Some(LinkTarget::Url("http://x.example".to_owned()))
    );
    assert_eq!(layout.link_at(Point::new(90.0, 16.0)), None, "plain text");
    assert_eq!(
        layout.link_at(Point::new(122.0, 16.0)),
        None,
        "behind the text"
    );
}

// ----- carets and offsets ---------------------------------------------------------------------

#[test]
fn the_caret_of_every_cell_offset_is_on_its_text_and_hit_testing_returns_it() {
    let lab = Lab::new(SAMPLE);
    let layout = lab.layout();
    // (word, offset of the word, x of its first character, top of its row)
    let cells = [
        ("Name", lab.at("Name"), 12.0, 0.0),
        ("Qty", lab.at("Qty"), 84.0, 0.0),
        ("Apple", lab.at("Apple"), 12.0, ROW),
        ("3", lab.at("3 |"), 100.0, ROW),
        ("Pear", lab.at("Pear"), 12.0, 2.0 * ROW),
        ("12", lab.at("12"), 92.0, 2.0 * ROW),
    ];
    for (word, offset, x, top) in cells {
        for k in 0..=word.len() {
            let caret = layout.caret_rect(offset + k);
            near(caret, x + 8.0 * k as f32, top, 0.0, ROW);
            let probe = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
            assert_eq!(layout.hit_test(probe), offset + k, "{word} +{k}");
        }
    }
}

#[test]
fn pipes_padding_and_the_delimiter_row_belong_to_a_neighbouring_cell() {
    let lab = Lab::new(SAMPLE);
    let layout = lab.layout();
    let pipe = lab.at("| Qty");
    // A pipe belongs to the cell before it; the space behind it to the cell after it.
    near(layout.caret_rect(pipe), 44.0, 0.0, 0.0, ROW);
    near(layout.caret_rect(pipe + 1), 84.0, 0.0, 0.0, ROW);
    // The delimiter row is not drawn: the caret stays at the end of the header.
    let delimiter = lab.at("|:--");
    near(layout.caret_rect(delimiter + 3), 108.0, 0.0, 0.0, ROW);
    // Before the table's first line it is the first cell.
    let padded = Lab::new("intro\n\n| a |\n|---|\n| 1 |\n");
    near(padded.layout().caret_rect(0), 12.0, 0.0, 0.0, ROW);
}

#[test]
fn hit_testing_clamps_to_the_nearest_cell_and_short_rows_end_at_their_last_cell() {
    let lab = Lab::new("| a | b |\n|---|---|\n| 1 |\n");
    let layout = lab.layout();
    let one = lab.at("1 |");
    // Row 2 has no second cell: its empty part hits the end of the last cell.
    assert_eq!(layout.hit_test(Point::new(100.0, ROW + 16.0)), one + 1);
    // Above, below and beside the grid.
    assert_eq!(layout.hit_test(Point::new(-30.0, -10.0)), lab.at("a |"));
    assert_eq!(layout.hit_test(Point::new(500.0, 500.0)), one + 1);
}

#[test]
fn a_selection_over_several_cells_is_painted_per_cell_and_skips_the_delimiter_row() {
    let lab = Lab::new(SAMPLE);
    let layout = lab.layout();
    let apple = lab.at("Apple");
    let three = lab.at("3 |");
    let rects = layout.selection_rects(TextRange::new(apple + 2, three + 1));
    assert_eq!(rects.len(), 2);
    near(rects[0], 12.0 + 16.0, ROW + 4.0, 24.0, 24.0);
    near(rects[1], 100.0, ROW + 4.0, 8.0, 24.0);
    let delimiter = lab.at("|:--");
    assert!(
        layout
            .selection_rects(TextRange::new(delimiter + 1, delimiter + 5))
            .is_empty()
    );
    assert!(layout.selection_rects(TextRange::at(apple)).is_empty());
}

#[test]
fn home_and_end_stay_inside_the_cell() {
    let lab = Lab::new(SAMPLE);
    let layout = lab.layout();
    let apple = lab.at("Apple");
    assert_eq!(
        layout.line_range(apple + 2),
        TextRange::new(apple, apple + 5)
    );
}

#[test]
fn an_ime_composition_is_shown_inside_its_cell_and_the_caret_follows_it() {
    let lab = Lab::new(SAMPLE);
    let apple = lab.at("Apple");
    let composition = Composition {
        at: apple + 2,
        text: "zz".to_owned(),
        caret: 2,
        attrs: Vec::new(),
    };
    let layout = lab.measure_with(352.0, Vec::new(), &NoObjects, Some(&composition));
    let drawn: Vec<String> = texts(&lab.paint(layout.as_ref()))
        .into_iter()
        .map(|(text, _)| text)
        .collect();
    assert!(drawn.contains(&"Apzzple".to_owned()), "{drawn:?}");
    near(
        layout.caret_rect(apple + 2),
        12.0 + 8.0 * 4.0,
        ROW,
        0.0,
        ROW,
    );

    // A caret in the padding in front of the text composes at the start of the text.
    let padding = Composition {
        at: apple - 1,
        text: "zz".to_owned(),
        caret: 2,
        attrs: Vec::new(),
    };
    let layout = lab.measure_with(352.0, Vec::new(), &NoObjects, Some(&padding));
    let drawn: Vec<String> = texts(&lab.paint(layout.as_ref()))
        .into_iter()
        .map(|(text, _)| text)
        .collect();
    assert!(drawn.contains(&"zzApple".to_owned()), "{drawn:?}");
    near(layout.caret_rect(apple - 1), 12.0 + 16.0, ROW, 0.0, ROW);
}

// ----- the cell menu --------------------------------------------------------------------------

#[test]
fn a_right_click_inside_the_grid_reports_the_cell_and_its_window_rectangle() {
    let lab = Lab::new(SAMPLE);
    let layout = lab.layout();
    let origin = Point::new(24.0, 100.0);
    let event = layout
        .context_menu(Point::new(100.0, 2.0 * ROW + 10.0), origin)
        .expect("a menu");
    let EditorEvent::TableMenu { anchor, cell, raw } = event else {
        panic!("expected a table menu, got {event:?}");
    };
    assert_eq!(cell, CellPos { row: 2, col: 1 });
    assert!(!raw);
    near(anchor, 24.0 + 64.0, 100.0 + 2.0 * ROW, 56.0, ROW);
    // Beside the grid there is no table.
    assert_eq!(layout.context_menu(Point::new(200.0, 10.0), origin), None);
    assert_eq!(layout.context_menu(Point::new(10.0, 400.0), origin), None);
}

#[test]
fn a_right_click_in_the_editor_moves_the_caret_into_the_cell_and_raises_the_menu() {
    let mut h = Harness::new(SAMPLE);
    blocks_table::register(h.view.registry_mut());
    h.paint();
    h.events();
    right_click(
        &mut h,
        Point::new(COLUMN_LEFT + 100.0, TOP + 2.0 * ROW + 16.0),
    );
    let head = h.selection().head;
    let twelve = h.doc.borrow().text().find("12").expect("12");
    assert!(
        (twelve..=twelve + 2).contains(&head),
        "caret in the cell: {head}"
    );
    let events = h.events();
    let menu = events
        .iter()
        .find_map(|event| match event {
            EditorEvent::TableMenu { anchor, cell, raw } => Some((*anchor, *cell, *raw)),
            _ => None,
        })
        .expect("a table menu event");
    assert_eq!(menu.1, CellPos { row: 2, col: 1 });
    assert!(!menu.2);
    near(menu.0, COLUMN_LEFT + 64.0, TOP + 2.0 * ROW, 56.0, ROW);
    // The menu commands act on the cell the click chose.
    assert!(h.view.execute(CommandId::TableInsertRowAbove));
    assert_eq!(h.text().lines().count(), 5);
}

// ----- editing flows --------------------------------------------------------------------------

fn table_harness(text: &str) -> Harness {
    let mut h = Harness::new(text);
    blocks_table::register(h.view.registry_mut());
    h.paint();
    h
}

#[test]
fn tab_walks_the_cells_row_by_row_and_adds_a_row_after_the_last_cell() {
    let mut h = table_harness(SAMPLE);
    h.set_caret(h.text().find("Name").expect("Name"));
    h.press(vk::TAB);
    assert_eq!(h.selection().head, h.text().find("Qty").expect("Qty") + 3);
    h.paint();
    let caret = h.view.caret_rect().expect("caret");
    near(caret, COLUMN_LEFT + 84.0 + 24.0, TOP, 1.0, ROW);
    h.press(vk::TAB);
    assert_eq!(
        h.selection().head,
        h.text().find("Apple").expect("Apple") + 5
    );
    h.press(vk::TAB);
    h.press(vk::TAB);
    h.press(vk::TAB);
    assert_eq!(h.selection().head, h.text().find("12").expect("12") + 2);
    let before = h.text().lines().count();
    h.press(vk::TAB);
    assert_eq!(h.text().lines().count(), before + 1, "a new row");
    h.paint();
    let caret = h.view.caret_rect().expect("caret in the new row");
    assert!(
        caret.y > TOP + 3.0 * ROW - 1.0,
        "the caret is in the fourth row"
    );
    // Shift+Tab goes back one cell at a time: from the first cell of the new row to the last
    // cell of the row above.
    let twelve = h.text().find("12").expect("12");
    h.key(vk::TAB, Modifiers::SHIFT);
    assert_eq!(
        h.selection().head,
        twelve + 2,
        "the last cell of the row above"
    );
    h.key(vk::TAB, Modifiers::SHIFT);
    assert_eq!(h.selection().head, h.text().find("Pear").expect("Pear") + 4);
    let row = h.text().lines().nth(4).expect("the new row").to_owned();
    assert!(row.starts_with('|'), "{row:?}");
}

#[test]
fn enter_moves_down_a_cell_and_adds_a_row_in_the_last_row() {
    let mut h = table_harness(SAMPLE);
    h.set_caret(h.text().find("Apple").expect("Apple") + 5);
    h.press(vk::RETURN);
    assert_eq!(h.selection().head, h.text().find("Pear").expect("Pear") + 4);
    let before = h.text().lines().count();
    h.press(vk::RETURN);
    assert_eq!(h.text().lines().count(), before + 1);
}

#[test]
fn typing_in_a_cell_edits_that_cells_source_and_the_grid_follows() {
    let mut h = table_harness(SAMPLE);
    h.set_caret(h.text().find("Apple").expect("Apple") + 5);
    h.type_text("sauce");
    assert!(h.text().contains("| Applesauce | 3 |"), "{}", h.text());
    let drawn = h.painted_texts();
    assert!(drawn.contains(&"Applesauce".to_owned()), "{drawn:?}");
}

#[test]
fn clicking_a_cell_puts_the_caret_into_its_source() {
    let mut h = table_harness(SAMPLE);
    let apple = h.text().find("Apple").expect("Apple");
    h.click(
        Point::new(COLUMN_LEFT + 12.0 + 3.0 * 8.0 + 1.0, TOP + ROW + 16.0),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection().head, apple + 3);
}

#[test]
fn arrow_keys_cross_from_row_to_row_and_back() {
    let mut h = table_harness(SAMPLE);
    let (name, apple, pear) = (
        h.text().find("Name").expect("Name"),
        h.text().find("Apple").expect("Apple"),
        h.text().find("Pear").expect("Pear"),
    );
    h.set_caret(name + 1);
    h.press(vk::DOWN);
    assert!((apple..=apple + 5).contains(&h.selection().head), "row 1");
    h.press(vk::DOWN);
    assert!((pear..=pear + 4).contains(&h.selection().head), "row 2");
    h.press(vk::UP);
    assert!(
        (apple..=apple + 5).contains(&h.selection().head),
        "row 1 again"
    );
    h.press(vk::UP);
    assert!(
        (name..=name + 4).contains(&h.selection().head),
        "the header"
    );
}

#[test]
fn pipes_are_realigned_when_the_caret_leaves_the_table_if_the_setting_is_on() {
    let text = format!("{SAMPLE}\nafter\n");
    for (on, aligned) in [(true, true), (false, false)] {
        let mut h = table_harness(&text);
        h.config(|config| config.format_tables_on_exit = on);
        h.set_caret(h.text().find("Apple").expect("Apple") + 5);
        h.type_text("sauce");
        let widths = |h: &Harness| -> Vec<usize> {
            h.text()
                .lines()
                .take(4)
                .map(|line| line.chars().count())
                .collect()
        };
        assert!(
            widths(&h).windows(2).any(|pair| pair[0] != pair[1]),
            "misaligned while typing"
        );
        h.set_caret(h.text().find("after").expect("after"));
        let widths = widths(&h);
        assert_eq!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            aligned,
            "format on exit = {on}: {widths:?}"
        );
    }
}

#[test]
fn the_editor_describes_the_table_for_assistive_technology() {
    let mut h = table_harness(SAMPLE);
    h.set_caret(h.text().find("Apple").expect("Apple"));
    assert_eq!(h.access().description, "Table with 3 rows and 2 columns");
    let mut single = table_harness("| a |\n|---|\n");
    single.set_caret(2);
    assert_eq!(single.access().description, "Table with 1 row and 1 column");
}

// ----- raw mode -------------------------------------------------------------------------------

#[test]
fn tables_the_cell_model_cannot_represent_open_as_raw_markdown() {
    for source in [
        "| a | b\\|c |\n|---|---|\n| 1 | 2 |\n",
        "| `a | b` | c |\n|---|---|---|\n| 1 | 2 | 3 |\n",
        "| a |\n|---|\n| 1 | 2 |\n",
    ] {
        let lab = Lab::new(source);
        let layout = lab.layout();
        let canvas = lab.paint(layout.as_ref());
        let drawn = texts(&canvas);
        assert_eq!(drawn.len(), 1, "one text with the whole source: {source:?}");
        assert_eq!(drawn[0].0, source.trim_end());
        assert_eq!(layout.access_label(&lab.strings), "Table, raw Markdown");
    }
}

#[test]
fn a_raw_table_still_offers_its_cell_menu_flagged_raw() {
    let lab = Lab::new("| a | b\\|c |\n|---|---|\n| 1 | 2 |\n");
    let layout = lab.layout();
    let origin = Point::new(24.0, 100.0);
    let second = lab.at("b\\|c");
    let caret = layout.caret_rect(second);
    let probe = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
    let Some(EditorEvent::TableMenu { anchor, cell, raw }) = layout.context_menu(probe, origin)
    else {
        panic!("a table menu");
    };
    assert!(raw);
    assert_eq!(cell, CellPos { row: 0, col: 1 });
    assert_eq!(anchor, caret.translate(origin.x, origin.y));
    // A point in the body row maps to the cell it is in (the delimiter row is no row).
    let body = layout.caret_rect(lab.at("2 |"));
    let probe = Point::new(body.x + 1.0, body.y + body.height / 2.0);
    let Some(EditorEvent::TableMenu { cell, .. }) = layout.context_menu(probe, origin) else {
        panic!("a table menu");
    };
    assert_eq!(cell, CellPos { row: 1, col: 1 });
    // The start of the table is its first cell.
    let intro = Lab::new("intro\n\n| a | b\\|c |\n|---|---|\n| 1 | 2 |\n");
    let layout = intro.layout();
    let Some(EditorEvent::TableMenu { cell, .. }) =
        layout.context_menu(Point::new(-5.0, -5.0), origin)
    else {
        panic!("a table menu");
    };
    assert_eq!(cell, CellPos { row: 0, col: 0 });
}

#[test]
fn a_raw_table_places_carets_hits_and_selections_in_its_source_text() {
    let lab = Lab::new("| a | b\\|c |\n|---|---|\n| 1 | 2 |\n");
    let layout = lab.layout();
    let last_row = lab.at("| 1");

    let caret = layout.caret_rect(last_row + 2);
    assert!(
        close(caret.x, 2.0 * 8.0),
        "two advances into the line: {caret:?}"
    );
    assert!(caret.y > 0.0, "on the third source line: {caret:?}");
    let probe = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
    assert_eq!(layout.hit_test(probe), last_row + 2);
    assert_eq!(
        layout.line_range(last_row + 3),
        TextRange::new(last_row, last_row + "| 1 | 2 |".len())
    );

    let rects = layout.selection_rects(TextRange::new(last_row + 2, last_row + 5));
    assert_eq!(rects.len(), 1);
    assert!(close(rects[0].x, 2.0 * 8.0) && close(rects[0].width, 3.0 * 8.0));
    assert!(close(rects[0].y, caret.y));
}

#[test]
fn edit_as_raw_markdown_shows_the_source_until_the_caret_leaves_the_table() {
    let text = format!("{SAMPLE}\nafter\n");
    let mut h = Harness::new(&text);
    h.config(|config| config.format_tables_on_exit = false);
    let control = blocks_table::register(h.view.registry_mut());
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    h.set_caret(h.text().find("after").expect("after"));
    assert!(
        !control.execute(&mut h.view, &clipboard, CommandId::TableEditRaw),
        "no table at the caret"
    );
    h.set_caret(h.text().find("Apple").expect("Apple"));
    assert!(h.painted_texts().contains(&"Apple".to_owned()), "a grid");
    assert!(control.execute(&mut h.view, &clipboard, CommandId::TableEditRaw));
    let drawn = h.painted_texts();
    assert!(
        drawn
            .iter()
            .any(|t| t.starts_with("| Name | Qty |\n|:-----|----:|")),
        "{drawn:?}"
    );
    assert!(!drawn.contains(&"Apple".to_owned()));
    // Moving the caret out of the table ends the request.
    h.set_caret(h.text().find("after").expect("after"));
    assert!(
        h.painted_texts().contains(&"Apple".to_owned()),
        "a grid again"
    );
    h.set_caret(h.text().find("Apple").expect("Apple"));
    assert!(
        h.painted_texts().contains(&"Apple".to_owned()),
        "back in the table, still a grid"
    );
    assert!(format!("{control:?}").contains("TableControl"));
}

#[test]
fn a_renderer_asked_for_another_block_shows_its_source() {
    let mut registry = BlockRegistry::with_defaults();
    registry.register(
        BlockClass::Paragraph,
        Rc::new(TableRenderer::new(TableStrings::default())),
    );
    let doc = Document::new("just text\n");
    let engine = MonoTextEngine::default();
    let theme = Theme::light(DEFAULT_ACCENT);
    let (config, strings, reveal) = (
        EditorConfig::default(),
        EditorStrings::default(),
        Reveal::new(false, Vec::new()),
    );
    let cx = BlockCx {
        text: &engine,
        theme: &theme,
        config: &config,
        buffer: doc.buffer(),
        ast: doc.ast(),
        reveal: &reveal,
        objects: &NoObjects,
        composition: None,
        width: 352.0,
        folded: false,
        strings: &strings,
        registry: &registry,
    };
    let layout = registry.measure(&cx, &doc.ast().blocks[0]);
    let paint = BlockPaintCx {
        theme: &theme,
        config: &config,
        strings: &strings,
        text: &engine,
        hovered: false,
    };
    let mut canvas = RecordingCanvas::new();
    layout.paint(&paint, &mut canvas);
    assert_eq!(canvas.texts(), ["just text"]);
}

#[test]
fn custom_strings_describe_the_table() {
    let localizer = Localizer::new(
        "en",
        &[(
            "en",
            "table-access = { $rows } by { $columns }\ntable-access-raw = Source",
        )],
    );
    let mut registry = BlockRegistry::with_defaults();
    blocks_table::register_with(&mut registry, TableStrings::new(Rc::new(localizer)));
    let doc = Document::new(SAMPLE);
    let engine = MonoTextEngine::default();
    let theme = Theme::light(DEFAULT_ACCENT);
    let (config, strings, reveal) = (
        EditorConfig::default(),
        EditorStrings::default(),
        Reveal::new(false, Vec::new()),
    );
    let cx = BlockCx {
        text: &engine,
        theme: &theme,
        config: &config,
        buffer: doc.buffer(),
        ast: doc.ast(),
        reveal: &reveal,
        objects: &NoObjects,
        composition: None,
        width: 352.0,
        folded: false,
        strings: &strings,
        registry: &registry,
    };
    let layout = registry.measure(&cx, &doc.ast().blocks[0]);
    assert_eq!(layout.access_label(&strings), "3 by 2");
}

// ----- copy -----------------------------------------------------------------------------------

#[test]
fn copy_commands_put_the_table_on_the_clipboard_in_the_requested_form() {
    let mut h = Harness::new(&format!("{SAMPLE}\nafter\n"));
    let control = blocks_table::register(h.view.registry_mut());
    let clipboard: SharedClipboard = Rc::new(RefCell::new(MemoryClipboard::default()));
    h.set_caret(h.text().find("after").expect("after"));
    assert!(
        !control.execute(&mut h.view, &clipboard, CommandId::TableCopyAsCsv),
        "the caret is not in a table"
    );
    h.set_caret(h.text().find("Pear").expect("Pear"));

    assert!(control.execute(&mut h.view, &clipboard, CommandId::TableCopyAsCsv));
    assert_eq!(
        clipboard.borrow().text().expect("text"),
        Some("Name,Qty\r\nApple,3\r\nPear,12".to_owned())
    );

    assert!(control.execute(&mut h.view, &clipboard, CommandId::TableCopyAsMarkdown));
    let markdown = clipboard.borrow().text().expect("text").expect("some");
    assert_eq!(markdown.lines().count(), 4);
    assert!(markdown.starts_with("| Name"), "{markdown}");
    assert_eq!(clipboard.borrow().html().expect("html"), None);

    assert!(control.execute(&mut h.view, &clipboard, CommandId::TableCopyAsHtml));
    let html = clipboard.borrow().html().expect("html").expect("some");
    assert!(html.starts_with("<table>"), "{html}");
    assert!(html.contains("Name</th>"));
    assert!(html.contains("style=\"text-align:right\""));
    assert_eq!(clipboard.borrow().text().expect("text"), Some(html));

    assert!(
        !control.execute(&mut h.view, &clipboard, CommandId::TableDeleteRow),
        "only the host commands are handled here"
    );
}
