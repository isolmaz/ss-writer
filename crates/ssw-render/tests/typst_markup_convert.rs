//! Markdown → Typst markup, compiled by the real Typst world: text must survive escaping, every
//! construct must typeset, and nothing in the input may break the compilation.

use image::{ExtendedColorType, ImageEncoder};
use ssw_core::markdown::parse_document;
use ssw_core::settings::PageSize;
use ssw_render::export::ExportOptions;
use ssw_render::typst_markup::{DiagramSvg, NoDiagrams, escape_markup, markdown_to_typst};
use ssw_render::world::TypstWorld;
use typst::layout::{Frame, FrameItem};
use typst::visualize::Paint;
use typst_layout::PagedDocument;

const SVG: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"60\" height=\"30\" viewBox=\"0 0 60 30\">\
                   <rect width=\"60\" height=\"30\" fill=\"#ccddee\"/></svg>";

fn png() -> Vec<u8> {
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(
            &[10, 120, 200, 255].repeat(16),
            4,
            4,
            ExtendedColorType::Rgba8,
        )
        .expect("png encoding");
    out
}

/// A world with an image and a diagram-free project root.
fn world() -> TypstWorld {
    let world = TypstWorld::new(None);
    world.set_virtual_file("pic.png", png()).expect("path");
    world
}

fn compile_with(
    world: &TypstWorld,
    markdown: &str,
    options: &ExportOptions,
    diagrams: &dyn DiagramSvg,
) -> PagedDocument {
    let parsed = parse_document(markdown);
    let typst = markdown_to_typst(markdown, &parsed, options, diagrams, &|dest| {
        world.file_available(dest)
    });
    for (path, bytes) in &typst.files {
        world
            .set_virtual_file(path, bytes.clone())
            .expect("virtual file path");
    }
    world.compile(&typst.source).unwrap_or_else(|e| {
        panic!(
            "{e}\n--- markdown ---\n{markdown}\n--- typst ---\n{}",
            typst.source
        )
    })
}

fn compile(world: &TypstWorld, markdown: &str) -> PagedDocument {
    compile_with(world, markdown, &ExportOptions::default(), &NoDiagrams)
}

fn visit(frame: &Frame, on_item: &mut dyn FnMut(&FrameItem)) {
    for (_, item) in frame.items() {
        on_item(item);
        if let FrameItem::Group(group) = item {
            visit(&group.frame, on_item);
        }
    }
}

/// All text of all pages, in frame order.
fn text_of(document: &PagedDocument) -> String {
    let mut out = String::new();
    for page in document.pages() {
        visit(&page.frame, &mut |item| {
            if let FrameItem::Text(text) = item {
                out.push_str(&text.text);
            }
        });
    }
    out
}

