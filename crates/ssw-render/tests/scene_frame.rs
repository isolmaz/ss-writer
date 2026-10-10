//! Typst frames converted to scenes: real documents compiled by the Typst world, the resulting
//! display list inspected item by item.

use std::sync::Arc;

use image::{ExtendedColorType, ImageEncoder};
use ssw_render::fonts::BUNDLED_FILES;
use ssw_render::frame_to_scene::frame_to_scene;
use ssw_render::scene::{
    Color, Dash, Fill, FillRule, FontRef, Glyphs, Group, Image, LineCap, LineJoin, LinearGradient,
    MIME_RGBA8, Paint, PathVerb, RadialGradient, Rect, Scene, SceneItem, Size, SpreadMode, Stroke,
    Transform,
};
use ssw_render::svg_to_scene::svg_to_scene;
use ssw_render::world::TypstWorld;

const PAGE: &str = "#set page(width: 200pt, height: 300pt, margin: 0pt)\n\
                    #set text(font: \"New Computer Modern\", size: 10pt)\n\
                    #show raw: set text(font: \"Cascadia Mono\")\n";

fn scene_in(world: &TypstWorld, source: &str) -> Scene {
    let document = world
        .compile(source)
        .unwrap_or_else(|e| panic!("{e}\n--- source ---\n{source}"));
    frame_to_scene(&document.pages()[0].frame)
}

fn scene_of(source: &str) -> Scene {
    scene_in(&TypstWorld::new(None), source)
}

/// Every item of the scene, groups before their children.
fn flatten(items: &[SceneItem]) -> Vec<&SceneItem> {
    let mut out = Vec::new();
    for item in items {
        out.push(item);
        if let SceneItem::Group(group) = item {
            out.extend(flatten(&group.items));
        }
    }
    out
}

fn fills(scene: &Scene) -> Vec<&Fill> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Fill(fill) => Some(fill),
            _ => None,
        })
        .collect()
}

fn strokes(scene: &Scene) -> Vec<&Stroke> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Stroke(stroke) => Some(stroke),
            _ => None,
        })
        .collect()
}

fn runs(scene: &Scene) -> Vec<&Glyphs> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Glyphs(run) => Some(run),
            _ => None,
        })
        .collect()
}

fn images(scene: &Scene) -> Vec<&Image> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Image(image) => Some(image),
            _ => None,
        })
        .collect()
}

fn groups(scene: &Scene) -> Vec<&Group> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Group(group) => Some(group),
            _ => None,
        })
        .collect()
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.05
}

fn assert_rect(actual: Rect, x: f32, y: f32, width: f32, height: f32) {
    assert!(
        close(actual.x, x)
            && close(actual.y, y)
            && close(actual.width, width)
            && close(actual.height, height),
        "{actual:?} != ({x}, {y}, {width}, {height})"
    );
}

/// Opaque color within 2 of `rgb` per channel.
fn near(color: Color, rgb: [u8; 3]) -> bool {
    color.a == 255
        && color.r.abs_diff(rgb[0]) <= 2
        && color.g.abs_diff(rgb[1]) <= 2
        && color.b.abs_diff(rgb[2]) <= 2
}

fn linear(paint: &Paint) -> &LinearGradient {
    match paint {
        Paint::Linear(gradient) => gradient,
        other => panic!("linear gradient expected, got {other:?}"),
    }
}

fn radial(paint: &Paint) -> &RadialGradient {
    match paint {
        Paint::Radial(gradient) => gradient,
        other => panic!("radial gradient expected, got {other:?}"),
    }
}

fn png(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .expect("png encoding");
    out
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let rgb = vec![200u8; (width * height * 3) as usize];
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .write_image(&rgb, width, height, ExtendedColorType::Rgb8)
        .expect("jpeg encoding");
    out
}

