//! `D2dCanvas` on a WIC bitmap render target: pixels of full frames, scenes drawn through the
//! injected `SceneRenderer`, DPI scaling, PNG export and the caption buttons of the chrome.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use ssw_render::scene::{
    Color as SceneColor, Fill, FillRule, Paint, Rect as SceneRect, Scene, SceneItem,
    Size as SceneSize,
};
use ssw_ui::canvas::{Canvas, IconFont, ImageId, SceneId};
use ssw_ui::geometry::{Color, CornerRadii, Point, Rect};
use ssw_ui::text::{FontWeight, StyledRange, TextLayoutEngine, TextStyle};
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_win::canvas::{CanvasAssets, D2dCanvas, ImageStore, SceneRenderer, TargetResources};
use ssw_win::chrome::{self, CaptionButton, ChromeState};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{WicTarget, d2d_factory};
use ssw_win::wic::{DecodedImage, Wic};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;

const WHITE: Color = Color::WHITE;
const RED: Color = Color::from_rgb_u32(0xFF0000);
const GREEN: Color = Color::from_rgb_u32(0x00A000);

/// `[b, g, r, a]` of a pixel that must exist.
fn px(image: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
    image
        .pixel(x, y)
        .unwrap_or_else(|| panic!("pixel ({x}, {y}) is outside the image"))
}

struct Rig {
    wic: Wic,
    engine: DWriteEngine,
    target: WicTarget,
    assets: CanvasAssets,
    resources: TargetResources,
    /// Declared last so it drops last: COM objects must be released before COM is torn down.
    _com: ComGuard,
}

fn rig(width: u32, height: u32, dpi: f32) -> Rig {
    let com = ComGuard::apartment_threaded().unwrap();
    let wic = Wic::new().unwrap();
    let factory = d2d_factory(false).unwrap();
    let engine = DWriteEngine::new().unwrap();
    let target = WicTarget::new(&factory, &wic, width, height, dpi).unwrap();
    let assets = CanvasAssets::new(ImageStore::new(wic.clone()), engine.clone());
    let resources = TargetResources::new(factory, 1);
    Rig {
        _com: com,
        wic,
        engine,
        target,
        assets,
        resources,
    }
}

