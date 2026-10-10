//! `D2dSceneRenderer` on WIC bitmap render targets: every kind of scene item is drawn and its
//! pixels are read back. Glyph runs are compared with `typst-render` rasterizing the same
//! Typst page, Mermaid diagrams are checked for bounds and colors, caches and device switches
//! are observed through the renderer's counters and the pixels of the second target.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use ssw_render::frame_to_scene::frame_to_scene;
use ssw_render::math::render_math;
use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, render_mermaid_within};
use ssw_render::scene::{
    Color as SceneColor, Dash, Fill, FillRule, FontData, FontRef, Glyph, Glyphs, GradientStop,
    Group, Image, ImageData, LineCap, LineJoin, LinearGradient, Paint, Path, Point, RadialGradient,
    Rect as SceneRect, Scene, SceneItem, Size as SceneSize, SpreadMode, Stroke, Transform,
};
use ssw_render::world::TypstWorld;
use ssw_ui::canvas::{Canvas, SceneId};
use ssw_ui::geometry::{Color, Rect};
use ssw_win::canvas::{CanvasAssets, D2dCanvas, ImageStore, SceneRenderer, TargetResources};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{WicTarget, d2d_factory};
use ssw_win::scene_render::{D2dSceneRenderer, baseline_in};
use ssw_win::wic::{DecodedImage, Wic};
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::ID2D1Factory1;
use windows_numerics::Matrix3x2;

const RED: [u8; 3] = [255, 0, 0];
const BLUE: [u8; 3] = [0, 0, 255];
const WHITE: [u8; 3] = [255, 255, 255];

struct Rig {
    wic: Wic,
    factory: ID2D1Factory1,
    renderer: D2dSceneRenderer,
    /// Declared last so it drops last: COM objects must be released before COM is torn down.
    _com: ComGuard,
}

fn rig() -> Rig {
    let com = ComGuard::apartment_threaded().unwrap();
    let wic = Wic::new().unwrap();
    let factory = d2d_factory(false).unwrap();
    let renderer = D2dSceneRenderer::new(wic.clone()).unwrap();
    Rig {
        wic,
        factory,
        renderer,
        _com: com,
    }
}

impl Rig {
    fn target(&self, width: u32, height: u32) -> WicTarget {
        WicTarget::new(&self.factory, &self.wic, width, height, 96.0).unwrap()
    }

    /// Draws `scene` scaled to fill a white `width` x `height` target.
    fn render(&self, scene: &Scene, width: u32, height: u32) -> DecodedImage {
        let dest = Rect::new(0.0, 0.0, width as f32, height as f32);
        self.render_in(scene, width, height, dest)
    }

    fn render_in(&self, scene: &Scene, width: u32, height: u32, dest: Rect) -> DecodedImage {
        let target = self.target(width, height);
        target
            .paint(Some(Color::WHITE), |render_target| {
                self.renderer.draw(render_target, scene, dest)
            })
            .unwrap();
        target.pixels().unwrap()
    }

    /// Draws inside a frame that is ended by hand: the drawing result and the `EndDraw` result.
    fn try_render(
        &self,
        scene: &Scene,
        width: u32,
        height: u32,
    ) -> (Result<(), WinError>, windows::core::Result<()>) {
        let target = self.target(width, height);
        let render_target = target.render_target();
        // SAFETY: valid target; the frame is ended below.
        unsafe { render_target.BeginDraw() };
        let dest = Rect::new(0.0, 0.0, width as f32, height as f32);
        let drawn = self.renderer.draw(render_target, scene, dest);
        // SAFETY: ends the frame begun above.
        let ended = unsafe { render_target.EndDraw(None, None) };
        (drawn, ended)
    }
}

/// `[r, g, b]` of a pixel that must exist.
fn rgb(image: &DecodedImage, x: u32, y: u32) -> [u8; 3] {
    let [b, g, r, _] = image
        .pixel(x, y)
        .unwrap_or_else(|| panic!("pixel ({x}, {y}) is outside the image"));
    [r, g, b]
}

#[track_caller]
fn assert_rgb(image: &DecodedImage, x: u32, y: u32, expected: [u8; 3], tolerance: u8) {
    let actual = rgb(image, x, y);
    assert!(
        actual
            .iter()
            .zip(expected)
            .all(|(a, e)| a.abs_diff(e) <= tolerance),
        "pixel ({x}, {y}) is {actual:?}, expected {expected:?} +- {tolerance}"
    );
}

/// True when every channel of `a` is within `tolerance` of `b`.
fn close(a: [u8; 3], b: [u8; 3], tolerance: u8) -> bool {
    a.iter().zip(b).all(|(a, b)| a.abs_diff(b) <= tolerance)
}

/// Number of pixels for which `predicate` holds.
fn count(image: &DecodedImage, predicate: impl Fn([u8; 3]) -> bool) -> usize {
    (0..image.height)
        .flat_map(|y| (0..image.width).map(move |x| (x, y)))
        .filter(|&(x, y)| predicate(rgb(image, x, y)))
        .count()
}

/// A pixel that is clearly not background white.
fn is_ink(pixel: [u8; 3]) -> bool {
    pixel.iter().any(|channel| *channel < 200)
}

/// `(left, top, right, bottom)` (inclusive) of the ink.
fn ink_bounds(image: &DecodedImage) -> Option<(u32, u32, u32, u32)> {
    let mut bounds: Option<(u32, u32, u32, u32)> = None;
    for y in 0..image.height {
        for x in 0..image.width {
            if is_ink(rgb(image, x, y)) {
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(x), b.max(y)),
                });
            }
        }
    }
    bounds
}

fn scene(width: f32, height: f32) -> Scene {
    Scene::new(SceneSize::new(width, height))
}

fn solid(color: [u8; 3]) -> Paint {
    Paint::Solid(SceneColor::rgb(color[0], color[1], color[2]))
}

fn rect_path(x: f32, y: f32, width: f32, height: f32) -> Path {
    SceneRect::new(x, y, width, height).to_path()
}

fn fill(path: Path, rule: FillRule, paint: Paint) -> SceneItem {
    SceneItem::Fill(Fill { path, rule, paint })
}

fn fill_rect(x: f32, y: f32, width: f32, height: f32, paint: Paint) -> SceneItem {
    fill(rect_path(x, y, width, height), FillRule::NonZero, paint)
}

fn stroke(path: Path, width: f32) -> Stroke {
    Stroke {
        path,
        paint: solid([0, 0, 0]),
        width,
        cap: LineCap::Butt,
        join: LineJoin::Miter,
        dash: None,
        miter_limit: 4.0,
    }
}

fn line(x1: f32, y1: f32, x2: f32, y2: f32) -> Path {
    let mut path = Path::new();
    path.move_to(x1, y1);
    path.line_to(x2, y2);
    path
}

fn group(transform: Transform, items: Vec<SceneItem>) -> SceneItem {
    SceneItem::Group(Group {
        transform,
        ..Group::new(items)
    })
}

fn stops(colors: &[(f32, [u8; 3])]) -> Vec<GradientStop> {
    colors
        .iter()
        .map(|(offset, color)| GradientStop {
            offset: *offset,
            color: SceneColor::rgb(color[0], color[1], color[2]),
        })
        .collect()
}

fn linear(start: (f32, f32), end: (f32, f32), spread: SpreadMode) -> LinearGradient {
    LinearGradient {
        start: Point::new(start.0, start.1),
        end: Point::new(end.0, end.1),
        stops: stops(&[(0.0, RED), (1.0, BLUE)]),
        spread,
        transform: Transform::IDENTITY,
    }
}

fn radial(radius: f32, focal_radius: f32) -> RadialGradient {
    RadialGradient {
        center: Point::new(20.0, 20.0),
        radius,
        focal: Point::new(20.0, 20.0),
        focal_radius,
        stops: stops(&[(0.0, RED), (1.0, BLUE)]),
        spread: SpreadMode::Pad,
        transform: Transform::IDENTITY,
    }
}

/// A circle of radius `r` made of four cubic curves.
fn circle(cx: f32, cy: f32, r: f32) -> Path {
    let k = 0.552_284_8 * r;
    let mut path = Path::new();
    path.move_to(cx + r, cy);
    path.cubic_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
    path.cubic_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy);
    path.cubic_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r);
    path.cubic_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
    path.close();
    path
}

// ---------------------------------------------------------------------------------------------
// Paths and fills
// ---------------------------------------------------------------------------------------------

#[test]
fn fill_rules_decide_which_overlaps_are_holes() {
    let rig = rig();
    // Two squares with the same direction: the inner one is a hole for even-odd only.
    let mut nested = Path::new();
    for (origin, size) in [(0.0, 20.0), (5.0, 10.0)] {
        nested.move_to(origin, origin);
        nested.line_to(origin + size, origin);
        nested.line_to(origin + size, origin + size);
        nested.line_to(origin, origin + size);
        nested.close();
    }
    let mut s = scene(40.0, 20.0);
    s.push(fill(nested.clone(), FillRule::EvenOdd, solid(RED)));
    s.push(group(
        Transform::translate(20.0, 0.0),
        vec![fill(nested, FillRule::NonZero, solid(RED))],
    ));
    let image = rig.render(&s, 40, 20);
    assert_rgb(&image, 2, 2, RED, 2);
    assert_rgb(&image, 10, 10, WHITE, 2);
    assert_rgb(&image, 22, 2, RED, 2);
    assert_rgb(&image, 30, 10, RED, 2);
}