/// An APP1 segment announcing EXIF orientation 6 (rotate 90° clockwise).
const EXIF_ORIENTATION_6: [u8; 36] = [
    0xff, 0xe1, 0x00, 0x22, b'E', b'x', b'i', b'f', 0, 0, // marker, length, "Exif\0\0"
    b'M', b'M', 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, // TIFF header (big endian), IFD0 offset
    0x00, 0x01, // one entry
    0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, // Orientation = 6
    0x00, 0x00, 0x00, 0x00, // no next IFD
];

#[test]
fn text_becomes_glyph_runs_with_shared_font_bytes() {
    let source = "#set page(width: 100pt, height: 50pt, margin: 10pt)\n\
                  #set text(font: \"New Computer Modern\", size: 20pt, fill: rgb(\"#336699\"))\nHi";
    let scene = scene_of(source);
    assert_eq!(scene.size, Size::new(100.0, 50.0));
    assert!(scene.baseline.is_none());
    let all = runs(&scene);
    assert_eq!(all.len(), 1);
    let run = all[0];
    assert_eq!(run.font, FontRef(0));
    assert_eq!(run.size, 20.0);
    assert_eq!(run.paint, Paint::Solid(Color::rgb(0x33, 0x66, 0x99)));
    assert_eq!(run.glyphs.len(), 2);
    assert!(close(run.glyphs[0].x, 10.0), "{:?}", run.glyphs[0]);
    assert!(run.glyphs[1].x > run.glyphs[0].x + 5.0);
    assert!(run.glyphs[0].id != 0 && run.glyphs[1].id != 0);
    assert!(
        run.glyphs[0].y > 15.0 && run.glyphs[0].y < 40.0,
        "{:?}",
        run.glyphs[0]
    );
    assert!(run.width > 10.0 && run.width < 40.0, "{}", run.width);

    assert_eq!(scene.fonts.len(), 1);
    assert_eq!(scene.fonts[0].bytes(), BUNDLED_FILES[1]);
    assert_eq!(scene.fonts[0].index, 0);
    // The face is shared between scenes, not copied.
    let again = scene_of(source);
    assert!(Arc::ptr_eq(&scene.fonts[0], &again.fonts[0]));
}

#[test]
fn each_face_is_registered_once_per_scene() {
    let scene = scene_of(&format!(
        "{PAGE}Plain _italic_ *bold* `mono` plain again `mono again`"
    ));
    let faces: Vec<_> = runs(&scene).iter().map(|run| run.font).collect();
    assert!(faces.len() >= 6, "{faces:?}");
    assert_eq!(scene.fonts.len(), 4, "regular, italic, bold and monospace");
    for face in &faces {
        assert!(face.0 < scene.fonts.len());
    }
    // More runs than faces: runs of the same face share one entry.
    let distinct: std::collections::HashSet<_> = faces.iter().collect();
    assert_eq!(distinct.len(), 4);
    let bytes: Vec<&[u8]> = scene.fonts.iter().map(|f| f.bytes()).collect();
    for bundled in [
        BUNDLED_FILES[1],
        BUNDLED_FILES[2],
        BUNDLED_FILES[3],
        BUNDLED_FILES[4],
    ] {
        assert!(bytes.contains(&bundled));
    }
}

#[test]
fn zero_width_strokes_are_dropped_and_visible_ones_kept() {
    let hairless = scene_of(&format!("{PAGE}#line(length: 20pt, stroke: 0pt)"));
    assert!(strokes(&hairless).is_empty());
    let visible = scene_of(&format!("{PAGE}#line(length: 20pt, stroke: 1pt)"));
    assert_eq!(strokes(&visible).len(), 1);
}

