//! The drawing contract: [`Canvas`] and a recording implementation for tests.
//!
//! All coordinates are DIPs in the current transform's space. `ssw-win` implements [`Canvas`]
//! with Direct2D; tests use [`RecordingCanvas`] and assert on the recorded [`DrawOp`]s.

use crate::geometry::{Color, CornerRadii, Insets, Point, Rect, Size, Transform2D};
use crate::text::TextLayout;

/// Handle of a decoded image registered with the platform canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageId(pub u64);

/// Handle of a vector `Scene` (math, Mermaid, SVG, print page) registered with the platform
/// canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SceneId(pub u64);

/// The icon fonts glyphs can be drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconFont {
    /// Segoe Fluent Icons.
    SegoeFluent,
    /// Segoe MDL2 Assets.
    SegoeMdl2,
}

/// Line dash style; the platform scales the pattern with the stroke width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Dash {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

/// One segment of a [`Path`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathVerb {
    MoveTo(Point),
    LineTo(Point),
    QuadTo {
        control: Point,
        to: Point,
    },
    CubicTo {
        control1: Point,
        control2: Point,
        to: Point,
    },
    /// An SVG-style elliptical arc from the current point to `to`.
    ArcTo {
        to: Point,
        radii: Size,
        rotation_degrees: f32,
        large_arc: bool,
        /// Draw in the positive-angle (clockwise on screen) direction.
        sweep_clockwise: bool,
    },
    Close,
}

/// A vector path built with chained calls. Segment calls made without an open subpath start one
/// at the current point (the origin for a new path, the subpath start after `close`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Path {
    verbs: Vec<PathVerb>,
    current: Point,
    start: Point,
    open: bool,
}

impl Path {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A closed rectangle path.
    #[must_use]
    pub fn from_rect(rect: Rect) -> Self {
        Self::new()
            .move_to(Point::new(rect.left(), rect.top()))
            .line_to(Point::new(rect.right(), rect.top()))
            .line_to(Point::new(rect.right(), rect.bottom()))
            .line_to(Point::new(rect.left(), rect.bottom()))
            .close()
    }

    #[must_use]
    pub fn move_to(mut self, to: Point) -> Self {
        self.verbs.push(PathVerb::MoveTo(to));
        self.current = to;
        self.start = to;
        self.open = true;
        self
    }

    #[must_use]
    pub fn line_to(mut self, to: Point) -> Self {
        self.ensure_open();
        self.verbs.push(PathVerb::LineTo(to));
        self.current = to;
        self
    }

    #[must_use]
    pub fn quad_to(mut self, control: Point, to: Point) -> Self {
        self.ensure_open();
        self.verbs.push(PathVerb::QuadTo { control, to });
        self.current = to;
        self
    }

    #[must_use]
    pub fn cubic_to(mut self, control1: Point, control2: Point, to: Point) -> Self {
        self.ensure_open();
        self.verbs.push(PathVerb::CubicTo {
            control1,
            control2,
            to,
        });
        self.current = to;
        self
    }

    #[must_use]
    pub fn arc_to(
        mut self,
        to: Point,
        radii: Size,
        rotation_degrees: f32,
        large_arc: bool,
        sweep_clockwise: bool,
    ) -> Self {
        self.ensure_open();
        self.verbs.push(PathVerb::ArcTo {
            to,
            radii,
            rotation_degrees,
            large_arc,
            sweep_clockwise,
        });
        self.current = to;
        self
    }

    /// Closes the open subpath; the current point returns to the subpath start. A no-op when no
    /// subpath is open.
    #[must_use]
    pub fn close(mut self) -> Self {
        if self.open {
            self.verbs.push(PathVerb::Close);
            self.current = self.start;
            self.open = false;
        }
        self
    }

    fn ensure_open(&mut self) {
        if !self.open {
            self.verbs.push(PathVerb::MoveTo(self.current));
            self.start = self.current;
            self.open = true;
        }
    }