#[test]
fn curves_are_filled_and_figures_restart_after_a_close() {
    let rig = rig();
    let mut s = scene(60.0, 60.0);
    s.push(fill(
        circle(20.0, 20.0, 15.0),
        FillRule::NonZero,
        solid(RED),
    ));
    // A parabola-like cap: peak at y = 5, base on y = 20.
    let mut cap = Path::new();
    cap.move_to(0.0, 20.0);
    cap.quad_to(20.0, -10.0, 40.0, 20.0);
    cap.close();
    s.push(group(
        Transform::translate(0.0, 40.0),
        vec![fill(cap, FillRule::NonZero, solid(BLUE))],
    ));
    // After the close the next figure starts at the start of the closed one: the filled area
    // is the triangle (0,0) (0,20) (10,20), not the one that was closed.
    let mut restart = Path::new();
    restart.move_to(0.0, 0.0);
    restart.line_to(10.0, 0.0);
    restart.line_to(10.0, 10.0);
    restart.close();
    restart.line_to(0.0, 20.0);
    restart.line_to(10.0, 20.0);
    s.push(group(
        Transform::translate(45.0, 0.0),
        vec![fill(restart, FillRule::NonZero, solid(RED))],
    ));
    let image = rig.render(&s, 60, 60);
    assert_rgb(&image, 20, 20, RED, 2);
    assert_rgb(&image, 28, 28, RED, 2);
    assert_rgb(&image, 34, 34, WHITE, 2);
    assert_rgb(&image, 3, 3, WHITE, 2);
    assert_rgb(&image, 20, 52, BLUE, 2);
    assert_rgb(&image, 20, 42, WHITE, 2);
    // Near the foot of the cap the arc is still low.
    assert_rgb(&image, 2, 52, WHITE, 2);
    // The closed triangle (below the diagonal) and the area right of the second figure's
    // diagonal stay empty.
    assert_rgb(&image, 45 + 3, 15, RED, 2);
    assert_rgb(&image, 45 + 8, 15, WHITE, 2);
    assert_rgb(&image, 45 + 8, 3, RED, 2);
}

#[test]
fn a_path_without_a_move_starts_at_the_origin() {
    let rig = rig();
    let mut path = Path::new();
    path.line_to(20.0, 0.0);
    path.line_to(20.0, 20.0);
    let mut s = scene(30.0, 30.0);
    s.push(fill(path, FillRule::NonZero, solid(RED)));
    // Empty paths and empty groups draw nothing.
    s.push(fill(Path::new(), FillRule::NonZero, solid(BLUE)));
    s.push(group(Transform::IDENTITY, Vec::new()));
    let image = rig.render(&s, 30, 30);
    assert_rgb(&image, 15, 5, RED, 2);
    assert_rgb(&image, 5, 15, WHITE, 2);
    assert_eq!(count(&image, |p| p == BLUE), 0);
}

#[test]
fn colors_keep_their_alpha() {
    let rig = rig();
    let mut s = scene(20.0, 10.0);
    s.push(fill_rect(
        0.0,
        0.0,
        10.0,
        10.0,
        Paint::Solid(SceneColor::rgba(255, 0, 0, 128)),
    ));
    let image = rig.render(&s, 20, 10);
    assert_rgb(&image, 5, 5, [255, 127, 127], 2);
    assert_rgb(&image, 15, 5, WHITE, 0);
}

// ---------------------------------------------------------------------------------------------
// Strokes
// ---------------------------------------------------------------------------------------------

#[test]
fn stroke_width_and_caps() {
    let rig = rig();
    let mut s = scene(40.0, 60.0);
    for (index, cap) in [LineCap::Butt, LineCap::Square, LineCap::Round]
        .into_iter()
        .enumerate()
    {
        let y = 10.0 + 20.0 * index as f32;
        s.push(SceneItem::Stroke(Stroke {
            cap,
            ..stroke(line(10.0, y, 30.0, y), 6.0)
        }));
    }
    let image = rig.render(&s, 40, 60);
    for row in [10, 30, 50] {
        // The middle of the line and its width (6 px: rows row-3 .. row+2).
        assert_rgb(&image, 20, row, [0, 0, 0], 2);
        assert_rgb(&image, 20, row - 3, [0, 0, 0], 2);
        assert_rgb(&image, 20, row + 2, [0, 0, 0], 2);
        assert_rgb(&image, 20, row - 5, WHITE, 2);
        assert_rgb(&image, 20, row + 4, WHITE, 2);
    }
    // Butt: nothing before the start. Square and round: the cap reaches 3 px beyond it, but
    // only the square cap fills the corner.
    assert_rgb(&image, 8, 10, WHITE, 2);
    assert_rgb(&image, 8, 30, [0, 0, 0], 2);
    assert_rgb(&image, 8, 50, [0, 0, 0], 2);
    assert_rgb(&image, 7, 27, [0, 0, 0], 40);
    assert!(
        rgb(&image, 7, 47)[0] > 100,
        "a round cap leaves the corner open"
    );
}

#[test]
fn stroke_dashes_use_absolute_lengths() {
    let rig = rig();
    let mut s = scene(60.0, 20.0);
    // Dash 10, gap 10 with a 4 wide line: dash lengths are not multiples of the width.
    s.push(SceneItem::Stroke(Stroke {
        dash: Some(Dash {
            array: vec![10.0, 10.0],
            phase: 0.0,
        }),
        ..stroke(line(0.0, 5.0, 60.0, 5.0), 4.0)
    }));
    // The same pattern shifted by one dash: the phase is absolute too.
    s.push(SceneItem::Stroke(Stroke {
        dash: Some(Dash {
            array: vec![10.0, 10.0],
            phase: 10.0,
        }),
        ..stroke(line(0.0, 15.0, 60.0, 15.0), 4.0)
    }));
    let image = rig.render(&s, 60, 20);
    for (x, painted) in [(5, true), (15, false), (25, true), (35, false), (45, true)] {
        let expected = if painted { [0, 0, 0] } else { WHITE };
        assert_rgb(&image, x, 5, expected, 2);
        let expected = if painted { WHITE } else { [0, 0, 0] };
        assert_rgb(&image, x, 15, expected, 2);
    }
}

#[test]
fn odd_dash_arrays_repeat_and_unusable_ones_draw_solid_lines() {
    let rig = rig();
    let mut s = scene(60.0, 40.0);
    // [10] repeats as dash 10, gap 10.
    s.push(SceneItem::Stroke(Stroke {
        dash: Some(Dash {
            array: vec![10.0],
            phase: 0.0,
        }),
        ..stroke(line(0.0, 5.0, 60.0, 5.0), 4.0)
    }));
    for (row, array) in [
        (15.0, vec![0.0, 0.0]),
        (25.0, vec![5.0, -1.0]),
        (35.0, Vec::new()),
    ] {
        s.push(SceneItem::Stroke(Stroke {
            dash: Some(Dash { array, phase: 0.0 }),
            ..stroke(line(0.0, row, 60.0, row), 4.0)
        }));
    }
    let image = rig.render(&s, 60, 40);
    assert_rgb(&image, 5, 5, [0, 0, 0], 2);
    assert_rgb(&image, 15, 5, WHITE, 2);
    for row in [15, 25, 35] {
        for x in [5, 15, 25, 35, 45] {
            assert_rgb(&image, x, row, [0, 0, 0], 2);
        }
    }
}

#[test]
fn joins_and_the_miter_limit() {
    let rig = rig();
    // A sharp "V" whose apex (20.5, 12) is the center of pixel column 20: the miter tip would
    // reach 5.2 px above the apex (y = 6.8), a bevel stops at y = 11.2 and a round join at y = 10.
    let mut v = Path::new();
    v.move_to(10.5, 36.0);
    v.line_to(20.5, 12.0);
    v.line_to(30.5, 36.0);
    let draw = |join: LineJoin, miter_limit: f32| {
        let mut s = scene(40.0, 40.0);
        s.push(SceneItem::Stroke(Stroke {
            join,
            miter_limit,
            ..stroke(v.clone(), 4.0)
        }));
        rig.render(&s, 40, 40)
    };
    let tip = |image: &DecodedImage| rgb(image, 20, 9);
    // Miter within the limit: the tip reaches past the apex.
    assert!(tip(&draw(LineJoin::Miter, 10.0))[0] < 100);
    // Miter beyond the limit becomes a bevel, as do round and bevel joins.
    assert!(tip(&draw(LineJoin::Miter, 1.0))[0] > 200);
    assert!(tip(&draw(LineJoin::Round, 10.0))[0] > 200);
    assert!(tip(&draw(LineJoin::Bevel, 10.0))[0] > 200);
    // A miter limit below one and one that is not a number draw instead of failing.
    assert!(tip(&draw(LineJoin::Miter, 0.0))[0] > 200);
    assert!(tip(&draw(LineJoin::Miter, f32::NAN))[0] < 100);
}

#[test]
fn strokes_without_width_or_geometry_draw_nothing() {
    let rig = rig();
    let mut s = scene(20.0, 20.0);
    for width in [0.0, -2.0, f32::NAN] {
        s.push(SceneItem::Stroke(stroke(
            line(0.0, 10.0, 20.0, 10.0),
            width,
        )));
    }
    s.push(SceneItem::Stroke(stroke(Path::new(), 4.0)));
    assert_eq!(count(&rig.render(&s, 20, 20), is_ink), 0);
}

#[test]
fn strokes_cover_every_subpath_and_skip_empty_paints() {
    let rig = rig();
    // Two open subpaths in one path, and a triangle that is closed twice.
    let mut path = Path::new();
    path.move_to(5.0, 5.0);
    path.line_to(15.0, 5.0);
    path.move_to(5.0, 15.0);
    path.line_to(15.0, 15.0);
    path.move_to(25.0, 5.0);
    path.line_to(35.0, 5.0);
    path.line_to(35.0, 15.0);
    path.close();
    path.close();
    let mut s = scene(40.0, 20.0);
    s.push(SceneItem::Stroke(stroke(path, 2.0)));
    // A gradient without stops paints nothing.
    let mut none = linear((0.0, 0.0), (40.0, 0.0), SpreadMode::Pad);
    none.stops = Vec::new();
    s.push(SceneItem::Stroke(Stroke {
        paint: Paint::Linear(none),
        ..stroke(line(0.0, 10.0, 40.0, 10.0), 4.0)
    }));
    let image = rig.render(&s, 40, 20);
    assert_rgb(&image, 10, 5, [0, 0, 0], 2);
    assert_rgb(&image, 10, 15, [0, 0, 0], 2);
    assert_rgb(&image, 30, 5, [0, 0, 0], 2);
    // The closing segment of the triangle runs from (35, 15) back to (25, 5).
    assert_rgb(&image, 30, 10, [0, 0, 0], 60);
    assert_rgb(&image, 20, 10, WHITE, 2);
    assert_rgb(&image, 10, 10, WHITE, 2);
}

