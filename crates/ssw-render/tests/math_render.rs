//! `render_math`: LaTeX formulas through MiTeX and Typst into scenes.

use ssw_render::fonts::BUNDLED_FILES;
use ssw_render::math::{MathError, render_math};
use ssw_render::scene::{Color, Glyphs, Paint, Scene, SceneItem, Stroke};

/// Formulas MiTeX and the vendored command specification handle (spike C, section 2).
const CORPUS: &[(&str, &str)] = &[
    ("frac", r"\frac{a+b}{c}"),
    ("sum", r"\sum_{i=1}^n i^2 = \frac{n(n+1)(2n+1)}{6}"),
    ("int", r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}"),
    ("pmatrix", r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}"),
    ("sqrt3", r"\sqrt[3]{x}"),
    ("mathbbR", r"\mathbb{R}"),
    (
        "aligned",
        r"\begin{aligned} a &= b + c \\ d &= e \end{aligned}",
    ),
    ("text", r"x = 1 \text{ if } y > 0"),
    (
        "cases",
        r"f(x)=\begin{cases} 1 & x>0 \\ 0 & \text{else} \end{cases}",
    ),
    ("limits", r"\lim_{x\to 0}\frac{\sin x}{x}=1"),
    ("greek", r"\alpha+\beta=\gamma,\ \Omega\cdot\nabla\varphi"),
    (
        "overset",
        r"\overbrace{a+b}^{n}\ \hat{x}\ \vec{v}\ \overline{AB}",
    ),
    (
        "bmatrix",
        r"\begin{bmatrix} 1 & 0 \\ 0 & 1 \end{bmatrix}\mathbf{x}",
    ),
    ("binom", r"\left( \frac{1}{2} \right)^{\!2}\ \binom{n}{k}"),
    (
        "mathcal",
        r"\mathcal{L}\mathfrak{g}\mathrm{d}x\ \operatorname{tr}A",
    ),
    ("boxed", r"\boxed{E=mc^2}\ \underbrace{x}_{y}"),
    ("array", r"\begin{array}{cc} 1 & 2 \\ 3 & 4 \end{array}"),
];

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

fn runs(scene: &Scene) -> Vec<&Glyphs> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Glyphs(run) => Some(run),
            _ => None,
        })
        .collect()
}

/// Rules (fraction bars, radical overlines) are stroked lines.
fn rules(scene: &Scene) -> Vec<&Stroke> {
    flatten(&scene.items)
        .into_iter()
        .filter_map(|item| match item {
            SceneItem::Stroke(stroke) => Some(stroke),
            _ => None,
        })
        .collect()
}

fn render(latex: &str, display: bool, size: f32) -> Scene {
    render_math(latex, display, size, Color::BLACK)
        .unwrap_or_else(|e| panic!("{latex} ({display}): {e}"))
}

#[test]
fn the_corpus_renders_inline_and_as_display() {
    for (name, latex) in CORPUS {
        for display in [false, true] {
            let scene = render(latex, display, 16.0);
            assert!(
                scene.size.width > 3.0 && scene.size.height > 3.0,
                "{name} {display}: {:?}",
                scene.size
            );
            assert!(!scene.items.is_empty(), "{name} {display}");
            assert!(!runs(&scene).is_empty(), "{name} {display}: no glyphs");
            assert!(!scene.fonts.is_empty(), "{name} {display}");
            assert_eq!(scene.baseline.is_some(), !display, "{name} {display}");
            if let Some(baseline) = scene.baseline {
                assert!(
                    baseline > 0.0 && baseline <= scene.size.height + 0.01,
                    "{name}: baseline {baseline} outside {:?}",
                    scene.size
                );
            }
        }
    }
}

#[test]
fn glyphs_come_from_the_bundled_math_font() {
    let scene = render(r"x^2 + \alpha", false, 16.0);
    assert!(
        scene
            .fonts
            .iter()
            .any(|font| font.bytes() == BUNDLED_FILES[0])
    );
    // The page frame is the formula: the first glyph starts at the left edge.
    let first = runs(&scene)[0].glyphs[0];
    assert!((0.0..2.0).contains(&first.x), "{first:?}");
}

#[test]
fn the_scene_covers_the_formula() {
    // A fraction bar is a stroked horizontal line that spans the numerator width and lies
    // inside the page.
    let scene = render(r"\frac{a+b}{c}", true, 20.0);
    let bars = rules(&scene);
    assert_eq!(bars.len(), 1, "one fraction bar");
    assert!(
        bars[0].width > 0.0 && bars[0].width < 3.0,
        "{}",
        bars[0].width
    );
    let bar = bars[0].path.bounds().expect("bounds");
    assert!(
        bar.x >= -0.01 && bar.right() <= scene.size.width + 0.01,
        "{bar:?} in {:?}",
        scene.size
    );
    assert!(
        bar.y > 0.0 && bar.bottom() < scene.size.height,
        "{bar:?} in {:?}",
        scene.size
    );
    assert!(bar.width > 20.0, "bar spans a+b: {bar:?}");
    assert!(bar.height < 0.01, "{bar:?}");
    // Numerator above the bar, denominator below.
    let all = runs(&scene);
    let ys: Vec<f32> = all
        .iter()
        .flat_map(|run| run.glyphs.iter().map(|g| g.y))
        .collect();
    assert!(
        ys.iter().any(|y| *y < bar.y) && ys.iter().any(|y| *y > bar.bottom()),
        "{ys:?} {bar:?}"
    );
}