fn squash(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn count_images(document: &PagedDocument) -> usize {
    let mut count = 0;
    for page in document.pages() {
        visit(&page.frame, &mut |item| {
            if matches!(item, FrameItem::Image(..)) {
                count += 1;
            }
        });
    }
    count
}

/// Strings full of markup syntax. They must come out of Typst exactly as they went in.
const TRICKY: &[&str] = &[
    "plain words here",
    r"back\slash and \u{41} and \\ double",
    "#let x = 1; #panic() $y$ and `z` and ``` fence",
    "*bold* _it_ ~tilde~ **b** __u__ a*b*c snake_case_word",
    "<label> @ref @ <b> a<b",
    "[a](b) [c] ]x[ {d} (e)",
    "= heading\n+ plus\n- minus\n/ term: desc\n1. one\n 2. two",
    "a -- b --- c -? d -1 e - f -",
    "https://example.com/a?b=c&d=e#frag and http://x.y",
    "50% $5 and 3 * 4 = 12 + 1 / 2 // not a comment /* nor this */",
    "caf\u{e9} \u{fc}n\u{ef} \u{df} \u{11f} \u{15f} \u{130} \u{131}",
    "ends with backslash\\",
    "1.",
    "7. seven",
    "-",
    "+",
    "=",
    "/",
    "*",
    "_",
    "~",
    "#",
    "$",
    "@a",
    "<a>",
    "a.b.c and 1.2.3",
];

#[test]
fn escaped_text_survives_typst_unchanged() {
    let world = TypstWorld::new(None);
    // One paragraph per string, so every string starts a line.
    let mut source = String::from(
        "#set page(width: 500pt, height: auto)\n#set text(font: \"New Computer Modern\")\n",
    );
    let mut expected = String::new();
    for text in TRICKY {
        source.push_str(&escape_markup(text));
        source.push_str("\n\n");
        expected.push_str(&squash(text));
    }
    let document = world
        .compile(&source)
        .unwrap_or_else(|e| panic!("{e}\n--- typst ---\n{source}"));
    assert_eq!(squash(&text_of(&document)), expected);
}

#[test]
fn escaping_holds_for_generated_strings() {
    let world = TypstWorld::new(None);
    let alphabet: Vec<char> = "\\#$*_~`<>@[]{}/+-=.:(),;a1 2^&|%!".chars().collect();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize
    };
    let mut source = String::from(
        "#set page(width: 500pt, height: auto)\n#set text(font: \"New Computer Modern\")\n",
    );
    let mut expected = String::new();
    for _ in 0..300 {
        let length = 1 + next() % 12;
        let text: String = (0..length)
            .map(|_| alphabet[next() % alphabet.len()])
            .collect();
        if text.contains("...") {
            // Typst turns three dots into an ellipsis: a typographic change, not an escaping bug.
            continue;
        }
        source.push_str(&escape_markup(&text));
        source.push_str("\n\n");
        expected.push_str(&squash(&text));
    }
    let document = world
        .compile(&source)
        .unwrap_or_else(|e| panic!("{e}\n--- typst ---\n{source}"));
    assert_eq!(squash(&text_of(&document)), expected);
}

const KITCHEN_SINK: &str = "# Heading One\n\n\
Some *emphasis*, **strong**, ~underline~, ~~strike~~, ==mark== =={red}red== and `inline code`.\n\n\
- item one\n- item two\n  1. nested number\n\n\
- [ ] open task\n- [x] done task\n\n\
> quoted words\n\n\
> [!NOTE] Custom title\n> callout body\n\n\
> [!WARNING]\n> warn body\n\n\
```rust\nfn main() {}\n```\n\n\
| Name | Value |\n|:-----|------:|\n| alpha | 1 |\n\n\
---\n\n\
Footnote ref[^1] and [link](https://example.com) and <https://auto.example.org>.\n\n\
[^1]: The footnote text.\n\n\
<!-- pagebreak -->\n\n\
## Second page\n\n\
Inline math $x^2$ and display:\n\n\
$$\\frac{a}{b}$$\n\n\
Bad math $\\foobar$ stays source.\n\n\
<div>raw html</div>\n\n\
![missing](nope.png)\n\n\
[[Wiki|Alias]] line\\\nbreak\n";

#[test]
fn every_construct_typesets_and_shows_its_text() {
    let world = world();
    let document = compile(&world, KITCHEN_SINK);
    assert!(document.pages().len() >= 2, "page break");
    let text = squash(&text_of(&document));
    for expected in [
        "HeadingOne",
        "emphasis",
        "strong",
        "underline",
        "strike",
        "mark",
        "red",
        "inlinecode",
        "itemone",
        "itemtwo",
        "nestednumber",
        "opentask",
        "donetask",
        "quotedwords",
        "Customtitle",
        "calloutbody",
        "Warning",
        "warnbody",
        "fnmain(){}",
        "Name",
        "Value",
        "alpha",
        "Thefootnotetext.",
        "link",
        "https://auto.example.org",
        "Secondpage",
        "Inlinemath",
        "\\foobar",
        "<div>rawhtml</div>",
        "missing",
        "Alias",
        "line",
        "break",
    ] {
        assert!(
            text.contains(expected),
            "{expected:?} missing from {text:?}"
        );
    }
    // "Note" is the default title of the first callout kind only when no title is given.
    assert!(!text.contains("Note"));
    assert!(text.contains("Warning"));
}

