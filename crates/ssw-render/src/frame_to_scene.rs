//! Typst `Frame` → [`Scene`].
//!
//! Groups keep their transform and clip, text becomes glyph runs that carry the font bytes,
//! shapes become filled/stroked paths (linear and radial gradients are preserved) and images
//! become scene images (SVG images are converted to nested scenes).
//!
//! Not representable and therefore skipped: text outlines (stroked text), PDF images, links and
//! introspection tags. Conic gradients and tilings are approximated by one solid color.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use typst::layout::{Abs, Frame, FrameItem, GroupItem, Point as TPoint, Transform as TTransform};
use typst::text::{Font, TextItem};
use typst::visualize::{
    Color as TColor, Curve, CurveItem, ExchangeFormat, FillRule as TFillRule, FixedStroke,
    Geometry, Gradient, Image as TImage, ImageKind, LineCap as TCap, LineJoin as TJoin,
    Paint as TPaint, RasterFormat, RasterImage,
};

use crate::scene::{
    Color, Dash, Fill, FillRule, FontData, Glyph, Glyphs, Group, Image, ImageData, LineCap,
    LineJoin, LinearGradient, Paint, Path, PathVerb, Point, RadialGradient, Rect, Scene, SceneItem,
    Size, SpreadMode, Stroke, Transform,
};
use crate::svg_to_scene::svg_to_scene;

/// Fill color of the invisible text run that marks the baseline of inline math
/// (see [`crate::math`]). Runs with this fill are never converted.
pub(crate) const BASELINE_MARKER: [u8; 4] = [1, 2, 3, 4];

type FontKey = (usize, usize, u32);

/// Font file bytes shared between all scenes. The cache entry keeps the bytes alive, so the
/// address used as key cannot be reused by another font.
fn font_data(font: &Font) -> Arc<FontData> {
    static CACHE: LazyLock<Mutex<HashMap<FontKey, Arc<FontData>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let bytes = font.data();
    let key = (bytes.as_ptr() as usize, bytes.len(), font.index());
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(FontData::new(bytes.clone(), font.index()))),
    )
}

fn pt(abs: Abs) -> f32 {
    abs.to_pt() as f32
}

fn color(c: &TColor) -> Color {
    // Gradient stops live in Oklab by default: convert to sRGB (as typst-render does) instead of
    // reading the raw components.
    let (r, g, b, a) = c
        .to_process()
        .to_rgb()
        .into_format::<u8, u8>()
        .into_components();
    Color::rgba(r, g, b, a)
}

/// Converts a whole frame (a page, or any laid-out box). The scene size is the frame size in
/// points. `baseline` is set when the frame contains the baseline marker of inline math.
pub fn frame_to_scene(frame: &Frame) -> Scene {
    let size = frame.size();
    let mut converter = Converter {
        scene: Scene::new(Size::new(pt(size.x), pt(size.y))),
    };
    let mut items = Vec::new();
    converter.walk(frame, 0.0, &mut items);
    let mut scene = converter.scene;
    scene.items = items;
    scene
}

/// Walks frames and collects fonts (and the baseline) in the scene under construction.
struct Converter {
    scene: Scene,
}

impl Converter {
    /// `origin_y` is the vertical offset of `frame` inside the converted frame (the sum of the
    /// translations of the enclosing groups); it positions the baseline marker.
    fn walk(&mut self, frame: &Frame, origin_y: f32, out: &mut Vec<SceneItem>) {
        for (pos, item) in frame.items() {
            match item {
                FrameItem::Group(group) => {
                    let mut inner = Vec::new();
                    let inner_origin = origin_y + pt(pos.y) + pt(group.transform.ty);
                    self.walk(&group.frame, inner_origin, &mut inner);
                    out.push(SceneItem::Group(convert_group(group, *pos, inner)));
                }
                FrameItem::Text(text) => self.text(text, *pos, origin_y, out),
                FrameItem::Shape(shape, _) => convert_shape(shape, *pos, out),
                FrameItem::Image(image, size, _) => {
                    self.image(image, *pos, pt(size.x), pt(size.y), out);
                }
                FrameItem::Link(..) | FrameItem::Tag(..) => {}
            }
        }
    }

