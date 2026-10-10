//! Code, math, Mermaid, image and attachment blocks: painted operations, reveal, controls and
//! resize edits, driven through the editor view.

mod editor_common;

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::command::CommandId;
use ssw_core::document::Document;
use ssw_core::edit::Selection;
use ssw_core::text::TextRange;
use ssw_ui::canvas::{DrawOp, ImageId, RecordingCanvas, SceneId};
use ssw_ui::editor::block::{BlockClass, BlockCx, BlockLayout, BlockPaintCx, BlockRegistry};
use ssw_ui::editor::blocks_media::{
    AttachmentInfo, AttachmentProvider, MediaEvent, MediaServices, MediaStrings, register,
    register_with,
};
use ssw_ui::editor::blocks_text;
use ssw_ui::editor::display::Reveal;
use ssw_ui::editor::object::NoObjects;
use ssw_ui::editor::object::{
    InlineObjectProvider, ObjectInfo, ObjectKind, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::editor::{DocumentKind, EditorEvent};
use ssw_ui::editor::{EditorConfig, EditorStrings, LinkTarget};
use ssw_ui::geometry::{Color, Point, Rect, Size};
use ssw_ui::input::{
    Cursor, ImeAttr, ImeAttrStyle, ImeEvent, PointerButton, PointerEvent, PointerType,
};
use ssw_ui::input::{Event, Modifiers, PointerEventKind, WheelEvent, vk};
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};

use editor_common::{ADVANCE, Harness, KEY_Z, close};

struct Provider {
    math: ObjectState,
    mermaid: ObjectState,
    image: ObjectState,
}

impl InlineObjectProvider for Provider {
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState {
        match request.kind {
            ObjectKind::DisplayMath => self.math,
            ObjectKind::Mermaid => self.mermaid,
            ObjectKind::Image => self.image,
            ObjectKind::Math => ObjectState::Pending,
        }
    }
}

fn ready(visual: ObjectVisual, width: f32, height: f32) -> ObjectState {
    ObjectState::Ready(ObjectInfo {
        visual,
        size: Size::new(width, height),
        baseline: 0.0,
    })
}

fn harness(text: &str, provider: Provider) -> Harness {
    let mut h = Harness::new(text);
    register(h.view.registry_mut());
    h.view.set_object_provider(Rc::new(provider));
    h
}

fn plain() -> Provider {
    Provider {
        math: ObjectState::Pending,
        mermaid: ObjectState::Pending,
        image: ObjectState::Pending,
    }
}

fn fills(canvas: &RecordingCanvas, color: Color) -> Vec<Rect> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match &entry.op {
            DrawOp::FillRoundedRect { color: c, .. } if *c == color => entry.bounds,
            _ => None,
        })
        .collect()
}

fn scene_rects(canvas: &RecordingCanvas, id: SceneId) -> Vec<Rect> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match &entry.op {
            DrawOp::Scene { scene, .. } if *scene == id => entry.bounds,
            _ => None,
        })
        .collect()
}

fn text_color(canvas: &RecordingCanvas, wanted: &str) -> Option<Color> {
    canvas.ops().find_map(|op| match op {
        DrawOp::Text { text, color, .. } if text == wanted => Some(*color),
        _ => None,
    })
}

const CODE: &str = "a\n\n```rust\nlet x = 1;\n```\n";

#[test]
fn code_block_shows_only_the_code_until_the_caret_enters() {
    let mut h = harness(CODE, plain());
    assert_eq!(h.painted_texts(), ["a", "let x = 1;"]);
    let boxes = fills(&h.paint(), h.theme.tokens.code_bg);
    assert_eq!(boxes.len(), 1);
    assert!(close(boxes[0].width, 352.0));

    h.set_caret(16);
    assert_eq!(h.painted_texts(), ["a", "```rust\nlet x = 1;\n```"]);
    h.set_caret(0);
    assert_eq!(h.painted_texts(), ["a", "let x = 1;"]);
}

#[test]
fn clicking_a_code_line_places_the_caret_in_the_source() {
    let mut h = harness(CODE, plain());
    let code_box = fills(&h.paint(), h.theme.tokens.code_bg)[0];
    // 14 DIPs of padding, then two characters of 8 DIPs: between `le` and `t`.
    h.click(
        Point::new(code_box.x + 14.0 + 17.0, code_box.y + 14.0),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(13));
}

#[test]
fn copy_button_appears_on_hover_and_reports_the_code() {
    let mut h = harness(CODE, plain());
    h.events();
    let code_box = fills(&h.paint(), h.theme.tokens.code_bg)[0];
    assert!(fills(&h.paint(), h.theme.tokens.hover_bg).is_empty());
    h.pointer(
        PointerEventKind::Move,
        code_box.center(),
        0,
        Modifiers::NONE,
    );
    let canvas = h.paint();
    assert!(canvas.texts().contains(&"Copy"));
    assert!(canvas.texts().contains(&"rust"));
    let pill = fills(&canvas, h.theme.tokens.hover_bg)[0];
    h.click(pill.center(), 1, Modifiers::NONE);
    let events = h.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EditorEvent::Media(MediaEvent::CopyCode { range, text })
                if range.start == 3 && text == "let x = 1;\n"
        )),
        "{events:?}"
    );
    assert_eq!(h.selection(), Selection::caret(0));
}

