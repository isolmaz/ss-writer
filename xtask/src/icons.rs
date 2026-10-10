//! `cargo xtask icons`: turns the SVG icon masters in `assets/icons/` into the committed
//! multi-size `.ico` files.
//!
//! The masters are square, shapes only (no `<text>`, so no font is involved) and are rendered
//! once per size, not scaled from one bitmap, so every size is anti-aliased for its own grid.
//! Sizes below 256 px are stored as 32-bit BMP images (what every Windows shell component and
//! old tool reads), the 256 px image as PNG (the only size the format reserves PNG for).
//! `crates/ssw-app/build.rs` embeds the generated files into `SSWriter.exe`; the build itself
//! never needs resvg, so the `.ico` files are committed.

use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

use crate::error::Error;
use crate::fsx;
use crate::workspace::Workspace;

/// Pixel sizes of every generated icon file, ascending.
pub const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

/// Images of this size and larger are stored as PNG, smaller ones as BMP.
const PNG_FROM_SIZE: u32 = 256;

/// The icon masters: `assets/icons/<stem>.svg` becomes `assets/icons/<stem>.ico`.
pub const MASTERS: [&str; 2] = ["app", "document"];

/// `cargo xtask icons`: regenerates `assets/icons/<stem>.ico` from `<stem>.svg` for every
/// master.
pub fn run(workspace: &Workspace) -> Result<(), Error> {
    let dir = workspace.path("assets/icons");
    for stem in MASTERS {
        let svg_path = dir.join(format!("{stem}.svg"));
        let svg = fsx::read_text(&svg_path)?;
        let ico = ico_from_svg(&svg)
            .map_err(|error| Error::Invalid(format!("{}: {error}", svg_path.display())))?;
        let ico_path = dir.join(format!("{stem}.ico"));
        fsx::write_file(&ico_path, &ico)?;
        println!(
            "wrote {} ({} bytes, sizes {SIZES:?})",
            ico_path.display(),
            ico.len()
        );
    }
    Ok(())
}

/// Rasterizes the SVG master `svg` at every size of [`SIZES`] and encodes the result as an
/// ICO file.
pub fn ico_from_svg(svg: &str) -> Result<Vec<u8>, Error> {
    let tree = parse_master(svg)?;
    let images = SIZES
        .iter()
        .map(|&size| rasterize(&tree, size))
        .collect::<Result<Vec<_>, _>>()?;
    encode_ico(&images)
}

/// Parses an SVG icon master; the drawing must be square.
pub fn parse_master(svg: &str) -> Result<usvg::Tree, Error> {
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default())
        .map_err(|error| Error::Invalid(format!("invalid SVG: {error}")))?;
    let size = tree.size();
    if size.width() != size.height() {
        return Err(Error::Invalid(format!(
            "an icon master must be square, not {}x{}",
            size.width(),
            size.height()
        )));
    }
    Ok(tree)
}

/// Renders `tree` scaled to a `size`×`size` pixmap (premultiplied RGBA, transparent background).
pub fn rasterize(tree: &usvg::Tree, size: u32) -> Result<Pixmap, Error> {
    let mut pixmap = Pixmap::new(size, size)
        .ok_or_else(|| Error::Invalid(format!("cannot allocate a {size}x{size} pixmap")))?;
    let scale = size as f32 / tree.size().width();
    resvg::render(
        tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Ok(pixmap)
}

/// The ICO width/height byte of a square image: the size itself, 0 for 256.
fn dimension_byte(image: &Pixmap) -> Result<u8, Error> {
    match (image.width(), image.height()) {
        (256, 256) => Ok(0),
        // Exclusive upper bound 256: the value fits the byte.
        (width, height) if width == height && (1..256).contains(&width) => Ok(width as u8),
        (width, height) => Err(Error::Invalid(format!(
            "an icon image must be square and at most 256 pixels, not {width}x{height}"
        ))),
    }
}

/// The BMP image of an ICO entry: a `BITMAPINFOHEADER` whose height counts the colour bitmap
/// plus the mask, the bottom-up 32-bit BGRA colour bitmap (straight alpha), and the bottom-up
/// 1-bit AND mask (1 = transparent) with rows padded to four bytes.
fn dib(image: &Pixmap) -> Vec<u8> {
    let width = image.width() as usize;
    let height = image.height() as usize;
    let mask_stride = width.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + width * height * 4 + mask_stride * height);
    out.extend_from_slice(&40_u32.to_le_bytes()); // biSize
    out.extend_from_slice(&image.width().to_le_bytes()); // biWidth
    out.extend_from_slice(&(image.height() * 2).to_le_bytes()); // biHeight: colour + mask
    out.extend_from_slice(&1_u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32_u16.to_le_bytes()); // biBitCount
    // biCompression (BI_RGB), biSizeImage, biXPelsPerMeter, biYPelsPerMeter, biClrUsed,
    // biClrImportant: all zero.
    out.extend_from_slice(&[0; 24]);
    for row in image.pixels().chunks_exact(width).rev() {
        for pixel in row {
            let color = pixel.demultiply();
            out.extend_from_slice(&[color.blue(), color.green(), color.red(), color.alpha()]);
        }
    }
    for row in image.pixels().chunks_exact(width).rev() {
        let mut mask = vec![0_u8; mask_stride];
        for (x, pixel) in row.iter().enumerate() {
            if pixel.alpha() == 0 {
                mask[x / 8] |= 0x80 >> (x % 8);
            }
        }
        out.extend_from_slice(&mask);
    }
    out
}