    fn text(&mut self, text: &TextItem, pos: TPoint, origin_y: f32, out: &mut Vec<SceneItem>) {
        if matches!(&text.fill, TPaint::Solid(c) if c.to_vec4_u8() == BASELINE_MARKER) {
            self.scene.baseline.get_or_insert(origin_y + pt(pos.y));
            return;
        }
        let size = text.size;
        let mut x = pos.x;
        let glyphs = text
            .glyphs
            .iter()
            .map(|g| {
                let glyph = Glyph {
                    id: g.id,
                    x: pt(x + g.x_offset.at(size)),
                    // Typst offsets are Y-up, the scene is Y-down.
                    y: pt(pos.y - g.y_offset.at(size)),
                };
                x += g.x_advance.at(size);
                glyph
            })
            .collect();
        let width = pt(text.width());
        let run_bounds = Rect::new(pt(pos.x), pt(pos.y - size), width, pt(size) * 1.3);
        let font = self.scene.add_font(font_data(text.font.font()));
        out.push(SceneItem::Glyphs(Glyphs {
            font,
            size: pt(size),
            glyphs,
            width,
            paint: convert_paint(&text.fill, run_bounds),
        }));
    }

    fn image(
        &mut self,
        image: &TImage,
        pos: TPoint,
        width: f32,
        height: f32,
        out: &mut Vec<SceneItem>,
    ) {
        let (x, y) = (pt(pos.x), pt(pos.y));
        match image.kind() {
            ImageKind::Raster(raster) => out.push(SceneItem::Image(Image {
                data: Arc::new(raster_data(raster)),
                rect: Rect::new(x, y, width, height),
            })),
            ImageKind::Svg(svg) => {
                // An SVG that usvg cannot parse is left out; Typst itself already parsed it.
                if let Ok(inner) = svg_to_scene(svg.data()) {
                    let sx = width / inner.size.width.max(f32::EPSILON);
                    let sy = height / inner.size.height.max(f32::EPSILON);
                    let placement = Transform::scale(sx, sy).then(&Transform::translate(x, y));
                    out.push(self.scene.import(&inner, placement));
                }
            }
            // PDF images cannot be drawn without a PDF renderer.
            ImageKind::Pdf(_) => {}
        }
    }
}

fn convert_group(group: &GroupItem, pos: TPoint, items: Vec<SceneItem>) -> Group {
    let t: &TTransform = &group.transform;
    let local = Transform::new(
        t.sx.get() as f32,
        t.ky.get() as f32,
        t.kx.get() as f32,
        t.sy.get() as f32,
        pt(t.tx),
        pt(t.ty),
    );
    // Typst applies the group's own transform first, then the translation to its position.
    let transform = local.then(&Transform::translate(pt(pos.x), pt(pos.y)));
    Group {
        transform,
        clip: group.clip.as_ref().map(|c| curve_path(c, TPoint::zero())),
        opacity: 1.0,
        items,
    }
}

/// A Typst curve as a path, translated by `origin`.
fn curve_path(curve: &Curve, origin: TPoint) -> Path {
    let map = |p: &TPoint| Point::new(pt(origin.x + p.x), pt(origin.y + p.y));
    Path {
        verbs: curve
            .0
            .iter()
            .map(|item| match item {
                CurveItem::Move(p) => PathVerb::Move(map(p)),
                CurveItem::Line(p) => PathVerb::Line(map(p)),
                CurveItem::Cubic(a, b, p) => PathVerb::Cubic(map(a), map(b), map(p)),
                CurveItem::Close => PathVerb::Close,
            })
            .collect(),
    }
}

fn geometry_path(geometry: &Geometry, origin: TPoint) -> Path {
    match geometry {
        Geometry::Line(to) => {
            let mut path = Path::new();
            path.move_to(pt(origin.x), pt(origin.y));
            path.line_to(pt(origin.x + to.x), pt(origin.y + to.y));
            path
        }
        Geometry::Rect(size) => {
            Rect::new(pt(origin.x), pt(origin.y), pt(size.x), pt(size.y)).to_path()
        }
        Geometry::Curve(curve) => curve_path(curve, origin),
    }
}

fn convert_shape(shape: &typst::visualize::Shape, pos: TPoint, out: &mut Vec<SceneItem>) {
    let path = geometry_path(&shape.geometry, pos);
    let bounds = path.bounds().unwrap_or_default();
    if let Some(paint) = &shape.fill {
        out.push(SceneItem::Fill(Fill {
            path: path.clone(),
            rule: match shape.fill_rule {
                TFillRule::NonZero => FillRule::NonZero,
                TFillRule::EvenOdd => FillRule::EvenOdd,
            },
            paint: convert_paint(paint, bounds),
        }));
    }
    if let Some(stroke) = &shape.stroke
        && let Some(item) = convert_stroke(stroke, path, bounds)
    {
        out.push(item);
    }
}

