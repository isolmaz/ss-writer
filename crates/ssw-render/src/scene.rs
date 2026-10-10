//! Platform-neutral vector display list.
//!
//! A [`Scene`] is what math formulas, Mermaid diagrams, SVG images and print pages are turned
//! into. `ssw-win` draws it with Direct2D/DirectWrite; nothing here depends on a platform.
//!
//! Units are device-independent: 1 unit is 1/72 inch for Typst output (points) and one CSS pixel
//! for SVG/Mermaid output. The origin is the top-left corner, `y` grows downwards.

use std::fmt;
use std::sync::Arc;

/// A 2-D point.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// A width/height pair.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub width: f32,
    pub height: f32,
}

impl Size {
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }
}

/// An axis-aligned rectangle (`x`, `y` = top-left corner).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// Smallest rectangle containing both.
    pub fn union(&self, other: &Rect) -> Rect {
        let left = self.x.min(other.x);
        let top = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(left, top, right - left, bottom - top)
    }

    /// Grown by `amount` on every side (negative shrinks).
    pub fn expanded(&self, amount: f32) -> Rect {
        Rect::new(
            self.x - amount,
            self.y - amount,
            self.width + 2.0 * amount,
            self.height + 2.0 * amount,
        )
    }

    /// Bounding box of the four corners after `transform`.
    pub fn transformed(&self, transform: &Transform) -> Rect {
        let corners = [
            transform.apply(Point::new(self.x, self.y)),
            transform.apply(Point::new(self.right(), self.y)),
            transform.apply(Point::new(self.right(), self.bottom())),
            transform.apply(Point::new(self.x, self.bottom())),
        ];
        let mut bounds = BoundsBuilder::default();
        for corner in corners {
            bounds.add(corner);
        }
        bounds.finish().unwrap_or_default()
    }

    /// The rectangle as a closed path.
    pub fn to_path(&self) -> Path {
        let mut path = Path::new();
        path.move_to(self.x, self.y);
        path.line_to(self.right(), self.y);
        path.line_to(self.right(), self.bottom());
        path.line_to(self.x, self.bottom());
        path.close();
        path
    }
}

/// Accumulates a bounding box.
#[derive(Clone, Copy, Debug, Default)]
struct BoundsBuilder {
    min: Option<Point>,
    max: Option<Point>,
}

impl BoundsBuilder {
    fn add(&mut self, p: Point) {
        let min = self.min.get_or_insert(p);
        min.x = min.x.min(p.x);
        min.y = min.y.min(p.y);
        let max = self.max.get_or_insert(p);
        max.x = max.x.max(p.x);
        max.y = max.y.max(p.y);
    }

    fn finish(self) -> Option<Rect> {
        let (min, max) = (self.min?, self.max?);
        Some(Rect::new(min.x, min.y, max.x - min.x, max.y - min.y))
    }
}

/// Affine transform. Field names and order match Typst's `Transform` and Direct2D's
/// `Matrix3x2` (`sx` = M11, `ky` = M12, `kx` = M21, `sy` = M22, `tx` = M31, `ty` = M32):
///
/// ```text
/// x' = sx * x + kx * y + tx
/// y' = ky * x + sy * y + ty
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub sx: f32,
    pub ky: f32,
    pub kx: f32,
    pub sy: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        sx: 1.0,
        ky: 0.0,
        kx: 0.0,
        sy: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub const fn new(sx: f32, ky: f32, kx: f32, sy: f32, tx: f32, ty: f32) -> Self {
        Self {
            sx,
            ky,
            kx,
            sy,
            tx,
            ty,
        }
    }

    pub const fn translate(tx: f32, ty: f32) -> Self {
        Self::new(1.0, 0.0, 0.0, 1.0, tx, ty)
    }

    pub const fn scale(sx: f32, sy: f32) -> Self {
        Self::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// Rotation by `radians` (clockwise on screen because `y` points down).
    pub fn rotate(radians: f32) -> Self {
        let (sin, cos) = radians.sin_cos();
        Self::new(cos, sin, -sin, cos, 0.0, 0.0)
    }

    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    /// `self` first, then `next`.
    pub fn then(&self, next: &Transform) -> Transform {
        Transform {
            sx: next.sx * self.sx + next.kx * self.ky,
            kx: next.sx * self.kx + next.kx * self.sy,
            tx: next.sx * self.tx + next.kx * self.ty + next.tx,
            ky: next.ky * self.sx + next.sy * self.ky,
            sy: next.ky * self.kx + next.sy * self.sy,
            ty: next.ky * self.tx + next.sy * self.ty + next.ty,
        }
    }

    pub fn apply(&self, p: Point) -> Point {
        Point::new(
            self.sx * p.x + self.kx * p.y + self.tx,
            self.ky * p.x + self.sy * p.y + self.ty,
        )
    }

    pub fn determinant(&self) -> f32 {
        self.sx * self.sy - self.kx * self.ky
    }

    /// Inverse transform; `None` when the matrix is singular or not finite.
    pub fn invert(&self) -> Option<Transform> {
        let det = self.determinant();
        if det == 0.0 || !det.is_finite() {
            return None;
        }
        let inv = 1.0 / det;
        let sx = self.sy * inv;
        let ky = -self.ky * inv;
        let kx = -self.kx * inv;
        let sy = self.sx * inv;
        Some(Transform {
            sx,
            ky,
            kx,
            sy,
            tx: -(sx * self.tx + kx * self.ty),
            ty: -(ky * self.tx + sy * self.ty),
        })
    }
}