/// Encodes square images (each at most 256 px) as one ICO file: `ICONDIR`, one 16-byte
/// `ICONDIRENTRY` per image in the given order, then the image data (PNG from
/// [`PNG_FROM_SIZE`], BMP below).
pub fn encode_ico(images: &[Pixmap]) -> Result<Vec<u8>, Error> {
    let mut entries = Vec::with_capacity(images.len());
    for image in images {
        let dimension = dimension_byte(image)?;
        let data = if image.width() >= PNG_FROM_SIZE {
            image
                .encode_png()
                .map_err(|error| Error::Invalid(format!("cannot encode the PNG image: {error}")))?
        } else {
            dib(image)
        };
        entries.push((dimension, data));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]); // reserved, type 1 = icon
    // Icon files hold a handful of images, far below the 16-bit limit.
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * entries.len();
    for (dimension, data) in &entries {
        out.extend_from_slice(&[*dimension, *dimension, 0, 0]); // size, no palette, reserved
        out.extend_from_slice(&1_u16.to_le_bytes()); // planes
        out.extend_from_slice(&32_u16.to_le_bytes()); // bits per pixel
        // Icon files are a few hundred kilobytes at most, far below the 32-bit limit.
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &entries {
        out.extend_from_slice(data);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use resvg::tiny_skia::PremultipliedColorU8;

    use super::*;

    const PNG_SIGNATURE: [u8; 8] = *b"\x89PNG\r\n\x1a\n";

    /// A square single-colour drawing, `size` units wide.
    fn solid_svg(size: u32, color: &str) -> String {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}"><rect width="{size}" height="{size}" fill="{color}"/></svg>"#
        )
    }

    fn u16_at(bytes: &[u8], at: usize) -> u16 {
        u16::from_le_bytes([bytes[at], bytes[at + 1]])
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    fn master(stem: &str) -> String {
        let path = Workspace::locate()
            .unwrap()
            .path(&format!("assets/icons/{stem}.svg"));
        fsx::read_text(&path).unwrap()
    }

    fn render_master(stem: &str, size: u32) -> Pixmap {
        rasterize(&parse_master(&master(stem)).unwrap(), size).unwrap()
    }

    /// Straight-alpha `[r, g, b, a]` of a pixel.
    fn rgba(image: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let color = image.pixel(x, y).unwrap().demultiply();
        [color.red(), color.green(), color.blue(), color.alpha()]
    }

    fn close(actual: [u8; 4], expected: [u8; 4]) -> bool {
        actual.iter().zip(expected).all(|(a, e)| a.abs_diff(e) <= 2)
    }

    #[test]
    fn the_directory_lists_one_entry_per_size_and_the_data_follows_back_to_back() {
        let ico = ico_from_svg(&solid_svg(64, "#1874D3")).unwrap();
        assert_eq!(&ico[..4], [0, 0, 1, 0], "reserved word and type 1 (icon)");
        assert_eq!(usize::from(u16_at(&ico, 4)), SIZES.len());
        let mut next_offset = 6 + 16 * SIZES.len();
        for (index, size) in SIZES.iter().enumerate() {
            let entry = 6 + 16 * index;
            let expected_byte = if *size == 256 { 0 } else { *size as u8 };
            assert_eq!(ico[entry], expected_byte, "width of entry {index}");
            assert_eq!(ico[entry + 1], expected_byte, "height of entry {index}");
            assert_eq!(ico[entry + 2], 0, "no palette");
            assert_eq!(ico[entry + 3], 0, "reserved");
            assert_eq!(u16_at(&ico, entry + 4), 1, "planes");
            assert_eq!(u16_at(&ico, entry + 6), 32, "bits per pixel");
            let length = u32_at(&ico, entry + 8) as usize;
            let offset = u32_at(&ico, entry + 12) as usize;
            assert_eq!(
                offset, next_offset,
                "entry {index} starts where the last ended"
            );
            next_offset = offset + length;
            let data = &ico[offset..offset + length];
            if *size == 256 {
                assert_eq!(data[..8], PNG_SIGNATURE, "the 256 px image is a PNG");
            } else {
                assert_eq!(u32_at(data, 0), 40, "BITMAPINFOHEADER size");
                assert_eq!(u32_at(data, 4), *size, "biWidth");
                assert_eq!(u32_at(data, 8), size * 2, "biHeight counts colour and mask");
                assert_eq!(u16_at(data, 12), 1, "biPlanes");
                assert_eq!(u16_at(data, 14), 32, "biBitCount");
                assert_eq!(u32_at(data, 16), 0, "BI_RGB");
                let mask_stride = (*size as usize).div_ceil(32) * 4;
                let expected =
                    40 + (*size as usize) * (*size as usize) * 4 + mask_stride * (*size as usize);
                assert_eq!(length, expected, "size {size}: header + colour + mask");
            }
        }
        assert_eq!(next_offset, ico.len(), "no trailing bytes");
    }

    #[test]
    fn the_png_entry_decodes_to_the_rendered_pixels_and_the_bmp_entries_carry_them_too() {
        let ico = ico_from_svg(&solid_svg(64, "#1874D3")).unwrap();
        let last = 6 + 16 * (SIZES.len() - 1);
        let offset = u32_at(&ico, last + 12) as usize;
        let length = u32_at(&ico, last + 8) as usize;
        let png = Pixmap::decode_png(&ico[offset..offset + length]).unwrap();
        assert_eq!((png.width(), png.height()), (256, 256));
        assert_eq!(rgba(&png, 0, 0), [0x18, 0x74, 0xD3, 255]);
        assert_eq!(rgba(&png, 255, 255), [0x18, 0x74, 0xD3, 255]);

        // The 16 px BMP: first BGRA pixel of the (bottom-up) colour bitmap.
        let first = u32_at(&ico, 6 + 12) as usize;
        assert_eq!(ico[first + 40..first + 44], [0xD3, 0x74, 0x18, 255]);
    }

    #[test]
    fn dib_rows_are_bottom_up_bgra_with_a_transparency_mask() {
        let mut image = Pixmap::new(2, 2).unwrap();
        let pixels = image.pixels_mut();
        pixels[0] = PremultipliedColorU8::from_rgba(255, 0, 0, 255).unwrap(); // (0,0) red
        pixels[1] = PremultipliedColorU8::TRANSPARENT; // (1,0)
        pixels[2] = PremultipliedColorU8::from_rgba(0, 32, 0, 128).unwrap(); // (0,1) half green
        pixels[3] = PremultipliedColorU8::from_rgba(0, 0, 255, 255).unwrap(); // (1,1) blue
        let bytes = dib(&image);

        let mut expected = Vec::new();
        expected.extend_from_slice(&40_u32.to_le_bytes());
        expected.extend_from_slice(&2_u32.to_le_bytes());
        expected.extend_from_slice(&4_u32.to_le_bytes());
        expected.extend_from_slice(&1_u16.to_le_bytes());
        expected.extend_from_slice(&32_u16.to_le_bytes());
        expected.extend_from_slice(&[0; 24]);
        // Bottom row first, straight (demultiplied) alpha, BGRA order.
        expected.extend_from_slice(&[0, 64, 0, 128, 255, 0, 0, 255]);
        expected.extend_from_slice(&[0, 0, 255, 255, 0, 0, 0, 0]);
        // Mask rows (bottom-up): nothing transparent, then pixel 1 transparent (bit 0x40).
        expected.extend_from_slice(&[0, 0, 0, 0]);
        expected.extend_from_slice(&[0x40, 0, 0, 0]);
        assert_eq!(bytes, expected);
    }

    #[test]
    fn images_that_are_not_square_or_exceed_256_px_are_rejected() {
        let wide = Pixmap::new(32, 16).unwrap();
        let error = encode_ico(&[wide]).unwrap_err().to_string();
        assert!(error.contains("32x16"), "{error}");
        let huge = Pixmap::new(300, 300).unwrap();
        let error = encode_ico(&[huge]).unwrap_err().to_string();
        assert!(error.contains("300x300"), "{error}");
    }

    #[test]
    fn invalid_or_non_square_masters_and_empty_sizes_are_reported() {
        let error = ico_from_svg("this is not svg").unwrap_err().to_string();
        assert!(error.contains("invalid SVG"), "{error}");
        let wide = r#"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="16"><rect width="32" height="16"/></svg>"#;
        let error = ico_from_svg(wide).unwrap_err().to_string();
        assert!(
            error.contains("square") && error.contains("32x16"),
            "{error}"
        );
        let tree = parse_master(&solid_svg(8, "#000")).unwrap();
        let error = rasterize(&tree, 0).unwrap_err().to_string();
        assert!(error.contains("0x0"), "{error}");
    }

    #[test]
    fn the_app_icon_is_a_blue_page_with_a_white_s_and_a_nib_on_a_transparent_ground() {
        let icon = render_master("app", 256);
        assert_eq!(rgba(&icon, 0, 0)[3], 0, "the corner is transparent");
        assert_eq!(rgba(&icon, 255, 255)[3], 0, "the corner is transparent");
        let page = rgba(&icon, 60, 128);
        assert_eq!(page[3], 255, "the page is opaque");
        assert!(
            page[2] > 180 && page[0] < 60,
            "the page is accent blue: {page:?}"
        );
        let s_centre = rgba(&icon, 128, 143);
        assert!(
            s_centre.iter().all(|c| *c >= 250),
            "the S monogram is white at its centre: {s_centre:?}"
        );
        let flap = rgba(&icon, 180, 70);
        assert!(
            flap[0] >= 120,
            "the folded corner is lighter than the page: {flap:?}"
        );
        assert!(
            close(rgba(&icon, 220, 162), [0x0B, 0x3B, 0x78, 255]),
            "the nib grip sticks out of the page: {:?}",
            rgba(&icon, 220, 162)
        );
    }

    #[test]
    fn the_document_icon_is_a_light_page_with_the_markdown_mark_in_accent_blue() {
        let icon = render_master("document", 256);
        assert_eq!(rgba(&icon, 0, 0)[3], 0, "the corner is transparent");
        let page = rgba(&icon, 60, 100);
        assert!(
            page.iter().all(|c| *c >= 225),
            "the page is light: {page:?}"
        );
        assert!(
            rgba(&icon, 140, 130).iter().all(|c| *c >= 215),
            "between M and arrow"
        );
        assert!(
            close(rgba(&icon, 80, 150), [0x18, 0x74, 0xD3, 255]),
            "the M's left stem"
        );
        assert!(
            close(rgba(&icon, 158, 150), [0x18, 0x74, 0xD3, 255]),
            "the arrow shaft"
        );
    }

    #[test]
    fn the_marks_survive_at_16_px() {
        let document = render_master("document", 16);
        let stem = rgba(&document, 5, 9);
        assert!(
            stem[2] > stem[0] + 60,
            "the M's stem is still bluish at 16 px: {stem:?}"
        );
        let app = render_master("app", 16);
        assert_eq!(rgba(&app, 0, 0)[3], 0, "the corner is transparent");
        let page = rgba(&app, 4, 8);
        assert!(
            page[3] == 255 && page[2] > page[0] + 100,
            "blue page: {page:?}"
        );
    }

    /// A scratch workspace whose `assets/icons` holds `files`.
    fn scratch_workspace(files: &[(&str, String)]) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            fsx::write_file(&dir.path().join("assets").join("icons").join(name), text).unwrap();
        }
        let workspace = Workspace {
            root: dir.path().to_path_buf(),
        };
        (dir, workspace)
    }

    #[test]
    fn run_writes_one_ico_per_master_next_to_its_svg() {
        let (_dir, workspace) = scratch_workspace(&[
            ("app.svg", solid_svg(32, "#102030")),
            ("document.svg", solid_svg(32, "#405060")),
        ]);
        run(&workspace).unwrap();
        let app = fsx::read_bytes(&workspace.path("assets/icons/app.ico")).unwrap();
        let document = fsx::read_bytes(&workspace.path("assets/icons/document.ico")).unwrap();
        assert_eq!(app, ico_from_svg(&solid_svg(32, "#102030")).unwrap());
        assert_eq!(document, ico_from_svg(&solid_svg(32, "#405060")).unwrap());
        assert_ne!(app, document);
    }

    #[test]
    fn run_names_the_master_that_is_missing_or_broken() {
        let (_dir, workspace) = scratch_workspace(&[]);
        let error = run(&workspace).unwrap_err().to_string();
        assert!(error.contains("app.svg"), "{error}");
        let (_dir, workspace) = scratch_workspace(&[("app.svg", "<svg".to_owned())]);
        let error = run(&workspace).unwrap_err().to_string();
        assert!(
            error.contains("app.svg") && error.contains("invalid SVG"),
            "{error}"
        );
    }

    #[test]
    fn the_committed_icons_are_what_the_masters_render_to() {
        let workspace = Workspace::locate().unwrap();
        for stem in MASTERS {
            let committed =
                fsx::read_bytes(&workspace.path(&format!("assets/icons/{stem}.ico"))).unwrap();
            assert!(
                committed == ico_from_svg(&master(stem)).unwrap(),
                "assets/icons/{stem}.ico is stale: run `cargo xtask icons` and commit the result"
            );
        }
    }
}