#[test]
fn rectangles_become_fill_and_stroke() {
    let scene = scene_of(&format!(
        "{PAGE}#rect(width: 40pt, height: 20pt, fill: rgb(\"#ff0000\"), stroke: 2pt + rgb(\"#0000ff\"))"
    ));
    let filled = fills(&scene);
    assert_eq!(filled.len(), 1);
    assert_eq!(filled[0].paint, Paint::Solid(Color::rgb(255, 0, 0)));
    assert_eq!(filled[0].rule, FillRule::NonZero);
    assert_rect(
        filled[0].path.bounds().expect("bounds"),
        0.0,
        0.0,
        40.0,
        20.0,
    );

    let stroked = strokes(&scene);
    assert_eq!(stroked.len(), 1);
    let stroke = stroked[0];
    assert_eq!(stroke.paint, Paint::Solid(Color::rgb(0, 0, 255)));
    assert_eq!(stroke.width, 2.0);
    assert_eq!(stroke.cap, LineCap::Butt);
    assert_eq!(stroke.join, LineJoin::Miter);
    assert_eq!(stroke.miter_limit, 4.0);
    assert_eq!(stroke.dash, None);
    assert_rect(stroke.path.bounds().expect("bounds"), 0.0, 0.0, 40.0, 20.0);
    // The item order is fill, then stroke on top.
    let order: Vec<_> = scene
        .items
        .iter()
        .map(|i| matches!(i, SceneItem::Fill(_)))
        .collect();
    assert_eq!(order, [true, false]);
}

#[test]
fn stroke_styles_are_preserved() {
    let scene = scene_of(&format!(
        "{PAGE}#line(start: (0pt, 10pt), end: (30pt, 10pt), stroke: (paint: rgb(\"#00ff00\"), thickness: 3pt, dash: \"dashed\", cap: \"round\"))\n\
         #rect(width: 10pt, height: 10pt, stroke: (thickness: 1pt, join: \"round\", miter-limit: 2))\n\
         #rect(width: 10pt, height: 10pt, stroke: (thickness: 1pt, join: \"bevel\", cap: \"square\"))"
    ));
    let all = strokes(&scene);
    assert_eq!(all.len(), 3);

    let line = all[0];
    assert_eq!(line.paint, Paint::Solid(Color::rgb(0, 255, 0)));
    assert_eq!(line.width, 3.0);
    assert_eq!(line.cap, LineCap::Round);
    let dash: &Dash = line.dash.as_ref().expect("dashed");
    assert!(
        !dash.array.is_empty() && dash.array.iter().all(|d| *d > 0.0),
        "{dash:?}"
    );
    assert!(close(dash.phase, 0.0));
    // A line is one move and one line segment 30 pt long.
    assert_eq!(line.path.verbs.len(), 2);
    let bounds = line.path.bounds().expect("bounds");
    assert!(
        close(bounds.width, 30.0) && close(bounds.height, 0.0),
        "{bounds:?}"
    );

    assert_eq!(all[1].join, LineJoin::Round);
    assert_eq!(all[1].miter_limit, 2.0);
    assert_eq!(all[2].join, LineJoin::Bevel);
    assert_eq!(all[2].cap, LineCap::Square);
}

#[test]
fn curves_keep_their_geometry_and_fill_rule() {
    let scene = scene_of(&format!(
        "{PAGE}#circle(radius: 10pt, fill: rgb(\"#ff0000\"))\n\
         #curve(fill: rgb(\"#000000\"), fill-rule: \"even-odd\", curve.move((0pt, 0pt)), curve.line((20pt, 0pt)), curve.line((20pt, 20pt)), curve.close())"
    ));
    let all = fills(&scene);
    assert_eq!(all.len(), 2);
    let circle = all[0];
    assert!(
        circle
            .path
            .verbs
            .iter()
            .any(|v| matches!(v, PathVerb::Cubic(..)))
    );
    let bounds = circle.path.bounds().expect("bounds");
    assert!(
        (bounds.width - 20.0).abs() < 0.3 && (bounds.height - 20.0).abs() < 0.3,
        "{bounds:?}"
    );
    assert_eq!(circle.rule, FillRule::NonZero);

    let triangle = all[1];
    assert_eq!(triangle.rule, FillRule::EvenOdd);
    assert!(matches!(triangle.path.verbs.first(), Some(PathVerb::Move(p)) if close(p.x, 0.0)));
    assert!(
        triangle
            .path
            .verbs
            .iter()
            .any(|v| matches!(v, PathVerb::Close))
    );
    assert_eq!(
        triangle
            .path
            .verbs
            .iter()
            .filter(|v| matches!(v, PathVerb::Line(_)))
            .count(),
        2
    );
    let corners = triangle.path.bounds().expect("bounds");
    assert!(
        close(corners.width, 20.0) && close(corners.height, 20.0),
        "{corners:?}"
    );
}