/// One path segment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PathVerb {
    Move(Point),
    Line(Point),
    Quad(Point, Point),
    Cubic(Point, Point, Point),
    Close,
}

/// A sequence of subpaths.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub verbs: Vec<PathVerb>,
}

impl Path {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    pub fn move_to(&mut self, x: f32, y: f32) {
        self.verbs.push(PathVerb::Move(Point::new(x, y)));
    }

    pub fn line_to(&mut self, x: f32, y: f32) {
        self.verbs.push(PathVerb::Line(Point::new(x, y)));
    }

    pub fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.verbs
            .push(PathVerb::Quad(Point::new(cx, cy), Point::new(x, y)));
    }

    pub fn cubic_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.verbs.push(PathVerb::Cubic(
            Point::new(c1x, c1y),
            Point::new(c2x, c2y),
            Point::new(x, y),
        ));
    }

    pub fn close(&mut self) {
        self.verbs.push(PathVerb::Close);
    }

    /// Copy with every point mapped through `transform`.
    pub fn transformed(&self, transform: &Transform) -> Path {
        let map = |p: &Point| transform.apply(*p);
        let verbs = self
            .verbs
            .iter()
            .map(|verb| match verb {
                PathVerb::Move(p) => PathVerb::Move(map(p)),
                PathVerb::Line(p) => PathVerb::Line(map(p)),
                PathVerb::Quad(c, p) => PathVerb::Quad(map(c), map(p)),
                PathVerb::Cubic(a, b, p) => PathVerb::Cubic(map(a), map(b), map(p)),
                PathVerb::Close => PathVerb::Close,
            })
            .collect();
        Path { verbs }
    }

    /// Exact bounding box of the geometry (curve extrema included); `None` for a path without
    /// any point.
    pub fn bounds(&self) -> Option<Rect> {
        let mut bounds = BoundsBuilder::default();
        let mut current = Point::default();
        for verb in &self.verbs {
            match *verb {
                PathVerb::Move(p) | PathVerb::Line(p) => {
                    bounds.add(p);
                    current = p;
                }
                PathVerb::Quad(c, p) => {
                    bounds.add(current);
                    bounds.add(p);
                    // The extremum of each axis is where the derivative vanishes.
                    for t in [
                        quad_extremum(current.x, c.x, p.x),
                        quad_extremum(current.y, c.y, p.y),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        bounds.add(Point::new(
                            quad_at(current.x, c.x, p.x, t),
                            quad_at(current.y, c.y, p.y, t),
                        ));
                    }
                    current = p;
                }
                PathVerb::Cubic(a, b, p) => {
                    bounds.add(current);
                    bounds.add(p);
                    let ts = cubic_extrema(current.x, a.x, b.x, p.x)
                        .into_iter()
                        .chain(cubic_extrema(current.y, a.y, b.y, p.y));
                    for t in ts.flatten() {
                        bounds.add(Point::new(
                            cubic_at(current.x, a.x, b.x, p.x, t),
                            cubic_at(current.y, a.y, b.y, p.y, t),
                        ));
                    }
                    current = p;
                }
                PathVerb::Close => {}
            }
        }
        bounds.finish()
    }
}

fn quad_at(p0: f32, p1: f32, p2: f32, t: f32) -> f32 {
    let u = 1.0 - t;
    u * u * p0 + 2.0 * u * t * p1 + t * t * p2
}

fn cubic_at(p0: f32, p1: f32, p2: f32, p3: f32, t: f32) -> f32 {
    let u = 1.0 - t;
    u * u * u * p0 + 3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t * p3
}

/// Parameter in `(0, 1)` where a quadratic Bézier coordinate has zero derivative.
fn quad_extremum(p0: f32, p1: f32, p2: f32) -> Option<f32> {
    let denom = p0 - 2.0 * p1 + p2;
    if denom == 0.0 {
        return None;
    }
    let t = (p0 - p1) / denom;
    (t > 0.0 && t < 1.0).then_some(t)
}

/// Parameters in `(0, 1)` where a cubic Bézier coordinate has zero derivative.
fn cubic_extrema(p0: f32, p1: f32, p2: f32, p3: f32) -> [Option<f32>; 2] {
    // B'(t)/3 = a t^2 + b t + c
    let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
    let b = 2.0 * (p0 - 2.0 * p1 + p2);
    let c = p1 - p0;
    let inside = |t: f32| (t > 0.0 && t < 1.0).then_some(t);
    if a.abs() < 1e-12 {
        if b.abs() < 1e-12 {
            return [None, None];
        }
        return [inside(-c / b), None];
    }
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return [None, None];
    }
    let root = disc.sqrt();
    [
        inside((-b + root) / (2.0 * a)),
        inside((-b - root) / (2.0 * a)),
    ]
}