#[test]
fn display_style_is_taller_and_has_no_baseline() {
    let inline = render(r"\sum_{i=1}^n i", false, 16.0);
    let display = render(r"\sum_{i=1}^n i", true, 16.0);
    assert!(
        display.size.height > inline.size.height + 4.0,
        "{:?} {:?}",
        display.size,
        inline.size
    );
    assert!(display.baseline.is_none());
    assert!(inline.baseline.is_some());
}

#[test]
fn the_font_size_scales_everything() {
    let small = render(r"\frac{a}{b}+x^2", false, 10.0);
    let large = render(r"\frac{a}{b}+x^2", false, 30.0);
    let ratio = large.size.width / small.size.width;
    assert!((ratio - 3.0).abs() < 0.05, "width ratio {ratio}");
    let ratio = large.size.height / small.size.height;
    assert!((ratio - 3.0).abs() < 0.15, "height ratio {ratio}");
    // Scripts are set smaller, so the largest run has the requested size.
    let largest = |scene: &Scene| {
        runs(scene)
            .iter()
            .map(|run| run.size)
            .fold(0.0f32, f32::max)
    };
    assert!((largest(&small) - 10.0).abs() < 0.01);
    assert!((largest(&large) - 30.0).abs() < 0.01);
    let (small_baseline, large_baseline) = (
        small.baseline.expect("baseline"),
        large.baseline.expect("baseline"),
    );
    assert!((large_baseline / small_baseline - 3.0).abs() < 0.2);
}

#[test]
fn the_color_paints_glyphs_and_rules() {
    let red = Color::rgb(255, 0, 0);
    let scene = render_math(r"\frac{a}{b}", true, 16.0, red).expect("renders");
    assert!(!runs(&scene).is_empty() && !rules(&scene).is_empty());
    for run in runs(&scene) {
        assert_eq!(run.paint, Paint::Solid(red));
    }
    for rule in rules(&scene) {
        assert_eq!(rule.paint, Paint::Solid(red));
    }

    let faded = Color::rgba(0, 0, 255, 128);
    let scene = render_math("x", false, 16.0, faded).expect("renders");
    assert_eq!(runs(&scene)[0].paint, Paint::Solid(faded));
    // The invisible baseline marker never shows up as drawn text.
    for run in runs(&scene) {
        assert_ne!(run.paint, Paint::Solid(Color::rgba(1, 2, 3, 4)));
    }
}

#[test]
fn rendering_is_deterministic_and_cached() {
    let first = render(r"\sum_{k=0}^{m} k", false, 14.0);
    let second = render(r"\sum_{k=0}^{m} k", false, 14.0);
    assert_eq!(first, second);
    let other = render(r"\sum_{k=0}^{m} k", true, 14.0);
    assert_ne!(first.size, other.size);
}

#[test]
fn concurrent_renders_agree() {
    let expected = render(r"\binom{n}{k}", false, 12.0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| render(r"\binom{n}{k}", false, 12.0)))
            .collect();
        for handle in handles {
            assert_eq!(handle.join().expect("thread"), expected);
        }
    });
}

#[test]
fn bad_formulas_report_why() {
    let table: &[(&str, &str)] = &[
        (r"\foobar{x}", "foobar"),
        (r"\frac{a}{", "unclosed"),
        (r"\frac a}", "unmatched"),
        (r"\left( x", "\\left"),
        (r"\begin{pmatrix} a & b", "pmatrix"),
        (r"\begin{pmatrix} a \end{bmatrix}", "bmatrix"),
        (r"A \xrightarrow{f} B", "xrightarrow"),
        (r"\xleftarrow{g}", "xleftarrow"),
        ("", "empty"),
        ("   ", "empty"),
    ];
    for (latex, expected) in table {
        for display in [false, true] {
            let err: MathError = render_math(latex, display, 16.0, Color::BLACK)
                .expect_err(&format!("{latex:?} must fail"));
            assert!(
                err.message.contains(expected),
                "{latex:?}: message {:?} does not mention {expected:?}",
                err.message
            );
            assert_eq!(err.to_string(), err.message);
        }
    }
}

#[test]
fn unsupported_arrows_are_errors_not_blank_output() {
    let err = render_math(r"x \xrightarrow{f} y", true, 16.0, Color::BLACK).expect_err("xarrow");
    assert!(err.message.contains("unsupported command"), "{err}");
    // The same error comes back from the cache.
    let again = render_math(r"x \xrightarrow{f} y", true, 16.0, Color::BLACK).expect_err("xarrow");
    assert_eq!(err, again);
}

#[test]
fn invalid_sizes_are_errors() {
    for size in [0.0, -3.0, f32::NAN, 5000.0] {
        let err = render_math("x", false, size, Color::BLACK).expect_err("size");
        assert!(err.message.contains("font size"), "{size}: {err}");
    }
}