#[test]
fn page_options_are_applied() {
    let world = world();
    let a4 = compile(&world, "x");
    let a4_size = a4.pages()[0].frame.size();
    let letter = compile_with(
        &world,
        "x",
        &ExportOptions {
            page: PageSize::Letter,
            ..ExportOptions::default()
        },
        &NoDiagrams,
    );
    let letter_size = letter.pages()[0].frame.size();
    assert!((a4_size.x.to_pt() - 595.28).abs() < 0.1 && (a4_size.y.to_pt() - 841.89).abs() < 0.1);
    assert!(
        (letter_size.x.to_pt() - 612.0).abs() < 0.1 && (letter_size.y.to_pt() - 792.0).abs() < 0.1
    );

    // The page number is printed in the footer only when asked for.
    let numbered = squash(&text_of(&compile(&world, "hello")));
    let bare = compile_with(
        &world,
        "hello",
        &ExportOptions {
            page_numbers: false,
            ..ExportOptions::default()
        },
        &NoDiagrams,
    );
    assert_eq!(numbered.replace('1', ""), "hello");
    assert_eq!(numbered.len(), "hello".len() + 1, "{numbered}");
    assert_eq!(squash(&text_of(&bare)), "hello");

    // Larger margins leave less room: a long text needs more pages.
    let long = "word ".repeat(2500);
    let normal = compile(&world, &long).pages().len();
    let wide = compile_with(
        &world,
        &long,
        &ExportOptions {
            margin_mm: 60.0,
            base_font_pt: 14.0,
            ..ExportOptions::default()
        },
        &NoDiagrams,
    )
    .pages()
    .len();
    assert!(wide > normal, "{wide} pages vs {normal}");
}

#[test]
fn the_title_is_printed_once_at_the_top() {
    let world = world();
    let titled = ExportOptions {
        title: Some("My *Title*".to_owned()),
        ..ExportOptions::default()
    };
    let document = compile_with(&world, "body text", &titled, &NoDiagrams);
    // Without the page number (a "1" printed in the footer) the text is title, then body.
    assert_eq!(
        squash(&text_of(&document)).replace('1', ""),
        "My*Title*bodytext"
    );
    let hidden = ExportOptions {
        include_title: false,
        ..titled
    };
    let document = compile_with(&world, "body text", &hidden, &NoDiagrams);
    assert_eq!(squash(&text_of(&document)).replace('1', ""), "bodytext");
}

#[test]
fn black_text_drops_the_accent_colors() {
    let world = world();
    let markdown = "[link](https://example.com) and plain\n\n> [!TIP]\n> tip body\n\n$\\foobar$";
    let colored = compile(&world, markdown);
    let black = compile_with(
        &world,
        markdown,
        &ExportOptions {
            black_text: true,
            ..ExportOptions::default()
        },
        &NoDiagrams,
    );
    let fills = |document: &PagedDocument| {
        let mut fills = Vec::new();
        for page in document.pages() {
            visit(&page.frame, &mut |item| {
                if let FrameItem::Text(text) = item
                    && let Paint::Solid(color) = &text.fill
                {
                    fills.push(color.to_vec4_u8());
                }
            });
        }
        fills
    };
    let colored_fills = fills(&colored);
    assert!(
        colored_fills.contains(&[0x09, 0x69, 0xda, 0xff]),
        "link blue in {colored_fills:?}"
    );
    assert!(
        colored_fills.contains(&[0x1a, 0x7f, 0x37, 0xff]),
        "tip green in {colored_fills:?}"
    );
    assert!(
        colored_fills.contains(&[0xcf, 0x22, 0x2e, 0xff]),
        "error red in {colored_fills:?}"
    );
    let black_fills = fills(&black);
    assert!(
        black_fills.iter().all(|c| *c == [0, 0, 0, 0xff]),
        "only black text expected, got {black_fills:?}"
    );
}

#[test]
fn images_are_embedded_only_when_readable() {
    let world = world();
    let diagrams = |source: &str| (source.starts_with("graph")).then(|| SVG.to_owned());
    let markdown = "![block](pic.png)\n\n\
                    inline ![small|20](pic.png) and ![sized|40x20](pic.png \"title\") here\n\n\
                    ![huge|5000](pic.png)\n\n\
                    <img src=\"pic.png\" width=\"30\" alt=\"html image\">\n\n\
                    ```mermaid\ngraph TD\n  A-->B\n```\n\n\
                    ```mermaid\nsankey-beta\nA,B,1\n```\n\n\
                    ![outside](../secret.png)\n\n![absolute](C:\\Windows\\logo.png)\n\n![remote](https://example.com/a.png)\n\n![format](pic.bmp)";
    let document = compile_with(&world, markdown, &ExportOptions::default(), &diagrams);
    // block, inline x2, huge, html, and the drawable diagram
    assert_eq!(count_images(&document), 6);
    let text = squash(&text_of(&document));
    for fallback in [
        "outside",
        "absolute",
        "remote",
        "format",
        "Unsupporteddiagram",
        "sankey-beta",
    ] {
        assert!(text.contains(fallback), "{fallback} missing from {text:?}");
    }
    assert!(
        !text.contains("A-->B"),
        "the drawn diagram does not show its source"
    );
}