#[test]
fn clips_and_transforms_become_groups() {
    let clipped = scene_of(&format!(
        "{PAGE}#box(width: 30pt, height: 20pt, clip: true)[#rect(width: 60pt, height: 10pt, fill: red)]"
    ));
    let all = groups(&clipped);
    let group = all.iter().find(|g| g.clip.is_some()).expect("clip group");
    assert_eq!(group.opacity, 1.0);
    assert_rect(
        group.clip.as_ref().and_then(|c| c.bounds()).expect("clip"),
        0.0,
        0.0,
        30.0,
        20.0,
    );
    assert_eq!(fills(&clipped).len(), 1);

    let rotated = scene_of(&format!(
        "{PAGE}#rotate(90deg)[#rect(width: 20pt, height: 10pt, fill: blue)]"
    ));
    let turned = groups(&rotated)
        .into_iter()
        .find(|g| !g.transform.is_identity())
        .expect("rotated group");
    assert!(turned.transform.sx.abs() < 1e-3 && turned.transform.sy.abs() < 1e-3);
    assert!(close(turned.transform.ky.abs(), 1.0) && close(turned.transform.kx.abs(), 1.0));
    let b = rotated.bounds().expect("bounds");
    assert!(close(b.width, 10.0) && close(b.height, 20.0), "{b:?}");

    let scaled = scene_of(&format!(
        "{PAGE}#scale(x: 200%, y: 50%)[#rect(width: 20pt, height: 20pt, fill: red)]"
    ));
    let stretched = groups(&scaled)
        .into_iter()
        .find(|g| !g.transform.is_identity())
        .expect("scaled group");
    assert!(close(stretched.transform.sx, 2.0) && close(stretched.transform.sy, 0.5));
    let b = scaled.bounds().expect("bounds");
    assert!(close(b.width, 40.0) && close(b.height, 10.0), "{b:?}");
}

#[test]
fn linear_gradients_run_along_their_angle() {
    let scene = scene_of(&format!(
        "{PAGE}#rect(width: 40pt, height: 20pt, fill: gradient.linear(rgb(\"#ff0000\"), rgb(\"#0000ff\")))\n\
         #rect(width: 40pt, height: 20pt, fill: gradient.linear(rgb(\"#ff0000\"), rgb(\"#0000ff\"), angle: 135deg))\n\
         #rect(width: 40pt, height: 20pt, fill: gradient.linear(rgb(\"#ff0000\"), rgb(\"#0000ff\"), angle: 315deg))"
    ));
    let all = fills(&scene);
    assert_eq!(all.len(), 3);

    let east = linear(&all[0].paint);
    assert!(
        close(east.start.x, 0.0) && close(east.start.y, 0.0),
        "{east:?}"
    );
    assert!(
        close(east.end.x, 40.0) && close(east.end.y, 0.0),
        "{east:?}"
    );
    assert_eq!(east.spread, SpreadMode::Pad);
    assert_eq!(east.transform, Transform::IDENTITY);
    assert_eq!(east.stops.len(), 2);
    // Stops are converted from Typst's Oklab interpolation space back to sRGB (rounding may be
    // off by one).
    assert!(
        near(east.stops[0].color, [255, 0, 0]),
        "{:?}",
        east.stops[0].color
    );
    assert!(
        near(east.stops[1].color, [0, 0, 255]),
        "{:?}",
        east.stops[1].color
    );
    assert!(close(east.stops[0].offset, 0.0) && close(east.stops[1].offset, 1.0));

    // 135°: towards the lower left, starting at the upper right corner of the item. The second
    // rectangle sits below the first one.
    let bounds = all[1].path.bounds().expect("bounds");
    let southwest = linear(&all[1].paint);
    assert!(
        close(southwest.start.x, bounds.right()) && close(southwest.start.y, bounds.y),
        "{southwest:?}"
    );
    assert!(southwest.end.x < southwest.start.x && southwest.end.y > southwest.start.y);

    // 315°: towards the upper right, starting at the lower left corner.
    let bounds = all[2].path.bounds().expect("bounds");
    let northeast = linear(&all[2].paint);
    assert!(
        close(northeast.start.x, bounds.x) && close(northeast.start.y, bounds.bottom()),
        "{northeast:?}"
    );
    assert!(northeast.end.x > northeast.start.x && northeast.end.y < northeast.start.y);
}

