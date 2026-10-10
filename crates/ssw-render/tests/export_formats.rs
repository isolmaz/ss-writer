//! Paged export end to end: Markdown in, PDF / PNG / JPEG bytes and page scenes out.

use std::ops::RangeInclusive;
use std::path::PathBuf;

use image::{ExtendedColorType, GenericImageView, ImageEncoder};
use ssw_core::settings::PageSize;
use ssw_render::export::{ExportError, ExportOptions, Exporter, PageSelection};
use ssw_render::fonts::BUNDLED_FILES;
use ssw_render::scene::{Paint, Scene, SceneItem};
use ssw_render::typst_markup::NoDiagrams;

/// Three pages: two explicit page breaks.
const THREE_PAGES: &str = "# First page\n\nSome text with *emphasis* and a formula $x^2$.\n\n\
                           <!-- pagebreak -->\n\n## Second page\n\n- one\n- two\n\n\
                           <!-- pagebreak -->\n\n## Third page\n\n| a | b |\n|---|---|\n| 1 | 2 |\n";

const TWO_PAGES: &str = "first\n\n<!-- pagebreak -->\n\nsecond";

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ssw-render-export-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn png_file() -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &[200, 30, 30, 255].repeat(64),
            8,
            8,
            ExtendedColorType::Rgba8,
        )
        .expect("png encoding");
    out
}

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

fn count(scene: &Scene, wanted: fn(&SceneItem) -> bool) -> usize {
    flatten(&scene.items)
        .into_iter()
        .filter(|item| wanted(item))
        .count()
}

fn is_glyphs(item: &SceneItem) -> bool {
    matches!(item, SceneItem::Glyphs(_))
}