#[test]
fn math_block_draws_a_scene_and_shows_source_in_error_color_on_failure() {
    let text = "a\n\n$$\nx^2\n$$\n";
    let mut h = harness(
        text,
        Provider {
            math: ready(ObjectVisual::Scene(SceneId(9)), 100.0, 40.0),
            ..plain()
        },
    );
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["a"]);
    let scenes = scene_rects(&canvas, SceneId(9));
    assert_eq!(scenes.len(), 1);
    assert!(close(scenes[0].width, 100.0) && close(scenes[0].x, 24.0 + 126.0));
    h.set_caret(5);
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["a", "$$\nx^2\n$$"]);
    assert_eq!(scene_rects(&canvas, SceneId(9)).len(), 1);

    let failed = harness(
        text,
        Provider {
            math: ObjectState::Failed,
            ..plain()
        },
    );
    let canvas = failed.paint();
    assert_eq!(
        text_color(&canvas, "x^2"),
        Some(failed.theme.tokens.error),
        "failed math shows its source in the error color"
    );
    assert!(scene_rects(&canvas, SceneId(9)).is_empty());
}

#[test]
fn mermaid_shows_the_diagram_and_falls_back_to_a_code_block() {
    let text = "a\n\n```mermaid\ngraph TD\n```\n";
    let scene = ready(ObjectVisual::Scene(SceneId(4)), 200.0, 100.0);
    let mut h = harness(
        text,
        Provider {
            mermaid: scene,
            ..plain()
        },
    );
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["a"]);
    assert_eq!(scene_rects(&canvas, SceneId(4)).len(), 1);
    h.set_caret(8);
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["a", "```mermaid\ngraph TD\n```"]);
    assert_eq!(scene_rects(&canvas, SceneId(4)).len(), 1);

    let failed = harness(
        text,
        Provider {
            mermaid: ObjectState::Failed,
            ..plain()
        },
    );
    assert_eq!(
        failed.painted_texts(),
        ["a", "graph TD", "Unsupported diagram"]
    );
}

fn image_at(canvas: &RecordingCanvas) -> Rect {
    canvas
        .recorded()
        .iter()
        .find_map(|entry| match &entry.op {
            DrawOp::Image { image, .. } if *image == ImageId(3) => entry.bounds,
            _ => None,
        })
        .expect("the image is drawn")
}

/// Hovers the image, grabs its resize handle and drags it by `dx`.
fn drag_handle(h: &mut Harness, dx: f32) {
    let image = image_at(&h.paint());
    h.pointer(PointerEventKind::Move, image.center(), 0, Modifiers::NONE);
    let canvas = h.paint();
    let handle = fills(&canvas, h.theme.tokens.accent)
        .into_iter()
        .find(|rect| close(rect.width, 10.0))
        .expect("the resize handle is drawn");
    let from = handle.center();
    h.pointer(PointerEventKind::Down, from, 1, Modifiers::NONE);
    let to = Point::new(from.x + dx, from.y);
    h.pointer(PointerEventKind::Move, to, 0, Modifiers::NONE);
    h.pointer(PointerEventKind::Up, to, 0, Modifiers::NONE);
}

fn image_provider(state: ObjectState) -> Provider {
    Provider {
        image: state,
        ..plain()
    }
}

#[test]
fn lone_image_is_a_block_no_wider_than_the_column() {
    let h = harness(
        "a\n\n![alt](p.png)\n",
        image_provider(ready(ObjectVisual::Image(ImageId(3)), 400.0, 100.0)),
    );
    let rect = image_at(&h.paint());
    assert!(close(rect.width, 352.0) && close(rect.height, 88.0));
    assert_eq!(h.painted_texts(), ["a"]);
}

#[test]
fn resize_handle_writes_the_width_as_one_undo_step() {
    let ready_image = ready(ObjectVisual::Image(ImageId(3)), 400.0, 100.0);
    let mut h = harness("a\n\n![alt](p.png)\n", image_provider(ready_image));
    drag_handle(&mut h, -100.0);
    assert_eq!(h.text(), "a\n\n![alt|252](p.png)\n");
    drag_handle(&mut h, -152.0);
    assert_eq!(h.text(), "a\n\n![alt|200](p.png)\n");
    h.key(KEY_Z, Modifiers::CTRL);
    assert_eq!(h.text(), "a\n\n![alt|252](p.png)\n");
    h.key(KEY_Z, Modifiers::CTRL);
    assert_eq!(h.text(), "a\n\n![alt](p.png)\n");
}

#[test]
fn resize_rewrites_the_width_of_an_img_tag_and_drops_its_height() {
    let ready_image = ready(ObjectVisual::Image(ImageId(3)), 400.0, 100.0);
    let mut h = harness(
        "a\n\n<img src=\"p.png\" width=\"40\" height=\"30\">\n",
        image_provider(ready_image),
    );
    drag_handle(&mut h, -100.0);
    assert_eq!(h.text(), "a\n\n<img src=\"p.png\" width=\"252\">\n");
}