impl Rig {
    /// Clears to `background`, draws one frame and reads the pixels back.
    fn frame(
        &mut self,
        background: Option<Color>,
        draw: impl FnOnce(&mut D2dCanvas<'_>),
    ) -> Result<DecodedImage, WinError> {
        let Rig {
            target,
            assets,
            resources,
            ..
        } = self;
        target.paint(background, |render_target| {
            let mut canvas = D2dCanvas::new(render_target, assets, resources);
            draw(&mut canvas);
            canvas.finish()
        })?;
        target.pixels()
    }
}

/// A renderer that fills the solid rectangles of a scene, scaled from the scene's size into
/// the destination, with plain D2D calls on the target it is handed.
struct RectRenderer {
    calls: Rc<RefCell<Vec<Rect>>>,
    fail: bool,
}

impl SceneRenderer for RectRenderer {
    fn draw(&self, target: &ID2D1RenderTarget, scene: &Scene, dest: Rect) -> Result<(), WinError> {
        self.calls.borrow_mut().push(dest);
        if self.fail {
            return Err(WinError::Invalid("renderer failed"));
        }
        let (sx, sy) = (
            dest.width / scene.size.width,
            dest.height / scene.size.height,
        );
        for item in &scene.items {
            let SceneItem::Fill(fill) = item else {
                continue;
            };
            let Paint::Solid(color) = &fill.paint else {
                continue;
            };
            let Some(bounds) = fill.path.bounds() else {
                continue;
            };
            let d2d = D2D1_COLOR_F {
                r: f32::from(color.r) / 255.0,
                g: f32::from(color.g) / 255.0,
                b: f32::from(color.b) / 255.0,
                a: f32::from(color.a) / 255.0,
            };
            // SAFETY: valid render target inside a frame and valid color / rectangle pointers.
            unsafe {
                let brush = target
                    .CreateSolidColorBrush(&d2d, None)
                    .map_err(|error| WinError::from_os("scene brush", error))?;
                let rect = D2D_RECT_F {
                    left: dest.x + bounds.x * sx,
                    top: dest.y + bounds.y * sy,
                    right: dest.x + (bounds.x + bounds.width) * sx,
                    bottom: dest.y + (bounds.y + bounds.height) * sy,
                };
                target.FillRectangle(&rect, &brush);
            }
        }
        Ok(())
    }
}

fn rect_fill(x: f32, y: f32, width: f32, height: f32, color: SceneColor) -> SceneItem {
    SceneItem::Fill(Fill {
        path: SceneRect::new(x, y, width, height).to_path(),
        rule: FillRule::NonZero,
        paint: Paint::Solid(color),
    })
}

/// A 20 x 10 scene: red left half, blue right half.
fn two_tone_scene() -> Scene {
    let mut scene = Scene::new(SceneSize::new(20.0, 10.0));
    scene.push(rect_fill(0.0, 0.0, 10.0, 10.0, SceneColor::rgb(255, 0, 0)));
    scene.push(rect_fill(10.0, 0.0, 10.0, 10.0, SceneColor::rgb(0, 0, 255)));
    scene
}

#[test]
fn scenes_are_drawn_into_the_destination_under_the_canvas_state() {
    let mut rig = rig(80, 60, 96.0);
    let calls = Rc::new(RefCell::new(Vec::new()));
    let scene = Arc::new(two_tone_scene());
    let id = SceneId(11);
    rig.assets.set_scene_renderer(Box::new(RectRenderer {
        calls: Rc::clone(&calls),
        fail: false,
    }));
    rig.assets.register_scene(id, Arc::clone(&scene));
    assert!(Arc::ptr_eq(rig.assets.scene(id).unwrap(), &scene));
    assert!(rig.assets.scene(SceneId(12)).is_none());

    let image = rig
        .frame(Some(WHITE), |canvas| {
            // Scene 20x10 into 40x20 at (10, 10): a uniform 2x scale.
            canvas.draw_scene(id, Rect::new(10.0, 10.0, 40.0, 20.0));
            // The same scene under a translation and a clip that cuts off its right half.
            canvas.save();
            canvas.translate(0.0, 30.0);
            canvas.push_clip_rect(Rect::new(0.0, 0.0, 30.0, 30.0));
            canvas.draw_scene(id, Rect::new(10.0, 0.0, 40.0, 20.0));
            canvas.pop_clip();
            canvas.restore();
        })
        .unwrap();

    assert_eq!(
        calls.borrow().as_slice(),
        [
            Rect::new(10.0, 10.0, 40.0, 20.0),
            Rect::new(10.0, 0.0, 40.0, 20.0)
        ]
    );
    // First draw: red over x 10..30, blue over x 30..50, y 10..30.
    assert_eq!(px(&image, 15, 15), [0, 0, 255, 255]);
    assert_eq!(px(&image, 29, 29), [0, 0, 255, 255]);
    assert_eq!(px(&image, 31, 15), [255, 0, 0, 255]);
    assert_eq!(px(&image, 49, 29), [255, 0, 0, 255]);
    assert_eq!(
        px(&image, 9, 15),
        [255, 255, 255, 255],
        "left of the destination"
    );
    assert_eq!(
        px(&image, 55, 15),
        [255, 255, 255, 255],
        "right of the destination"
    );
    // The second draw starts right below the first one (translated by 30), so y = 31 is
    // already its red half; only below it (y >= 50) is the background left.
    assert_eq!(
        px(&image, 15, 31),
        [0, 0, 255, 255],
        "first row of the translated second draw"
    );
    assert_eq!(px(&image, 15, 55), [255, 255, 255, 255], "below both draws");
    // Second draw: translated down by 30 and clipped at x = 30.
    assert_eq!(px(&image, 15, 45), [0, 0, 255, 255]);
    assert_eq!(px(&image, 29, 45), [0, 0, 255, 255]);
    assert_eq!(
        px(&image, 35, 45),
        [255, 255, 255, 255],
        "the clip removes the blue half"
    );
    // Removing the scene makes later draws fail.
    assert!(rig.assets.remove_scene(id));
    assert!(!rig.assets.remove_scene(id));
    let error = rig
        .frame(Some(WHITE), |canvas| {
            canvas.draw_scene(id, Rect::new(0.0, 0.0, 10.0, 10.0))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "scene is not registered");
}

#[test]
fn registering_a_scene_again_replaces_what_is_drawn() {
    let mut rig = rig(40, 20, 96.0);
    let calls = Rc::new(RefCell::new(Vec::new()));
    rig.assets.set_scene_renderer(Box::new(RectRenderer {
        calls: Rc::clone(&calls),
        fail: false,
    }));
    let id = SceneId(1);
    let mut green = Scene::new(SceneSize::new(20.0, 10.0));
    green.push(rect_fill(0.0, 0.0, 20.0, 10.0, SceneColor::rgb(0, 255, 0)));
    rig.assets.register_scene(id, Arc::new(green));
    let first = rig
        .frame(Some(WHITE), |canvas| {
            canvas.draw_scene(id, Rect::new(0.0, 0.0, 20.0, 10.0))
        })
        .unwrap();
    assert_eq!(px(&first, 5, 5), [0, 255, 0, 255]);
    assert_eq!(px(&first, 15, 5), [0, 255, 0, 255]);
    rig.assets.register_scene(id, Arc::new(two_tone_scene()));
    let second = rig
        .frame(Some(WHITE), |canvas| {
            canvas.draw_scene(id, Rect::new(0.0, 0.0, 20.0, 10.0))
        })
        .unwrap();
    assert_eq!(px(&second, 5, 5), [0, 0, 255, 255]);
    assert_eq!(px(&second, 15, 5), [255, 0, 0, 255]);
    assert_eq!(calls.borrow().len(), 2);
}

#[test]
fn scene_failures_are_reported_by_finish() {
    let mut rig = rig(32, 32, 96.0);
    let id = SceneId(5);
    rig.assets.register_scene(id, Arc::new(two_tone_scene()));
    // No renderer installed yet.
    let error = rig
        .frame(None, |canvas| {
            canvas.draw_scene(id, Rect::new(0.0, 0.0, 20.0, 10.0))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "no scene renderer is installed");
    // A failing renderer: its error comes out of `finish`.
    let calls = Rc::new(RefCell::new(Vec::new()));
    rig.assets.set_scene_renderer(Box::new(RectRenderer {
        calls: Rc::clone(&calls),
        fail: true,
    }));
    let error = rig
        .frame(None, |canvas| {
            canvas.draw_scene(id, Rect::new(0.0, 0.0, 20.0, 10.0));
            canvas.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), RED);
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "renderer failed");
    assert_eq!(calls.borrow().len(), 1);
}

#[test]
fn a_frame_composites_shapes_text_images_and_icons() {
    let mut rig = rig(160, 120, 96.0);
    let png = rig
        .wic
        .encode_png(&DecodedImage {
            width: 2,
            height: 1,
            bgra: vec![0, 0, 255, 255, 255, 0, 0, 255],
        })
        .unwrap();
    rig.assets
        .images
        .register_encoded(ImageId(1), &png)
        .unwrap();
    let base = TextStyle::new("Segoe UI", 40.0).with_weight(FontWeight::BOLD);
    let layout = rig.engine.layout(
        "II",
        &base,
        &[StyledRange {
            range: 0..1,
            style: base.clone().with_color(RED),
        }],
        &[],
        f32::INFINITY,
    );
    let split = layout.caret_rect(1, false).x;
    let text_size = layout.size();
    let origin = Point::new(8.0, 6.0);

    let image = rig
        .frame(Some(WHITE), |canvas| {
            canvas.fill_rounded_rect(
                Rect::new(0.0, 60.0, 50.0, 50.0),
                CornerRadii::uniform(20.0),
                GREEN,
            );
            canvas.draw_image(ImageId(1), Rect::new(100.0, 0.0, 40.0, 20.0), 1.0);
            canvas.draw_text_layout(&*layout, origin, Color::BLACK);
            canvas.draw_icon(
                '\u{E921}',
                IconFont::SegoeFluent,
                20.0,
                Color::BLACK,
                Point::new(120.0, 90.0),
            );
        })
        .unwrap();

    // Rounded rectangle: its corner stays background, its middle is solid.
    assert_eq!(px(&image, 0, 60), [255, 255, 255, 255]);
    assert_eq!(px(&image, 25, 85), [0x00, 0xA0, 0x00, 255]);
    // Image halves.
    assert_eq!(px(&image, 102, 10), [0, 0, 255, 255]);
    assert_eq!(px(&image, 138, 10), [255, 0, 0, 255]);
    // Text: the first letter takes its range color, the second the color given to the draw call.
    let count = |x0: f32, x1: f32, test: &dyn Fn([u8; 4]) -> bool| {
        let mut found = 0;
        for y in origin.y as u32..(origin.y + text_size.height) as u32 {
            for x in (origin.x + x0) as u32..(origin.x + x1) as u32 {
                found += u32::from(test(px(&image, x, y)));
            }
        }
        found
    };
    let is_red = |p: [u8; 4]| p[2] > 200 && p[1] < 80 && p[0] < 80;
    let is_black = |p: [u8; 4]| p[0] < 60 && p[1] < 60 && p[2] < 60;
    assert!(count(0.0, split, &is_red) > 30, "first letter is red");
    assert_eq!(
        count(split, text_size.width, &is_red),
        0,
        "no red in the second letter"
    );
    assert!(
        count(split, text_size.width, &is_black) > 30,
        "second letter is black"
    );
    // Icon glyph: non-background pixels around the center.
    let mut ink = 0;
    for y in 80..100 {
        for x in 110..130 {
            ink += u32::from(px(&image, x, y)[1] < 200);
        }
    }
    assert!(ink > 5, "the minimize glyph is drawn ({ink} inked pixels)");
}

#[test]
fn dips_scale_with_the_target_dpi() {
    let mut rig = rig(128, 128, 192.0);
    let image = rig
        .frame(Some(WHITE), |canvas| {
            canvas.fill_rect(Rect::new(0.0, 0.0, 32.0, 32.0), RED);
            canvas.stroke_rounded_rect(
                Rect::new(40.0, 40.0, 20.0, 20.0),
                CornerRadii::ZERO,
                GREEN,
                4.0,
            );
        })
        .unwrap();
    // 32 DIPs at 200 % are 64 pixels.
    assert_eq!(px(&image, 62, 62), [0, 0, 255, 255]);
    assert_eq!(px(&image, 66, 66), [255, 255, 255, 255]);
    // The 4 DIP stroke is 8 pixels wide, centered on the DIP outline at 40 (80 px).
    assert_eq!(px(&image, 78, 100), [0x00, 0xA0, 0x00, 255]);
    assert_eq!(px(&image, 83, 100), [0x00, 0xA0, 0x00, 255]);
    assert_eq!(px(&image, 90, 100), [255, 255, 255, 255]);
}

#[test]
fn rendered_frames_export_as_png_without_loss() {
    let mut rig = rig(48, 32, 96.0);
    let image = rig
        .frame(Some(Color::from_rgb_u32(0x336699)), |canvas| {
            canvas.fill_rounded_rect(
                Rect::new(4.0, 4.0, 30.0, 20.0),
                CornerRadii::uniform(6.0),
                RED,
            );
            canvas.fill_rect(Rect::new(36.0, 8.0, 8.0, 8.0), GREEN);
        })
        .unwrap();
    assert!(
        image
            .bgra
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| pixel[3] == 255),
        "opaque frame"
    );
    let png = rig.wic.encode_png(&image).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let decoded = rig.wic.decode(&png).unwrap();
    assert_eq!((decoded.width, decoded.height), (48, 32));
    assert_eq!(decoded.bgra, image.bgra);
}

#[test]
fn caption_buttons_paint_their_states() {
    let mut rig = rig(300, 40, 96.0);
    let tokens = Theme::light(DEFAULT_ACCENT).tokens;
    let width = 300.0;
    let paint = |rig: &mut Rig, state: ChromeState| {
        rig.frame(Some(WHITE), |canvas| {
            chrome::paint_caption_buttons(canvas, width, state, &tokens);
        })
        .unwrap()
    };
    let close = chrome::button_rect(CaptionButton::Close, width);
    let maximize = chrome::button_rect(CaptionButton::Maximize, width);
    let minimize = chrome::button_rect(CaptionButton::Minimize, width);
    let (cx, my) = (close.x as u32 + 2, 2);

    // Resting: backgrounds untouched, glyphs drawn.
    let resting = paint(&mut rig, ChromeState::default());
    assert_eq!(px(&resting, cx, my), [255, 255, 255, 255]);
    let ink = |image: &DecodedImage, rect: Rect| {
        let mut darkest = 255u8;
        for y in rect.y as u32..(rect.y + rect.height) as u32 {
            for x in rect.x as u32..(rect.x + rect.width) as u32 {
                darkest = darkest.min(px(image, x, y)[1]);
            }
        }
        darkest
    };
    for button in [minimize, maximize, close] {
        assert!(ink(&resting, button) < 230, "glyph in {button:?} is drawn");
    }
    // Hovered close: red background and a white glyph.
    let hovered = paint(
        &mut rig,
        ChromeState {
            hover: Some(CaptionButton::Close),
            ..ChromeState::default()
        },
    );
    assert_eq!(px(&hovered, cx, my), [0x1C, 0x2B, 0xC4, 255]);
    assert_eq!(
        px(&hovered, maximize.x as u32 + 2, my),
        [255, 255, 255, 255]
    );
    // Pressed close is the dimmer red.
    let pressed = paint(
        &mut rig,
        ChromeState {
            hover: Some(CaptionButton::Close),
            pressed: Some(CaptionButton::Close),
            ..ChromeState::default()
        },
    );
    assert_ne!(px(&pressed, cx, my), px(&hovered, cx, my));
    assert!(px(&pressed, cx, my)[2] > 150, "still reddish");
    // Hovered maximize gets the theme's hover fill.
    let hover_max = paint(
        &mut rig,
        ChromeState {
            hover: Some(CaptionButton::Maximize),
            ..ChromeState::default()
        },
    );
    assert_ne!(
        px(&hover_max, maximize.x as u32 + 2, my),
        [255, 255, 255, 255]
    );
    // Inactive windows dim the glyphs.
    let inactive = paint(
        &mut rig,
        ChromeState {
            active: false,
            ..ChromeState::default()
        },
    );
    assert!(
        ink(&inactive, minimize) > ink(&resting, minimize),
        "dimmed glyph is lighter"
    );
    // The restore glyph replaces the maximize glyph of a maximized window.
    let maximized = paint(
        &mut rig,
        ChromeState {
            maximized: true,
            ..ChromeState::default()
        },
    );
    assert_ne!(
        maximized.bgra, resting.bgra,
        "the maximized state draws a different glyph"
    );
    assert_eq!(
        chrome::glyph(CaptionButton::Maximize, true),
        chrome::GLYPH_RESTORE
    );
}