#[test]
fn a_flat_target_transform_draws_nothing() {
    let rig = rig();
    let mut s = scene(10.0, 10.0);
    s.push(fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED)));
    let target = rig.target(10, 10);
    let render_target = target.render_target();
    let flat = Matrix3x2 {
        M11: 0.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 0.0,
        M32: 0.0,
    };
    // SAFETY: valid target and matrix pointer; the frame is ended below.
    unsafe {
        render_target.BeginDraw();
        render_target.SetTransform(&flat);
    }
    rig.renderer
        .draw(render_target, &s, Rect::new(0.0, 0.0, 10.0, 10.0))
        .unwrap();
    // SAFETY: ends the frame begun above.
    unsafe { render_target.EndDraw(None, None).unwrap() };
    assert_eq!(
        count(&target.pixels().unwrap(), |p| p[0] > 0 || p[1] > 0),
        0
    );
}

#[test]
fn strokes_scale_with_the_transform_and_take_gradients() {
    let rig = rig();
    let mut s = scene(40.0, 20.0);
    s.push(group(
        Transform::scale(2.0, 2.0),
        vec![SceneItem::Stroke(Stroke {
            paint: Paint::Linear(linear((0.0, 0.0), (20.0, 0.0), SpreadMode::Pad)),
            ..stroke(line(0.0, 5.0, 20.0, 5.0), 2.0)
        })],
    ));
    let image = rig.render(&s, 40, 20);
    // 2 wide in local units: 4 px at scale 2, centered on y = 10.
    assert_rgb(&image, 20, 8, [128, 0, 127], 25);
    assert_rgb(&image, 20, 11, [128, 0, 127], 25);
    assert_rgb(&image, 20, 6, WHITE, 2);
    assert_rgb(&image, 20, 13, WHITE, 2);
    assert!(rgb(&image, 2, 10)[0] > 200);
    assert!(rgb(&image, 37, 10)[2] > 200);
}

// ---------------------------------------------------------------------------------------------
// Gradients
// ---------------------------------------------------------------------------------------------

fn gradient_row(paint: Paint) -> DecodedImage {
    let rig = rig();
    let mut s = scene(40.0, 10.0);
    s.push(fill_rect(0.0, 0.0, 40.0, 10.0, paint));
    rig.render(&s, 40, 10)
}

#[test]
fn linear_gradients_pad_outside_their_extent() {
    let image = gradient_row(Paint::Linear(linear(
        (10.0, 0.0),
        (30.0, 0.0),
        SpreadMode::Pad,
    )));
    assert_rgb(&image, 2, 5, RED, 3);
    assert_rgb(&image, 20, 5, [128, 0, 128], 12);
    assert_rgb(&image, 38, 5, BLUE, 3);
    let reds: Vec<u8> = (10..30).map(|x| rgb(&image, x, 5)[0]).collect();
    assert!(reds.windows(2).all(|pair| pair[0] >= pair[1]), "{reds:?}");
}

#[test]
fn linear_gradients_reflect_and_repeat() {
    let reflected = gradient_row(Paint::Linear(linear(
        (0.0, 0.0),
        (10.0, 0.0),
        SpreadMode::Reflect,
    )));
    let repeated = gradient_row(Paint::Linear(linear(
        (0.0, 0.0),
        (10.0, 0.0),
        SpreadMode::Repeat,
    )));
    // Same position in the pattern: 1.5 px into the first period.
    assert_rgb(&reflected, 18, 5, rgb(&reflected, 1, 5), 8);
    assert_rgb(&repeated, 11, 5, rgb(&repeated, 1, 5), 8);
    // Mirrored versus restarted: 8.5 px into the pattern.
    assert!(rgb(&repeated, 8, 5)[2] > 200);
    assert!(rgb(&repeated, 12, 5)[0] > 150);
    assert!(rgb(&reflected, 12, 5)[2] > 150);
}

#[test]
fn gradient_transforms_map_gradient_space_into_the_item() {
    let mut gradient = linear((0.0, 0.0), (1.0, 0.0), SpreadMode::Pad);
    gradient.transform = Transform::scale(40.0, 1.0);
    let image = gradient_row(Paint::Linear(gradient));
    assert_rgb(&image, 20, 5, [128, 0, 128], 12);
    assert!(rgb(&image, 2, 5)[0] > 225);
    assert!(rgb(&image, 37, 5)[2] > 225);
}

#[test]
fn stops_are_sorted_and_hard_edges_survive() {
    let mut gradient = linear((0.0, 0.0), (40.0, 0.0), SpreadMode::Pad);
    // Unsorted, out of range and with a hard edge at the middle (red, then blue at 0.5).
    gradient.stops = stops(&[
        (1.0, BLUE),
        (0.5, RED),
        (-1.0, RED),
        (0.5, BLUE),
        (f32::NAN, RED),
    ]);
    let image = gradient_row(Paint::Linear(gradient));
    assert_rgb(&image, 5, 5, RED, 3);
    assert_rgb(&image, 19, 5, RED, 6);
    assert_rgb(&image, 20, 5, BLUE, 6);
    assert_rgb(&image, 35, 5, BLUE, 3);
}

#[test]
fn degenerate_gradients_follow_svg() {
    // One stop, no extent and no radius paint the last color; no stops paint nothing.
    let mut one = linear((0.0, 0.0), (40.0, 0.0), SpreadMode::Pad);
    one.stops = stops(&[(0.3, RED)]);
    assert_rgb(&gradient_row(Paint::Linear(one)), 20, 5, RED, 1);

    let flat = linear((20.0, 0.0), (20.0, 0.0), SpreadMode::Pad);
    assert_rgb(&gradient_row(Paint::Linear(flat)), 5, 5, BLUE, 1);

    let mut none = linear((0.0, 0.0), (40.0, 0.0), SpreadMode::Pad);
    none.stops = Vec::new();
    assert_eq!(count(&gradient_row(Paint::Linear(none)), is_ink), 0);

    let empty_circle = radial(0.0, 0.0);
    assert_rgb(&gradient_row(Paint::Radial(empty_circle)), 20, 5, BLUE, 1);
    let mut one = radial(20.0, 0.0);
    one.stops = stops(&[(0.0, RED)]);
    assert_rgb(&gradient_row(Paint::Radial(one)), 5, 5, RED, 1);
    let mut none = radial(20.0, 0.0);
    none.stops = Vec::new();
    assert_eq!(count(&gradient_row(Paint::Radial(none)), is_ink), 0);
    // The start circle covers the end circle.
    assert_rgb(
        &gradient_row(Paint::Radial(radial(10.0, 10.0))),
        20,
        5,
        BLUE,
        1,
    );
    assert_rgb(
        &gradient_row(Paint::Radial(radial(f32::INFINITY, 0.0))),
        20,
        5,
        BLUE,
        1,
    );
}

fn radial_square(gradient: RadialGradient) -> DecodedImage {
    let rig = rig();
    let mut s = scene(40.0, 40.0);
    s.push(fill_rect(0.0, 0.0, 40.0, 40.0, Paint::Radial(gradient)));
    rig.render(&s, 40, 40)
}

#[test]
fn radial_gradients_run_from_the_center_to_the_edge() {
    let image = radial_square(radial(20.0, 0.0));
    assert!(rgb(&image, 20, 20)[0] > 235);
    assert_rgb(&image, 25, 20, [186, 0, 69], 14);
    assert!(rgb(&image, 38, 20)[2] > 215);
    // Padding beyond the circle.
    assert_rgb(&image, 1, 1, BLUE, 1);
}

#[test]
fn a_focal_radius_keeps_the_start_color_inside_the_start_circle() {
    let image = radial_square(radial(20.0, 10.0));
    assert_rgb(&image, 25, 20, RED, 3);
    assert_rgb(&image, 20, 20, RED, 3);
    // Past the start circle the colors run to the edge of the end circle: 15.5 px from the
    // center is 55 % of the way from radius 10 to radius 20.
    assert_rgb(&image, 35, 20, [115, 0, 140], 20);
    assert_rgb(&image, 1, 1, BLUE, 1);
}

#[test]
fn radial_gradients_use_a_focal_point_and_their_transform() {
    let mut gradient = radial(20.0, 0.0);
    gradient.focal = Point::new(10.0, 20.0);
    let shifted = radial_square(gradient);
    // The start color sits at the focal point, not at the center.
    assert!(rgb(&shifted, 10, 20)[0] > 235);
    assert!(rgb(&shifted, 20, 20)[0] < 215);

    // The gradient lives in the unit square and the transform stretches it over the item.
    let unit = RadialGradient {
        center: Point::new(0.5, 0.5),
        radius: 0.5,
        focal: Point::new(0.5, 0.5),
        focal_radius: 0.0,
        stops: stops(&[(0.0, RED), (1.0, BLUE)]),
        spread: SpreadMode::Pad,
        transform: Transform::scale(40.0, 40.0),
    };
    let stretched = radial_square(unit);
    assert!(rgb(&stretched, 20, 20)[0] > 235);
    assert!(rgb(&stretched, 38, 20)[2] > 215);
}

// ---------------------------------------------------------------------------------------------
// Transforms, groups, clips and opacity
// ---------------------------------------------------------------------------------------------