#[test]
fn image_between_text_stays_inline() {
    let ready_image = ready(ObjectVisual::Image(ImageId(3)), 20.0, 20.0);
    let h = harness("a ![x](p.png) b\n", image_provider(ready_image));
    assert_eq!(h.painted_texts(), ["a \u{FFFC} b"]);
}

#[test]
fn blocked_image_offers_click_to_load_and_failed_shows_the_error() {
    let text = "a\n\n![alt](https://x.test/p.png)\n";
    let mut h = harness(text, image_provider(ObjectState::Blocked));
    let canvas = h.paint();
    assert!(canvas.texts().contains(&"Click to load"));
    let placeholder = fills(&canvas, h.theme.tokens.code_bg)[0];
    h.events();
    h.click(placeholder.center(), 1, Modifiers::NONE);
    let events = h.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EditorEvent::Media(MediaEvent::LoadImage { dest, range })
                if dest == "https://x.test/p.png" && range.start == 3
        )),
        "{events:?}"
    );

    let failed = harness(text, image_provider(ObjectState::Failed));
    let canvas = failed.paint();
    assert_eq!(
        text_color(&canvas, "Image could not be loaded"),
        Some(failed.theme.tokens.error)
    );
    let loading = harness(text, image_provider(ObjectState::Pending));
    assert!(loading.paint().texts().contains(&"Loading image…"));
}

/// Answers Mermaid requests with `state` and remembers the sources it was asked about.
struct Recording {
    state: ObjectState,
    sources: Rc<RefCell<Vec<String>>>,
}

impl InlineObjectProvider for Recording {
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState {
        if request.kind == ObjectKind::Mermaid {
            self.sources.borrow_mut().push(request.source.to_owned());
            self.state
        } else {
            ObjectState::Pending
        }
    }
}

fn mmd_harness(state: ObjectState, sources: &Rc<RefCell<Vec<String>>>) -> Harness {
    let mut h = Harness::new("graph TD\n  A-->B\n");
    register(h.view.registry_mut());
    h.view.set_object_provider(Rc::new(Recording {
        state,
        sources: Rc::clone(sources),
    }));
    h.config(|config| config.document_kind = DocumentKind::Mermaid);
    h
}

#[test]
fn mmd_document_is_one_mermaid_block_with_the_diagram_below_the_source() {
    let sources = Rc::new(RefCell::new(Vec::new()));
    let scene = ready(ObjectVisual::Scene(SceneId(4)), 200.0, 100.0);
    let mut h = mmd_harness(scene, &sources);
    let canvas = h.paint();
    assert_eq!(canvas.texts(), ["graph TD\n  A-->B\n"]);
    let scenes = scene_rects(&canvas, SceneId(4));
    assert_eq!(scenes.len(), 1);
    assert!(scenes[0].y > 24.0 + 28.0, "the diagram is below the source");
    assert_eq!(
        sources.borrow().last().map(String::as_str),
        Some("graph TD\n  A-->B\n")
    );
    h.type_text("x");
    assert_eq!(h.text(), "xgraph TD\n  A-->B\n");

    let failed = mmd_harness(ObjectState::Failed, &sources);
    assert_eq!(
        failed.painted_texts(),
        ["graph TD\n  A-->B\n", "Unsupported diagram"]
    );
}

#[test]
fn plain_text_documents_show_every_line_as_written() {
    let mut h = Harness::new("# not a heading\n- not a list\n**x**\n");
    h.config(|config| config.document_kind = DocumentKind::PlainText);
    let lines = ["# not a heading", "- not a list", "**x**"];
    assert_eq!(&h.painted_texts()[..3], lines);
    h.set_caret(18);
    assert!(h.view.caret_rect().is_some());
    assert_eq!(&h.painted_texts()[..3], lines);
    h.type_text("!");
    assert_eq!(h.text(), "# not a heading\n- !not a list\n**x**\n");
}

#[test]
fn plain_and_mermaid_documents_type_plain_text() {
    // Markdown continues the list on Enter ...
    let mut md = Harness::new("- item\n");
    md.set_caret(6);
    md.press(vk::RETURN);
    assert!(md.text().starts_with("- item\n- "), "{:?}", md.text());

    for kind in [DocumentKind::PlainText, DocumentKind::Mermaid] {
        let mut h = Harness::new("- item\n");
        h.config(|config| config.document_kind = kind);
        h.set_caret(6);
        h.press(vk::RETURN);
        assert_eq!(
            h.text(),
            "- item\n\n",
            "{kind:?}: Enter only breaks the line"
        );
        h.press(vk::TAB);
        assert_eq!(h.text(), "- item\n\t\n", "{kind:?}: Tab indents");
        h.key(vk::TAB, Modifiers::SHIFT);
        assert_eq!(h.text(), "- item\n\n", "{kind:?}: Shift+Tab outdents");
        h.type_text("**");
        assert_eq!(h.text(), "- item\n**\n", "{kind:?}: nothing is paired");
        h.press(vk::BACK);
        assert_eq!(h.text(), "- item\n*\n");
        h.view.set_selection(Selection::new(0, 6));
        assert!(!h.view.execute(CommandId::FormatBold), "{kind:?}");
        assert!(!h.view.execute(CommandId::FormatBulletList), "{kind:?}");
        assert!(!h.view.execute(CommandId::FormatCodeBlock), "{kind:?}");
        assert_eq!(h.text(), "- item\n*\n", "formatting commands do nothing");
        assert!(h.view.execute(CommandId::EditSelectAll));
    }
}