fn is_image(item: &SceneItem) -> bool {
    matches!(item, SceneItem::Image(_))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn layout_pages_returns_one_white_scene_per_page() {
    let exporter = Exporter::new(None);
    let pages = exporter
        .layout_pages(THREE_PAGES, &ExportOptions::default(), &NoDiagrams)
        .expect("layout");
    assert_eq!(pages.len(), 3);
    for scene in &pages {
        // A4 in points.
        assert!((scene.size.width - 595.28).abs() < 0.1, "{:?}", scene.size);
        assert!((scene.size.height - 841.89).abs() < 0.1, "{:?}", scene.size);
        // White paper first, then the content.
        match &scene.items[0] {
            SceneItem::Fill(paper) => {
                assert_eq!(paper.paint, Paint::Solid(ssw_render::scene::Color::WHITE));
                let bounds = paper.path.bounds().expect("paper bounds");
                assert_eq!((bounds.x, bounds.y), (0.0, 0.0));
                assert!((bounds.width - scene.size.width).abs() < 0.01);
                assert!((bounds.height - scene.size.height).abs() < 0.01);
            }
            other => panic!("paper expected first, got {other:?}"),
        }
        assert!(count(scene, is_glyphs) > 0);
        assert!(!scene.fonts.is_empty());
    }
    // The first page holds the formula's glyphs from the math font as well.
    assert!(
        pages[0]
            .fonts
            .iter()
            .any(|font| font.bytes() == BUNDLED_FILES[0])
    );

    let letter = ExportOptions {
        page: PageSize::Letter,
        ..ExportOptions::default()
    };
    let pages = exporter
        .layout_pages(TWO_PAGES, &letter, &NoDiagrams)
        .expect("layout");
    assert_eq!(pages.len(), 2);
    assert!(
        (pages[0].size.width - 612.0).abs() < 0.1 && (pages[0].size.height - 792.0).abs() < 0.1
    );
}

#[test]
fn pdf_export_is_a_valid_deterministic_pdf() {
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();
    let pdf = exporter
        .export_pdf(THREE_PAGES, &options, &NoDiagrams)
        .expect("pdf");
    assert!(pdf.starts_with(b"%PDF-"), "{:?}", &pdf[..8.min(pdf.len())]);
    assert!(contains(&pdf[pdf.len().saturating_sub(32)..], b"%%EOF"));
    assert!(contains(&pdf, b"/Count 3"), "three pages expected");
    assert!(pdf.len() > 3000);

    let two = exporter
        .export_pdf(TWO_PAGES, &options, &NoDiagrams)
        .expect("pdf");
    assert!(contains(&two, b"/Count 2"));
    assert!(!contains(&two, b"/Count 3"));

    // Same input, same bytes: from the same exporter and from a fresh one.
    assert_eq!(
        pdf,
        exporter
            .export_pdf(THREE_PAGES, &options, &NoDiagrams)
            .expect("pdf")
    );
    assert_eq!(
        pdf,
        Exporter::new(None)
            .export_pdf(THREE_PAGES, &options, &NoDiagrams)
            .expect("pdf")
    );
    assert_ne!(pdf, two);
}

#[test]
fn the_pdf_carries_the_title() {
    let exporter = Exporter::new(None);
    let options = ExportOptions {
        title: Some("Quarterly Report".to_owned()),
        ..ExportOptions::default()
    };
    let pdf = exporter
        .export_pdf("body", &options, &NoDiagrams)
        .expect("pdf");
    assert!(contains(&pdf, b"Quarterly Report"));
    let untitled = exporter
        .export_pdf("body", &ExportOptions::default(), &NoDiagrams)
        .expect("pdf");
    assert!(!contains(&untitled, b"Quarterly Report"));
}

#[test]
fn png_pages_have_the_size_of_the_page_at_the_resolution() {
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();
    let scenes = exporter
        .layout_pages(THREE_PAGES, &options, &NoDiagrams)
        .expect("layout");
    for dpi in [72.0f32, 96.0, 150.0, 300.0] {
        let pngs = exporter
            .export_png(THREE_PAGES, &options, &NoDiagrams, &PageSelection::All, dpi)
            .expect("png");
        assert_eq!(pngs.len(), 3, "{dpi} dpi");
        for (png, scene) in pngs.iter().zip(&scenes) {
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
            let decoded = image::load_from_memory(png).expect("decodes");
            let scale = f64::from(dpi) / 72.0;
            let expected = (
                (f64::from(scene.size.width) * scale).round() as u32,
                (f64::from(scene.size.height) * scale).round() as u32,
            );
            let (width, height) = decoded.dimensions();
            assert!(
                width.abs_diff(expected.0) <= 1 && height.abs_diff(expected.1) <= 1,
                "{dpi} dpi: {width}x{height}, expected about {expected:?}"
            );
        }
    }
    // 300 dpi A4 is the familiar 2480 x 3508.
    let big = exporter
        .export_png("x", &options, &NoDiagrams, &PageSelection::All, 300.0)
        .expect("png");
    let (width, height) = image::load_from_memory(&big[0])
        .expect("decodes")
        .dimensions();
    assert!(
        width.abs_diff(2480) <= 1 && height.abs_diff(3508) <= 1,
        "{width}x{height}"
    );
}

#[test]
fn png_pixels_are_white_paper_with_dark_text() {
    let exporter = Exporter::new(None);
    let pngs = exporter
        .export_png(
            "# Big heading\n\nbody text",
            &ExportOptions::default(),
            &NoDiagrams,
            &PageSelection::All,
            100.0,
        )
        .expect("png");
    let page = image::load_from_memory(&pngs[0])
        .expect("decodes")
        .to_rgba8();
    assert_eq!(
        page.get_pixel(0, 0).0,
        [255, 255, 255, 255],
        "page corner is paper"
    );
    let darkest = page.pixels().map(|p| p.0[0]).min().expect("pixels");
    assert!(
        darkest < 80,
        "text pixels are dark, darkest red channel {darkest}"
    );
    assert!(page.pixels().all(|p| p.0[3] == 255), "the page is opaque");
    let ink = page.pixels().filter(|p| p.0[0] < 128).count();
    assert!(ink > 200, "{ink} dark pixels");
}

fn dark_options() -> ExportOptions {
    ExportOptions {
        light_theme: false,
        ..ExportOptions::default()
    }
}

#[test]
fn the_dark_theme_paints_dark_pages_with_light_text() {
    let exporter = Exporter::new(None);
    let render = |options: &ExportOptions| {
        let pngs = exporter
            .export_png(
                "# Big heading\n\nbody text",
                options,
                &NoDiagrams,
                &PageSelection::All,
                100.0,
            )
            .expect("png");
        image::load_from_memory(&pngs[0])
            .expect("decodes")
            .to_rgba8()
    };
    let page = render(&dark_options());
    let surface = [0x27, 0x27, 0x27, 255];
    assert_eq!(
        page.get_pixel(0, 0).0,
        surface,
        "the corner is the dark surface"
    );
    assert_eq!(
        page.get_pixel(page.width() - 1, page.height() - 1).0,
        surface
    );
    assert!(page.pixels().all(|p| p.0[3] == 255), "the page is opaque");
    let brightest = page.pixels().map(|p| p.0[0]).max().expect("pixels");
    assert!(
        brightest > 200,
        "text pixels are light, brightest {brightest}"
    );
    let ink = page.pixels().filter(|p| p.0[0] > 150).count();
    assert!(ink > 200, "{ink} light pixels");

    // Black ink needs light paper: black text wins over the dark theme.
    let black = render(&ExportOptions {
        black_text: true,
        ..dark_options()
    });
    assert_eq!(black.get_pixel(0, 0).0, [255, 255, 255, 255]);
    assert_eq!(
        render(&ExportOptions::default()).get_pixel(0, 0).0,
        [255, 255, 255, 255]
    );
}

#[test]
fn dark_pages_are_dark_paper_in_scenes_jpegs_and_pdfs() {
    let exporter = Exporter::new(None);
    let scenes = exporter
        .layout_pages(TWO_PAGES, &dark_options(), &NoDiagrams)
        .expect("layout");
    assert_eq!(scenes.len(), 2);
    for scene in &scenes {
        match &scene.items[0] {
            SceneItem::Fill(paper) => assert_eq!(
                paper.paint,
                Paint::Solid(ssw_render::scene::Color::rgb(0x27, 0x27, 0x27))
            ),
            other => panic!("paper expected first, got {other:?}"),
        }
    }

    let jpegs = exporter
        .export_jpg(
            TWO_PAGES,
            &dark_options(),
            &NoDiagrams,
            &PageSelection::All,
            72.0,
            95,
        )
        .expect("jpeg");
    let page = image::load_from_memory(&jpegs[0])
        .expect("decodes")
        .to_rgb8();
    let [r, g, b] = page.get_pixel(0, 0).0;
    assert!(
        [r, g, b].iter().all(|c| c.abs_diff(0x27) <= 3),
        "{r} {g} {b}"
    );

    let dark = exporter
        .export_pdf(TWO_PAGES, &dark_options(), &NoDiagrams)
        .expect("pdf");
    let light = exporter
        .export_pdf(TWO_PAGES, &ExportOptions::default(), &NoDiagrams)
        .expect("pdf");
    assert!(dark.starts_with(b"%PDF"));
    assert_ne!(dark, light, "the page fill is part of the PDF");
}

#[test]
fn page_selection_picks_pages_and_rejects_missing_ones() {
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();
    let all = exporter
        .export_png(
            THREE_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::All,
            50.0,
        )
        .expect("png");
    let middle = exporter
        .export_png(
            THREE_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::Range(1..=1),
            50.0,
        )
        .expect("png");
    assert_eq!(middle.len(), 1);
    assert_eq!(middle[0], all[1]);
    let tail = exporter
        .export_png(
            THREE_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::Range(1..=2),
            50.0,
        )
        .expect("png");
    assert_eq!(tail, all[1..].to_vec());

    let err = exporter
        .export_png(
            THREE_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::Range(2..=3),
            50.0,
        )
        .expect_err("page 3 does not exist");
    assert_eq!(
        err,
        ExportError::PageRange {
            first: 2,
            last: 3,
            pages: 3
        }
    );
    let err = exporter
        .export_jpg(
            TWO_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::Range(RangeInclusive::new(1, 0)),
            50.0,
            80,
        )
        .expect_err("empty range");
    assert!(matches!(err, ExportError::PageRange { .. }));
}

#[test]
fn resolutions_outside_the_range_are_rejected() {
    let exporter = Exporter::new(None);
    for dpi in [0.0f32, 0.5, 600.5, 10_000.0, f32::NAN, f32::INFINITY, -72.0] {
        let png = exporter.export_png(
            "x",
            &ExportOptions::default(),
            &NoDiagrams,
            &PageSelection::All,
            dpi,
        );
        assert!(
            matches!(png, Err(ExportError::Resolution(_))),
            "{dpi}: {png:?}"
        );
        let jpg = exporter.export_jpg(
            "x",
            &ExportOptions::default(),
            &NoDiagrams,
            &PageSelection::All,
            dpi,
            90,
        );
        assert!(
            matches!(jpg, Err(ExportError::Resolution(_))),
            "{dpi}: {jpg:?}"
        );
    }
    // The lowest resolution is accepted and renders a tiny page; the upper bound is covered by
    // the unit tests because a 600 dpi A4 page has 35 megapixels.
    let one = exporter
        .export_png(
            "x",
            &ExportOptions::default(),
            &NoDiagrams,
            &PageSelection::All,
            1.0,
        )
        .expect("png at 1 dpi");
    let (width, height) = image::load_from_memory(&one[0])
        .expect("decodes")
        .dimensions();
    assert_eq!((width, height), (8, 12));
}

#[test]
fn jpeg_pages_are_decodable_and_quality_changes_the_size() {
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();
    let markdown = "# Photo-ish\n\nLorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n<!-- pagebreak -->\n\nsecond page";
    let sharp = exporter
        .export_jpg(
            markdown,
            &options,
            &NoDiagrams,
            &PageSelection::All,
            100.0,
            95,
        )
        .expect("jpeg");
    let blurry = exporter
        .export_jpg(
            markdown,
            &options,
            &NoDiagrams,
            &PageSelection::All,
            100.0,
            5,
        )
        .expect("jpeg");
    assert_eq!(sharp.len(), 2);
    for page in sharp.iter().chain(&blurry) {
        assert_eq!(&page[..3], &[0xff, 0xd8, 0xff]);
        let decoded = image::load_from_memory(page).expect("decodes");
        let (width, height) = decoded.dimensions();
        assert!(
            width.abs_diff(827) <= 1 && height.abs_diff(1169) <= 1,
            "{width}x{height}"
        );
        assert_eq!(
            decoded
                .to_rgb8()
                .get_pixel(2, 2)
                .0
                .iter()
                .filter(|c| **c > 245)
                .count(),
            3
        );
    }
    assert!(
        blurry[0].len() < sharp[0].len(),
        "{} vs {}",
        blurry[0].len(),
        sharp[0].len()
    );
    // Qualities outside 1-100 are clamped, not rejected.
    for quality in [0u8, 255] {
        assert!(
            exporter
                .export_jpg(
                    "x",
                    &options,
                    &NoDiagrams,
                    &PageSelection::All,
                    72.0,
                    quality
                )
                .is_ok()
        );
    }
    // Deterministic.
    let again = exporter
        .export_jpg(
            markdown,
            &options,
            &NoDiagrams,
            &PageSelection::All,
            100.0,
            95,
        )
        .expect("jpeg");
    assert_eq!(sharp, again);
}

#[test]
fn raster_export_is_deterministic() {
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();
    let first = exporter
        .export_png(
            THREE_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::All,
            60.0,
        )
        .expect("png");
    let second = Exporter::new(None)
        .export_png(
            THREE_PAGES,
            &options,
            &NoDiagrams,
            &PageSelection::All,
            60.0,
        )
        .expect("png");
    assert_eq!(first, second);
}

#[test]
fn images_are_read_from_the_document_directory_only() {
    let outer = temp_dir("images");
    let root = outer.join("doc");
    std::fs::create_dir_all(root.join("img")).expect("dirs");
    std::fs::write(root.join("img").join("in side.png"), png_file()).expect("write");
    std::fs::write(outer.join("secret.png"), png_file()).expect("write");
    std::fs::write(root.join("broken.png"), b"not an image at all").expect("write");

    let exporter = Exporter::new(Some(&root));
    let options = ExportOptions::default();
    let inside = exporter
        .layout_pages(
            "![shown](img/in%20side.png)\n\n![also](<./img/in side.png>)",
            &options,
            &NoDiagrams,
        )
        .expect("layout");
    assert_eq!(count(&inside[0], is_image), 2);

    let outside = exporter
        .layout_pages("![escape](../secret.png)", &options, &NoDiagrams)
        .expect("layout");
    assert_eq!(
        count(&outside[0], is_image),
        0,
        "files outside the directory are not read"
    );

    let missing = exporter
        .layout_pages("![nothing](nowhere.png)", &options, &NoDiagrams)
        .expect("layout");
    assert_eq!(count(&missing[0], is_image), 0);

    // A file that exists but is not an image makes Typst fail: reported, not swallowed.
    let err = exporter
        .layout_pages("![broken](broken.png)", &options, &NoDiagrams)
        .expect_err("undecodable image");
    assert!(matches!(err, ExportError::Layout(_)), "{err:?}");
    assert!(err.to_string().starts_with("layout failed:"), "{err}");
    // Every output reports the same layout failure instead of producing a partial file.
    let broken = "![broken](broken.png)";
    let pdf = exporter
        .export_pdf(broken, &options, &NoDiagrams)
        .expect_err("pdf of an undecodable image");
    let png = exporter
        .export_png(broken, &options, &NoDiagrams, &PageSelection::All, 72.0)
        .expect_err("png of an undecodable image");
    let jpg = exporter
        .export_jpg(broken, &options, &NoDiagrams, &PageSelection::All, 72.0, 80)
        .expect_err("jpeg of an undecodable image");
    for failure in [&pdf, &png, &jpg] {
        assert_eq!(failure, &err);
    }
    // The exporter keeps working.
    assert!(exporter.layout_pages("fine", &options, &NoDiagrams).is_ok());

    // Without a directory nothing is read at all.
    let sealed = Exporter::new(None);
    let pages = sealed
        .layout_pages("![shown](img/in%20side.png)", &options, &NoDiagrams)
        .expect("layout");
    assert_eq!(count(&pages[0], is_image), 0);
    std::fs::remove_dir_all(outer).expect("cleanup");
}

#[test]
fn diagrams_become_images_in_every_output() {
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"120\" height=\"60\" viewBox=\"0 0 120 60\">\
               <rect width=\"120\" height=\"60\" fill=\"#ddeeff\" stroke=\"#336699\"/></svg>";
    let drawn = |source: &str| source.starts_with("graph").then(|| svg.to_owned());
    let markdown = "before\n\n```mermaid\ngraph TD\n  A-->B\n```\n\nafter";
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();

    let with = exporter
        .layout_pages(markdown, &options, &drawn)
        .expect("layout");
    let without = exporter
        .layout_pages(markdown, &options, &NoDiagrams)
        .expect("layout");
    // The drawn diagram is a nested scene, not text: fewer glyph runs than the code block.
    assert!(count(&with[0], is_glyphs) < count(&without[0], is_glyphs));
    assert!(with[0].bounds().is_some());

    let typst = exporter.to_typst(markdown, &options, &drawn);
    assert_eq!(typst.files.len(), 1);
    assert_eq!(typst.files[0].0, "ssw-diagram-0.svg");
    assert!(typst.source.contains("image(\"ssw-diagram-0.svg\")"));
    assert!(
        exporter
            .to_typst(markdown, &options, &NoDiagrams)
            .files
            .is_empty()
    );

    let pdf = exporter
        .export_pdf(markdown, &options, &drawn)
        .expect("pdf");
    assert!(pdf.starts_with(b"%PDF-"));
    let png = exporter
        .export_png(markdown, &options, &drawn, &PageSelection::All, 50.0)
        .expect("png");
    let plain = exporter
        .export_png(markdown, &options, &NoDiagrams, &PageSelection::All, 50.0)
        .expect("png");
    assert_ne!(png, plain, "the diagram changes the page");
}

#[test]
fn concurrent_exports_do_not_mix_up_their_diagrams() {
    let exporter = Exporter::new(None);
    let options = ExportOptions::default();
    let red = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><rect width=\"100\" height=\"100\" fill=\"#ff0000\"/></svg>";
    let blue = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"100\" height=\"100\"><rect width=\"100\" height=\"100\" fill=\"#0000ff\"/></svg>";
    let markdown = "```mermaid\nd\n```";
    let red_draw = |_: &str| Some(red.to_owned());
    let blue_draw = |_: &str| Some(blue.to_owned());
    let expected_red = exporter
        .export_png(markdown, &options, &red_draw, &PageSelection::All, 40.0)
        .expect("png");
    let expected_blue = exporter
        .export_png(markdown, &options, &blue_draw, &PageSelection::All, 40.0)
        .expect("png");
    assert_ne!(expected_red, expected_blue);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..6)
            .map(|n| {
                let (exporter, options) = (&exporter, &options);
                let (red_draw, blue_draw) = (&red_draw, &blue_draw);
                scope.spawn(move || {
                    if n % 2 == 0 {
                        exporter.export_png(markdown, options, red_draw, &PageSelection::All, 40.0)
                    } else {
                        exporter.export_png(markdown, options, blue_draw, &PageSelection::All, 40.0)
                    }
                })
            })
            .collect();
        for (n, handle) in handles.into_iter().enumerate() {
            let png = handle.join().expect("thread").expect("png");
            assert_eq!(
                png,
                if n % 2 == 0 {
                    expected_red.clone()
                } else {
                    expected_blue.clone()
                },
                "thread {n}"
            );
        }
    });
}