/// Fill rule of a [`Fill`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// 8-bit straight (non-premultiplied) RGBA color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const WHITE: Color = Color::rgb(255, 255, 255);
    pub const TRANSPARENT: Color = Color::rgba(0, 0, 0, 0);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Parses `#rgb`, `#rgba`, `#rrggbb` and `#rrggbbaa`.
    pub fn from_hex(text: &str) -> Option<Color> {
        let hex = text.trim().strip_prefix('#')?;
        if !hex.is_ascii() {
            return None;
        }
        let digit = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).ok().map(|d| d * 17);
        let pair = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        match hex.len() {
            3 => Some(Color::rgb(digit(0)?, digit(1)?, digit(2)?)),
            4 => Some(Color::rgba(digit(0)?, digit(1)?, digit(2)?, digit(3)?)),
            6 => Some(Color::rgb(pair(0)?, pair(2)?, pair(4)?)),
            8 => Some(Color::rgba(pair(0)?, pair(2)?, pair(4)?, pair(6)?)),
            _ => None,
        }
    }

    /// Opacity in `0.0..=1.0`.
    pub fn alpha_f32(&self) -> f32 {
        f32::from(self.a) / 255.0
    }

    /// Same color with its alpha multiplied by `opacity` (clamped to `0..=1`).
    pub fn with_opacity(&self, opacity: f32) -> Color {
        let a = (f32::from(self.a) * opacity.clamp(0.0, 1.0)).round();
        // `a` is within 0..=255 by construction.
        Color {
            a: a as u8,
            ..*self
        }
    }
}

/// One gradient stop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GradientStop {
    /// Position in `0.0..=1.0`.
    pub offset: f32,
    pub color: Color,
}

/// What a gradient does outside of its `0..1` range.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpreadMode {
    #[default]
    Pad,
    Reflect,
    Repeat,
}

/// Linear gradient from `start` to `end` (in gradient space, mapped to the item's local space by
/// `transform`).
#[derive(Clone, Debug, PartialEq)]
pub struct LinearGradient {
    pub start: Point,
    pub end: Point,
    pub stops: Vec<GradientStop>,
    pub spread: SpreadMode,
    pub transform: Transform,
}

/// Radial gradient: the colors run from the focal circle (`focal`, `focal_radius`) to the end
/// circle (`center`, `radius`).
#[derive(Clone, Debug, PartialEq)]
pub struct RadialGradient {
    pub center: Point,
    pub radius: f32,
    pub focal: Point,
    pub focal_radius: f32,
    pub stops: Vec<GradientStop>,
    pub spread: SpreadMode,
    pub transform: Transform,
}

/// How a fill or stroke is colored.
#[derive(Clone, Debug, PartialEq)]
pub enum Paint {
    Solid(Color),
    Linear(LinearGradient),
    Radial(RadialGradient),
}

impl Paint {
    /// Color stops of a gradient; `None` for a solid color.
    pub fn stops(&self) -> Option<&[GradientStop]> {
        match self {
            Paint::Solid(_) => None,
            Paint::Linear(gradient) => Some(gradient.stops.as_slice()),
            Paint::Radial(gradient) => Some(gradient.stops.as_slice()),
        }
    }

    /// Same paint with all colors' alpha multiplied by `opacity`.
    pub fn with_opacity(&self, opacity: f32) -> Paint {
        let scale = |stops: &[GradientStop]| -> Vec<GradientStop> {
            stops
                .iter()
                .map(|s| GradientStop {
                    offset: s.offset,
                    color: s.color.with_opacity(opacity),
                })
                .collect()
        };
        match self {
            Paint::Solid(c) => Paint::Solid(c.with_opacity(opacity)),
            Paint::Linear(g) => Paint::Linear(LinearGradient {
                stops: scale(&g.stops),
                ..g.clone()
            }),
            Paint::Radial(g) => Paint::Radial(RadialGradient {
                stops: scale(&g.stops),
                ..g.clone()
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// Dash pattern in absolute units (not multiples of the stroke width).
#[derive(Clone, Debug, PartialEq)]
pub struct Dash {
    /// Alternating dash/gap lengths.
    pub array: Vec<f32>,
    pub phase: f32,
}

/// Filled path.
#[derive(Clone, Debug, PartialEq)]
pub struct Fill {
    pub path: Path,
    pub rule: FillRule,
    pub paint: Paint,
}

/// Stroked path.
#[derive(Clone, Debug, PartialEq)]
pub struct Stroke {
    pub path: Path,
    pub paint: Paint,
    pub width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub dash: Option<Dash>,
    pub miter_limit: f32,
}

/// Index into [`Scene::fonts`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontRef(pub usize);

/// Font file bytes (shared, never copied) and the face index inside it.
#[derive(Clone)]
pub struct FontData {
    bytes: Arc<dyn AsRef<[u8]> + Send + Sync>,
    /// Face index inside a font collection (0 for single-face files).
    pub index: u32,
}

impl FontData {
    pub fn new(bytes: impl AsRef<[u8]> + Send + Sync + 'static, index: u32) -> Self {
        Self {
            bytes: Arc::new(bytes),
            index,
        }
    }

    /// The complete font file (TrueType/OpenType/collection).
    pub fn bytes(&self) -> &[u8] {
        (*self.bytes).as_ref()
    }

    fn same_face(&self, other: &FontData) -> bool {
        self.index == other.index
            && std::ptr::eq(self.bytes().as_ptr(), other.bytes().as_ptr())
            && self.bytes().len() == other.bytes().len()
    }
}

/// Two fonts are equal when they are the same face of the same shared bytes.
impl PartialEq for FontData {
    fn eq(&self, other: &Self) -> bool {
        self.same_face(other)
    }
}

impl fmt::Debug for FontData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FontData")
            .field("len", &self.bytes().len())
            .field("index", &self.index)
            .finish()
    }
}

/// One positioned glyph; `x`/`y` are the glyph origin relative to the run origin (the baseline
/// start), `y` growing downwards.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Glyph {
    pub id: u16,
    pub x: f32,
    pub y: f32,
}

/// A run of glyphs of one font and size, drawn at the origin of the enclosing group.
#[derive(Clone, Debug, PartialEq)]
pub struct Glyphs {
    pub font: FontRef,
    /// Font size (em) in scene units.
    pub size: f32,
    pub glyphs: Vec<Glyph>,
    /// Total advance width of the run.
    pub width: f32,
    pub paint: Paint,
}

/// Mime type used by [`ImageData::rgba8`] for raw pixel buffers.
pub const MIME_RGBA8: &str = "image/x-rgba8";

/// Image payload: an encoded file (`image/png`, `image/jpeg`, `image/gif`, `image/webp`) or a raw
/// straight-alpha RGBA8 buffer (`image/x-rgba8`, `bytes.len() == 4 * width * height`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageData {
    pub bytes: Vec<u8>,
    pub mime: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
}

impl ImageData {
    /// Raw straight-alpha RGBA8 pixels.
    pub fn rgba8(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        Self {
            bytes: pixels,
            mime: MIME_RGBA8.to_owned(),
            width,
            height,
        }
    }
}

/// An image drawn into `rect` (scene units).
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub data: Arc<ImageData>,
    pub rect: Rect,
}