#[test]
fn groups_translate_and_rotate_their_content() {
    let rig = rig();
    let mut s = scene(40.0, 30.0);
    s.push(group(
        Transform::translate(20.0, 10.0),
        vec![fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED))],
    ));
    // Rotating by 90 degrees maps (x, y) to (-y, x): the 10 x 5 rectangle covers x in -5..0
    // and y in 0..10 before the translation by 15.
    s.push(group(
        Transform::rotate(std::f32::consts::FRAC_PI_2).then(&Transform::translate(15.0, 0.0)),
        vec![fill_rect(0.0, 0.0, 10.0, 5.0, solid(BLUE))],
    ));
    let image = rig.render(&s, 40, 30);
    assert_rgb(&image, 25, 15, RED, 2);
    assert_rgb(&image, 5, 5, WHITE, 2);
    assert_rgb(&image, 12, 8, BLUE, 2);
    assert_rgb(&image, 17, 8, WHITE, 2);
    assert_rgb(&image, 12, 12, WHITE, 2);
}

#[test]
fn nested_groups_compose_their_transforms() {
    let rig = rig();
    let mut s = scene(40.0, 40.0);
    s.push(group(
        Transform::translate(10.0, 10.0),
        vec![group(
            Transform::scale(2.0, 2.0),
            vec![fill_rect(0.0, 0.0, 5.0, 5.0, solid(RED))],
        )],
    ));
    let image = rig.render(&s, 40, 40);
    // The inner scale applies first: a 10 x 10 square at (10, 10).
    assert_rgb(&image, 12, 12, RED, 2);
    assert_rgb(&image, 18, 18, RED, 2);
    assert_rgb(&image, 22, 22, WHITE, 2);
    assert_rgb(&image, 8, 8, WHITE, 2);
}

#[test]
fn the_scene_is_scaled_into_the_destination() {
    let rig = rig();
    let mut s = scene(10.0, 10.0);
    s.push(fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED)));
    let image = rig.render_in(&s, 40, 40, Rect::new(10.0, 10.0, 20.0, 20.0));
    assert_rgb(&image, 20, 20, RED, 2);
    assert_rgb(&image, 11, 11, RED, 2);
    assert_rgb(&image, 28, 28, RED, 2);
    assert_rgb(&image, 5, 5, WHITE, 2);
    assert_rgb(&image, 35, 35, WHITE, 2);
    // Axes are scaled independently.
    let wide = rig.render_in(&s, 40, 40, Rect::new(0.0, 0.0, 40.0, 10.0));
    assert_rgb(&wide, 39, 9, RED, 2);
    assert_rgb(&wide, 20, 15, WHITE, 2);
}

#[test]
fn nothing_is_drawn_without_an_area() {
    let rig = rig();
    let mut s = scene(10.0, 10.0);
    s.push(fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED)));
    for dest in [
        Rect::new(0.0, 0.0, 0.0, 20.0),
        Rect::new(0.0, 0.0, 20.0, -1.0),
        Rect::new(f32::NAN, 0.0, 20.0, 20.0),
        Rect::new(0.0, 0.0, f32::INFINITY, 20.0),
    ] {
        assert_eq!(count(&rig.render_in(&s, 20, 20, dest), is_ink), 0);
    }
    let mut empty = scene(0.0, 10.0);
    empty.push(fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED)));
    assert_eq!(count(&rig.render(&empty, 20, 20), is_ink), 0);
}

#[test]
fn the_target_transform_applies_and_is_restored() {
    let rig = rig();
    let mut s = scene(10.0, 10.0);
    s.push(fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED)));
    s.push(group(
        Transform::IDENTITY,
        vec![SceneItem::Group(Group {
            clip: Some(rect_path(0.0, 0.0, 5.0, 5.0)),
            ..Group::new(vec![fill_rect(0.0, 0.0, 10.0, 10.0, solid(BLUE))])
        })],
    ));
    let target = rig.target(40, 20);
    let render_target = target.render_target();
    let shifted = Matrix3x2 {
        M11: 1.0,
        M12: 0.0,
        M21: 0.0,
        M22: 1.0,
        M31: 12.0,
        M32: 4.0,
    };
    let white = D2D1_COLOR_F {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };
    // SAFETY: valid target, color and matrix pointers, inside a frame that is ended below.
    unsafe {
        render_target.BeginDraw();
        render_target.SetTransform(&shifted);
        render_target.Clear(Some(&white));
    }
    rig.renderer
        .draw(render_target, &s, Rect::new(0.0, 0.0, 10.0, 10.0))
        .unwrap();
    let mut after = Matrix3x2::identity();
    // SAFETY: valid target and out pointer; ends the frame begun above.
    unsafe {
        render_target.GetTransform(&mut after);
        render_target.EndDraw(None, None).unwrap();
    }
    assert_eq!(
        (after.M11, after.M22, after.M31, after.M32),
        (1.0, 1.0, 12.0, 4.0)
    );
    let image = target.pixels().unwrap();
    // The rectangle moved by the target transform, and so did the clip of the blue group.
    assert_rgb(&image, 20, 10, RED, 2);
    assert_rgb(&image, 14, 6, BLUE, 2);
    assert_rgb(&image, 5, 5, WHITE, 2);
    assert_rgb(&image, 30, 10, WHITE, 2);
}

fn clipped(transform: Transform, clip: Path) -> Scene {
    let mut s = scene(20.0, 20.0);
    s.push(SceneItem::Group(Group {
        transform,
        clip: Some(clip),
        ..Group::new(vec![fill_rect(-100.0, -100.0, 300.0, 300.0, solid(RED))])
    }));
    s
}

#[test]
fn clips_use_the_group_transform_and_the_destination() {
    let rig = rig();
    // The clip is in the group's own coordinates: a 10 x 10 square moved to x = 10.
    let s = clipped(
        Transform::translate(10.0, 0.0),
        rect_path(0.0, 0.0, 10.0, 10.0),
    );
    let image = rig.render(&s, 20, 20);
    assert_rgb(&image, 15, 5, RED, 2);
    assert_rgb(&image, 5, 5, WHITE, 2);
    assert_rgb(&image, 15, 15, WHITE, 2);
    // Scaled by the destination.
    let image = rig.render(&s, 40, 40);
    assert_rgb(&image, 30, 10, RED, 2);
    assert_rgb(&image, 10, 10, WHITE, 2);
    assert_rgb(&image, 30, 30, WHITE, 2);
    // Rotation: a quarter turn around the origin plus a translation.
    let turned = clipped(
        Transform::rotate(std::f32::consts::FRAC_PI_2).then(&Transform::translate(20.0, 0.0)),
        rect_path(0.0, 0.0, 10.0, 5.0),
    );
    let image = rig.render(&turned, 20, 20);
    assert_rgb(&image, 17, 5, RED, 2);
    assert_rgb(&image, 12, 5, WHITE, 2);
    assert_rgb(&image, 17, 15, WHITE, 2);
}

#[test]
fn clips_can_be_curved_and_nest() {
    let rig = rig();
    let mut s = scene(40.0, 20.0);
    s.push(SceneItem::Group(Group {
        clip: Some(circle(10.0, 10.0, 8.0)),
        ..Group::new(vec![
            fill_rect(0.0, 0.0, 40.0, 20.0, solid(RED)),
            // A second clip inside the first: the intersection stays.
            SceneItem::Group(Group {
                clip: Some(rect_path(10.0, 0.0, 30.0, 20.0)),
                ..Group::new(vec![fill_rect(0.0, 0.0, 40.0, 20.0, solid(BLUE))])
            }),
        ])
    }));
    let image = rig.render(&s, 40, 20);
    assert_rgb(&image, 5, 10, RED, 2);
    assert_rgb(&image, 14, 10, BLUE, 2);
    assert_rgb(&image, 1, 1, WHITE, 2);
    assert_rgb(&image, 30, 10, WHITE, 2);
    assert!(count(&image, |p| p == BLUE) > 20);
}

#[test]
fn empty_flat_and_transparent_groups_draw_nothing() {
    let rig = rig();
    let content = vec![fill_rect(0.0, 0.0, 20.0, 20.0, solid(RED))];
    let mut s = scene(20.0, 20.0);
    s.push(SceneItem::Group(Group {
        clip: Some(Path::new()),
        ..Group::new(content.clone())
    }));
    s.push(SceneItem::Group(Group {
        opacity: 0.0,
        ..Group::new(content.clone())
    }));
    s.push(SceneItem::Group(Group {
        opacity: f32::NAN,
        ..Group::new(content.clone())
    }));
    s.push(SceneItem::Group(Group {
        transform: Transform::scale(0.0, 1.0),
        ..Group::new(content.clone())
    }));
    assert_eq!(count(&rig.render(&s, 20, 20), is_ink), 0);
}

#[test]
fn group_opacity_blends_the_group_as_a_whole() {
    let rig = rig();
    let mut s = scene(40.0, 20.0);
    s.push(SceneItem::Group(Group {
        opacity: 0.5,
        ..Group::new(vec![
            fill_rect(0.0, 0.0, 20.0, 20.0, solid(RED)),
            fill_rect(10.0, 0.0, 20.0, 20.0, solid(RED)),
        ])
    }));
    let image = rig.render(&s, 40, 20);
    // The overlap is not darker than the rest: the group is composited once.
    assert_rgb(&image, 5, 10, [255, 128, 128], 3);
    assert_rgb(&image, 15, 10, [255, 128, 128], 3);
    assert_rgb(&image, 25, 10, [255, 128, 128], 3);
    assert_rgb(&image, 35, 10, WHITE, 0);
    // Opacity above one is full opacity.
    let mut s = scene(10.0, 10.0);
    s.push(SceneItem::Group(Group {
        opacity: 3.0,
        ..Group::new(vec![fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED))])
    }));
    assert_rgb(&rig.render(&s, 10, 10), 5, 5, RED, 0);
}

