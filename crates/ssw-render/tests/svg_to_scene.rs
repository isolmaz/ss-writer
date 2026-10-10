//! `svg_to_scene` on SVG documents exercising images, nested clips, paint order and the
//! documented fallbacks.

use image::{ExtendedColorType, ImageEncoder};
use ssw_render::scene::{Color, Group, LineCap, LineJoin, Paint, Rect, Scene, SceneItem};
use ssw_render::svg_to_scene::svg_to_scene;

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn scene(svg: &str) -> Scene {
    svg_to_scene(svg.as_bytes()).expect("valid svg")
}

fn leaves(items: &[SceneItem], out: &mut Vec<SceneItem>) {
    for item in items {
        match item {
            SceneItem::Group(group) => leaves(&group.items, out),
            other => out.push(other.clone()),
        }
    }
}

fn all_leaves(scene: &Scene) -> Vec<SceneItem> {
    let mut out = Vec::new();
    leaves(&scene.items, &mut out);
    out
}

fn image_svg(mime: &str, bytes: &[u8]) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="30"><image x="2" y="3" width="10" height="5" href="data:{mime};base64,{}"/></svg>"#,
        base64(bytes)
    )
}

fn assert_rect(rect: Rect, x: f32, y: f32, w: f32, h: f32) {
    let close = |a: f32, b: f32| (a - b).abs() < 1e-3;
    assert!(
        close(rect.x, x) && close(rect.y, y) && close(rect.width, w) && close(rect.height, h),
        "{rect:?} != ({x}, {y}, {w}, {h})"
    );
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![200u8; (width * height * 4) as usize];
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&pixels, width, height, ExtendedColorType::Rgba8)
        .expect("png");
    out
}

fn jpeg(width: u32, height: u32) -> Vec<u8> {
    let pixels = vec![120u8; (width * height * 3) as usize];
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut out)
        .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
        .expect("jpeg");
    out
}

fn gif_header(width: u16, height: u16) -> Vec<u8> {
    let mut out = b"GIF89a".to_vec();
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&[0; 8]);
    out
}

fn webp_header(width: u32, height: u32) -> Vec<u8> {
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(b"WEBPVP8X");
    out.extend_from_slice(&[0; 8]);
    out.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
    out.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
    out
}

#[test]
fn embedded_rasters_keep_their_encoding_and_are_placed() {
    let cases: [(&str, Vec<u8>, (u32, u32)); 4] = [
        ("image/png", png(4, 2), (4, 2)),
        ("image/jpeg", jpeg(8, 4), (8, 4)),
        ("image/gif", gif_header(6, 3), (6, 3)),
        ("image/webp", webp_header(10, 5), (10, 5)),
    ];
    for (mime, bytes, (width, height)) in cases {
        let s = scene(&image_svg(mime, &bytes));
        let images: Vec<_> = all_leaves(&s)
            .into_iter()
            .filter_map(|item| match item {
                SceneItem::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1, "{mime}");
        let image = &images[0];
        assert_eq!(image.data.mime, mime);
        assert_eq!(image.data.bytes, bytes, "{mime}: payload must stay encoded");
        assert_eq!(
            (image.data.width, image.data.height),
            (width, height),
            "{mime}"
        );
        assert_rect(image.rect, 0.0, 0.0, width as f32, height as f32);
        // The group transforms map the image onto its 10 x 5 box at (2, 3).
        assert_rect(s.bounds().expect("bounds"), 2.0, 3.0, 10.0, 5.0);
    }
}

#[test]
fn hidden_images_and_unreadable_references_are_dropped() {
    let hidden =
        image_svg("image/png", &png(2, 2)).replace("<image ", "<image visibility=\"hidden\" ");
    assert!(all_leaves(&scene(&hidden)).is_empty());
    // A file path is never read: only data URLs resolve.
    let file = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><image width="5" height="5" href="Cargo.toml"/></svg>"#;
    assert!(all_leaves(&scene(file)).is_empty());
}

#[test]
fn nested_svg_image_becomes_a_clipped_group() {
    let inner = br##"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="#0000ff"/></svg>"##;
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20"><image width="8" height="8" href="data:image/svg+xml;base64,{}"/></svg>"#,
        base64(inner)
    );
    let s = scene(&svg);
    let fills = all_leaves(&s);
    assert_eq!(fills.len(), 1);
    let SceneItem::Fill(fill) = &fills[0] else {
        panic!("fill expected, got {fills:?}");
    };
    assert_eq!(fill.paint, Paint::Solid(Color::rgb(0, 0, 255)));
    assert_rect(s.bounds().expect("bounds"), 0.0, 0.0, 8.0, 8.0);

    fn has_clipped_group(items: &[SceneItem]) -> bool {
        items.iter().any(|item| match item {
            SceneItem::Group(Group { clip, items, .. }) => {
                clip.is_some() || has_clipped_group(items)
            }
            _ => false,
        })
    }
    assert!(has_clipped_group(&s.items));
}