#[test]
fn unwrapped_plain_text_scrolls_horizontally_and_keeps_the_caret_visible() {
    let long = format!("{}\n", "x".repeat(100));
    let mut h = Harness::new(&long);
    h.config(|config| {
        config.document_kind = DocumentKind::PlainText;
        config.wrap = false;
    });
    h.press(vk::END);
    h.arrange();
    let caret = h
        .view
        .caret_rect()
        .expect("the caret is scrolled into view");
    assert!(caret.x >= 0.0 && caret.x <= 400.0, "{caret:?}");
    let first_line = caret.y;

    // The 100 characters are 800 DIPs wide; the view shows 352 of them (400 minus two margins),
    // so the scroll tops out at 448 and the text starts 448 DIPs left of the column (x = 24).
    let text_x = |h: &Harness| {
        h.paint()
            .recorded()
            .iter()
            .find_map(|entry| match &entry.op {
                DrawOp::Text { .. } => entry.bounds.map(|bounds| bounds.x),
                _ => None,
            })
            .expect("the line is drawn")
    };
    let wheel = |h: &mut Harness, dx: f32| {
        h.event(Event::Wheel(WheelEvent {
            delta: Point::new(dx, 0.0),
            precise: true,
            position: Point::new(100.0, 100.0),
            mods: Modifiers::NONE,
        }));
        h.arrange();
    };
    assert!(close(text_x(&h), 24.0 - 448.0), "{}", text_x(&h));
    wheel(&mut h, 500.0);
    assert!(
        close(text_x(&h), 24.0 - 448.0),
        "no scrolling past the widest line"
    );
    wheel(&mut h, -20.0);
    assert!(close(text_x(&h), 24.0 - 428.0));
    let scrolled = h
        .view
        .caret_rect()
        .expect("still visible after scrolling back a little");
    assert!(close(scrolled.x - caret.x, 20.0) && close(scrolled.y, first_line));
    wheel(&mut h, -1000.0);
    assert!(
        close(text_x(&h), 24.0),
        "no scrolling before the line start"
    );

    // Wrapped (the default), the same line breaks over several rows and the view stays put.
    let mut wrapped = Harness::new(&long);
    wrapped.config(|config| config.document_kind = DocumentKind::PlainText);
    wrapped.press(vk::END);
    wrapped.arrange();
    let end = wrapped.view.caret_rect().expect("visible");
    assert!(
        end.y > first_line,
        "the line wrapped: {end:?} vs {first_line}"
    );
    assert!(end.x <= 400.0);
}

struct Files;

impl AttachmentProvider for Files {
    fn attachment(&self, dest: &str) -> Option<AttachmentInfo> {
        (dest == "doc.pdf").then(|| AttachmentInfo {
            name: "doc.pdf".to_owned(),
            type_label: "PDF".to_owned(),
            detail: "1.2 MB · today".to_owned(),
            icon: Some(ImageId(5)),
        })
    }
}

#[test]
fn link_to_a_local_file_becomes_a_chip() {
    let mut h = Harness::new("a\n\n[doc.pdf](doc.pdf)\n\n[x](web.html)\n");
    register_with(
        h.view.registry_mut(),
        MediaServices {
            strings: MediaStrings::default(),
            attachments: Rc::new(Files),
        },
    );
    let canvas = h.paint();
    assert_eq!(
        canvas.texts(),
        ["a", "doc.pdf", "1.2 MB · today", "PDF", "x"]
    );
    assert!(
        canvas
            .ops()
            .any(|op| matches!(op, DrawOp::Image { image, .. } if *image == ImageId(5)))
    );
}

/// Rectangles filled with `FillRect` (selections) in `color`.
fn rect_fills(canvas: &RecordingCanvas, color: Color) -> Vec<Rect> {
    canvas
        .recorded()
        .iter()
        .filter_map(|entry| match &entry.op {
            DrawOp::FillRect { color: c, .. } if *c == color => entry.bounds,
            _ => None,
        })
        .collect()
}

/// Answers every destination with a file.
struct AnyFile;

impl AttachmentProvider for AnyFile {
    fn attachment(&self, dest: &str) -> Option<AttachmentInfo> {
        Some(AttachmentInfo {
            name: dest.to_owned(),
            type_label: "FILE".to_owned(),
            detail: "1 KB".to_owned(),
            icon: None,
        })
    }
}

const CHIP: &str = "a\n\n[doc.pdf](doc.pdf)\n";

fn chip_harness(text: &str, attachments: Rc<dyn AttachmentProvider>, read_only: bool) -> Harness {
    let mut h = Harness::new(text);
    register_with(
        h.view.registry_mut(),
        MediaServices {
            strings: MediaStrings::default(),
            attachments,
        },
    );
    h.view.set_read_only(read_only);
    h
}