    #[must_use]
    pub fn verbs(&self) -> &[PathVerb] {
        &self.verbs
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// A conservative bounding box: the hull of all points and control points. An arc extends
    /// the box by twice the larger of its radii and its chord length around its end points, which
    /// always contains it. `None` for an empty path.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        fn extend(bounds: &mut Option<[f32; 4]>, point: Point) {
            *bounds = Some(match *bounds {
                None => [point.x, point.y, point.x, point.y],
                Some([left, top, right, bottom]) => [
                    left.min(point.x),
                    top.min(point.y),
                    right.max(point.x),
                    bottom.max(point.y),
                ],
            });
        }

        let mut bounds = None;
        let mut current = Point::ZERO;
        let mut start = Point::ZERO;
        for verb in &self.verbs {
            match *verb {
                PathVerb::MoveTo(to) => {
                    extend(&mut bounds, to);
                    current = to;
                    start = to;
                }
                PathVerb::LineTo(to) => {
                    extend(&mut bounds, to);
                    current = to;
                }
                PathVerb::QuadTo { control, to } => {
                    extend(&mut bounds, control);
                    extend(&mut bounds, to);
                    current = to;
                }
                PathVerb::CubicTo {
                    control1,
                    control2,
                    to,
                } => {
                    extend(&mut bounds, control1);
                    extend(&mut bounds, control2);
                    extend(&mut bounds, to);
                    current = to;
                }
                PathVerb::ArcTo { to, radii, .. } => {
                    let reach = 2.0 * radii.width.max(radii.height).max(current.distance(to));
                    extend(
                        &mut bounds,
                        Point::new(current.x.min(to.x) - reach, current.y.min(to.y) - reach),
                    );
                    extend(
                        &mut bounds,
                        Point::new(current.x.max(to.x) + reach, current.y.max(to.y) + reach),
                    );
                    current = to;
                }
                PathVerb::Close => current = start,
            }
        }
        bounds.map(|[left, top, right, bottom]| Rect::from_ltrb(left, top, right, bottom))
    }
}

/// Draws into a window, bitmap or print page.
///
/// `save`/`restore` bracket the transform only; clips and layers are separate stacks and must
/// be popped by the code that pushed them.
pub trait Canvas {
    /// Saves the current transform.
    fn save(&mut self);
    /// Restores the transform saved by the matching [`Canvas::save`].
    fn restore(&mut self);
    /// Applies `transform` inside the current one: subsequent coordinates are first mapped by
    /// `transform`, then by the transform that was active.
    fn concat_transform(&mut self, transform: Transform2D);
    fn push_clip_rect(&mut self, rect: Rect);
    fn push_clip_rounded_rect(&mut self, rect: Rect, radii: CornerRadii);
    fn pop_clip(&mut self);
    /// Starts a group composited with `opacity` (0..=1) when popped.
    fn push_layer(&mut self, opacity: f32);
    fn pop_layer(&mut self);
    fn fill_rect(&mut self, rect: Rect, color: Color);
    fn fill_rounded_rect(&mut self, rect: Rect, radii: CornerRadii, color: Color);
    /// Strokes the outline centered on the rectangle's edge.
    fn stroke_rounded_rect(&mut self, rect: Rect, radii: CornerRadii, color: Color, width: f32);
    fn stroke_line(&mut self, from: Point, to: Point, color: Color, width: f32, dash: Dash);
    fn fill_path(&mut self, path: &Path, color: Color);
    fn stroke_path(&mut self, path: &Path, color: Color, width: f32, dash: Dash);
    /// Draws a laid-out text block with its top-left at `origin`. Runs whose style has no color
    /// use `color`.
    fn draw_text_layout(&mut self, layout: &dyn TextLayout, origin: Point, color: Color);
    fn draw_image(&mut self, image: ImageId, dest: Rect, opacity: f32);
    fn draw_scene(&mut self, scene: SceneId, dest: Rect);
    /// Draws one icon-font glyph of em size `size` centered on `center`.
    fn draw_icon(&mut self, glyph: char, font: IconFont, size: f32, color: Color, center: Point);

