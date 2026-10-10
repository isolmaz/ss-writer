//! SVG → [`Scene`] through `usvg` (used for SVG images and Mermaid diagrams).
//!
//! Text is converted to outlines by usvg (default family Segoe UI, system fonts), so scenes made
//! here never contain glyph runs. Supported: paths with fills and strokes (solid, linear and
//! radial gradients, dashes, paint order), nested groups with transforms, opacity and (nested)
//! clip paths, and embedded images (PNG, JPEG, GIF and WebP payloads stay encoded; nested SVG
//! images become groups). Only `data:` URLs are resolved, an SVG can never read local files.
//!
//! Not supported and skipped, so the content is drawn as if the feature were absent: masks,
//! filters, blend modes and the clip rule of clip paths (`Scene` clips carry no fill rule).
//! Pattern paints are drawn as mid gray.

use std::sync::{Arc, LazyLock};

use thiserror::Error;
use usvg::tiny_skia_path::PathSegment;

use crate::scene::{
    Color, Dash, Fill, FillRule, GradientStop, Group, Image, ImageData, LineCap, LineJoin,
    LinearGradient, Paint, Path, Point, RadialGradient, Rect, Scene, SceneItem, Size, SpreadMode,
    Stroke, Transform,
};

/// Default font family for SVG text without a usable `font-family`.
pub const DEFAULT_FONT_FAMILY: &str = "Segoe UI";

/// The SVG could not be parsed.
#[derive(Debug, Error)]
#[error("invalid SVG: {0}")]
pub struct SvgError(#[from] usvg::Error);

static SYSTEM_FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut db = usvg::fontdb::Database::new();
    db.load_system_fonts();
    Arc::new(db)
});

fn options() -> usvg::Options<'static> {
    usvg::Options {
        font_family: DEFAULT_FONT_FAMILY.to_owned(),
        fontdb: Arc::clone(&SYSTEM_FONTS),
        image_href_resolver: usvg::ImageHrefResolver {
            // Only `data:` URLs: an SVG must not be able to read arbitrary local files.
            resolve_string: Box::new(|_, _| None),
            ..usvg::ImageHrefResolver::default()
        },
        ..usvg::Options::default()
    }
}

/// Parses `data` (SVG text) and converts it. The scene size is the SVG's size in CSS pixels.
pub fn svg_to_scene(data: &[u8]) -> Result<Scene, SvgError> {
    let tree = usvg::Tree::from_data(data, &options())?;
    Ok(tree_to_scene(&tree))
}

/// Converts a parsed tree.
pub fn tree_to_scene(tree: &usvg::Tree) -> Scene {
    let size = tree.size();
    let mut scene = Scene::new(Size::new(size.width(), size.height()));
    scene.items = convert_children(tree.root());
    scene
}

fn convert_transform(t: usvg::Transform) -> Transform {
    Transform::new(t.sx, t.ky, t.kx, t.sy, t.tx, t.ty)
}

fn convert_children(group: &usvg::Group) -> Vec<SceneItem> {
    let mut out = Vec::new();
    for node in group.children() {
        match node {
            usvg::Node::Group(g) => out.push(SceneItem::Group(convert_group(g))),
            usvg::Node::Path(p) => convert_path(p, &mut out),
            usvg::Node::Text(t) => out.push(SceneItem::Group(convert_group(t.flattened()))),
            usvg::Node::Image(image) => convert_image(image, &mut out),
        }
    }
    out
}

fn convert_group(group: &usvg::Group) -> Group {
    let items = convert_children(group);
    let (clip, items) = match group.clip_path() {
        Some(clip) => (Some(clip_path(clip)), nested_clips(clip, items)),
        None => (None, items),
    };
    Group {
        transform: convert_transform(group.transform()),
        clip,
        opacity: group.opacity().get(),
        items,
    }
}

/// A clip path can itself be clipped (`clip-path` on a `<clipPath>`); the result is the
/// intersection of all of them, expressed as nested single-clip groups. The first clip is
/// applied by the caller, this wraps `items` in groups for the remaining ones. Nested clips use
/// the coordinate system of the clipped element, like the first one.
fn nested_clips(clip: &usvg::ClipPath, items: Vec<SceneItem>) -> Vec<SceneItem> {
    match clip.clip_path() {
        None => items,
        Some(next) => {
            let inner = nested_clips(next, items);
            vec![SceneItem::Group(Group {
                clip: Some(clip_path(next)),
                ..Group::new(inner)
            })]
        }
    }
}