#[test]
fn attachment_chip_clicks_pick_the_nearest_edge_and_ctrl_click_opens_the_file() {
    let end = CHIP.trim_end().len();
    let chip_of = |h: &Harness| fills(&h.paint(), h.theme.tokens.code_bg)[0];

    let mut h = chip_harness(CHIP, Rc::new(Files), false);
    let chip = chip_of(&h);
    h.click(
        Point::new(chip.x + 10.0, chip.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(3));

    let mut h = chip_harness(CHIP, Rc::new(Files), false);
    let chip = chip_of(&h);
    h.click(
        Point::new(chip.right() - 10.0, chip.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(end));

    let mut h = chip_harness(CHIP, Rc::new(Files), false);
    let chip = chip_of(&h);
    h.events();
    h.click(chip.center(), 1, Modifiers::CTRL);
    let events = h.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EditorEvent::LinkActivated(LinkTarget::Url(dest)) if dest == "doc.pdf"
        )),
        "{events:?}"
    );
    let below = Point::new(chip.x + 10.0, chip.bottom() + 2.0);
    h.click(below, 1, Modifiers::CTRL);
    assert!(
        !h.events()
            .iter()
            .any(|event| matches!(event, EditorEvent::LinkActivated(_))),
        "only the chip itself is the link"
    );
}

#[test]
fn a_revealed_attachment_shows_its_source_below_the_chip() {
    let end = CHIP.trim_end().len();
    let mut h = chip_harness(CHIP, Rc::new(Files), false);
    let chip = fills(&h.paint(), h.theme.tokens.code_bg)[0];
    h.set_caret(7);
    assert!(h.paint().texts().contains(&"[doc.pdf](doc.pdf)"));
    assert_eq!(h.access().description, "Attachment: doc.pdf");
    let caret = h.view.rect_at(7).expect("the caret is in the source");
    assert!(caret.y > chip.bottom(), "{caret:?} below {chip:?}");

    let target = h.view.rect_at(9).expect("inside the source");
    h.click(
        Point::new(target.x + 1.0, target.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(9));
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(3));
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(end));

    h.view.set_selection(Selection::new(4, 9));
    let selected = rect_fills(&h.paint(), h.theme.tokens.selection_bg);
    assert_eq!(selected.len(), 1);
    let (from, to) = (h.view.rect_at(4).unwrap(), h.view.rect_at(9).unwrap());
    assert!(close(selected[0].x, from.x) && close(selected[0].width, to.x - from.x));
    assert!(selected[0].y > chip.bottom());
}

#[test]
fn a_hidden_attachment_chip_is_one_unit_for_carets_and_selections() {
    let end = CHIP.trim_end().len();
    let mut h = chip_harness(CHIP, Rc::new(Files), true);
    let chip = fills(&h.paint(), h.theme.tokens.code_bg)[0];
    let (start_rect, end_rect) = (h.view.rect_at(3).unwrap(), h.view.rect_at(end).unwrap());
    assert!(close(start_rect.x, chip.x) && close(end_rect.x, chip.right()));

    let selection_bg = h.theme.tokens.selection_bg;
    h.view.set_selection(Selection::new(3, end));
    let selected = rect_fills(&h.paint(), selection_bg);
    assert_eq!(selected.len(), 1);
    assert!(close(selected[0].x, chip.x) && close(selected[0].width, chip.width));
    h.view.set_selection(Selection::caret(3));
    assert!(rect_fills(&h.paint(), selection_bg).is_empty());

    h.click(
        Point::new(chip.x + 10.0, chip.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(3));
    h.click(
        Point::new(chip.right() - 10.0, chip.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(end));
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(3), "the chip is one line");
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(end));
}

#[test]
fn only_inline_links_become_chips_and_without_a_provider_links_stay_links() {
    let h = chip_harness(
        "a\n\n<https://x.test/f.zip>\n\n[f](f.zip)\n",
        Rc::new(AnyFile),
        false,
    );
    let canvas = h.paint();
    assert!(canvas.texts().contains(&"https://x.test/f.zip"));
    assert_eq!(
        canvas
            .texts()
            .iter()
            .filter(|text| **text == "FILE")
            .count(),
        1,
        "the autolink stays a link, the inline link is a chip"
    );

    let mut none = Harness::new("a\n\n[x](web.html)\n");
    register(none.view.registry_mut());
    assert_eq!(none.painted_texts(), ["a", "x"]);
    assert!(format!("{:?}", MediaServices::default()).contains("AttachmentProvider"));
}

#[test]
fn media_renderers_show_a_block_of_another_kind_as_its_source() {
    let doc = Document::new("plain\n");
    let theme = Theme::light(DEFAULT_ACCENT);
    let config = EditorConfig::default();
    let engine = MonoTextEngine::default();
    let strings = EditorStrings::default();
    let mut registry = BlockRegistry::with_defaults();
    register(&mut registry);
    let reveal = Reveal::new(false, Vec::new());
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
    let block = &doc.ast().blocks[0];
    let layouts = [
        registry.renderer(BlockClass::CodeBlock).measure(&cx, block),
        registry.renderer(BlockClass::MathBlock).measure(&cx, block),
        registry.measure_document(DocumentKind::Mermaid, &cx, block),
    ];
    let paint_cx = BlockPaintCx {
        theme: &theme,
        config: &config,
        strings: &strings,
        text: &engine,
        hovered: false,
    };
    for layout in &layouts {
        let mut canvas = RecordingCanvas::new();
        layout.paint(&paint_cx, &mut canvas);
        assert!(canvas.texts().contains(&"plain"), "{:?}", canvas.texts());
    }
}