/// A nested coordinate system with optional clip and opacity.
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub transform: Transform,
    /// Clip in the group's own (already transformed) coordinate system.
    pub clip: Option<Path>,
    pub opacity: f32,
    pub items: Vec<SceneItem>,
}

impl Group {
    pub fn new(items: Vec<SceneItem>) -> Self {
        Self {
            transform: Transform::IDENTITY,
            clip: None,
            opacity: 1.0,
            items,
        }
    }
}

/// One display-list entry.
#[derive(Clone, Debug, PartialEq)]
pub enum SceneItem {
    Group(Group),
    Fill(Fill),
    Stroke(Stroke),
    Glyphs(Glyphs),
    Image(Image),
}

/// Approximate glyph box relative to the baseline, in em: no font metrics are available here.
const GLYPH_ASCENT_EM: f32 = 1.0;
const GLYPH_DESCENT_EM: f32 = 0.3;

impl SceneItem {
    /// Bounding box in the coordinate system of the item's parent. Strokes are expanded by half
    /// their width (miter joins excluded); glyph runs use `size` for the vertical extent
    /// (`1.0` em above and `0.3` em below the baseline) and the exact advance for the horizontal
    /// one. Clips are not applied; `None` for items without any geometry.
    pub fn bounds(&self) -> Option<Rect> {
        match self {
            SceneItem::Group(group) => {
                let mut inner = items_bounds(&group.items)?;
                if let Some(clip) = group.clip.as_ref().and_then(Path::bounds) {
                    inner = intersect(&inner, &clip)?;
                }
                Some(inner.transformed(&group.transform))
            }
            SceneItem::Fill(fill) => fill.path.bounds(),
            SceneItem::Stroke(stroke) => stroke
                .path
                .bounds()
                .map(|b| b.expanded(stroke.width.max(0.0) / 2.0)),
            SceneItem::Glyphs(run) => {
                let mut bounds = BoundsBuilder::default();
                for glyph in &run.glyphs {
                    bounds.add(Point::new(glyph.x, glyph.y - GLYPH_ASCENT_EM * run.size));
                    bounds.add(Point::new(
                        glyph.x + run.width.min(run.size),
                        glyph.y + GLYPH_DESCENT_EM * run.size,
                    ));
                }
                if let Some(first) = run.glyphs.first() {
                    bounds.add(Point::new(
                        first.x + run.width,
                        first.y + GLYPH_DESCENT_EM * run.size,
                    ));
                }
                bounds.finish()
            }
            SceneItem::Image(image) => Some(image.rect),
        }
    }

    /// The group, when this item is one.
    pub fn as_group(&self) -> Option<&Group> {
        match self {
            SceneItem::Group(group) => Some(group),
            _ => None,
        }
    }

    /// The fill, when this item is one.
    pub fn as_fill(&self) -> Option<&Fill> {
        match self {
            SceneItem::Fill(fill) => Some(fill),
            _ => None,
        }
    }

    /// The stroke, when this item is one.
    pub fn as_stroke(&self) -> Option<&Stroke> {
        match self {
            SceneItem::Stroke(stroke) => Some(stroke),
            _ => None,
        }
    }

    /// The glyph run, when this item is one.
    pub fn as_glyphs(&self) -> Option<&Glyphs> {
        match self {
            SceneItem::Glyphs(run) => Some(run),
            _ => None,
        }
    }

    /// Rewrites font references (`map[old.0]` is the new reference).
    fn remap_fonts(&mut self, map: &[FontRef]) {
        match self {
            SceneItem::Group(group) => {
                for item in &mut group.items {
                    item.remap_fonts(map);
                }
            }
            SceneItem::Glyphs(run) => run.font = map[run.font.0],
            SceneItem::Fill(_) | SceneItem::Stroke(_) | SceneItem::Image(_) => {}
        }
    }
}