#[test]
fn opacity_and_clip_combine() {
    let rig = rig();
    let mut s = scene(30.0, 10.0);
    // Two groups one after the other: the second one reuses the first one's layer.
    for x in [0.0, 20.0] {
        s.push(SceneItem::Group(Group {
            opacity: 0.5,
            clip: Some(rect_path(x, 0.0, 10.0, 10.0)),
            ..Group::new(vec![fill_rect(0.0, 0.0, 30.0, 10.0, solid(BLUE))])
        }));
    }
    let image = rig.render(&s, 30, 10);
    assert_rgb(&image, 5, 5, [128, 128, 255], 3);
    assert_rgb(&image, 15, 5, WHITE, 0);
    assert_rgb(&image, 25, 5, [128, 128, 255], 3);
}

#[test]
fn layers_are_balanced_when_nesting_is_too_deep() {
    let rig = rig();
    let mut item = fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED));
    for _ in 0..300 {
        item = SceneItem::Group(Group {
            opacity: 0.9,
            ..Group::new(vec![item])
        });
    }
    let mut s = scene(10.0, 10.0);
    s.push(item);
    let (drawn, ended) = rig.try_render(&s, 10, 10);
    assert!(matches!(drawn, Err(WinError::Invalid(_))), "{drawn:?}");
    // Every layer that was pushed has been popped: the frame still ends.
    ended.unwrap();
    // Scenes nested just within the limit are drawn.
    let mut item = fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED));
    for _ in 0..200 {
        item = SceneItem::Group(Group::new(vec![item]));
    }
    let mut s = scene(10.0, 10.0);
    s.push(item);
    assert_rgb(&rig.render(&s, 10, 10), 5, 5, RED, 0);
}

// ---------------------------------------------------------------------------------------------
// Glyph runs
// ---------------------------------------------------------------------------------------------

fn find_glyphs(items: &[SceneItem]) -> Option<&Glyphs> {
    items.iter().find_map(|item| match item {
        SceneItem::Glyphs(run) => Some(run),
        SceneItem::Group(group) => find_glyphs(&group.items),
        _ => None,
    })
}

/// The Typst source of a page that shrinks to `markup`, set in the bundled New Computer Modern.
fn typst_source(markup: &str, size_pt: u32, margin_pt: u32) -> String {
    format!(
        "#set page(width: auto, height: auto, margin: {margin_pt}pt)\n\
         #set text(font: \"New Computer Modern\", size: {size_pt}pt)\n{markup}\n"
    )
}

/// A scene with the glyph of an "H" (taken from a Typst page) drawn at the given origins.
fn h_scene(
    origins: &[(f32, f32)],
    paint: Paint,
    width: f32,
    height: f32,
) -> (Scene, Arc<FontData>) {
    let world = TypstWorld::new(None);
    let document = world.compile(&typst_source("H", 40, 4)).unwrap();
    let page = frame_to_scene(&document.pages()[0].frame);
    let run = find_glyphs(&page.items).expect("the page has a glyph run");
    let font = Arc::clone(&page.fonts[run.font.0]);
    let mut s = scene(width, height);
    let font_ref = s.add_font(Arc::clone(&font));
    s.push(SceneItem::Glyphs(Glyphs {
        font: font_ref,
        size: 40.0,
        glyphs: origins
            .iter()
            .map(|&(x, y)| Glyph {
                id: run.glyphs[0].id,
                x,
                y,
            })
            .collect(),
        width: 30.0,
        paint,
    }));
    (s, font)
}

#[test]
fn glyphs_are_placed_on_the_baseline_at_their_origins() {
    let rig = rig();
    let (s, _) = h_scene(&[(10.0, 50.0), (60.0, 50.0)], solid(RED), 120.0, 80.0);
    let image = rig.render(&s, 120, 80);
    let (left, top, _, bottom) = ink_bounds(&image).expect("the glyphs have ink");
    // An H stands on the baseline (y = 50) and is about 0.69 em (27 px) tall.
    assert!((48..=50).contains(&bottom), "bottom row {bottom}");
    assert!((18..=26).contains(&top), "top row {top}");
    assert!((10..=14).contains(&left), "left column {left}");
    // The second glyph is the first one moved by exactly 50 px (up to rounding of the
    // coordinates, which can change a coverage value by one step).
    for y in 0..80 {
        for x in 0..55 {
            let (here, there) = (rgb(&image, x, y), rgb(&image, x + 50, y));
            assert!(close(here, there, 4), "({x}, {y}): {here:?} / {there:?}");
        }
    }
    // Full coverage inside the stems has the paint color.
    assert!(count(&image, |p| p == RED) > 40);
}

#[test]
fn glyph_origins_move_down_when_y_grows() {
    let rig = rig();
    let (high, _) = h_scene(&[(10.0, 40.0)], solid(RED), 60.0, 80.0);
    let (low, _) = h_scene(&[(10.0, 60.0)], solid(RED), 60.0, 80.0);
    let high = rig.render(&high, 60, 80);
    let low = rig.render(&low, 60, 80);
    for y in 0..60 {
        for x in 0..60 {
            let (up, down) = (rgb(&high, x, y), rgb(&low, x, y + 20));
            assert!(close(up, down, 4), "({x}, {y}): {up:?} / {down:?}");
        }
    }
    assert!(count(&high, is_ink) > 50);
}

#[test]
fn glyph_runs_take_gradient_paints() {
    let rig = rig();
    let paint = Paint::Linear(linear((10.0, 0.0), (40.0, 0.0), SpreadMode::Pad));
    let (s, _) = h_scene(&[(10.0, 50.0)], paint, 60.0, 80.0);
    let image = rig.render(&s, 60, 80);
    // The left side of the glyph is red, the right side blue.
    let (left, _, right, _) = ink_bounds(&image).unwrap();
    let ink_in_column = |x: u32| {
        (0..80)
            .map(|y| rgb(&image, x, y))
            .find(|pixel| is_ink(*pixel))
            .expect("the outermost columns have ink")
    };
    let (start, end) = (ink_in_column(left), ink_in_column(right));
    assert!(start[0] > start[2], "{start:?}");
    assert!(end[2] > end[0], "{end:?}");
}

#[test]
fn font_faces_are_created_once_per_font_file() {
    let rig = rig();
    assert_eq!(rig.renderer.cached_font_count(), 0);
    let (s, font) = h_scene(&[(10.0, 50.0)], solid(RED), 60.0, 80.0);
    let first = rig.render(&s, 60, 80);
    assert_eq!(rig.renderer.cached_font_count(), 1);
    // Same scene again, and another scene that shares the font.
    let again = rig.render(&s, 60, 80);
    assert_eq!(first, again);
    let (other_scene, _) = h_scene(&[(20.0, 50.0)], solid(BLUE), 60.0, 80.0);
    rig.render(&other_scene, 60, 80);
    assert_eq!(rig.renderer.cached_font_count(), 1);
    // A copy of the bytes is another font file.
    let copy = Arc::new(FontData::new(font.bytes().to_vec(), font.index));
    let mut copied = scene(60.0, 80.0);
    let copy_ref = copied.add_font(Arc::clone(&copy));
    let SceneItem::Glyphs(run) = s.items[0].clone() else {
        panic!("the scene starts with the glyph run");
    };
    copied.push(SceneItem::Glyphs(Glyphs {
        font: copy_ref,
        ..run
    }));
    assert_eq!(rig.render(&copied, 60, 80), first);
    assert_eq!(rig.renderer.cached_font_count(), 2);
    // Dropping the caches forgets the faces; drawing creates them again.
    rig.renderer.clear_caches();
    assert_eq!(rig.renderer.cached_font_count(), 0);
    assert_eq!(rig.render(&s, 60, 80), first);
    assert_eq!(rig.renderer.cached_font_count(), 1);
}

#[test]
fn the_face_cache_is_bounded() {
    let rig = rig();
    let (s, font) = h_scene(&[(10.0, 50.0)], solid(RED), 60.0, 80.0);
    let SceneItem::Glyphs(run) = s.items[0].clone() else {
        panic!("the scene starts with the glyph run");
    };
    // Every copy of the bytes is a new font file: far more than the cache holds.
    for _ in 0..140 {
        let copy = Arc::new(FontData::new(font.bytes().to_vec(), font.index));
        let mut copied = scene(60.0, 80.0);
        let copy_ref = copied.add_font(copy);
        copied.push(SceneItem::Glyphs(Glyphs {
            font: copy_ref,
            ..run.clone()
        }));
        let (drawn, ended) = rig.try_render(&copied, 8, 8);
        drawn.unwrap();
        ended.unwrap();
    }
    assert!(rig.renderer.cached_font_count() <= 128);
    assert!(rig.renderer.cached_font_count() >= 1);
}

#[test]
fn unusable_fonts_and_references_are_errors_that_leave_the_frame_intact() {
    let rig = rig();
    let (good, font) = h_scene(&[(10.0, 50.0)], solid(RED), 60.0, 80.0);
    let SceneItem::Glyphs(run) = good.items[0].clone() else {
        panic!("the scene starts with the glyph run");
    };
    let broken_variants: Vec<Scene> = {
        let mut missing = scene(60.0, 80.0);
        missing.push(SceneItem::Glyphs(Glyphs {
            font: FontRef(3),
            ..run.clone()
        }));
        let mut garbage = scene(60.0, 80.0);
        let garbage_font = garbage.add_font(Arc::new(FontData::new(vec![0u8; 64], 0)));
        garbage.push(SceneItem::Glyphs(Glyphs {
            font: garbage_font,
            ..run.clone()
        }));
        let mut face = scene(60.0, 80.0);
        let face_font = face.add_font(Arc::new(FontData::new(font.bytes().to_vec(), 40)));
        face.push(SceneItem::Glyphs(Glyphs {
            font: face_font,
            ..run.clone()
        }));
        vec![missing, garbage, face]
    };
    for broken in broken_variants {
        // Inside an opacity layer, to prove that the layer is closed again.
        let mut wrapped = scene(60.0, 80.0);
        wrapped.fonts = broken.fonts.clone();
        wrapped.push(SceneItem::Group(Group {
            opacity: 0.5,
            ..Group::new(broken.items.clone())
        }));
        let (drawn, ended) = rig.try_render(&wrapped, 60, 80);
        assert!(drawn.is_err());
        ended.unwrap();
    }
    // The renderer keeps working.
    assert!(count(&rig.render(&good, 60, 80), is_ink) > 50);
}