/// The layout of the last block of `text` with every media renderer and nothing revealed.
fn hidden_layout(
    text: &str,
    provider: &dyn InlineObjectProvider,
    attachments: Rc<dyn AttachmentProvider>,
) -> Box<dyn BlockLayout> {
    let doc = Document::new(text);
    let theme = Theme::light(DEFAULT_ACCENT);
    let config = EditorConfig::default();
    let engine = MonoTextEngine::default();
    let strings = EditorStrings::default();
    let mut registry = BlockRegistry::with_defaults();
    register_with(
        &mut registry,
        MediaServices {
            strings: MediaStrings::default(),
            attachments,
        },
    );
    let reveal = Reveal::new(false, Vec::new());
    let cx = BlockCx {
        text: &engine,
        theme: &theme,
        config: &config,
        buffer: doc.buffer(),
        ast: doc.ast(),
        reveal: &reveal,
        objects: provider,
        composition: None,
        width: 352.0,
        folded: false,
        strings: &strings,
        registry: &registry,
    };
    registry.measure(&cx, doc.ast().blocks.last().expect("a block"))
}

#[test]
fn hidden_media_layouts_select_nothing_for_an_empty_range() {
    let chip = hidden_layout(CHIP, &plain(), Rc::new(Files));
    let diagram = hidden_layout(
        MMD,
        &Provider {
            mermaid: ready(ObjectVisual::Scene(SceneId(4)), 200.0, 100.0),
            ..plain()
        },
        Rc::new(Files),
    );
    let image = hidden_layout(
        "a\n\n![alt](p.png)\n",
        &image_provider(ready(ObjectVisual::Image(ImageId(3)), 100.0, 40.0)),
        Rc::new(Files),
    );
    for layout in [chip, diagram, image] {
        assert!(layout.selection_rects(TextRange::at(3)).is_empty());
        assert_eq!(layout.selection_rects(TextRange::new(3, 8)).len(), 1);
    }
}

#[test]
fn math_source_is_muted_while_pending_and_a_one_line_block_reveals_whole() {
    let h = harness("a\n\n$$\nx^2\ny\n$$\n", plain());
    assert_eq!(
        text_color(&h.paint(), "x^2\ny"),
        Some(h.theme.tokens.text_muted)
    );

    let mut one = harness(
        "a\n\n$$x$$\n",
        Provider {
            math: ready(ObjectVisual::Scene(SceneId(9)), 40.0, 20.0),
            ..plain()
        },
    );
    one.set_caret(5);
    assert_eq!(one.painted_texts(), ["a", "$$x$$"]);
}

#[test]
fn revealed_math_source_selects_navigates_by_line_and_is_named() {
    let mut h = harness(
        "a\n\n$$\nx^2\ny\n$$\n",
        Provider {
            math: ready(ObjectVisual::Scene(SceneId(9)), 100.0, 40.0),
            ..plain()
        },
    );
    h.set_caret(7);
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(6));
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(9));
    assert_eq!(h.access().description, "Math block");

    h.view.set_selection(Selection::new(6, 9));
    let selected = rect_fills(&h.paint(), h.theme.tokens.selection_bg);
    assert_eq!(selected.len(), 1);
    assert!(close(selected[0].width, 3.0 * ADVANCE), "{selected:?}");

    h.set_caret(7);
    let target = h.view.rect_at(8).expect("inside the source");
    h.click(
        Point::new(target.x + 1.0, target.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(8));
}

const MMD: &str = "a\n\n```mermaid\ngraph TD\n```\n";

fn mermaid_harness(state: ObjectState, read_only: bool) -> Harness {
    let mut h = harness(
        MMD,
        Provider {
            mermaid: state,
            ..plain()
        },
    );
    h.view.set_read_only(read_only);
    h
}

#[test]
fn a_pending_mermaid_diagram_shows_its_source_without_a_note() {
    let h = mermaid_harness(ObjectState::Pending, false);
    assert_eq!(h.painted_texts(), ["a", "graph TD"]);
}