fn convert_image(image: &usvg::Image, out: &mut Vec<SceneItem>) {
    if !image.is_visible() {
        return;
    }
    let size = image.size();
    let rect = Rect::new(0.0, 0.0, size.width(), size.height());
    let (bytes, mime) = match image.kind() {
        usvg::ImageKind::PNG(bytes) => (bytes, "image/png"),
        usvg::ImageKind::JPEG(bytes) => (bytes, "image/jpeg"),
        usvg::ImageKind::GIF(bytes) => (bytes, "image/gif"),
        usvg::ImageKind::WEBP(bytes) => (bytes, "image/webp"),
        usvg::ImageKind::SVG(tree) => {
            // A nested document is clipped to its own viewport.
            out.push(SceneItem::Group(Group {
                clip: Some(rect.to_path()),
                ..Group::new(convert_children(tree.root()))
            }));
            return;
        }
    };
    out.push(SceneItem::Image(Image {
        data: Arc::new(ImageData {
            bytes: bytes.to_vec(),
            mime: mime.to_owned(),
            // usvg only reports images whose pixel size it could read, as a positive integer.
            width: size.width() as u32,
            height: size.height() as u32,
        }),
        rect,
    }));
}

/// Union of all geometry of a clip path, in the coordinate system of the clipped element.
fn clip_path(clip: &usvg::ClipPath) -> Path {
    let mut path = Path::new();
    collect_geometry(clip.root(), &Transform::IDENTITY, &mut path);
    path.transformed(&convert_transform(clip.transform()))
}

/// usvg only keeps paths, groups and text in a clip path (`<image>` is dropped while parsing),
/// so any other node kind contributes no geometry.
fn collect_geometry(group: &usvg::Group, parent: &Transform, out: &mut Path) {
    for node in group.children() {
        if let usvg::Node::Path(p) = node {
            out.verbs
                .extend(convert_geometry(p.data()).transformed(parent).verbs);
        } else if let usvg::Node::Group(g) = node {
            let t = convert_transform(g.transform()).then(parent);
            collect_geometry(g, &t, out);
        } else if let usvg::Node::Text(t) = node {
            collect_geometry(t.flattened(), parent, out);
        }
    }
}

fn convert_geometry(path: &usvg::tiny_skia_path::Path) -> Path {
    let mut out = Path::new();
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(p) => out.move_to(p.x, p.y),
            PathSegment::LineTo(p) => out.line_to(p.x, p.y),
            PathSegment::QuadTo(c, p) => out.quad_to(c.x, c.y, p.x, p.y),
            PathSegment::CubicTo(a, b, p) => out.cubic_to(a.x, a.y, b.x, b.y, p.x, p.y),
            PathSegment::Close => out.close(),
        }
    }
    out
}

fn convert_path(path: &usvg::Path, out: &mut Vec<SceneItem>) {
    if !path.is_visible() {
        return;
    }
    let geometry = convert_geometry(path.data());
    let fill = path.fill().map(|fill| {
        SceneItem::Fill(Fill {
            path: geometry.clone(),
            rule: match fill.rule() {
                usvg::FillRule::NonZero => FillRule::NonZero,
                usvg::FillRule::EvenOdd => FillRule::EvenOdd,
            },
            paint: convert_paint(fill.paint(), fill.opacity().get()),
        })
    });
    let stroke = path.stroke().map(move |stroke| {
        SceneItem::Stroke(Stroke {
            path: geometry,
            paint: convert_paint(stroke.paint(), stroke.opacity().get()),
            width: stroke.width().get(),
            cap: match stroke.linecap() {
                usvg::LineCap::Butt => LineCap::Butt,
                usvg::LineCap::Round => LineCap::Round,
                usvg::LineCap::Square => LineCap::Square,
            },
            join: match stroke.linejoin() {
                usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => LineJoin::Miter,
                usvg::LineJoin::Round => LineJoin::Round,
                usvg::LineJoin::Bevel => LineJoin::Bevel,
            },
            dash: stroke.dasharray().map(|array| Dash {
                array: array.to_vec(),
                phase: stroke.dashoffset(),
            }),
            miter_limit: stroke.miterlimit().get(),
        })
    });
    if matches!(path.paint_order(), usvg::PaintOrder::StrokeAndFill) {
        out.extend(stroke);
        out.extend(fill);
    } else {
        out.extend(fill);
        out.extend(stroke);
    }
}