    /// Shorthand for `concat_transform(Transform2D::translation(dx, dy))`.
    fn translate(&mut self, dx: f32, dy: f32) {
        self.concat_transform(Transform2D::translation(dx, dy));
    }
}

/// One recorded canvas call.
#[derive(Debug, Clone, PartialEq)]
pub enum DrawOp {
    Save,
    Restore,
    Transform(Transform2D),
    PushClipRect(Rect),
    PushClipRoundedRect {
        rect: Rect,
        radii: CornerRadii,
    },
    PopClip,
    PushLayer {
        opacity: f32,
    },
    PopLayer,
    FillRect {
        rect: Rect,
        color: Color,
    },
    FillRoundedRect {
        rect: Rect,
        radii: CornerRadii,
        color: Color,
    },
    StrokeRoundedRect {
        rect: Rect,
        radii: CornerRadii,
        color: Color,
        width: f32,
    },
    StrokeLine {
        from: Point,
        to: Point,
        color: Color,
        width: f32,
        dash: Dash,
    },
    FillPath {
        path: Path,
        color: Color,
    },
    StrokePath {
        path: Path,
        color: Color,
        width: f32,
        dash: Dash,
    },
    /// A text layout; `text` and `size` are copied from the layout at draw time.
    Text {
        text: String,
        size: Size,
        origin: Point,
        color: Color,
    },
    Image {
        image: ImageId,
        dest: Rect,
        opacity: f32,
    },
    Scene {
        scene: SceneId,
        dest: Rect,
    },
    Icon {
        glyph: char,
        font: IconFont,
        size: f32,
        color: Color,
        center: Point,
    },
}

impl DrawOp {
    /// The area the operation paints in the space it was issued in, or `None` for state
    /// changes (save, restore, transform, clips, layers) and empty paths.
    #[must_use]
    pub fn bounds(&self) -> Option<Rect> {
        match self {
            DrawOp::FillRect { rect, .. } | DrawOp::FillRoundedRect { rect, .. } => Some(*rect),
            DrawOp::StrokeRoundedRect { rect, width, .. } => {
                Some(rect.outset(Insets::uniform(*width / 2.0)))
            }
            DrawOp::StrokeLine {
                from, to, width, ..
            } => Some(
                Rect::from_ltrb(
                    from.x.min(to.x),
                    from.y.min(to.y),
                    from.x.max(to.x),
                    from.y.max(to.y),
                )
                .outset(Insets::uniform(*width / 2.0)),
            ),
            DrawOp::FillPath { path, .. } => path.bounds(),
            DrawOp::StrokePath { path, width, .. } => path
                .bounds()
                .map(|bounds| bounds.outset(Insets::uniform(*width / 2.0))),
            DrawOp::Text { size, origin, .. } => Some(Rect::from_origin_size(*origin, *size)),
            DrawOp::Image { dest, .. } | DrawOp::Scene { dest, .. } => Some(*dest),
            DrawOp::Icon { size, center, .. } => Some(Rect::new(
                center.x - *size / 2.0,
                center.y - *size / 2.0,
                *size,
                *size,
            )),
            DrawOp::Save
            | DrawOp::Restore
            | DrawOp::Transform(_)
            | DrawOp::PushClipRect(_)
            | DrawOp::PushClipRoundedRect { .. }
            | DrawOp::PopClip
            | DrawOp::PushLayer { .. }
            | DrawOp::PopLayer => None,
        }
    }
}

/// A recorded operation with the area it painted in root (untransformed) coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedOp {
    pub op: DrawOp,
    /// `None` for state changes.
    pub bounds: Option<Rect>,
}

/// A [`Canvas`] that records every call, for tests.
#[derive(Debug, Default)]
pub struct RecordingCanvas {
    recorded: Vec<RecordedOp>,
    transform: Transform2D,
    saved: Vec<Transform2D>,
    clip_depth: usize,
    layer_depth: usize,
    underflow: bool,
}

impl RecordingCanvas {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded call in order, with root-space bounds.
    #[must_use]
    pub fn recorded(&self) -> &[RecordedOp] {
        &self.recorded
    }