#[test]
fn degenerate_glyph_runs_draw_nothing() {
    let rig = rig();
    let (good, _) = h_scene(&[(10.0, 50.0)], solid(RED), 60.0, 80.0);
    let SceneItem::Glyphs(run) = good.items[0].clone() else {
        panic!("the scene starts with the glyph run");
    };
    let mut s = Scene {
        items: Vec::new(),
        ..good.clone()
    };
    s.push(SceneItem::Glyphs(Glyphs {
        glyphs: Vec::new(),
        ..run.clone()
    }));
    for size in [0.0, -10.0, f32::NAN, f32::INFINITY] {
        s.push(SceneItem::Glyphs(Glyphs {
            size,
            ..run.clone()
        }));
    }
    // A gradient without stops paints nothing either.
    let mut no_stops = linear((0.0, 0.0), (10.0, 0.0), SpreadMode::Pad);
    no_stops.stops = Vec::new();
    s.push(SceneItem::Glyphs(Glyphs {
        paint: Paint::Linear(no_stops),
        ..run
    }));
    assert_eq!(count(&rig.render(&s, 60, 80), is_ink), 0);
}

// ---------------------------------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------------------------------

fn image_item(data: &Arc<ImageData>, rect: SceneRect) -> SceneItem {
    SceneItem::Image(Image {
        data: Arc::clone(data),
        rect,
    })
}

/// 2 x 2 pixels: red, green / blue, white.
fn four_colors() -> Arc<ImageData> {
    Arc::new(ImageData::rgba8(
        2,
        2,
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 255,
        ],
    ))
}

fn check_four_colors(image: &DecodedImage, left: u32, top: u32, size: u32) {
    let near = size / 5;
    assert_rgb(image, left + near, top + near, RED, 4);
    assert_rgb(image, left + size - near, top + near, [0, 255, 0], 4);
    assert_rgb(image, left + near, top + size - near, BLUE, 4);
    assert_rgb(image, left + size - near, top + size - near, WHITE, 4);
}

#[test]
fn raw_images_are_drawn_scaled_into_their_rectangle() {
    let rig = rig();
    let mut s = scene(40.0, 40.0);
    s.push(image_item(
        &four_colors(),
        SceneRect::new(5.0, 5.0, 30.0, 30.0),
    ));
    let image = rig.render(&s, 40, 40);
    check_four_colors(&image, 5, 5, 30);
    assert_rgb(&image, 2, 2, WHITE, 0);
    assert_rgb(&image, 38, 38, WHITE, 0);
    // The rectangle follows the group transform.
    let mut moved = scene(40.0, 40.0);
    moved.push(group(
        Transform::translate(10.0, 0.0),
        vec![image_item(
            &four_colors(),
            SceneRect::new(0.0, 10.0, 20.0, 20.0),
        )],
    ));
    check_four_colors(&rig.render(&moved, 40, 40), 10, 10, 20);
}

#[test]
fn raw_images_keep_their_alpha() {
    let rig = rig();
    let half_red = Arc::new(ImageData::rgba8(1, 1, vec![255, 0, 0, 128]));
    let mut s = scene(10.0, 10.0);
    s.push(image_item(&half_red, SceneRect::new(0.0, 0.0, 10.0, 10.0)));
    assert_rgb(&rig.render(&s, 10, 10), 5, 5, [255, 127, 127], 3);
}

#[test]
fn encoded_images_are_decoded() {
    let rig = rig();
    // A flat 8 x 8 PNG: premultiplied BGRA pixels of an opaque orange.
    let pixels = DecodedImage {
        width: 8,
        height: 8,
        bgra: [0u8, 128, 255, 255].repeat(64),
    };
    let png = rig.wic.encode_png(&pixels).unwrap();
    let data = Arc::new(ImageData {
        bytes: png,
        mime: "image/png".to_owned(),
        width: 8,
        height: 8,
    });
    let mut s = scene(20.0, 20.0);
    s.push(image_item(&data, SceneRect::new(2.0, 2.0, 16.0, 16.0)));
    let image = rig.render(&s, 20, 20);
    assert_rgb(&image, 10, 10, [255, 128, 0], 3);
    assert_rgb(&image, 1, 1, WHITE, 0);
}

#[test]
fn image_bitmaps_are_cached_and_survive_a_change_of_device() {
    let rig = rig();
    let data = four_colors();
    let mut s = scene(40.0, 40.0);
    s.push(image_item(&data, SceneRect::new(0.0, 0.0, 40.0, 40.0)));
    assert_eq!(rig.renderer.cached_image_count(), 0);
    let first = rig.render(&s, 40, 40);
    assert_eq!(rig.renderer.cached_image_count(), 1);
    // Same target: the cached bitmap is shared again.
    let target = rig.target(40, 40);
    for _ in 0..2 {
        target
            .paint(Some(Color::WHITE), |render_target| {
                rig.renderer
                    .draw(render_target, &s, Rect::new(0.0, 0.0, 40.0, 40.0))
            })
            .unwrap();
    }
    assert_eq!(target.pixels().unwrap(), first);
    assert_eq!(rig.renderer.cached_image_count(), 1);
    // Another factory is another resource domain, as after a device loss: the bitmap is made
    // again from the cached pixels.
    let other_factory = d2d_factory(false).unwrap();
    let other = WicTarget::new(&other_factory, &rig.wic, 40, 40, 96.0).unwrap();
    other
        .paint(Some(Color::WHITE), |render_target| {
            rig.renderer
                .draw(render_target, &s, Rect::new(0.0, 0.0, 40.0, 40.0))
        })
        .unwrap();
    assert_eq!(other.pixels().unwrap(), first);
    assert_eq!(rig.renderer.cached_image_count(), 1);
    // And back again.
    assert_eq!(rig.render(&s, 40, 40), first);
    // An equal image in another allocation is another cache entry; clearing forgets both.
    let twin = Arc::new((*data).clone());
    let mut t = scene(40.0, 40.0);
    t.push(image_item(&twin, SceneRect::new(0.0, 0.0, 40.0, 40.0)));
    assert_eq!(rig.render(&t, 40, 40), first);
    assert_eq!(rig.renderer.cached_image_count(), 2);
    rig.renderer.clear_caches();
    assert_eq!(rig.renderer.cached_image_count(), 0);
}

#[test]
fn undecodable_images_are_skipped_and_the_rest_is_drawn() {
    let rig = rig();
    let garbage = Arc::new(ImageData {
        bytes: b"definitely not a png".to_vec(),
        mime: "image/png".to_owned(),
        width: 4,
        height: 4,
    });
    let short = Arc::new(ImageData::rgba8(4, 4, vec![0; 10]));
    let mut s = scene(30.0, 10.0);
    s.push(image_item(&garbage, SceneRect::new(0.0, 0.0, 10.0, 10.0)));
    s.push(image_item(&short, SceneRect::new(10.0, 0.0, 10.0, 10.0)));
    s.push(fill_rect(20.0, 0.0, 10.0, 10.0, solid(RED)));
    let image = rig.render(&s, 30, 10);
    assert_eq!(rig.renderer.skipped_image_count(), 2);
    assert_rgb(&image, 5, 5, WHITE, 0);
    assert_rgb(&image, 15, 5, WHITE, 0);
    assert_rgb(&image, 25, 5, RED, 2);
    // The negative result is cached too.
    assert_eq!(rig.renderer.cached_image_count(), 2);
    rig.render(&s, 30, 10);
    assert_eq!(rig.renderer.skipped_image_count(), 4);
    assert_eq!(rig.renderer.cached_image_count(), 2);
}

#[test]
fn images_without_area_are_ignored() {
    let rig = rig();
    let mut s = scene(20.0, 20.0);
    for rect in [
        SceneRect::new(0.0, 0.0, 0.0, 10.0),
        SceneRect::new(0.0, 0.0, 10.0, -4.0),
        SceneRect::new(0.0, 0.0, f32::NAN, 10.0),
    ] {
        s.push(image_item(&four_colors(), rect));
    }
    assert_eq!(count(&rig.render(&s, 20, 20), is_ink), 0);
    assert_eq!(rig.renderer.cached_image_count(), 0);
}

#[test]
fn the_image_cache_is_bounded_by_count_and_size() {
    let rig = rig();
    let mut s = scene(4.0, 4.0);
    let images: Vec<Arc<ImageData>> = (0..70)
        .map(|shade| Arc::new(ImageData::rgba8(1, 1, vec![shade, 0, 0, 255])))
        .collect();
    for data in &images {
        s.items = vec![image_item(data, SceneRect::new(0.0, 0.0, 4.0, 4.0))];
        rig.render(&s, 4, 4);
    }
    assert!(rig.renderer.cached_image_count() <= 64);
    // A huge declared size (the bytes are not there, so it is skipped) empties the cache.
    let huge = Arc::new(ImageData {
        bytes: Vec::new(),
        mime: ssw_render::scene::MIME_RGBA8.to_owned(),
        width: 10_000,
        height: 10_000,
    });
    s.items = vec![image_item(&huge, SceneRect::new(0.0, 0.0, 4.0, 4.0))];
    rig.render(&s, 4, 4);
    assert_eq!(rig.renderer.cached_image_count(), 1);
    // Drawing a small image again decodes it again (and empties the cache once more).
    s.items = vec![image_item(&images[3], SceneRect::new(0.0, 0.0, 4.0, 4.0))];
    assert_rgb(&rig.render(&s, 4, 4), 2, 2, [3, 0, 0], 1);
    assert_eq!(rig.renderer.cached_image_count(), 1);
}