#[test]
fn clip_paths_can_be_nested() {
    let s = scene(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
            <defs>
              <clipPath id="outer"><rect width="8" height="8"/></clipPath>
              <clipPath id="inner" clip-path="url(#outer)"><rect width="4" height="20"/></clipPath>
            </defs>
            <rect width="20" height="20" fill="#000" clip-path="url(#inner)"/></svg>"##,
    );
    fn find(items: &[SceneItem]) -> Option<(&Group, &Group)> {
        for item in items {
            if let SceneItem::Group(group) = item {
                if group.clip.is_some()
                    && let Some(SceneItem::Group(next)) = group.items.first()
                    && next.clip.is_some()
                {
                    return Some((group, next));
                }
                if let Some(found) = find(&group.items) {
                    return Some(found);
                }
            }
        }
        None
    }
    let (first, second) = find(&s.items).expect("two nested clips");
    let first_clip = first.clip.as_ref().expect("clip").bounds().expect("bounds");
    let second_clip = second
        .clip
        .as_ref()
        .expect("clip")
        .bounds()
        .expect("bounds");
    assert_rect(first_clip, 0.0, 0.0, 4.0, 20.0);
    assert_rect(second_clip, 0.0, 0.0, 8.0, 8.0);
}

#[test]
fn paint_order_decides_which_comes_first() {
    let order = |attribute: &str| {
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
                <path d="M1 1H9V9H1Z" fill="#f00" stroke="#00f" stroke-width="2" {attribute}/></svg>"##
        );
        all_leaves(&scene(&svg))
            .iter()
            .map(|item| matches!(item, SceneItem::Fill(_)))
            .collect::<Vec<bool>>()
    };
    assert_eq!(order(""), vec![true, false]);
    assert_eq!(order("paint-order=\"stroke\""), vec![false, true]);
}

#[test]
fn patterns_are_drawn_gray_and_hidden_shapes_are_skipped() {
    let s = scene(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">
            <defs><pattern id="p" width="2" height="2" patternUnits="userSpaceOnUse"><rect width="1" height="1"/></pattern></defs>
            <rect width="8" height="8" fill="url(#p)"/>
            <rect width="4" height="4" fill="#f00" visibility="hidden"/></svg>"##,
    );
    let items = all_leaves(&s);
    assert_eq!(items.len(), 1);
    let SceneItem::Fill(fill) = &items[0] else {
        panic!("fill expected");
    };
    assert_eq!(fill.paint, Paint::Solid(Color::rgb(128, 128, 128)));
}

#[test]
fn stroke_ends_and_corners_keep_their_style() {
    let s = scene(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="10">
            <path d="M1 5H9" fill="none" stroke="#00f" stroke-width="2" stroke-linecap="square"/>
            <path d="M11 1H19V9" fill="none" stroke="#00f" stroke-width="2" stroke-linejoin="bevel"/></svg>"##,
    );
    let styles: Vec<(LineCap, LineJoin)> = all_leaves(&s)
        .iter()
        .filter_map(SceneItem::as_stroke)
        .map(|stroke| (stroke.cap, stroke.join))
        .collect();
    assert_eq!(
        styles,
        [
            (LineCap::Square, LineJoin::Miter),
            (LineCap::Butt, LineJoin::Bevel)
        ]
    );
}

#[test]
fn clip_geometry_includes_groups_and_text() {
    let s = scene(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
            <defs><clipPath id="c"><g transform="translate(10 0)"><rect width="5" height="5"/></g><text x="20" y="12" font-size="10">I</text></clipPath></defs>
            <rect width="40" height="20" fill="#000" clip-path="url(#c)"/></svg>"##,
    );
    fn first_clip(items: &[SceneItem]) -> Option<&ssw_render::scene::Path> {
        items.iter().find_map(|item| match item {
            SceneItem::Group(group) => group.clip.as_ref().or_else(|| first_clip(&group.items)),
            _ => None,
        })
    }
    let clip = first_clip(&s.items).expect("clip path");
    let bounds = clip.bounds().expect("bounds");
    assert!(bounds.x >= 9.99 && bounds.right() > 15.0, "{bounds:?}");
}