    /// Every recorded call in order.
    pub fn ops(&self) -> impl Iterator<Item = &DrawOp> {
        self.recorded.iter().map(|entry| &entry.op)
    }

    /// The drawing operations (not state changes) whose root-space bounds overlap `rect`.
    #[must_use]
    pub fn ops_in(&self, rect: Rect) -> Vec<&DrawOp> {
        self.recorded
            .iter()
            .filter(|entry| entry.bounds.is_some_and(|bounds| bounds.intersects(rect)))
            .map(|entry| &entry.op)
            .collect()
    }

    /// The text of every drawn text layout, in draw order.
    #[must_use]
    pub fn texts(&self) -> Vec<&str> {
        self.recorded
            .iter()
            .filter_map(|entry| match &entry.op {
                DrawOp::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The current transform.
    #[must_use]
    pub fn transform(&self) -> Transform2D {
        self.transform
    }

    /// True when every save, clip and layer was matched and nothing was popped too often.
    #[must_use]
    pub fn is_balanced(&self) -> bool {
        !self.underflow && self.saved.is_empty() && self.clip_depth == 0 && self.layer_depth == 0
    }

    /// Forgets the recorded calls and resets the transform and stacks.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn record(&mut self, op: DrawOp) {
        let bounds = op
            .bounds()
            .map(|local| self.transform.transform_rect(local));
        self.recorded.push(RecordedOp { op, bounds });
    }
}

impl Canvas for RecordingCanvas {
    fn save(&mut self) {
        self.saved.push(self.transform);
        self.record(DrawOp::Save);
    }

    fn restore(&mut self) {
        match self.saved.pop() {
            Some(transform) => self.transform = transform,
            None => self.underflow = true,
        }
        self.record(DrawOp::Restore);
    }

    fn concat_transform(&mut self, transform: Transform2D) {
        self.record(DrawOp::Transform(transform));
        self.transform = transform.compose(self.transform);
    }

    fn push_clip_rect(&mut self, rect: Rect) {
        self.clip_depth += 1;
        self.record(DrawOp::PushClipRect(rect));
    }

    fn push_clip_rounded_rect(&mut self, rect: Rect, radii: CornerRadii) {
        self.clip_depth += 1;
        self.record(DrawOp::PushClipRoundedRect { rect, radii });
    }

    fn pop_clip(&mut self) {
        match self.clip_depth.checked_sub(1) {
            Some(depth) => self.clip_depth = depth,
            None => self.underflow = true,
        }
        self.record(DrawOp::PopClip);
    }

    fn push_layer(&mut self, opacity: f32) {
        self.layer_depth += 1;
        self.record(DrawOp::PushLayer { opacity });
    }

    fn pop_layer(&mut self) {
        match self.layer_depth.checked_sub(1) {
            Some(depth) => self.layer_depth = depth,
            None => self.underflow = true,
        }
        self.record(DrawOp::PopLayer);
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        self.record(DrawOp::FillRect { rect, color });
    }

    fn fill_rounded_rect(&mut self, rect: Rect, radii: CornerRadii, color: Color) {
        self.record(DrawOp::FillRoundedRect { rect, radii, color });
    }

    fn stroke_rounded_rect(&mut self, rect: Rect, radii: CornerRadii, color: Color, width: f32) {
        self.record(DrawOp::StrokeRoundedRect {
            rect,
            radii,
            color,
            width,
        });
    }

    fn stroke_line(&mut self, from: Point, to: Point, color: Color, width: f32, dash: Dash) {
        self.record(DrawOp::StrokeLine {
            from,
            to,
            color,
            width,
            dash,
        });
    }

    fn fill_path(&mut self, path: &Path, color: Color) {
        self.record(DrawOp::FillPath {
            path: path.clone(),
            color,
        });
    }

    fn stroke_path(&mut self, path: &Path, color: Color, width: f32, dash: Dash) {
        self.record(DrawOp::StrokePath {
            path: path.clone(),
            color,
            width,
            dash,
        });
    }

    fn draw_text_layout(&mut self, layout: &dyn TextLayout, origin: Point, color: Color) {
        self.record(DrawOp::Text {
            text: layout.text().to_owned(),
            size: layout.size(),
            origin,
            color,
        });
    }

    fn draw_image(&mut self, image: ImageId, dest: Rect, opacity: f32) {
        self.record(DrawOp::Image {
            image,
            dest,
            opacity,
        });
    }

    fn draw_scene(&mut self, scene: SceneId, dest: Rect) {
        self.record(DrawOp::Scene { scene, dest });
    }

    fn draw_icon(&mut self, glyph: char, font: IconFont, size: f32, color: Color, center: Point) {
        self.record(DrawOp::Icon {
            glyph,
            font,
            size,
            color,
            center,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::{MonoTextEngine, TextLayoutEngine, TextStyle};

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn rect_close(a: Rect, b: Rect) -> bool {
        close(a.x, b.x) && close(a.y, b.y) && close(a.width, b.width) && close(a.height, b.height)
    }

    fn red() -> Color {
        Color::from_rgb_u32(0xFF0000)
    }

    #[test]
    fn path_builder_tracks_subpaths() {
        let p = Path::new();
        assert!(p.is_empty());
        assert_eq!(p.bounds(), None);

        let triangle = Path::new()
            .move_to(Point::new(0.0, 0.0))
            .line_to(Point::new(10.0, 0.0))
            .line_to(Point::new(5.0, 8.0))
            .close();
        assert_eq!(triangle.verbs().len(), 4);
        assert_eq!(triangle.verbs()[3], PathVerb::Close);
        assert_eq!(triangle.bounds(), Some(Rect::new(0.0, 0.0, 10.0, 8.0)));
        assert!(!triangle.is_empty());

        // Closing twice does nothing the second time.
        assert_eq!(triangle.clone().close(), triangle);
        // Segments after close open a new subpath at the previous start.
        let reopened = triangle.line_to(Point::new(0.0, 20.0));
        assert_eq!(
            &reopened.verbs()[4..],
            &[
                PathVerb::MoveTo(Point::new(0.0, 0.0)),
                PathVerb::LineTo(Point::new(0.0, 20.0))
            ]
        );
        assert_eq!(reopened.bounds(), Some(Rect::new(0.0, 0.0, 10.0, 20.0)));
    }

    #[test]
    fn path_segment_without_move_starts_at_the_origin() {
        let p = Path::new().line_to(Point::new(3.0, 4.0));
        assert_eq!(
            p.verbs(),
            &[
                PathVerb::MoveTo(Point::ZERO),
                PathVerb::LineTo(Point::new(3.0, 4.0))
            ]
        );
        let q = Path::new().quad_to(Point::new(1.0, 5.0), Point::new(2.0, 0.0));
        assert_eq!(q.verbs()[0], PathVerb::MoveTo(Point::ZERO));
        let c = Path::new().cubic_to(
            Point::new(0.0, 9.0),
            Point::new(4.0, -3.0),
            Point::new(4.0, 0.0),
        );
        assert_eq!(c.bounds(), Some(Rect::new(0.0, -3.0, 4.0, 12.0)));
        assert_eq!(q.bounds(), Some(Rect::new(0.0, 0.0, 2.0, 5.0)));
    }

    #[test]
    fn path_from_rect_is_a_closed_rectangle() {
        let p = Path::from_rect(Rect::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(
            p.verbs(),
            &[
                PathVerb::MoveTo(Point::new(1.0, 2.0)),
                PathVerb::LineTo(Point::new(4.0, 2.0)),
                PathVerb::LineTo(Point::new(4.0, 6.0)),
                PathVerb::LineTo(Point::new(1.0, 6.0)),
                PathVerb::Close,
            ]
        );
        assert_eq!(p.bounds(), Some(Rect::new(1.0, 2.0, 3.0, 4.0)));
    }

    #[test]
    fn path_arc_bounds_contain_the_arc() {
        // A half circle of radius 10 from (0,0) to (20,0) bulging up reaches y = -10.
        let p = Path::new().move_to(Point::new(0.0, 0.0)).arc_to(
            Point::new(20.0, 0.0),
            Size::new(10.0, 10.0),
            0.0,
            false,
            true,
        );
        let bounds = p.bounds().expect("non-empty");
        assert!(
            bounds.contains_rect(Rect::new(0.0, -10.0, 20.0, 10.0)),
            "{bounds:?}"
        );
        assert!(matches!(
            p.verbs()[1],
            PathVerb::ArcTo {
                large_arc: false,
                sweep_clockwise: true,
                ..
            }
        ));
        // Radii smaller than half the chord scale up; the chord length is part of the reach.
        let tight = Path::new().move_to(Point::ZERO).arc_to(
            Point::new(100.0, 0.0),
            Size::new(1.0, 1.0),
            0.0,
            true,
            false,
        );
        let bounds = tight.bounds().expect("non-empty");
        assert!(
            bounds.contains_rect(Rect::new(0.0, -50.0, 100.0, 100.0)),
            "{bounds:?}"
        );
        // A close after an arc resets the current point for the next arc.
        let closed =
            tight
                .close()
                .arc_to(Point::new(1.0, 1.0), Size::new(1.0, 1.0), 0.0, false, false);
        assert_eq!(closed.verbs()[3], PathVerb::MoveTo(Point::ZERO));
        assert!(matches!(closed.verbs()[4], PathVerb::ArcTo { .. }));
    }

    #[test]
    fn draw_op_bounds_cover_the_painted_area() {
        let rect = Rect::new(10.0, 10.0, 20.0, 10.0);
        let fill = DrawOp::FillRect { rect, color: red() };
        assert_eq!(fill.bounds(), Some(rect));
        let rounded = DrawOp::FillRoundedRect {
            rect,
            radii: CornerRadii::uniform(4.0),
            color: red(),
        };
        assert_eq!(rounded.bounds(), Some(rect));
        let stroke = DrawOp::StrokeRoundedRect {
            rect,
            radii: CornerRadii::ZERO,
            color: red(),
            width: 2.0,
        };
        assert_eq!(stroke.bounds(), Some(Rect::new(9.0, 9.0, 22.0, 12.0)));
        let line = DrawOp::StrokeLine {
            from: Point::new(30.0, 5.0),
            to: Point::new(10.0, 5.0),
            color: red(),
            width: 2.0,
            dash: Dash::Dashed,
        };
        assert_eq!(line.bounds(), Some(Rect::new(9.0, 4.0, 22.0, 2.0)));
        let path = Path::from_rect(rect);
        let fill_path = DrawOp::FillPath {
            path: path.clone(),
            color: red(),
        };
        assert_eq!(fill_path.bounds(), Some(rect));
        let stroke_path = DrawOp::StrokePath {
            path,
            color: red(),
            width: 4.0,
            dash: Dash::Solid,
        };
        assert_eq!(stroke_path.bounds(), Some(Rect::new(8.0, 8.0, 24.0, 14.0)));
        let empty_path = DrawOp::FillPath {
            path: Path::new(),
            color: red(),
        };
        assert_eq!(empty_path.bounds(), None);
        let empty_stroke = DrawOp::StrokePath {
            path: Path::new(),
            color: red(),
            width: 1.0,
            dash: Dash::Dotted,
        };
        assert_eq!(empty_stroke.bounds(), None);
        let text = DrawOp::Text {
            text: "x".to_owned(),
            size: Size::new(8.0, 12.0),
            origin: Point::new(1.0, 2.0),
            color: red(),
        };
        assert_eq!(text.bounds(), Some(Rect::new(1.0, 2.0, 8.0, 12.0)));
        let image = DrawOp::Image {
            image: ImageId(1),
            dest: rect,
            opacity: 1.0,
        };
        assert_eq!(image.bounds(), Some(rect));
        let scene = DrawOp::Scene {
            scene: SceneId(1),
            dest: rect,
        };
        assert_eq!(scene.bounds(), Some(rect));
        let icon = DrawOp::Icon {
            glyph: '\u{E921}',
            font: IconFont::SegoeFluent,
            size: 10.0,
            color: red(),
            center: Point::new(20.0, 20.0),
        };
        assert_eq!(icon.bounds(), Some(Rect::new(15.0, 15.0, 10.0, 10.0)));
        for state in [
            DrawOp::Save,
            DrawOp::Restore,
            DrawOp::Transform(Transform2D::IDENTITY),
            DrawOp::PushClipRect(rect),
            DrawOp::PushClipRoundedRect {
                rect,
                radii: CornerRadii::ZERO,
            },
            DrawOp::PopClip,
            DrawOp::PushLayer { opacity: 0.5 },
            DrawOp::PopLayer,
        ] {
            assert_eq!(state.bounds(), None, "{state:?}");
        }
    }

    #[test]
    fn recording_canvas_records_every_call_in_order() {
        let mut canvas = RecordingCanvas::new();
        let rect = Rect::new(0.0, 0.0, 10.0, 10.0);
        let path = Path::from_rect(rect);
        let engine = MonoTextEngine::new(10.0);
        let layout = engine.layout("hi", &TextStyle::new("Mono", 10.0), &[], &[], f32::INFINITY);

        canvas.save();
        canvas.push_clip_rect(rect);
        canvas.push_clip_rounded_rect(rect, CornerRadii::uniform(2.0));
        canvas.push_layer(0.5);
        canvas.fill_rect(rect, red());
        canvas.fill_rounded_rect(rect, CornerRadii::uniform(3.0), red());
        canvas.stroke_rounded_rect(rect, CornerRadii::uniform(3.0), red(), 1.0);
        canvas.stroke_line(Point::ZERO, Point::new(5.0, 5.0), red(), 1.0, Dash::Dotted);
        canvas.fill_path(&path, red());
        canvas.stroke_path(&path, red(), 1.0, Dash::Solid);
        canvas.draw_text_layout(&*layout, Point::new(2.0, 3.0), red());
        canvas.draw_image(ImageId(9), rect, 0.8);
        canvas.draw_scene(SceneId(4), rect);
        canvas.draw_icon(
            '\u{E700}',
            IconFont::SegoeMdl2,
            16.0,
            red(),
            Point::new(8.0, 8.0),
        );
        canvas.pop_layer();
        canvas.pop_clip();
        canvas.pop_clip();
        canvas.restore();

        assert!(canvas.is_balanced());
        let kinds: Vec<&DrawOp> = canvas.ops().collect();
        assert_eq!(kinds.len(), 18);
        assert_eq!(kinds[0], &DrawOp::Save);
        assert_eq!(kinds[3], &DrawOp::PushLayer { opacity: 0.5 });
        assert_eq!(kinds[4], &DrawOp::FillRect { rect, color: red() });
        assert_eq!(kinds[17], &DrawOp::Restore);
        assert_eq!(canvas.recorded().len(), 18);
        assert_eq!(canvas.texts(), vec!["hi"]);
        assert_eq!(
            kinds[10],
            &DrawOp::Text {
                text: "hi".to_owned(),
                size: layout.size(),
                origin: Point::new(2.0, 3.0),
                color: red(),
            }
        );
        assert!(close(layout.size().width, 20.0));
    }

    #[test]
    fn ops_in_filters_by_root_space_bounds() {
        let mut canvas = RecordingCanvas::new();
        canvas.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), red());
        canvas.fill_rect(Rect::new(100.0, 100.0, 10.0, 10.0), Color::BLACK);
        canvas.push_clip_rect(Rect::new(0.0, 0.0, 500.0, 500.0));
        canvas.pop_clip();

        let near_origin = canvas.ops_in(Rect::new(5.0, 5.0, 2.0, 2.0));
        assert_eq!(near_origin.len(), 1);
        assert_eq!(
            near_origin[0],
            &DrawOp::FillRect {
                rect: Rect::new(0.0, 0.0, 10.0, 10.0),
                color: red()
            }
        );
        let far = canvas.ops_in(Rect::new(105.0, 105.0, 1.0, 1.0));
        assert_eq!(far.len(), 1);
        // State changes are never returned, even when their rect overlaps.
        assert_eq!(canvas.ops_in(Rect::new(0.0, 0.0, 1000.0, 1000.0)).len(), 2);
        assert!(canvas.ops_in(Rect::new(50.0, 50.0, 10.0, 10.0)).is_empty());
    }

    #[test]
    fn transforms_apply_to_recorded_bounds_and_save_restore_scopes_them() {
        let mut canvas = RecordingCanvas::new();
        canvas.save();
        canvas.translate(100.0, 50.0);
        canvas.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), red());
        // Inner transforms apply first: scale about the translated origin.
        canvas.concat_transform(Transform2D::scale(2.0, 2.0));
        canvas.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), red());
        assert_eq!(
            canvas.transform(),
            Transform2D::scale(2.0, 2.0).compose(Transform2D::translation(100.0, 50.0))
        );
        canvas.restore();
        canvas.fill_rect(Rect::new(0.0, 0.0, 10.0, 10.0), red());
        assert_eq!(canvas.transform(), Transform2D::IDENTITY);

        let bounds: Vec<Rect> = canvas
            .recorded()
            .iter()
            .filter_map(|entry| entry.bounds)
            .collect();
        assert_eq!(bounds.len(), 3);
        assert!(rect_close(bounds[0], Rect::new(100.0, 50.0, 10.0, 10.0)));
        assert!(rect_close(bounds[1], Rect::new(100.0, 50.0, 20.0, 20.0)));
        assert!(rect_close(bounds[2], Rect::new(0.0, 0.0, 10.0, 10.0)));
        assert!(canvas.is_balanced());
        // The recorded transform ops keep their local-space arguments.
        assert!(
            canvas
                .ops()
                .any(|op| *op == DrawOp::Transform(Transform2D::translation(100.0, 50.0)))
        );
        // Queries use root-space bounds: the translated fill is found where it landed.
        assert_eq!(canvas.ops_in(Rect::new(105.0, 55.0, 1.0, 1.0)).len(), 2);
    }