fn convert_stroke(stroke: &FixedStroke, path: Path, bounds: Rect) -> Option<SceneItem> {
    let width = pt(stroke.thickness);
    if width <= 0.0 {
        return None;
    }
    Some(SceneItem::Stroke(Stroke {
        path,
        paint: convert_paint(&stroke.paint, bounds.expanded(width / 2.0)),
        width,
        cap: match stroke.cap {
            TCap::Butt => LineCap::Butt,
            TCap::Round => LineCap::Round,
            TCap::Square => LineCap::Square,
        },
        join: match stroke.join {
            TJoin::Miter => LineJoin::Miter,
            TJoin::Round => LineJoin::Round,
            TJoin::Bevel => LineJoin::Bevel,
        },
        dash: stroke.dash.as_ref().map(|d| Dash {
            array: d.array.iter().map(|a| pt(*a)).collect(),
            phase: pt(d.phase),
        }),
        miter_limit: stroke.miter_limit.get() as f32,
    }))
}

/// Typst paint → scene paint for an item occupying `bounds` (gradients are always resolved
/// relative to the item itself).
fn convert_paint(paint: &TPaint, bounds: Rect) -> Paint {
    match paint {
        TPaint::Solid(c) => Paint::Solid(color(c)),
        TPaint::Gradient(gradient) => convert_gradient(gradient, bounds),
        // Tilings have no scene representation: use a neutral gray.
        TPaint::Tiling(_) => Paint::Solid(Color::rgb(128, 128, 128)),
    }
}

fn convert_gradient(gradient: &Gradient, bounds: Rect) -> Paint {
    let stops = |list: &[(TColor, typst::layout::Ratio)]| -> Vec<crate::scene::GradientStop> {
        list.iter()
            .map(|(c, offset)| crate::scene::GradientStop {
                offset: offset.get() as f32,
                color: color(c),
            })
            .collect()
    };
    match gradient {
        Gradient::Linear(linear) => {
            let angle = linear.angle.to_rad() as f32;
            let (sin, cos) = angle.sin_cos();
            let length = (bounds.width * cos).abs() + (bounds.height * sin).abs();
            // The gradient starts at the corner with the smallest projection on its direction.
            let start = Point::new(
                if cos >= 0.0 { bounds.x } else { bounds.right() },
                if sin >= 0.0 {
                    bounds.y
                } else {
                    bounds.bottom()
                },
            );
            Paint::Linear(LinearGradient {
                start,
                end: Point::new(start.x + cos * length, start.y + sin * length),
                stops: stops(&linear.stops),
                spread: SpreadMode::Pad,
                transform: Transform::IDENTITY,
            })
        }
        Gradient::Radial(radial) => {
            let ratio = |v: typst::layout::Ratio| v.get() as f32;
            // Radial gradients live in the unit square of the item.
            Paint::Radial(RadialGradient {
                center: Point::new(ratio(radial.center.x), ratio(radial.center.y)),
                radius: ratio(radial.radius),
                focal: Point::new(ratio(radial.focal_center.x), ratio(radial.focal_center.y)),
                focal_radius: ratio(radial.focal_radius),
                stops: stops(&radial.stops),
                spread: SpreadMode::Pad,
                transform: Transform::new(
                    bounds.width,
                    0.0,
                    0.0,
                    bounds.height,
                    bounds.x,
                    bounds.y,
                ),
            })
        }
        Gradient::Conic(conic) => {
            // No conic gradients in the scene: the middle stop stands in for the whole fill.
            let middle = conic
                .stops
                .get(conic.stops.len() / 2)
                .map_or(Color::rgb(128, 128, 128), |(c, _)| color(c));
            Paint::Solid(middle)
        }
    }
}

fn exchange_mime(format: ExchangeFormat) -> &'static str {
    match format {
        ExchangeFormat::Png => "image/png",
        ExchangeFormat::Jpg => "image/jpeg",
        ExchangeFormat::Gif => "image/gif",
        ExchangeFormat::Webp => "image/webp",
    }
}

/// The image as the original encoded file, or as decoded pixels when the file cannot be drawn
/// as it is (raw pixel buffers, and files that need an EXIF rotation).
fn raster_data(raster: &RasterImage) -> ImageData {
    if let RasterFormat::Exchange(format) = raster.format()
        && raster.exif_rotation().is_none()
    {
        return ImageData {
            bytes: raster.data().to_vec(),
            mime: exchange_mime(format).to_owned(),
            width: raster.width(),
            height: raster.height(),
        };
    }
    let rgba = raster.dynamic().to_rgba8();
    ImageData::rgba8(rgba.width(), rgba.height(), rgba.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_formats_map_to_mime_types() {
        let table = [
            (ExchangeFormat::Png, "image/png"),
            (ExchangeFormat::Jpg, "image/jpeg"),
            (ExchangeFormat::Gif, "image/gif"),
            (ExchangeFormat::Webp, "image/webp"),
        ];
        for (format, mime) in table {
            assert_eq!(exchange_mime(format), mime);
        }
    }
}