fn intersect(a: &Rect, b: &Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    (right >= left && bottom >= top).then(|| Rect::new(left, top, right - left, bottom - top))
}

fn items_bounds(items: &[SceneItem]) -> Option<Rect> {
    items
        .iter()
        .filter_map(SceneItem::bounds)
        .reduce(|a, b| a.union(&b))
}

/// A complete drawing: a canvas size, the display list and the fonts it refers to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Scene {
    pub size: Size,
    pub items: Vec<SceneItem>,
    /// Fonts referenced by [`Glyphs::font`].
    pub fonts: Vec<Arc<FontData>>,
    /// Distance from the top edge to the text baseline, when the scene is a line of text or an
    /// inline formula that must be aligned with the surrounding text.
    pub baseline: Option<f32>,
}

impl Scene {
    pub fn new(size: Size) -> Self {
        Self {
            size,
            ..Self::default()
        }
    }

    pub fn push(&mut self, item: SceneItem) {
        self.items.push(item);
    }

    /// Registers `font` (reusing an existing entry for the same face) and returns its reference.
    pub fn add_font(&mut self, font: Arc<FontData>) -> FontRef {
        let existing = self
            .fonts
            .iter()
            .position(|f| Arc::ptr_eq(f, &font) || f.same_face(&font));
        FontRef(existing.unwrap_or_else(|| {
            self.fonts.push(font);
            self.fonts.len() - 1
        }))
    }

    /// Bounding box of all items; `None` for an empty scene.
    pub fn bounds(&self) -> Option<Rect> {
        items_bounds(&self.items)
    }

    /// Copies `other`'s items into one group placed with `transform`, merging its fonts into
    /// this scene and remapping glyph references. The group is returned, not added.
    pub fn import(&mut self, other: &Scene, transform: Transform) -> SceneItem {
        let map: Vec<FontRef> = other
            .fonts
            .iter()
            .map(|font| self.add_font(Arc::clone(font)))
            .collect();
        let mut items = other.items.clone();
        for item in &mut items {
            item.remap_fonts(&map);
        }
        SceneItem::Group(Group {
            transform,
            ..Group::new(items)
        })
    }

    /// Appends `other`'s items as one group placed with `transform` (see [`Scene::import`]).
    pub fn embed(&mut self, other: &Scene, transform: Transform) {
        let group = self.import(other, transform);
        self.items.push(group);
    }