    #[test]
    fn unbalanced_use_is_detected_and_clear_resets() {
        let mut canvas = RecordingCanvas::new();
        canvas.push_clip_rect(Rect::new(0.0, 0.0, 1.0, 1.0));
        assert!(!canvas.is_balanced());
        canvas.pop_clip();
        assert!(canvas.is_balanced());

        canvas.save();
        assert!(!canvas.is_balanced());
        canvas.restore();
        canvas.push_layer(1.0);
        assert!(!canvas.is_balanced());
        canvas.pop_layer();
        assert!(canvas.is_balanced());

        // Popping more than was pushed is an imbalance even though depths are back at zero.
        canvas.pop_clip();
        assert!(!canvas.is_balanced());
        canvas.clear();
        assert!(canvas.is_balanced());
        assert!(canvas.recorded().is_empty());

        canvas.pop_layer();
        assert!(!canvas.is_balanced());
        canvas.clear();
        canvas.restore();
        assert!(!canvas.is_balanced());
        canvas.clear();
        assert_eq!(canvas.transform(), Transform2D::IDENTITY);
    }

    #[test]
    fn text_bounds_follow_the_layout_size_under_transform() {
        let mut canvas = RecordingCanvas::new();
        let engine = MonoTextEngine::new(10.0);
        let layout = engine.layout(
            "abc",
            &TextStyle::new("Mono", 10.0),
            &[],
            &[],
            f32::INFINITY,
        );
        canvas.translate(0.0, 40.0);
        canvas.draw_text_layout(&*layout, Point::new(5.0, 0.0), Color::BLACK);
        let entry = &canvas.recorded()[1];
        let bounds = entry.bounds.expect("text has bounds");
        assert!(
            rect_close(bounds, Rect::new(5.0, 40.0, 30.0, 12.0)),
            "{bounds:?}"
        );
    }
}