#[test]
fn a_hidden_diagram_is_one_unit_with_an_export_menu() {
    let end = MMD.trim_end().len();
    let scene = ready(ObjectVisual::Scene(SceneId(4)), 200.0, 100.0);
    let mut h = mermaid_harness(scene, true);
    let rect = scene_rects(&h.paint(), SceneId(4))[0];
    let (start_rect, end_rect) = (h.view.rect_at(3).unwrap(), h.view.rect_at(end).unwrap());
    assert!(close(start_rect.x, rect.x) && close(end_rect.x, rect.right()));

    h.click(
        Point::new(rect.x + 10.0, rect.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(3));
    h.click(
        Point::new(rect.right() - 10.0, rect.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(end));
    h.press(vk::HOME);
    assert_eq!(
        h.selection(),
        Selection::caret(3),
        "the diagram is one line"
    );
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(end));
    assert!(
        rect_fills(&h.paint(), h.theme.tokens.selection_bg).is_empty(),
        "a caret selects nothing"
    );

    h.view.set_selection(Selection::new(3, end));
    let selected = rect_fills(&h.paint(), h.theme.tokens.selection_bg);
    assert_eq!(selected.len(), 1);
    assert!(close(selected[0].x, rect.x) && close(selected[0].width, rect.width));
    assert_eq!(h.access().description, "Mermaid diagram");

    h.events();
    let at = rect.center();
    h.event(Event::Pointer(PointerEvent {
        kind: PointerEventKind::Down,
        button: Some(PointerButton::Right),
        position: at,
        pointer_type: PointerType::Mouse,
        click_count: 1,
        mods: Modifiers::NONE,
    }));
    let events = h.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EditorEvent::Media(MediaEvent::MermaidMenu { source, anchor, .. })
                if source == "graph TD" && close(anchor.x, at.x) && close(anchor.y, at.y)
        )),
        "{events:?}"
    );
}

#[test]
fn a_revealed_diagram_keeps_the_copy_button_and_line_navigation_of_its_source() {
    let scene = ready(ObjectVisual::Scene(SceneId(4)), 200.0, 100.0);
    let mut h = mermaid_harness(scene, false);
    h.set_caret(16);
    let code_box = fills(&h.paint(), h.theme.tokens.code_bg)[0];
    h.pointer(
        PointerEventKind::Move,
        code_box.center(),
        0,
        Modifiers::NONE,
    );
    let pill = fills(&h.paint(), h.theme.tokens.hover_bg)[0];
    h.requests.take_cursor();
    h.pointer(PointerEventKind::Move, pill.center(), 0, Modifiers::NONE);
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
    h.events();
    h.click(pill.center(), 1, Modifiers::NONE);
    let events = h.events();
    assert!(
        events.iter().any(|event| matches!(
            event,
            EditorEvent::Media(MediaEvent::CopyCode { text, .. }) if text == "graph TD\n"
        )),
        "{events:?}"
    );

    h.set_caret(16);
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(14));
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(22));
}

#[test]
fn code_blocks_select_by_line_and_are_named_after_their_language() {
    let mut h = harness(CODE, plain());
    h.view.set_selection(Selection::new(13, 17));
    let selected = rect_fills(&h.paint(), h.theme.tokens.selection_bg);
    assert_eq!(selected.len(), 1);
    let (from, to) = (h.view.rect_at(13).unwrap(), h.view.rect_at(17).unwrap());
    assert!(close(selected[0].x, from.x) && close(selected[0].width, to.x - from.x));
    assert_eq!(h.access().description, "Code block: rust");

    h.set_caret(16);
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(11));
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(21));

    let mut bare = harness("a\n\n```\nx\n```\n", plain());
    bare.set_caret(5);
    assert_eq!(bare.access().description, "Code block");
}

#[test]
fn a_partly_consumed_tab_in_a_quoted_code_block_shows_as_spaces() {
    let mut h = Harness::new("a\n\n>\t\tfoo\n");
    blocks_text::register(h.view.registry_mut());
    register(h.view.registry_mut());
    let texts = h.painted_texts();
    assert!(texts.contains(&"  foo".to_owned()), "{texts:?}");
}

#[test]
fn ime_composition_inside_a_code_line_splits_the_token_it_interrupts() {
    let text = "a\n\n```rust\nlet s = \"abc\"; let n = 1;\n```\n";
    let mut h = harness(text, plain());
    h.config(|config| config.syntax_highlighting = true);
    h.set_caret(21);
    h.ime(ImeEvent::Start);
    h.ime(ImeEvent::Update {
        text: "かな".into(),
        caret: 1,
        attrs: vec![ImeAttr {
            range: 0..3,
            style: ImeAttrStyle::TargetConverted,
        }],
    });
    let canvas = h.paint();
    assert!(
        canvas
            .texts()
            .iter()
            .any(|text| text.contains("let s = \"aかなbc\"; let n = 1;")),
        "{:?}",
        canvas.texts()
    );
    assert_eq!(h.text(), text, "nothing is written before the commit");
    let caret = h
        .view
        .caret_rect()
        .expect("the caret is in the composition");
    let start = h.left + 14.0 + 10.0 * ADVANCE;
    assert!(
        caret.x >= start - 0.01 && caret.x <= start + 2.0 * ADVANCE + 0.01,
        "{caret:?}"
    );
    h.ime(ImeEvent::End);
}