#[test]
fn radial_conic_and_tiling_paints() {
    let scene = scene_of(&format!(
        "{PAGE}#rect(width: 40pt, height: 20pt, fill: gradient.radial(rgb(\"#ff0000\"), rgb(\"#0000ff\")))\n\
         #rect(width: 40pt, height: 20pt, fill: gradient.conic(rgb(\"#ff0000\"), rgb(\"#0000ff\")))\n\
         #rect(width: 40pt, height: 20pt, fill: tiling(size: (10pt, 10pt))[#circle(radius: 3pt)])\n\
         #text(fill: gradient.linear(rgb(\"#ff0000\"), rgb(\"#0000ff\")))[Hello]"
    ));
    let all = fills(&scene);
    assert_eq!(all.len(), 3);

    let bounds = all[0].path.bounds().expect("bounds");
    let radial = radial(&all[0].paint);
    assert!(close(radial.center.x, 0.5) && close(radial.center.y, 0.5));
    assert!(close(radial.radius, 0.5));
    assert!(close(radial.focal.x, 0.5) && close(radial.focal_radius, 0.0));
    assert_eq!(
        radial.transform,
        Transform::new(bounds.width, 0.0, 0.0, bounds.height, bounds.x, bounds.y)
    );
    assert_eq!(radial.stops.len(), 2);

    // Conic gradients are approximated by one stop color.
    match &all[1].paint {
        Paint::Solid(color) => {
            assert!(
                near(*color, [255, 0, 0]) || near(*color, [0, 0, 255]),
                "{color:?}"
            );
        }
        other => panic!("solid fallback expected, got {other:?}"),
    }
    // Tilings are approximated by gray.
    assert_eq!(all[2].paint, Paint::Solid(Color::rgb(128, 128, 128)));

    // Text keeps its gradient.
    let text = runs(&scene);
    assert_eq!(text.len(), 1);
    assert_eq!(linear(&text[0].paint).stops.len(), 2);
}

#[test]
fn raster_images_keep_their_encoded_bytes() {
    let world = TypstWorld::new(None);
    let pixels = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
    ];
    let file = png(2, 2, &pixels);
    world
        .set_virtual_file("pic.png", file.clone())
        .expect("path");
    let scene = scene_in(
        &world,
        &format!("{PAGE}#image(\"pic.png\", width: 20pt, height: 10pt, fit: \"stretch\")"),
    );
    let all = images(&scene);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].data.mime, "image/png");
    assert_eq!((all[0].data.width, all[0].data.height), (2, 2));
    assert_eq!(all[0].data.bytes, file);
    assert_rect(all[0].rect, 0.0, 0.0, 20.0, 10.0);
}