fn convert_color(c: usvg::Color, opacity: f32) -> Color {
    Color::rgb(c.red, c.green, c.blue).with_opacity(opacity)
}

fn convert_stops(stops: &[usvg::Stop], opacity: f32) -> Vec<GradientStop> {
    stops
        .iter()
        .map(|s| GradientStop {
            offset: s.offset().get(),
            color: convert_color(s.color(), s.opacity().get() * opacity),
        })
        .collect()
}

fn convert_spread(spread: usvg::SpreadMethod) -> SpreadMode {
    match spread {
        usvg::SpreadMethod::Pad => SpreadMode::Pad,
        usvg::SpreadMethod::Reflect => SpreadMode::Reflect,
        usvg::SpreadMethod::Repeat => SpreadMode::Repeat,
    }
}

fn convert_paint(paint: &usvg::Paint, opacity: f32) -> Paint {
    match paint {
        usvg::Paint::Color(c) => Paint::Solid(convert_color(*c, opacity)),
        usvg::Paint::LinearGradient(g) => Paint::Linear(LinearGradient {
            start: Point::new(g.x1(), g.y1()),
            end: Point::new(g.x2(), g.y2()),
            stops: convert_stops(g.stops(), opacity),
            spread: convert_spread(g.spread_method()),
            transform: convert_transform(g.transform()),
        }),
        usvg::Paint::RadialGradient(g) => Paint::Radial(RadialGradient {
            center: Point::new(g.cx(), g.cy()),
            radius: g.r().get(),
            focal: Point::new(g.fx(), g.fy()),
            focal_radius: g.fr().get(),
            stops: convert_stops(g.stops(), opacity),
            spread: convert_spread(g.spread_method()),
            transform: convert_transform(g.transform()),
        }),
        usvg::Paint::Pattern(_) => Paint::Solid(Color::rgb(128, 128, 128).with_opacity(opacity)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::PathVerb;

    fn scene(svg: &str) -> Scene {
        svg_to_scene(svg.as_bytes()).expect("valid svg")
    }

    fn flatten(items: &[SceneItem], out: &mut Vec<SceneItem>) {
        for item in items {
            match item {
                SceneItem::Group(g) => flatten(&g.items, out),
                other => out.push(other.clone()),
            }
        }
    }

    fn leaves(scene: &Scene) -> Vec<SceneItem> {
        let mut out = Vec::new();
        flatten(&scene.items, &mut out);
        out
    }

    #[test]
    fn rect_fill_and_size() {
        let s = scene(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20" viewBox="0 0 40 20">
                <rect x="5" y="2" width="10" height="6" fill="#ff0000" fill-opacity="0.5"/></svg>"##,
        );
        assert_eq!(s.size, Size::new(40.0, 20.0));
        let items = leaves(&s);
        assert_eq!(items.len(), 1);
        let fill = items[0].as_fill().expect("fill expected");
        assert_eq!(fill.paint, Paint::Solid(Color::rgba(255, 0, 0, 128)));
        let b = fill.path.bounds().expect("bounds");
        assert_eq!((b.x, b.y, b.width, b.height), (5.0, 2.0, 10.0, 6.0));
    }

    #[test]
    fn even_odd_fills_square_caps_and_repeating_gradients() {
        let s = scene(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10" viewBox="0 0 20 10">
                <defs><linearGradient id="g" spreadMethod="repeat"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#00f"/></linearGradient></defs>
                <path d="M0 0H10V10H0Z" fill="#000" fill-rule="evenodd"/>
                <path d="M0 5H10" fill="none" stroke="#00f" stroke-width="2" stroke-linecap="square"/>
                <rect x="10" width="10" height="10" fill="url(#g)"/></svg>"##,
        );
        let items = leaves(&s);
        assert!(
            items
                .iter()
                .any(|i| matches!(i, SceneItem::Fill(f) if f.rule == FillRule::EvenOdd))
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, SceneItem::Stroke(st) if st.cap == LineCap::Square))
        );
        assert!(items.iter().any(|i| matches!(
            i,
            SceneItem::Fill(Fill { paint: Paint::Linear(g), .. }) if g.spread == SpreadMode::Repeat
        )));
    }

    #[test]
    fn stroke_properties_and_dash() {
        let s = scene(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
                <path d="M0 0 Q5 10 10 0 C0 0 1 1 2 2 Z" fill="none" stroke="#00f" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="bevel" stroke-dasharray="3 1" stroke-dashoffset="1"/></svg>"##,
        );
        let items = leaves(&s);
        let stroke = items[0].as_stroke().expect("stroke expected");
        assert_eq!(stroke.width, 2.0);
        assert_eq!(stroke.cap, LineCap::Round);
        assert_eq!(stroke.join, LineJoin::Bevel);
        let dash = stroke.dash.as_ref().expect("dash");
        assert_eq!(dash.array, vec![3.0, 1.0]);
        assert_eq!(dash.phase, 1.0);
        assert!(
            stroke
                .path
                .verbs
                .iter()
                .any(|v| matches!(v, PathVerb::Quad(..)))
        );
        assert!(
            stroke
                .path
                .verbs
                .iter()
                .any(|v| matches!(v, PathVerb::Cubic(..)))
        );
        assert!(stroke.path.verbs.contains(&PathVerb::Close));
    }

    #[test]
    fn gradients_groups_and_clips() {
        let s = scene(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
              <defs>
                <linearGradient id="l" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="#000"/><stop offset="1" stop-color="#fff"/></linearGradient>
                <radialGradient id="r" cx="0.5" cy="0.5" r="0.5" spreadMethod="reflect"><stop offset="0" stop-color="#f00"/><stop offset="1" stop-color="#00f"/></radialGradient>
                <clipPath id="c"><rect width="5" height="5"/></clipPath>
              </defs>
              <g transform="translate(3 4)" opacity="0.5" clip-path="url(#c)">
                <rect width="10" height="10" fill="url(#l)"/>
                <rect width="10" height="10" fill="url(#r)"/>
              </g></svg>"##,
        );
        let g = s.items[0].as_group().expect("group expected");
        assert_eq!(g.opacity, 0.5);
        assert!(g.clip.is_some());
        assert_eq!(g.transform, Transform::translate(3.0, 4.0));
        let items = leaves(&s);
        assert!(items.iter().any(|i| matches!(
            i,
            SceneItem::Fill(Fill {
                paint: Paint::Linear(_),
                ..
            })
        )));
        assert!(items.iter().any(|i| matches!(
            i,
            SceneItem::Fill(Fill { paint: Paint::Radial(r), .. }) if r.spread == SpreadMode::Reflect
        )));
    }

    #[test]
    fn clip_path_children_keep_their_transforms() {
        // A transformed child stays a nested group inside usvg's clip path; an `<image>` child is
        // dropped by usvg and contributes no geometry.
        let s = scene(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="20" height="20">
              <defs><clipPath id="c">
                <rect transform="translate(3 4)" width="5" height="5"/>
                <image width="20" height="20" xlink:href="data:image/png;base64,AAAA"/>
              </clipPath></defs>
              <g clip-path="url(#c)"><rect width="20" height="20" fill="#000"/></g></svg>"##,
        );
        let group = s.items[0].as_group().expect("clipped group");
        let clip = group.clip.as_ref().expect("clip path");
        let b = clip.bounds().expect("clip bounds");
        assert_eq!((b.x, b.y, b.width, b.height), (3.0, 4.0, 5.0, 5.0));
    }

    #[test]
    fn text_becomes_filled_outlines() {
        let s = scene(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="60" height="20">
                <text x="2" y="15" font-size="12" fill="#000">Hi</text></svg>"##,
        );
        // Windows ships Segoe UI, the default family: usvg flattens the span into filled
        // outlines (one path per text span, not per letter).
        let fills: Vec<SceneItem> = leaves(&s)
            .into_iter()
            .filter(|item| matches!(item, SceneItem::Fill(f) if !f.path.is_empty()))
            .collect();
        assert!(!fills.is_empty(), "text must produce outlines");
        let outline = fills[0].as_fill().expect("fill expected");
        assert_eq!(outline.paint, Paint::Solid(Color::BLACK));
        let move_count = outline
            .path
            .verbs
            .iter()
            .filter(|verb| matches!(verb, crate::scene::PathVerb::Move(_)))
            .count();
        assert!(
            move_count >= 2,
            "'H' and 'i' are separate contours, got {move_count}"
        );
        let bounds = s.bounds().expect("text bounds");
        assert!(bounds.x >= 1.0 && bounds.right() < 30.0, "{bounds:?}");
        assert!(bounds.bottom() <= 16.0 && bounds.y > 4.0, "{bounds:?}");
    }

    #[test]
    fn invalid_svg_is_an_error() {
        let err = svg_to_scene(b"not svg").expect_err("invalid");
        assert!(err.to_string().starts_with("invalid SVG"));
    }
}