#[test]
fn awkward_documents_still_typeset() {
    let world = world();
    let long_line = "deeply nested: ".repeat(40);
    let documents: &[&str] = &[
        "",
        "   \n\n",
        "---\ntitle: only front matter\n---\n",
        "+++\ntitle = 'toml'\n+++\n\ntext",
        "[ref]: https://example.com\n",
        "$$\n\\begin{aligned}\n$$",
        "$ \\frac{ $ and $\\xrightarrow{x}$ and $$\\left($$",
        "| a |\n|---|\n| 1 | 2 | 3 |\n\n| x | y | z |\n|:-:|--:|---|\n",
        "- [ ] a\n- [x] b\n  - [ ] nested\n- plain\n\n1. [x] numbered task\n2. other",
        "> > > deep quote\n> > [!CAUTION]\n> > careful",
        "text[^missing] and [^undefined]\n\n[^unused]: never referenced",
        "[^a]: loops back to [^a]\n\nuse[^a]",
        "* a\n\n    code in list\n\n* b",
        "<script>alert('x')</script>\n\n<!-- comment --> and <span>x</span>",
        "# #[not code] $not math$ `#raw` <label> @ref\n\n`` ` ``",
        "\\# \\* \\_ \\\\ \\` \\[ \\]",
        "Line one  \nline two\\\nline three",
        "==== not highlight ====  ===== five =====",
        "[a](https://example.com/a(b)c) [b](<https://example.com/a b>) <mailto:a@b.c> www.example.com",
        &long_line,
        "![](pic.png)![](pic.png)",
        "# Title with `code` and $m$ and ![i](pic.png) and [l](https://x.org)",
        "```\nunterminated fence",
        "```mermaid\n```",
        "<img src=pic.png>\n\n<img>",
    ];
    for markdown in documents {
        let document = compile(&world, markdown);
        assert!(!document.pages().is_empty(), "no page for {markdown:?}");
    }
}

#[test]
fn every_option_combination_compiles() {
    let world = world();
    for page in [PageSize::A4, PageSize::Letter] {
        for black_text in [false, true] {
            for page_numbers in [false, true] {
                for title in [None, Some("T"), Some("   ")] {
                    let options = ExportOptions {
                        page,
                        margin_mm: 10.0,
                        base_font_pt: 9.0,
                        page_numbers,
                        black_text,
                        include_title: title.is_some(),
                        title: title.map(str::to_owned),
                    };
                    let document = compile_with(&world, KITCHEN_SINK, &options, &NoDiagrams);
                    assert!(document.pages().len() >= 2);
                }
            }
        }
    }
    // Numbers that cannot be used are replaced, not passed to Typst.
    let broken = ExportOptions {
        margin_mm: f32::NAN,
        base_font_pt: f32::INFINITY,
        ..ExportOptions::default()
    };
    assert!(
        !compile_with(&world, "x", &broken, &NoDiagrams)
            .pages()
            .is_empty()
    );
}

#[test]
fn a_title_cannot_inject_markup() {
    let world = world();
    let options = ExportOptions {
        title: Some("@SIZE@ #panic() ] \\".to_owned()),
        ..ExportOptions::default()
    };
    let document = compile_with(&world, "x", &options, &NoDiagrams);
    assert_eq!(
        squash(&text_of(&document)).replace('1', ""),
        "@SIZE@#panic()]\\x"
    );

    // The title is also a string in the document metadata.
    let metadata_only = ExportOptions {
        include_title: false,
        title: Some("quote \" backslash \\ newline \n tab \t #panic()".to_owned()),
        ..ExportOptions::default()
    };
    let document = compile_with(&world, "x", &metadata_only, &NoDiagrams);
    assert_eq!(squash(&text_of(&document)).replace('1', ""), "x");
}