#[test]
fn raw_pixels_become_rgba_buffers() {
    let scene = scene_of(&format!(
        "{PAGE}#image(bytes((255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0)), format: (encoding: \"rgb8\", width: 2, height: 2), width: 20pt, height: 20pt)"
    ));
    let all = images(&scene);
    assert_eq!(all.len(), 1);
    let data = &all[0].data;
    assert_eq!(data.mime, MIME_RGBA8);
    assert_eq!((data.width, data.height), (2, 2));
    assert_eq!(
        data.bytes,
        [
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255
        ]
    );
}

#[test]
fn rotated_jpegs_are_decoded_but_plain_ones_are_passed_through() {
    let world = TypstWorld::new(None);
    let plain = jpeg(8, 4);
    let mut rotated = vec![0xff, 0xd8];
    rotated.extend_from_slice(&EXIF_ORIENTATION_6);
    rotated.extend_from_slice(&plain[2..]);
    world
        .set_virtual_file("plain.jpg", plain.clone())
        .expect("path");
    world
        .set_virtual_file("rotated.jpg", rotated)
        .expect("path");
    let scene = scene_in(
        &world,
        &format!(
            "{PAGE}#image(\"plain.jpg\", width: 16pt, height: 8pt, fit: \"stretch\")\n\
             #image(\"rotated.jpg\", width: 8pt, height: 16pt, fit: \"stretch\")"
        ),
    );
    let all = images(&scene);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].data.mime, "image/jpeg");
    assert_eq!((all[0].data.width, all[0].data.height), (8, 4));
    assert_eq!(all[0].data.bytes, plain);
    // The EXIF rotation is already applied to the pixels, so the file itself cannot be drawn.
    assert_eq!(all[1].data.mime, MIME_RGBA8);
    assert_eq!((all[1].data.width, all[1].data.height), (4, 8));
    assert_eq!(all[1].data.bytes.len(), 4 * 8 * 4);
}

#[test]
fn svg_images_become_nested_scenes() {
    let world = TypstWorld::new(None);
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"10\" viewBox=\"0 0 20 10\">\
               <rect width=\"20\" height=\"10\" fill=\"#ff0000\"/></svg>";
    world
        .set_virtual_file("shape.svg", svg.as_bytes().to_vec())
        .expect("path");
    let scene = scene_in(
        &world,
        &format!("{PAGE}#image(\"shape.svg\", width: 40pt, height: 20pt, fit: \"stretch\")"),
    );
    let placed = groups(&scene)
        .into_iter()
        .find(|g| close(g.transform.sx, 2.0))
        .expect("scaled svg group");
    assert!(close(placed.transform.sy, 2.0));
    assert!(close(placed.transform.tx, 0.0) && close(placed.transform.ty, 0.0));
    let bounds = scene.bounds().expect("bounds");
    assert_rect(bounds, 0.0, 0.0, 40.0, 20.0);
}

/// CRC-32 (the checksum of gzip files).
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A gzip file around `data`: one stored (uncompressed) deflate block, valid checksums.
fn gzip_stored(data: &[u8]) -> Vec<u8> {
    let len = u16::try_from(data.len()).expect("one stored block holds at most 65535 bytes");
    // Magic, deflate, no flags, no time, no extra flags, unknown OS.
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    // Final block, stored: LEN and its complement, then the bytes.
    out.push(1);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&u32::from(len).to_le_bytes());
    out
}