#[test]
fn extra_fonts_can_be_added_to_the_exporter() {
    let dir = temp_dir("fonts");
    let file = dir.join("copy.ttf");
    std::fs::write(&file, BUNDLED_FILES[4]).expect("write font");
    let mut exporter = Exporter::new(None);
    assert_eq!(exporter.add_font_file(&file).expect("added"), 1);
    assert!(exporter.add_font_file(&dir.join("missing.ttf")).is_err());
    assert!(format!("{exporter:?}").contains("Exporter"));
    std::fs::remove_dir_all(dir).expect("cleanup");
}

#[test]
fn black_text_and_options_change_the_output() {
    let exporter = Exporter::new(None);
    let markdown = "[link](https://example.com)\n\n> [!NOTE]\n> colored";
    let colored = exporter
        .export_png(
            markdown,
            &ExportOptions::default(),
            &NoDiagrams,
            &PageSelection::All,
            60.0,
        )
        .expect("png");
    let black = exporter
        .export_png(
            markdown,
            &ExportOptions {
                black_text: true,
                ..ExportOptions::default()
            },
            &NoDiagrams,
            &PageSelection::All,
            60.0,
        )
        .expect("png");
    assert_ne!(colored, black);
    let blue_pixels = |png: &[u8]| {
        image::load_from_memory(png)
            .expect("decodes")
            .to_rgba8()
            .pixels()
            .filter(|p| {
                u16::from(p.0[2]) > u16::from(p.0[0]) + 60
                    && p.0[2] > 150
                    && p.0[3] == 255
                    && p.0[0] < 120
            })
            .count()
    };
    assert!(
        blue_pixels(&colored[0]) > 20,
        "the link and callout are blue"
    );
    assert_eq!(blue_pixels(&black[0]), 0, "black text has no accent color");
}

#[test]
fn from_settings_options_export() {
    let settings = ssw_core::settings::ExportSettings {
        page_size: PageSize::Letter,
        margin_mm: 25.0,
        base_font_size_pt: 12.0,
        page_numbers: false,
        ..ssw_core::settings::ExportSettings::default()
    };
    let options = ExportOptions::from_settings(&settings, Some("From settings".to_owned()));
    let pages = Exporter::new(None)
        .layout_pages("hello", &options, &NoDiagrams)
        .expect("layout");
    assert!((pages[0].size.width - 612.0).abs() < 0.1);
}