// ---------------------------------------------------------------------------------------------
// Math and Mermaid scenes
// ---------------------------------------------------------------------------------------------

/// Mean absolute difference of the color channels of two same-sized opaque images.
fn mean_difference(a: &DecodedImage, b: &[u8]) -> f64 {
    assert_eq!(a.bgra.len(), b.len());
    let mut sum = 0u64;
    for (pixel, reference) in a
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
    {
        // D2D: BGRA, tiny-skia: RGBA; both opaque here.
        sum += u64::from(pixel[2].abs_diff(reference[0]))
            + u64::from(pixel[1].abs_diff(reference[1]))
            + u64::from(pixel[0].abs_diff(reference[2]));
    }
    sum as f64 / (a.bgra.len() / 4 * 3) as f64
}

/// Intersection over union of the dark pixels of the two images.
fn dark_overlap(a: &DecodedImage, b: &[u8]) -> f64 {
    let (mut both, mut either) = (0usize, 0usize);
    for (pixel, reference) in a
        .bgra
        .as_chunks::<4>()
        .0
        .iter()
        .zip(b.as_chunks::<4>().0.iter())
    {
        let ours = pixel[2] < 128;
        let theirs = reference[0] < 128;
        both += usize::from(ours && theirs);
        either += usize::from(ours || theirs);
    }
    both as f64 / either.max(1) as f64
}

/// Pixels per point of `typst_render::RenderOptions::default()`.
const TYPST_RENDER_SCALE: f32 = 2.0;

/// Environment variable that makes the comparison test write its images (see
/// [`dump_comparison`]).
const DEBUG_ENV: &str = "SSW_SCENE_DEBUG";

/// When [`DEBUG_ENV`] is set, writes `typst-page-<index>-{ours,typst,diff}.png` into cargo's
/// target tmp directory for inspection. The diff image is white where the images agree and
/// darker the more they differ (amplified four times).
fn dump_comparison(rig: &Rig, index: usize, ours: &DecodedImage, reference_rgba: &[u8]) {
    if std::env::var_os(DEBUG_ENV).is_none() {
        return;
    }
    let mut reference = ours.clone();
    reference.bgra.copy_from_slice(reference_rgba);
    for pixel in reference.bgra.as_chunks_mut::<4>().0.iter_mut() {
        // tiny-skia: RGBA, Direct2D: BGRA.
        pixel.swap(0, 2);
    }
    let mut diff = ours.clone();
    for (out, (a, b)) in diff.bgra.as_chunks_mut::<4>().0.iter_mut().zip(
        ours.bgra
            .as_chunks::<4>()
            .0
            .iter()
            .zip(reference.bgra.as_chunks::<4>().0.iter()),
    ) {
        let worst = (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0);
        let shade = 255 - worst.saturating_mul(4);
        out.copy_from_slice(&[shade, shade, shade, 255]);
    }
    let directory = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    for (suffix, image) in [("ours", ours), ("typst", &reference), ("diff", &diff)] {
        let png = rig.wic.encode_png(image).unwrap();
        let path = directory.join(format!("typst-page-{index}-{suffix}.png"));
        std::fs::write(&path, png).unwrap();
        eprintln!("wrote {}", path.display());
    }
}

#[test]
fn typst_pages_match_typst_render() {
    let rig = rig();
    for (index, (markup, size)) in [
        ("$ sum_(k=1)^n k^2 = (n(n+1)(2n+1))/6 $", 36),
        ("$ sqrt(a^2 + b^2) = c quad integral_0^1 x dif x $", 36),
        (
            "Hello, #text(fill: rgb(\"#c00000\"))[wörld] *bold* _italic_ 0123",
            28,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let world = TypstWorld::new(None);
        let document = world.compile(&typst_source(markup, size, 6)).unwrap();
        let page = &document.pages()[0];
        let converted = frame_to_scene(&page.frame);
        assert!(find_glyphs(&converted.items).is_some(), "{markup}");

        // `RenderOptions::default()` rasterizes at 2 pixels per point: draw the scene at the
        // same scale, so that both images show the same page area.
        let reference = typst_render::render(page, &typst_render::RenderOptions::default());
        let (width, height) = (reference.width(), reference.height());
        let dest = Rect::new(
            0.0,
            0.0,
            converted.size.width * TYPST_RENDER_SCALE,
            converted.size.height * TYPST_RENDER_SCALE,
        );
        assert_eq!(
            (width, height),
            (dest.width.round() as u32, dest.height.round() as u32),
            "{markup}: the reference has another size than the scene at scale 2"
        );
        let ours = rig.render_in(&converted, width, height, dest);

        let reference = reference.data();
        dump_comparison(&rig, index, &ours, reference);
        assert!(count(&ours, |p| p[0] < 128) > 500, "{markup}: no ink");
        let difference = mean_difference(&ours, reference);
        let overlap = dark_overlap(&ours, reference);
        assert!(
            difference < 4.0,
            "{markup}: mean difference {difference} against typst-render"
        );
        assert!(overlap > 0.9, "{markup}: ink overlap {overlap}");
    }
}

#[test]
fn rendered_formulas_fill_their_scene_and_sit_on_the_baseline() {
    let rig = rig();
    let display = render_math("\\frac{a+b}{c} + \\sqrt{x}", true, 32.0, SceneColor::BLACK).unwrap();
    let (width, height) = (
        display.size.width.ceil() as u32 + 2,
        display.size.height.ceil() as u32 + 2,
    );
    let dest = Rect::new(0.0, 0.0, display.size.width, display.size.height);
    let image = rig.render_in(&display, width, height, dest);
    let (left, top, right, bottom) = ink_bounds(&image).expect("the formula has ink");
    // The scene is as large as the formula: the ink spans most of it and stays inside.
    assert!(
        left <= width / 6 && right + 3 >= (width * 5) / 6,
        "{left}..{right} of {width}"
    );
    assert!(
        top <= height / 3 && bottom + 3 >= (height * 2) / 3,
        "{top}..{bottom} of {height}"
    );
    assert!(right < width && bottom < height);

    // An inline "x" stands on the baseline of its scene, also when the scene is scaled.
    let inline = render_math("x", false, 40.0, SceneColor::BLACK).unwrap();
    let baseline = inline.baseline.expect("inline formulas have a baseline");
    for factor in [1.0f32, 2.0] {
        let dest = Rect::new(
            0.0,
            0.0,
            inline.size.width * factor,
            inline.size.height * factor,
        );
        let width = dest.width.ceil() as u32 + 1;
        let height = dest.height.ceil() as u32 + 1;
        let image = rig.render_in(&inline, width, height, dest);
        let (_, _, _, bottom) = ink_bounds(&image).expect("the formula has ink");
        let expected = baseline_in(&inline, dest).unwrap();
        assert!(
            (bottom as f32 + 1.0 - expected).abs() <= 1.5 * factor,
            "ink ends at row {bottom}, the baseline is at {expected}"
        );
        assert!((expected - baseline * factor).abs() < 1e-3);
    }
    // Colored formulas use their color.
    let green = render_math("x^2", false, 40.0, SceneColor::rgb(0, 160, 0)).unwrap();
    let dest = Rect::new(0.0, 0.0, green.size.width, green.size.height);
    let image = rig.render_in(
        &green,
        green.size.width.ceil() as u32 + 1,
        green.size.height.ceil() as u32 + 1,
        dest,
    );
    assert!(count(&image, |p| p == [0, 160, 0]) > 20);
    assert_eq!(count(&image, |p| p[0] < 60 && p[2] < 60 && p[1] < 100), 0);
}

fn svg_colors(svg: &str) -> Vec<[u8; 3]> {
    let bytes = svg.as_bytes();
    let mut colors: Vec<[u8; 3]> = Vec::new();
    for (index, _) in svg.match_indices('#') {
        let Some(digits) = bytes.get(index + 1..index + 7) else {
            continue;
        };
        if !digits.iter().all(u8::is_ascii_hexdigit) {
            continue;
        }
        let digits = std::str::from_utf8(digits).unwrap();
        let channel = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).unwrap();
        let color = [channel(0), channel(2), channel(4)];
        if color != WHITE && !colors.contains(&color) {
            colors.push(color);
        }
    }
    colors
}

#[test]
fn mermaid_diagrams_are_drawn_inside_their_scene() {
    let rig = rig();
    let outcome = render_mermaid_within(
        "flowchart LR\n  A[Start] --> B{Choice}\n  B -->|yes| C[Done]\n  B -->|no| D[Retry]",
        MermaidTheme::Light,
        Duration::from_secs(60),
    );
    let (diagram, svg) = match outcome {
        MermaidOutcome::Diagram(diagram, svg) => (diagram, svg),
        MermaidOutcome::Unsupported { reason } => panic!("the diagram is supported: {reason}"),
    };
    let (width, height) = (
        diagram.size.width.ceil() as u32,
        diagram.size.height.ceil() as u32,
    );
    assert!(width > 100 && height > 40, "{width} x {height}");
    let image = rig.render(&diagram, width, height);

    // Ink: the diagram is not empty, spans most of the scene and does not leave it.
    let inked = count(&image, is_ink);
    assert!(inked > (width * height / 50) as usize, "{inked} ink pixels");
    let (left, top, right, bottom) = ink_bounds(&image).unwrap();
    assert!(right - left > width / 2 && bottom - top > height / 3);
    assert!(left <= width / 4 && top <= height / 2);

    // Colors: the most common non-white color of the picture is one the SVG names (the flat
    // fill of the nodes), exactly.
    let wanted = svg_colors(&svg);
    assert!(!wanted.is_empty());
    let mut histogram: HashMap<[u8; 3], usize> = HashMap::new();
    for y in 0..height {
        for x in 0..width {
            let pixel = rgb(&image, x, y);
            if pixel != WHITE {
                *histogram.entry(pixel).or_default() += 1;
            }
        }
    }
    let (dominant, pixels) = histogram
        .into_iter()
        .max_by_key(|&(color, pixels)| (pixels, color))
        .expect("the picture has colored pixels");
    assert!(pixels > 200, "{pixels} pixels of {dominant:?}");
    assert!(
        wanted.contains(&dominant),
        "{dominant:?} is not in {wanted:?}"
    );

    // Scaled into half the size: everything stays in the top-left quarter.
    let half = Rect::new(0.0, 0.0, width as f32 / 2.0, height as f32 / 2.0);
    let small = rig.render_in(&diagram, width, height, half);
    let (_, _, right, bottom) = ink_bounds(&small).unwrap();
    assert!(right <= width / 2 + 2 && bottom <= height / 2 + 2);
    assert!(count(&small, is_ink) > 100);
}