#[test]
fn a_hidden_image_is_one_unit_and_a_revealed_one_shows_its_source() {
    const IMG: &str = "a\n\n![alt](p.png)\n";
    let end = IMG.trim_end().len();
    let ready_image = ready(ObjectVisual::Image(ImageId(3)), 100.0, 40.0);
    let mut h = harness(IMG, image_provider(ready_image));
    h.view.set_read_only(true);
    let rect = image_at(&h.paint());
    let (start_rect, end_rect) = (h.view.rect_at(3).unwrap(), h.view.rect_at(end).unwrap());
    assert!(close(start_rect.x, rect.x) && close(end_rect.x, rect.right()));
    h.click(
        Point::new(rect.x + 10.0, rect.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(3));
    h.click(
        Point::new(h.left + 300.0, rect.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(end));
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(3), "the image is one line");
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(end));
    let selection_bg = h.theme.tokens.selection_bg;
    assert!(
        rect_fills(&h.paint(), selection_bg).is_empty(),
        "a caret selects nothing"
    );
    h.view.set_selection(Selection::new(3, end));
    let selected = rect_fills(&h.paint(), selection_bg);
    assert_eq!(selected.len(), 1);
    assert!(close(selected[0].width, rect.width) && close(selected[0].x, rect.x));

    let mut h = harness(IMG, image_provider(ready_image));
    h.set_caret(6);
    assert_eq!(h.painted_texts(), ["a", "![alt](p.png)"]);
    assert_eq!(h.access().description, "Image: alt");
    h.press(vk::HOME);
    assert_eq!(h.selection(), Selection::caret(3));
    h.press(vk::END);
    assert_eq!(h.selection(), Selection::caret(end));
    h.view.set_selection(Selection::new(4, 7));
    let selected = rect_fills(&h.paint(), h.theme.tokens.selection_bg);
    assert_eq!(selected.len(), 1);
    assert!(close(selected[0].width, 3.0 * ADVANCE), "{selected:?}");
    h.set_caret(6);
    let target = h.view.rect_at(8).expect("inside the source");
    h.click(
        Point::new(target.x + 1.0, target.center().y),
        1,
        Modifiers::NONE,
    );
    assert_eq!(h.selection(), Selection::caret(8));
}

#[test]
fn the_resize_handle_shows_its_cursor_previews_the_drag_and_ignores_a_zero_drag() {
    let ready_image = ready(ObjectVisual::Image(ImageId(3)), 400.0, 100.0);
    let mut h = harness("a\n\n![alt](p.png)\n", image_provider(ready_image));
    let image = image_at(&h.paint());
    h.pointer(PointerEventKind::Move, image.center(), 0, Modifiers::NONE);
    let handle = fills(&h.paint(), h.theme.tokens.accent)
        .into_iter()
        .find(|rect| close(rect.width, 10.0))
        .expect("the resize handle is drawn");
    h.requests.take_cursor();
    h.pointer(PointerEventKind::Move, handle.center(), 0, Modifiers::NONE);
    assert_eq!(h.requests.take_cursor(), Some(Cursor::SizeWE));

    h.pointer(PointerEventKind::Down, handle.center(), 1, Modifiers::NONE);
    let to = Point::new(handle.center().x - 100.0, handle.center().y);
    h.pointer(PointerEventKind::Move, to, 0, Modifiers::NONE);
    let shown = image_at(&h.paint());
    assert!(
        close(shown.width, 252.0) && close(shown.height, 63.0),
        "the drag previews the new size: {shown:?}"
    );
    h.pointer(PointerEventKind::Up, to, 0, Modifiers::NONE);
    assert_eq!(h.text(), "a\n\n![alt|252](p.png)\n");

    let mut still = harness("a\n\n![alt](p.png)\n", image_provider(ready_image));
    drag_handle(&mut still, 0.0);
    assert_eq!(
        still.text(),
        "a\n\n![alt](p.png)\n",
        "no width change, no edit"
    );
}

#[test]
fn a_hovered_blocked_image_shows_the_hand_cursor() {
    let mut h = harness(
        "a\n\n![alt](https://x.test/p.png)\n",
        image_provider(ObjectState::Blocked),
    );
    let placeholder = fills(&h.paint(), h.theme.tokens.code_bg)[0];
    h.pointer(
        PointerEventKind::Move,
        placeholder.center(),
        0,
        Modifiers::NONE,
    );
    h.paint();
    h.requests.take_cursor();
    h.pointer(
        PointerEventKind::Move,
        placeholder.center(),
        0,
        Modifiers::NONE,
    );
    assert_eq!(h.requests.take_cursor(), Some(Cursor::Hand));
}

#[test]
fn resizing_an_img_tag_handles_unquoted_values_foreign_attributes_and_self_closing_tags() {
    let ready_image = ready(ObjectVisual::Image(ImageId(3)), 400.0, 100.0);
    for (tag, expected) in [
        (
            "<img src=\"p.png\" width=40>",
            "<img src=\"p.png\" width=\"252\">",
        ),
        (
            "<img src=\"p.png\" data-width=\"9\">",
            "<img src=\"p.png\" data-width=\"9\" width=\"252\">",
        ),
        (
            "<img src=\"p.png\" />",
            "<img src=\"p.png\" width=\"252\" />",
        ),
    ] {
        let mut h = harness(&format!("a\n\n{tag}\n"), image_provider(ready_image));
        drag_handle(&mut h, -100.0);
        assert_eq!(h.text(), format!("a\n\n{expected}\n"), "{tag}");
    }
}