#[test]
fn compressed_svg_images_that_usvg_cannot_read_are_left_out() {
    let world = TypstWorld::new(None);
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"10\" viewBox=\"0 0 20 10\">\
               <rect width=\"20\" height=\"10\" fill=\"#ff0000\"/></svg>";
    let packed = gzip_stored(svg.as_bytes());
    world
        .set_virtual_file("plain.svg", svg.as_bytes().to_vec())
        .expect("path");
    world
        .set_virtual_file("packed.svg", packed.clone())
        .expect("path");
    let page = |name: &str| {
        format!("{PAGE}#image(\"{name}\", width: 40pt, height: 20pt, fit: \"stretch\")\nafter")
    };

    let plain = scene_in(&world, &page("plain.svg"));
    assert_eq!(fills(&plain).len(), 1, "the picture's rectangle");

    // Typst unpacks compressed SVG itself, this crate's usvg is built without that feature: the
    // picture is dropped, the text after it is still converted.
    let skipped = scene_in(&world, &page("packed.svg"));
    assert!(fills(&skipped).is_empty());
    assert_eq!(runs(&skipped).len(), 1);
    assert_eq!(
        svg_to_scene(&packed)
            .expect_err("compressed SVG is not supported")
            .to_string(),
        "invalid SVG: enable svgz cargo feature to decode SVGZ data"
    );
}

#[test]
fn pdf_images_cannot_be_drawn_and_are_skipped() {
    let world = TypstWorld::new(None);
    let inner = world.compile("inner page").expect("compiles");
    let pdf = typst_pdf::pdf(&inner, &typst_pdf::PdfOptions::default()).expect("pdf");
    world.set_virtual_file("inner.pdf", pdf).expect("path");
    let scene = scene_in(
        &world,
        &format!("{PAGE}#image(\"inner.pdf\", width: 30pt)\nafter"),
    );
    assert!(images(&scene).is_empty());
    assert_eq!(
        runs(&scene).len(),
        1,
        "the text after the image is still converted"
    );
}

#[test]
fn links_and_tags_do_not_produce_items() {
    let scene = scene_of(&format!(
        "{PAGE}#link(\"https://example.com\")[Hello]\n#heading[Title] <anchor>"
    ));
    let texts = runs(&scene);
    assert_eq!(texts.len(), 2);
    assert!(fills(&scene).is_empty() && strokes(&scene).is_empty());
}

#[test]
fn baseline_marker_gives_the_baseline_and_is_not_drawn() {
    let scene = scene_of(&format!(
        "{PAGE}#v(7pt)\n#box(width: 0pt, text(fill: rgb(\"#01020304\"))[x])Hello"
    ));
    let all = runs(&scene);
    assert_eq!(all.len(), 1, "only the visible text is converted");
    assert_eq!(all[0].glyphs.len(), 5);
    let baseline = scene.baseline.expect("baseline");
    assert!(
        close(baseline, all[0].glyphs[0].y),
        "{baseline} vs {:?}",
        all[0].glyphs[0]
    );
    assert!(baseline > 7.0);
}

#[test]
fn baseline_inside_groups_is_measured_from_the_top_of_the_scene() {
    let scene = scene_of(&format!(
        "{PAGE}#v(7pt)\n#box(width: 80pt, height: 30pt, clip: true)[#v(3pt)#box(width: 0pt, text(fill: rgb(\"#01020304\"))[x])Hello]"
    ));
    let group = groups(&scene)
        .into_iter()
        .find(|g| g.clip.is_some())
        .expect("clip group");
    let run = runs(&scene)[0];
    let baseline = scene.baseline.expect("baseline");
    let expected = group.transform.ty + run.glyphs[0].y;
    assert!(close(baseline, expected), "{baseline} vs {expected}");
    assert!(group.transform.ty > 7.0 - 0.01);
}

#[test]
fn math_scripts_are_raised_and_smaller() {
    // The superscript sits above the baseline of the base letter: smaller scene y.
    let scene = scene_of(&format!("{PAGE}$x^2$"));
    let all = runs(&scene);
    assert!(all.len() >= 2, "base and exponent are separate runs");
    let base = all[0].glyphs[0].y;
    let exponent = all[1].glyphs[0].y;
    assert!(exponent < base - 1.0, "exponent {exponent} base {base}");
    assert!(all[1].size < all[0].size, "the exponent is set smaller");
}