// ---------------------------------------------------------------------------------------------
// The canvas wiring and the baseline
// ---------------------------------------------------------------------------------------------

#[test]
fn the_default_assets_draw_scenes_through_the_canvas() {
    let rig = rig();
    let engine = DWriteEngine::new().unwrap();
    let mut assets = CanvasAssets::with_scene_renderer(
        ImageStore::new(rig.wic.clone()),
        engine,
        rig.wic.clone(),
    )
    .unwrap();
    let mut s = scene(10.0, 10.0);
    s.push(fill_rect(0.0, 0.0, 10.0, 10.0, solid(RED)));
    s.push(fill_rect(0.0, 0.0, 5.0, 5.0, solid(BLUE)));
    assets.register_scene(SceneId(1), Arc::new(s));
    let mut resources = TargetResources::new(rig.factory.clone(), 1);
    let target = rig.target(60, 40);
    target
        .paint(Some(Color::WHITE), |render_target| {
            let mut canvas = D2dCanvas::new(render_target, &assets, &mut resources);
            canvas.draw_scene(SceneId(1), Rect::new(10.0, 10.0, 20.0, 20.0));
            // Below a canvas transform the scene follows it.
            canvas.save();
            canvas.concat_transform(ssw_ui::geometry::Transform2D::translation(30.0, 0.0));
            canvas.draw_scene(SceneId(1), Rect::new(0.0, 10.0, 10.0, 10.0));
            canvas.restore();
            canvas.finish()
        })
        .unwrap();
    let image = target.pixels().unwrap();
    assert_rgb(&image, 25, 25, RED, 2);
    assert_rgb(&image, 15, 15, BLUE, 2);
    assert_rgb(&image, 5, 5, WHITE, 2);
    assert_rgb(&image, 38, 17, RED, 2);
    assert_rgb(&image, 31, 11, BLUE, 2);
    assert_rgb(&image, 50, 30, WHITE, 2);
}

#[test]
fn a_scene_renderer_error_is_reported_by_the_canvas() {
    let rig = rig();
    let engine = DWriteEngine::new().unwrap();
    let mut assets = CanvasAssets::with_scene_renderer(
        ImageStore::new(rig.wic.clone()),
        engine,
        rig.wic.clone(),
    )
    .unwrap();
    let mut s = scene(10.0, 10.0);
    s.push(SceneItem::Glyphs(Glyphs {
        font: FontRef(0),
        size: 10.0,
        glyphs: vec![Glyph {
            id: 1,
            x: 0.0,
            y: 5.0,
        }],
        width: 5.0,
        paint: solid(RED),
    }));
    assets.register_scene(SceneId(2), Arc::new(s));
    let mut resources = TargetResources::new(rig.factory.clone(), 1);
    let target = rig.target(20, 20);
    let result = target.paint(Some(Color::WHITE), |render_target| {
        let mut canvas = D2dCanvas::new(render_target, &assets, &mut resources);
        canvas.draw_scene(SceneId(2), Rect::new(0.0, 0.0, 10.0, 10.0));
        canvas.finish()
    });
    assert!(matches!(result, Err(WinError::Invalid(_))), "{result:?}");
}

#[test]
fn the_baseline_maps_into_the_destination() {
    let rig = rig();
    // A scene whose baseline is marked by a one unit line below y = 8.
    let mut s = scene(20.0, 10.0);
    s.baseline = Some(8.0);
    s.push(fill_rect(0.0, 8.0, 20.0, 0.5, solid([0, 0, 0])));
    let dest = Rect::new(0.0, 100.0, 40.0, 30.0);
    let baseline = baseline_in(&s, dest).unwrap();
    assert!((baseline - 124.0).abs() < 1e-4);
    let image = rig.render_in(&s, 40, 140, dest);
    assert_rgb(&image, 20, 124, [0, 0, 0], 40);
    assert_rgb(&image, 20, 122, WHITE, 2);
    assert_rgb(&image, 20, 127, WHITE, 2);
    assert_eq!(baseline_in(&scene(20.0, 10.0), dest), None);
}

// ---------------------------------------------------------------------------------------------
// Failing system calls (fault injection)
// ---------------------------------------------------------------------------------------------

#[cfg(feature = "fault-injection")]
mod faults {
    use std::collections::BTreeSet;

    use super::*;
    use ssw_win::fault;

    /// The operation an OS error names.
    fn context(error: WinError) -> &'static str {
        match error {
            WinError::Os { context, .. } => context,
            other => panic!("not an OS error: {other:?}"),
        }
    }

    /// Draws `scene` once per system call it makes (on a new renderer, so that no cache hides
    /// a call), with that call failing: the failure ends the drawing with an error, the frame
    /// can still be ended, and what the failures named is returned in call order.
    fn failed_calls(rig: &Rig, scene: &Scene) -> Vec<&'static str> {
        let dest = Rect::new(0.0, 0.0, 120.0, 80.0);
        let mut named = Vec::new();
        let mut n = 0;
        loop {
            let renderer = D2dSceneRenderer::new(rig.wic.clone()).unwrap();
            let target = rig.target(120, 80);
            let mut reached = false;
            let outcome = target.paint(Some(Color::WHITE), |render_target| {
                fault::arm(n);
                let drawn = renderer.draw(render_target, scene, dest);
                reached = fault::tripped();
                fault::disarm();
                drawn
            });
            if !reached {
                outcome.unwrap();
                return named;
            }
            named.push(context(outcome.unwrap_err()));
            n += 1;
        }
    }

    #[test]
    fn every_failing_system_call_ends_the_drawing_with_an_error_that_names_it() {
        let rig = rig();
        let (mut s, _) = h_scene(&[(10.0, 50.0)], solid(RED), 120.0, 80.0);
        // A clip with an opacity: a mask geometry and a layer.
        let mut faded = Group::new(vec![fill_rect(70.0, 5.0, 30.0, 30.0, solid(BLUE))]);
        faded.clip = Some(rect_path(60.0, 0.0, 50.0, 40.0));
        faded.opacity = 0.5;
        s.push(SceneItem::Group(faded));
        let gradient = Paint::Linear(linear((0.0, 0.0), (40.0, 0.0), SpreadMode::Pad));
        s.push(fill_rect(0.0, 0.0, 40.0, 10.0, gradient.clone()));
        s.push(fill_rect(
            0.0,
            10.0,
            40.0,
            10.0,
            Paint::Radial(radial(20.0, 0.0)),
        ));
        let mut dashed = stroke(line(0.0, 30.0, 50.0, 30.0), 2.0);
        dashed.dash = Some(Dash {
            array: vec![4.0, 2.0],
            phase: 0.0,
        });
        s.push(SceneItem::Stroke(dashed));
        s.push(SceneItem::Stroke(Stroke {
            paint: gradient,
            ..stroke(line(0.0, 40.0, 50.0, 40.0), 3.0)
        }));
        s.push(image_item(
            &four_colors(),
            SceneRect::new(80.0, 50.0, 20.0, 20.0),
        ));

        let named: BTreeSet<&str> = failed_calls(&rig, &s).into_iter().collect();
        let expected = BTreeSet::from([
            "scene render target factory",
            "create scene brush",
            "create font file reference",
            "analyze font file",
            "create font face",
            "create glyph geometry",
            "open glyph geometry",
            "get glyph outline",
            "close glyph geometry",
            "create path geometry",
            "open geometry sink",
            "close geometry sink",
            "create clip geometry",
            "create layer",
            "create gradient stops",
            "create linear gradient",
            "create radial gradient",
            "create stroke style",
            "create scene bitmap",
        ]);
        assert_eq!(named, expected);
        // The renderer of the rig was not affected.
        assert!(count(&rig.render(&s, 120, 80), is_ink) > 50);
    }

    #[test]
    fn a_renderer_whose_font_cache_cannot_be_created_is_an_error() {
        let rig = rig();
        let mut named = Vec::new();
        let mut n = 0;
        loop {
            fault::arm(n);
            let outcome = D2dSceneRenderer::new(rig.wic.clone());
            let reached = fault::tripped();
            fault::disarm();
            if !reached {
                outcome.unwrap();
                break;
            }
            named.push(context(outcome.unwrap_err()));
            n += 1;
        }
        assert_eq!(
            named,
            [
                "create DirectWrite factory",
                "create in-memory font loader",
                "register in-memory font loader",
            ]
        );
        // The same error leaves the assets that install the renderer.
        let engine = DWriteEngine::new().unwrap();
        fault::arm(0);
        let assets = CanvasAssets::with_scene_renderer(
            ImageStore::new(rig.wic.clone()),
            engine,
            rig.wic.clone(),
        );
        let reached = fault::tripped();
        fault::disarm();
        assert!(reached);
        assert_eq!(context(assets.unwrap_err()), "create DirectWrite factory");
    }
}