    /// New scene of the given size containing this one scaled by `factor`.
    pub fn scaled(&self, factor: f32) -> Scene {
        let mut out = Scene::new(Size::new(
            self.size.width * factor,
            self.size.height * factor,
        ));
        out.baseline = self.baseline.map(|b| b * factor);
        out.embed(self, Transform::scale(factor, factor));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    fn rect_close(r: Rect, x: f32, y: f32, w: f32, h: f32) -> bool {
        close(r.x, x) && close(r.y, y) && close(r.width, w) && close(r.height, h)
    }

    fn solid() -> Paint {
        Paint::Solid(Color::BLACK)
    }

    fn fill(path: Path) -> SceneItem {
        SceneItem::Fill(Fill {
            path,
            rule: FillRule::NonZero,
            paint: solid(),
        })
    }

    #[test]
    fn transform_compose_apply_and_invert() {
        let t = Transform::scale(2.0, 3.0).then(&Transform::translate(10.0, 20.0));
        let p = t.apply(Point::new(1.0, 1.0));
        assert_eq!((p.x, p.y), (12.0, 23.0));
        let inv = t.invert().expect("invertible");
        let back = inv.apply(p);
        assert!(close(back.x, 1.0) && close(back.y, 1.0));
        // Order matters: translate first, then scale.
        let u = Transform::translate(10.0, 20.0).then(&Transform::scale(2.0, 3.0));
        let q = u.apply(Point::new(1.0, 1.0));
        assert_eq!((q.x, q.y), (22.0, 63.0));
        assert!(Transform::IDENTITY.is_identity());
        assert!(!t.is_identity());
        assert_eq!(Transform::default(), Transform::IDENTITY);
    }

    #[test]
    fn transform_rotation_and_singular() {
        let r = Transform::rotate(std::f32::consts::FRAC_PI_2);
        let p = r.apply(Point::new(1.0, 0.0));
        assert!(close(p.x, 0.0) && close(p.y, 1.0));
        assert!(Transform::scale(0.0, 1.0).invert().is_none());
        assert!(Transform::scale(f32::NAN, 1.0).invert().is_none());
        let t = Transform::new(1.0, 0.5, -0.25, 2.0, 3.0, 4.0);
        let round = t.then(&t.invert().expect("invertible"));
        for (a, b) in [
            (round.sx, 1.0),
            (round.ky, 0.0),
            (round.kx, 0.0),
            (round.sy, 1.0),
            (round.tx, 0.0),
            (round.ty, 0.0),
        ] {
            assert!(close(a, b), "{a} != {b}");
        }
    }

    #[test]
    fn rect_union_expand_transform() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, -5.0, 10.0, 10.0);
        assert_eq!(a.union(&b), Rect::new(0.0, -5.0, 15.0, 15.0));
        assert_eq!(a.expanded(1.0), Rect::new(-1.0, -1.0, 12.0, 12.0));
        assert_eq!((a.right(), a.bottom()), (10.0, 10.0));
        let t = a.transformed(&Transform::rotate(std::f32::consts::FRAC_PI_2));
        assert!(rect_close(t, -10.0, 0.0, 10.0, 10.0));
        assert_eq!(a.to_path().bounds(), Some(a));
    }

    #[test]
    fn path_bounds_include_curve_extrema() {
        let mut quad = Path::new();
        quad.move_to(0.0, 0.0);
        quad.quad_to(5.0, 10.0, 10.0, 0.0);
        let b = quad.bounds().expect("bounds");
        assert!(rect_close(b, 0.0, 0.0, 10.0, 5.0), "{b:?}");

        // Quarter circle of radius 10 approximated by one cubic.
        let k = 0.552_284_8 * 10.0;
        let mut cubic = Path::new();
        cubic.move_to(10.0, 0.0);
        cubic.cubic_to(10.0, k, k, 10.0, 0.0, 10.0);
        let b = cubic.bounds().expect("bounds");
        assert!(rect_close(b, 0.0, 0.0, 10.0, 10.0), "{b:?}");

        // A cubic that overshoots its end points on both axes.
        let mut s = Path::new();
        s.move_to(0.0, 0.0);
        s.cubic_to(40.0, -30.0, -30.0, 40.0, 10.0, 10.0);
        let b = s.bounds().expect("bounds");
        assert!(
            b.x < 0.0 && b.y < 0.0 && b.right() > 10.0 && b.bottom() > 10.0,
            "{b:?}"
        );

        // Degenerate (linear) cubic and quad: control points on the chord.
        let mut flat = Path::new();
        flat.move_to(0.0, 0.0);
        flat.cubic_to(1.0, 1.0, 2.0, 2.0, 3.0, 3.0);
        flat.move_to(0.0, 0.0);
        flat.quad_to(1.0, 1.0, 2.0, 2.0);
        assert_eq!(flat.bounds(), Some(Rect::new(0.0, 0.0, 3.0, 3.0)));
        let mut axis = Path::new();
        axis.move_to(0.0, 0.0);
        axis.cubic_to(0.0, 1.0, 0.0, 2.0, 0.0, 3.0);
        assert_eq!(axis.bounds(), Some(Rect::new(0.0, 0.0, 0.0, 3.0)));
        // x control points 0, 2, 2, 0 cancel the cubic term: a parabola peaking at x = 1.5.
        let mut parabola = Path::new();
        parabola.move_to(0.0, 0.0);
        parabola.cubic_to(2.0, 0.0, 2.0, 0.0, 0.0, 0.0);
        let b = parabola.bounds().expect("bounds");
        assert!(rect_close(b, 0.0, 0.0, 1.5, 0.0), "{b:?}");
        // x control points 0, 1, 2, 4 give a strictly increasing cubic: no real extremum.
        let mut monotone = Path::new();
        monotone.move_to(0.0, 0.0);
        monotone.cubic_to(1.0, 0.0, 2.0, 0.0, 4.0, 0.0);
        assert_eq!(monotone.bounds(), Some(Rect::new(0.0, 0.0, 4.0, 0.0)));
        // Quadratic with the control point outside only on one axis, and with t outside (0, 1).
        let mut q2 = Path::new();
        q2.move_to(0.0, 0.0);
        q2.quad_to(20.0, 5.0, 10.0, 10.0);
        let b = q2.bounds().expect("bounds");
        assert!(b.right() > 10.0 && close(b.bottom(), 10.0), "{b:?}");

        assert!(Path::new().bounds().is_none());
        let mut close_only = Path::new();
        close_only.close();
        assert!(close_only.bounds().is_none());
        assert!(Path::new().is_empty() && !quad.is_empty());
    }

    #[test]
    fn path_transform_maps_every_verb() {
        let mut p = Path::new();
        p.move_to(0.0, 0.0);
        p.line_to(1.0, 0.0);
        p.quad_to(1.0, 1.0, 0.0, 1.0);
        p.cubic_to(0.0, 0.5, 0.0, 0.25, 0.0, 0.0);
        p.close();
        let moved = p.transformed(&Transform::translate(5.0, 7.0));
        assert_eq!(
            moved.verbs,
            vec![
                PathVerb::Move(Point::new(5.0, 7.0)),
                PathVerb::Line(Point::new(6.0, 7.0)),
                PathVerb::Quad(Point::new(6.0, 8.0), Point::new(5.0, 8.0)),
                PathVerb::Cubic(
                    Point::new(5.0, 7.5),
                    Point::new(5.0, 7.25),
                    Point::new(5.0, 7.0)
                ),
                PathVerb::Close,
            ]
        );
    }

    #[test]
    fn scene_bounds_cover_all_item_kinds() {
        let mut scene = Scene::new(Size::new(100.0, 100.0));
        assert!(scene.bounds().is_none());
        scene.push(fill(Rect::new(10.0, 10.0, 10.0, 10.0).to_path()));
        let b = scene.bounds().expect("bounds");
        assert!(rect_close(b, 10.0, 10.0, 10.0, 10.0));

        let mut line = Path::new();
        line.move_to(0.0, 50.0);
        line.line_to(40.0, 50.0);
        scene.push(SceneItem::Stroke(Stroke {
            path: line,
            paint: solid(),
            width: 4.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            dash: None,
            miter_limit: 4.0,
        }));
        let b = scene.bounds().expect("bounds");
        // The 4-wide stroke grows by 2 on every side, also along the line (butt caps excluded
        // from the exact outline, as documented on `SceneItem::bounds`).
        assert!(rect_close(b, -2.0, 10.0, 44.0, 42.0), "{b:?}");

        let font = scene.add_font(Arc::new(FontData::new(vec![0u8; 4], 0)));
        scene.push(SceneItem::Glyphs(Glyphs {
            font,
            size: 10.0,
            glyphs: vec![
                Glyph {
                    id: 1,
                    x: 60.0,
                    y: 80.0,
                },
                Glyph {
                    id: 2,
                    x: 66.0,
                    y: 80.0,
                },
            ],
            width: 12.0,
            paint: solid(),
        }));
        scene.push(SceneItem::Image(Image {
            data: Arc::new(ImageData::rgba8(1, 1, vec![0, 0, 0, 255])),
            rect: Rect::new(90.0, 90.0, 10.0, 10.0),
        }));
        let b = scene.bounds().expect("bounds");
        assert!(close(b.right(), 100.0) && close(b.bottom(), 100.0), "{b:?}");
    }

    #[test]
    fn glyph_bounds_use_run_geometry() {
        let item = SceneItem::Glyphs(Glyphs {
            font: FontRef(0),
            size: 10.0,
            glyphs: vec![Glyph {
                id: 1,
                x: 5.0,
                y: 20.0,
            }],
            width: 6.0,
            paint: solid(),
        });
        let b = item.bounds().expect("bounds");
        assert!(rect_close(b, 5.0, 10.0, 6.0, 13.0), "{b:?}");
        let empty = SceneItem::Glyphs(Glyphs {
            font: FontRef(0),
            size: 10.0,
            glyphs: Vec::new(),
            width: 0.0,
            paint: solid(),
        });
        assert!(empty.bounds().is_none());
    }

    #[test]
    fn group_bounds_apply_transform_and_clip() {
        let inner = fill(Rect::new(0.0, 0.0, 100.0, 100.0).to_path());
        let mut group = Group::new(vec![inner.clone()]);
        group.transform = Transform::translate(10.0, 10.0);
        let b = SceneItem::Group(group.clone()).bounds().expect("bounds");
        assert!(rect_close(b, 10.0, 10.0, 100.0, 100.0));

        group.clip = Some(Rect::new(0.0, 0.0, 20.0, 30.0).to_path());
        let b = SceneItem::Group(group.clone()).bounds().expect("bounds");
        assert!(rect_close(b, 10.0, 10.0, 20.0, 30.0), "{b:?}");

        // Clip not overlapping the content leaves nothing visible.
        group.clip = Some(Rect::new(500.0, 500.0, 1.0, 1.0).to_path());
        assert!(SceneItem::Group(group.clone()).bounds().is_none());

        // An empty clip path does not restrict the bounds.
        group.clip = Some(Path::new());
        assert!(SceneItem::Group(group).bounds().is_some());
        assert!(SceneItem::Group(Group::new(Vec::new())).bounds().is_none());
    }

    #[test]
    fn fonts_are_deduplicated_by_face() {
        let mut scene = Scene::default();
        let shared: Arc<[u8]> = Arc::from(vec![1u8, 2, 3]);
        let a = Arc::new(FontData::new(Arc::clone(&shared), 0));
        let a2 = Arc::new(FontData::new(Arc::clone(&shared), 0));
        let other_face = Arc::new(FontData::new(Arc::clone(&shared), 1));
        let other_file = Arc::new(FontData::new(vec![9u8, 9, 9], 0));
        let r0 = scene.add_font(Arc::clone(&a));
        assert_eq!(scene.add_font(Arc::clone(&a)), r0);
        assert_eq!(scene.add_font(a2), r0);
        let r1 = scene.add_font(other_face);
        let r2 = scene.add_font(other_file);
        assert_eq!((r0, r1, r2), (FontRef(0), FontRef(1), FontRef(2)));
        assert_eq!(scene.fonts.len(), 3);
        assert_eq!(a.bytes(), &[1, 2, 3]);
        assert!(format!("{a:?}").contains("len"));
    }

    #[test]
    fn embed_remaps_fonts_and_scaled_resizes() {
        let mut inner = Scene::new(Size::new(10.0, 5.0));
        inner.baseline = Some(4.0);
        let f = inner.add_font(Arc::new(FontData::new(vec![7u8; 3], 0)));
        inner.push(SceneItem::Group(Group::new(vec![SceneItem::Glyphs(
            Glyphs {
                font: f,
                size: 5.0,
                glyphs: vec![Glyph {
                    id: 3,
                    x: 0.0,
                    y: 4.0,
                }],
                width: 3.0,
                paint: solid(),
            },
        )])));

        let mut outer = Scene::new(Size::new(50.0, 50.0));
        let _unused = outer.add_font(Arc::new(FontData::new(vec![1u8], 0)));
        outer.embed(&inner, Transform::translate(5.0, 5.0));
        assert_eq!(outer.fonts.len(), 2);
        let g = outer.items[0].as_group().expect("embed must add a group");
        let g2 = g.items[0].as_group().expect("nested group expected");
        let run = g2.items[0].as_glyphs().expect("glyph run expected");
        assert_eq!(run.font, FontRef(1));
        assert_eq!(g.transform, Transform::translate(5.0, 5.0));

        let big = inner.scaled(2.0);
        assert_eq!(big.size, Size::new(20.0, 10.0));
        assert_eq!(big.baseline, Some(8.0));
        let b = big.bounds().expect("bounds");
        assert!(b.width > 0.0);
        assert_eq!(Scene::new(Size::new(1.0, 1.0)).scaled(2.0).baseline, None);
    }

    #[test]
    fn color_parsing_and_opacity() {
        let table = [
            ("#fff", Some(Color::rgb(255, 255, 255))),
            ("#f008", Some(Color::rgba(255, 0, 0, 136))),
            ("#336699", Some(Color::rgb(0x33, 0x66, 0x99))),
            ("#33669980", Some(Color::rgba(0x33, 0x66, 0x99, 0x80))),
            (" #000 ", Some(Color::BLACK)),
            ("fff", None),
            ("#ff", None),
            ("#ggg", None),
            ("#gggggg", None),
            ("#\u{1000}", None),
            ("#a\u{00ff}bcd", None),
        ];
        for (text, expected) in table {
            assert_eq!(Color::from_hex(text), expected, "{text}");
        }
        assert_eq!(Color::WHITE.with_opacity(0.5).a, 128);
        assert_eq!(Color::WHITE.with_opacity(7.0).a, 255);
        assert_eq!(Color::WHITE.with_opacity(-1.0).a, 0);
        assert!((Color::rgba(0, 0, 0, 51).alpha_f32() - 0.2).abs() < 1e-6);
        assert_eq!(Color::TRANSPARENT.a, 0);
    }

    #[test]
    fn paint_opacity_scales_gradient_stops() {
        let stops = vec![
            GradientStop {
                offset: 0.0,
                color: Color::rgb(255, 0, 0),
            },
            GradientStop {
                offset: 1.0,
                color: Color::rgb(0, 0, 255),
            },
        ];
        let linear = Paint::Linear(LinearGradient {
            start: Point::new(0.0, 0.0),
            end: Point::new(1.0, 0.0),
            stops: stops.clone(),
            spread: SpreadMode::Pad,
            transform: Transform::IDENTITY,
        });
        let radial = Paint::Radial(RadialGradient {
            center: Point::new(0.5, 0.5),
            radius: 0.5,
            focal: Point::new(0.5, 0.5),
            focal_radius: 0.0,
            stops,
            spread: SpreadMode::Reflect,
            transform: Transform::IDENTITY,
        });
        for paint in [linear, radial] {
            let faded = paint.with_opacity(0.5);
            let stops = faded.stops().expect("gradient expected");
            assert_eq!(stops.len(), 2);
            assert!(stops.iter().all(|s| s.color.a == 128));
        }
        assert!(Paint::Solid(Color::BLACK).stops().is_none());
        assert_eq!(
            Paint::Solid(Color::BLACK).with_opacity(0.0),
            Paint::Solid(Color::TRANSPARENT)
        );
    }

    #[test]
    fn group_new_defaults() {
        let g = Group::new(Vec::new());
        assert_eq!(g.transform, Transform::IDENTITY);
        assert_eq!(g.opacity, 1.0);
        assert!(g.clip.is_none());
        assert_eq!(FillRule::default(), FillRule::NonZero);
        assert_eq!(LineCap::default(), LineCap::Butt);
        assert_eq!(LineJoin::default(), LineJoin::Miter);
        assert_eq!(SpreadMode::default(), SpreadMode::Pad);
    }

    #[test]
    fn scene_item_accessors_match_only_their_variant() {
        let glyphs = SceneItem::Glyphs(Glyphs {
            font: FontRef(0),
            size: 10.0,
            glyphs: Vec::new(),
            width: 0.0,
            paint: solid(),
        });
        let stroke = SceneItem::Stroke(Stroke {
            path: Path::new(),
            paint: solid(),
            width: 2.0,
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            dash: None,
            miter_limit: 4.0,
        });
        let group = SceneItem::Group(Group::new(Vec::new()));
        let filled = fill(Path::new());
        // The payload is returned intact.
        assert_eq!(stroke.as_stroke().map(|s| s.width), Some(2.0));
        assert_eq!(glyphs.as_glyphs().map(|g| g.size), Some(10.0));
        // Each accessor yields exactly its own variant.
        let items = [&group, &filled, &stroke, &glyphs];
        let variants: Vec<[bool; 4]> = items
            .iter()
            .map(|item| {
                [
                    item.as_group().is_some(),
                    item.as_fill().is_some(),
                    item.as_stroke().is_some(),
                    item.as_glyphs().is_some(),
                ]
            })
            .collect();
        assert_eq!(
            variants,
            [
                [true, false, false, false],
                [false, true, false, false],
                [false, false, true, false],
                [false, false, false, true],
            ]
        );
    }

    #[test]
    fn rgba_image_data_is_tagged() {
        let img = ImageData::rgba8(2, 1, vec![0; 8]);
        assert_eq!(img.mime, MIME_RGBA8);
        assert_eq!((img.width, img.height), (2, 1));
    }
}
