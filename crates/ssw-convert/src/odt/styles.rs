//! The ODF style sheet: common and automatic styles of `styles.xml` and `content.xml`, list
//! styles, font faces and the page furniture that Markdown cannot carry.

use std::collections::HashMap;

use ssw_core::markdown::ast::HighlightColor;

use crate::doc::md::Align;

use super::xml::{Element, Node};

/// The style families the importer resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Family {
    Paragraph,
    Text,
    Table,
    Section,
    Other,
}

/// Character formatting of one style; `None` means "inherit".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct TextProps {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub underline: Option<bool>,
    pub strike: Option<bool>,
    /// `Some(None)`: an explicit "no highlight" (transparent or a grey).
    pub highlight: Option<Option<HighlightColor>>,
    /// A text color other than black.
    pub colored: Option<bool>,
    /// Superscript or subscript.
    pub sub_super: Option<bool>,
    pub hidden: Option<bool>,
    pub mono: Option<bool>,
}

fn over<T: Copy>(slot: &mut Option<T>, top: Option<T>) {
    if top.is_some() {
        *slot = top;
    }
}

impl TextProps {
    /// Applies `top` on top of `self`: every property `top` sets wins.
    pub fn overlay(&mut self, top: &TextProps) {
        over(&mut self.bold, top.bold);
        over(&mut self.italic, top.italic);
        over(&mut self.underline, top.underline);
        over(&mut self.strike, top.strike);
        over(&mut self.highlight, top.highlight);
        over(&mut self.colored, top.colored);
        over(&mut self.sub_super, top.sub_super);
        over(&mut self.hidden, top.hidden);
        over(&mut self.mono, top.mono);
    }
}

/// How a paragraph is used, decided by its style (or an ancestor).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ParaKind {
    #[default]
    Normal,
    Code,
    Quote,
    Rule,
}

/// A paragraph (or table) style with its ancestors folded in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ParaInfo {
    pub text: TextProps,
    pub break_before: bool,
    pub break_after: bool,
    /// Centered, right-aligned or justified.
    pub aligned: bool,
    /// The alignment as a Markdown column alignment (`Align::None` for justified text and
    /// when the style says nothing).
    pub halign: Align,
    /// The style starts a new page style (`style:master-page-name`).
    pub master_page: bool,
    /// Heading level 1..=6 when the style is a heading style.
    pub heading: Option<u8>,
    pub kind: ParaKind,
}

/// One level of a list style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ListKind {
    Bullet,
    /// A check-box bullet (`checked` for ☑ / ☒).
    Task(bool),
    Number {
        decimal: bool,
        start: u32,
    },
}

#[derive(Debug, Clone, Default)]
struct ListStyle {
    /// Levels are 1-based.
    levels: HashMap<usize, ListKind>,
}

#[derive(Debug, Clone, Default)]
struct StyleDef {
    parent: Option<String>,
    /// Normalized `style:name` and `style:display-name`.
    names: [String; 2],
    outline: Option<u8>,
    master_page: bool,
    text: TextProps,
    break_before: Option<bool>,
    break_after: Option<bool>,
    aligned: Option<bool>,
    halign: Option<Align>,
    columns: Option<usize>,
}

/// `fo:text-align` as a Markdown column alignment; justified text has none.
fn text_align(value: &str) -> Align {
    match value {
        "start" | "left" => Align::Left,
        "center" => Align::Center,
        "end" | "right" => Align::Right,
        _ => Align::None,
    }
}

/// All styles of a document.
#[derive(Debug, Clone, Default)]
pub(super) struct Styles {
    defs: HashMap<(Family, String), StyleDef>,
    lists: HashMap<String, ListStyle>,
    fonts: HashMap<String, bool>,
    /// Page layout name → more than one column.
    layouts: HashMap<String, bool>,
    /// Headers and footers with content in the master pages.
    pub header_footers: usize,
    /// Master pages whose page layout has several columns.
    pub column_pages: usize,
}

const CODE_PARAGRAPHS: [&str; 6] = [
    "preformatted text",
    "source code",
    "sourcecode",
    "code",
    "code block",
    "verbatim",
];
const QUOTE_PARAGRAPHS: [&str; 7] = [
    "quotations",
    "quotation",
    "quote",
    "block quote",
    "blockquote",
    "block text",
    "block quotation",
];
const CODE_CHARACTERS: [&str; 6] = [
    "source text",
    "source code",
    "code",
    "inline code",
    "verbatim",
    "teletype",
];
const MONO_FONTS: [&str; 12] = [
    "monospace",
    "courier",
    "consolas",
    "lucida console",
    "menlo",
    "monaco",
    "typewriter",
    "source code",
    "inconsolata",
    "cascadia",
    "fira code",
    "terminal",
];
/// Longest chain of `style:parent-style-name` links that is followed.
const MAX_CHAIN: usize = 32;

/// `Heading_20_1` / `Heading 1` → `heading 1`.
fn normalize(name: &str) -> String {
    name.replace("_20_", " ")
        .replace('_', " ")
        .trim()
        .to_lowercase()
}

fn name_heading(norm: &str) -> Option<u8> {
    if norm == "title" {
        return Some(1);
    }
    let digits = norm.strip_prefix("heading ")?;
    let level: u32 = digits.trim().parse().ok()?;
    (level >= 1).then(|| u8::try_from(level.min(6)).unwrap_or(6))
}

/// Monospace by name only: the `fixed` font pitch is no evidence, because East Asian faces
/// (`標楷體`, `MS Gothic`, `SimSun`) are declared fixed-pitch for their half-width glyphs.
fn font_is_mono(name: &str) -> bool {
    let lower = name.to_lowercase();
    // "Liberation Mono", "Cascadia Mono Italic" — but not "Monotype Corsiva"
    lower.ends_with("mono")
        || lower.contains(" mono")
        || MONO_FONTS.iter().any(|m| lower.contains(m))
}

fn is_black(color: &str) -> bool {
    color.eq_ignore_ascii_case("#000000")
}

fn weight_is_bold(weight: &str) -> bool {
    weight == "bold" || weight.parse::<u32>().is_ok_and(|n| n >= 600)
}

/// `super 58%`, `sub 58%`, `33% 58%`, `0% 100%`.
fn position_is_raised_or_lowered(position: &str) -> bool {
    let first = position.split_whitespace().next().unwrap_or("");
    if first == "super" || first == "sub" {
        return true;
    }
    first
        .trim_end_matches('%')
        .parse::<f64>()
        .is_ok_and(|v| v != 0.0)
}

/// The highlight color nearest to a background color, `None` for greys and white.
pub(super) fn highlight_from_hex(hex: &str) -> Option<HighlightColor> {
    let digits = hex.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    let channel = |at: usize| {
        digits
            .get(at..at + 2)
            .and_then(|c| i32::from_str_radix(c, 16).ok())
    };
    let (r, g, b) = (channel(0)?, channel(2)?, channel(4)?);
    let (max, min) = (r.max(g).max(b), r.min(g).min(b));
    let delta = max - min;
    if max == 0 || delta * 100 < max * 12 {
        return None;
    }
    let hue = if max == r {
        60 * (g - b) / delta
    } else if max == g {
        60 * (b - r) / delta + 120
    } else {
        60 * (r - g) / delta + 240
    };
    let hue = (hue + 360) % 360;
    Some(match hue {
        15..=41 => HighlightColor::Orange,
        42..=74 => HighlightColor::Yellow,
        75..=164 => HighlightColor::Green,
        165..=254 => HighlightColor::Blue,
        255..=344 => HighlightColor::Purple,
        _ => HighlightColor::Red,
    })
}

fn text_props(el: &Element, fonts: &HashMap<String, bool>) -> TextProps {
    let mut p = TextProps::default();
    if let Some(w) = el.attr("fo:font-weight") {
        p.bold = Some(weight_is_bold(w));
    }
    if let Some(s) = el.attr("fo:font-style") {
        p.italic = Some(matches!(s, "italic" | "oblique"));
    }
    if let Some(u) = el
        .attr("style:text-underline-style")
        .or_else(|| el.attr("style:text-underline-type"))
    {
        p.underline = Some(u != "none");
    }
    if let Some(s) = el
        .attr("style:text-line-through-style")
        .or_else(|| el.attr("style:text-line-through-type"))
    {
        p.strike = Some(s != "none");
    }
    if let Some(bg) = el.attr("fo:background-color") {
        p.highlight = Some(highlight_from_hex(bg));
    }
    if let Some(c) = el.attr("fo:color") {
        p.colored = Some(!is_black(c));
    } else if el.attr("style:use-window-font-color") == Some("true") {
        p.colored = Some(false);
    }
    if let Some(pos) = el.attr("style:text-position") {
        p.sub_super = Some(position_is_raised_or_lowered(pos));
    }
    if let Some(d) = el.attr("text:display") {
        p.hidden = Some(d == "none");
    }
    p.mono = if let Some(face) = el.attr("style:font-name") {
        Some(
            fonts
                .get(face)
                .copied()
                .unwrap_or_else(|| font_is_mono(face)),
        )
    } else {
        el.attr("fo:font-family")
            .or_else(|| el.attr("svg:font-family"))
            .map(font_is_mono)
    };
    p
}

fn list_style(el: &Element) -> ListStyle {
    let mut style = ListStyle::default();
    for level in el.elements() {
        let Some(n) = level
            .attr("text:level")
            .and_then(|l| l.trim().parse::<usize>().ok())
        else {
            continue;
        };
        let kind = match level.name.as_str() {
            "text:list-level-style-bullet" => match level.attr("text:bullet-char") {
                Some("\u{2610}") => ListKind::Task(false),
                Some("\u{2611}" | "\u{2612}") => ListKind::Task(true),
                _ => ListKind::Bullet,
            },
            "text:list-level-style-number" => match level.attr("style:num-format") {
                None | Some("") => ListKind::Bullet,
                Some(format) => ListKind::Number {
                    decimal: format == "1",
                    start: level
                        .attr("text:start-value")
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(1),
                },
            },
            "text:list-level-style-image" => ListKind::Bullet,
            _ => continue,
        };
        style.levels.insert(n, kind);
    }
    style
}

/// The number of columns declared below a property element (`style:columns`).
fn column_count(properties: &Element) -> Option<usize> {
    properties
        .child("style:columns")
        .and_then(|c| c.attr("fo:column-count"))
        .and_then(|n| n.trim().parse().ok())
}

/// True when a header or footer holds text or a drawing.
fn has_content(el: &Element) -> bool {
    el.children.iter().any(|node| match node {
        Node::Text(t) => !t.trim().is_empty(),
        Node::Element(e) => e.name == "draw:frame" || e.name == "draw:g" || has_content(e),
    })
}

impl Styles {
    /// Collects the styles of `content.xml` (which win) and `styles.xml`.
    pub fn parse(content: &Element, styles: Option<&Element>) -> Styles {
        let mut s = Styles::default();
        let roots: Vec<&Element> = styles.into_iter().chain(std::iter::once(content)).collect();
        for root in &roots {
            if let Some(decls) = root.child("office:font-face-decls") {
                for face in decls.elements() {
                    let Some(name) = face.attr("style:name") else {
                        continue;
                    };
                    let family = face.attr("svg:font-family").unwrap_or(name);
                    s.fonts
                        .insert(name.to_string(), font_is_mono(family) || font_is_mono(name));
                }
            }
        }
        for root in &roots {
            for group in ["office:styles", "office:automatic-styles"] {
                if let Some(group) = root.child(group) {
                    s.add_group(group);
                }
            }
        }
        if let Some(root) = styles
            && let Some(masters) = root.child("office:master-styles")
        {
            s.add_masters(masters);
        }
        s
    }

    fn add_group(&mut self, group: &Element) {
        for el in group.elements() {
            match el.name.as_str() {
                "style:style" => self.add_style(el),
                "text:list-style" => {
                    if let Some(name) = el.attr("style:name") {
                        self.lists.insert(name.to_string(), list_style(el));
                    }
                }
                "style:page-layout" => {
                    if let Some(name) = el.attr("style:name") {
                        let columns = el
                            .child("style:page-layout-properties")
                            .and_then(column_count);
                        self.layouts
                            .insert(name.to_string(), columns.is_some_and(|n| n > 1));
                    }
                }
                _ => {}
            }
        }
    }

    fn add_style(&mut self, el: &Element) {
        let Some(name) = el.attr("style:name") else {
            return;
        };
        let family = match el.attr("style:family") {
            Some("paragraph") => Family::Paragraph,
            Some("text") => Family::Text,
            Some("table") => Family::Table,
            Some("section") => Family::Section,
            _ => Family::Other,
        };
        let mut def = StyleDef {
            parent: el.attr("style:parent-style-name").map(str::to_string),
            names: [
                normalize(name),
                normalize(el.attr("style:display-name").unwrap_or("")),
            ],
            outline: el
                .attr("style:default-outline-level")
                .and_then(|l| l.trim().parse::<u32>().ok())
                .filter(|&l| l >= 1)
                .map(|l| u8::try_from(l.min(6)).unwrap_or(6)),
            master_page: el
                .attr("style:master-page-name")
                .is_some_and(|m| !m.is_empty()),
            ..StyleDef::default()
        };
        for props in el.elements() {
            match props.name.as_str() {
                "style:text-properties" => def.text = text_props(props, &self.fonts),
                "style:paragraph-properties" | "style:table-properties" => {
                    def.break_before = props.attr("fo:break-before").map(|b| b == "page");
                    def.break_after = props.attr("fo:break-after").map(|b| b == "page");
                    def.aligned = props
                        .attr("fo:text-align")
                        .map(|a| matches!(a, "center" | "end" | "right" | "justify"));
                    def.halign = props.attr("fo:text-align").map(text_align);
                }
                "style:section-properties" => def.columns = column_count(props),
                _ => {}
            }
        }
        self.defs.insert((family, name.to_string()), def);
    }

    fn add_masters(&mut self, masters: &Element) {
        for page in masters.elements().filter(|e| e.name == "style:master-page") {
            for part in page.elements() {
                let furniture = matches!(
                    part.name.as_str(),
                    "style:header"
                        | "style:footer"
                        | "style:header-left"
                        | "style:footer-left"
                        | "style:header-first"
                        | "style:footer-first"
                );
                if furniture && has_content(part) {
                    self.header_footers += 1;
                }
            }
            let wide = page
                .attr("style:page-layout-name")
                .and_then(|l| self.layouts.get(l))
                .copied();
            if wide == Some(true) {
                self.column_pages += 1;
            }
        }
    }

    /// The style and its ancestors, nearest first.
    fn chain(&self, family: Family, name: &str) -> Vec<&StyleDef> {
        let mut out = Vec::new();
        let mut next = Some(name);
        while let Some(current) = next {
            if out.len() >= MAX_CHAIN {
                break;
            }
            let Some(def) = self.defs.get(&(family, current.to_string())) else {
                break;
            };
            out.push(def);
            next = def.parent.as_deref();
        }
        out
    }

    /// Character formatting of a text style, ancestors folded in.
    pub fn text(&self, name: &str) -> TextProps {
        let mut props = TextProps::default();
        for def in self.chain(Family::Text, name).into_iter().rev() {
            props.overlay(&def.text);
            if def.text.mono.is_none()
                && def
                    .names
                    .iter()
                    .any(|n| CODE_CHARACTERS.contains(&n.as_str()))
            {
                props.mono = Some(true);
            }
        }
        props
    }

    fn resolve(&self, family: Family, name: Option<&str>) -> ParaInfo {
        let mut info = ParaInfo::default();
        let Some(name) = name else { return info };
        let chain = self.chain(family, name);
        for def in chain.iter().rev() {
            info.text.overlay(&def.text);
            info.break_before = def.break_before.unwrap_or(info.break_before);
            info.break_after = def.break_after.unwrap_or(info.break_after);
            info.aligned = def.aligned.unwrap_or(info.aligned);
            info.halign = def.halign.unwrap_or(info.halign);
        }
        // the style itself decides; `master_page` is the style's own attribute
        info.master_page = chain.first().is_some_and(|d| d.master_page);
        for def in &chain {
            if info.heading.is_none() {
                info.heading = def
                    .outline
                    .or_else(|| def.names.iter().find_map(|n| name_heading(n)));
            }
            if info.kind == ParaKind::Normal {
                let any = |list: &[&str]| def.names.iter().any(|n| list.contains(&n.as_str()));
                info.kind = if any(&CODE_PARAGRAPHS) {
                    ParaKind::Code
                } else if any(&QUOTE_PARAGRAPHS) {
                    ParaKind::Quote
                } else if any(&["horizontal line"]) {
                    ParaKind::Rule
                } else {
                    ParaKind::Normal
                };
            }
        }
        info
    }

    /// A paragraph style (`None` for a paragraph without one).
    pub fn paragraph(&self, name: Option<&str>) -> ParaInfo {
        self.resolve(Family::Paragraph, name)
    }

    /// A table style: only the page-break and page-style flags are meaningful.
    pub fn table(&self, name: Option<&str>) -> ParaInfo {
        self.resolve(Family::Table, name)
    }

    /// Number of columns of a section style (1 when it declares none).
    pub fn section_columns(&self, name: &str) -> usize {
        self.chain(Family::Section, name)
            .iter()
            .find_map(|d| d.columns)
            .unwrap_or(1)
    }

    /// The kind of one level (1-based) of a list style. Missing levels inherit the nearest
    /// lower one; an unknown style is a plain bullet list.
    pub fn list_level(&self, style: Option<&str>, level: usize) -> ListKind {
        let Some(list) = style.and_then(|s| self.lists.get(s)) else {
            return ListKind::Bullet;
        };
        (1..=level)
            .rev()
            .find_map(|l| list.levels.get(&l))
            .copied()
            .unwrap_or(ListKind::Bullet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::odt::xml::parse;

    fn styles(content: &str, common: Option<&str>) -> Styles {
        let content = parse(
            format!("<office:document-content>{content}</office:document-content>").as_bytes(),
        )
        .unwrap();
        let common = common.map(|c| {
            parse(format!("<office:document-styles>{c}</office:document-styles>").as_bytes())
                .unwrap()
        });
        Styles::parse(&content, common.as_ref())
    }

    fn props(attrs: &str) -> TextProps {
        let s = styles(
            &format!(
                r#"<office:font-face-decls><style:font-face style:name="Mono1" svg:font-family="'Liberation Mono'" style:font-pitch="fixed"/><style:font-face style:name="Plain" svg:font-family="Arial"/></office:font-face-decls><office:automatic-styles><style:style style:name="T" style:family="text"><style:text-properties {attrs}/></style:style></office:automatic-styles>"#
            ),
            None,
        );
        s.text("T")
    }

    #[test]
    fn styles_without_a_name_are_ignored() {
        let s = styles(
            r#"<office:automatic-styles><style:style style:family="text"><style:text-properties fo:font-weight="bold"/></style:style><style:style style:name="B" style:family="text"><style:text-properties fo:font-weight="bold"/></style:style></office:automatic-styles>"#,
            None,
        );
        assert_eq!(s.text("B").bold, Some(true));
        assert_eq!(s.text(""), TextProps::default());
    }

    #[test]
    fn reads_character_properties() {
        let p = props(
            r##"fo:font-weight="bold" fo:font-style="oblique" style:text-underline-style="solid" style:text-line-through-style="solid" fo:background-color="#ffff00" fo:color="#ff0000" style:text-position="super 58%" text:display="none" style:font-name="Plain""##,
        );
        assert_eq!(
            p,
            TextProps {
                bold: Some(true),
                italic: Some(true),
                underline: Some(true),
                strike: Some(true),
                highlight: Some(Some(HighlightColor::Yellow)),
                colored: Some(true),
                sub_super: Some(true),
                hidden: Some(true),
                mono: Some(false),
            }
        );
        let n = props(
            r##"fo:font-weight="400" fo:font-style="normal" style:text-underline-style="none" style:text-underline-type="single" style:text-line-through-style="none" fo:background-color="transparent" fo:color="#000000" style:text-position="0% 100%" text:display="true""##,
        );
        assert_eq!(
            n,
            TextProps {
                bold: Some(false),
                italic: Some(false),
                underline: Some(false),
                strike: Some(false),
                highlight: Some(None),
                colored: Some(false),
                sub_super: Some(false),
                hidden: Some(false),
                mono: None,
            }
        );
        let by_type = props(
            r#"style:text-underline-type="double" style:text-line-through-type="single" fo:font-weight="700" style:use-window-font-color="true" style:text-position="-33% 58%""#,
        );
        assert_eq!(by_type.underline, Some(true));
        assert_eq!(by_type.strike, Some(true));
        assert_eq!(by_type.bold, Some(true));
        assert_eq!(by_type.colored, Some(false));
        assert_eq!(by_type.sub_super, Some(true));
        assert_eq!(props("").bold, None);
        assert_eq!(props(r#"fo:font-weight="bolder""#).bold, Some(false));
    }

    #[test]
    fn detects_monospace_fonts() {
        assert_eq!(
            props(r#"style:font-name="Mono1""#).mono,
            Some(true),
            "declared face with fixed pitch"
        );
        assert_eq!(props(r#"style:font-name="Plain""#).mono, Some(false));
        assert_eq!(
            props(r#"style:font-name="Courier New""#).mono,
            Some(true),
            "undeclared face, known name"
        );
        assert_eq!(
            props(r#"style:font-name="DejaVu Sans Mono""#).mono,
            Some(true)
        );
        assert_eq!(
            props(r#"style:font-name="Cascadia Mono Italic""#).mono,
            Some(true)
        );
        assert_eq!(
            props(r#"style:font-name="Monotype Corsiva""#).mono,
            Some(false)
        );
        assert_eq!(props(r#"fo:font-family="Consolas""#).mono, Some(true));
        assert_eq!(props(r#"svg:font-family="Calibri""#).mono, Some(false));
        assert_eq!(
            props(r#"style:font-pitch="fixed""#).mono,
            None,
            "the pitch alone says nothing"
        );
        assert_eq!(props("").mono, None);
    }

    #[test]
    fn highlight_colors_follow_the_hue() {
        let cases = [
            ("#ff0000", Some(HighlightColor::Red)),
            ("#FFC9C9", Some(HighlightColor::Red)),
            ("#ff0020", Some(HighlightColor::Red)),
            ("#ff00aa", Some(HighlightColor::Purple)),
            ("#ffa500", Some(HighlightColor::Orange)),
            ("#ffd8a8", Some(HighlightColor::Orange)),
            ("#ffff00", Some(HighlightColor::Yellow)),
            ("#fff3a3", Some(HighlightColor::Yellow)),
            ("#00ff00", Some(HighlightColor::Green)),
            ("#c3f0c8", Some(HighlightColor::Green)),
            ("#00ffff", Some(HighlightColor::Blue)),
            ("#c5ddfb", Some(HighlightColor::Blue)),
            ("#0000ff", Some(HighlightColor::Blue)),
            ("#e3d0f8", Some(HighlightColor::Purple)),
            ("#ff00ff", Some(HighlightColor::Purple)),
            ("#ffffff", None),
            ("#808080", None),
            ("#000000", None),
            ("#fff", None),
            ("ffff00", None),
            ("#gg0000", None),
            ("#ff00é", None),
            ("#fé000", None),
        ];
        for (hex, want) in cases {
            assert_eq!(highlight_from_hex(hex), want, "{hex}");
        }
    }

    #[test]
    fn styles_inherit_and_content_overrides_common() {
        let s = styles(
            r#"<office:automatic-styles><style:style style:name="P1" style:family="paragraph" style:parent-style-name="Mid" style:master-page-name="Standard"><style:paragraph-properties fo:break-before="page" fo:text-align="center"/></style:style><style:style style:name="Shared" style:family="paragraph"><style:paragraph-properties fo:break-after="page"/></style:style><style:style style:name="Loop" style:family="paragraph" style:parent-style-name="Loop"/><style:style style:name="Odd" style:family="graphic"/><style:style style:name="Tbl" style:family="table"><style:table-properties fo:break-before="page"/></style:style></office:automatic-styles>"#,
            Some(
                r#"<office:styles><style:style style:name="Mid" style:family="paragraph" style:parent-style-name="Top"><style:text-properties fo:font-style="italic"/><style:paragraph-properties fo:break-before="auto" fo:break-after="page"/></style:style><style:style style:name="Top" style:family="paragraph"><style:text-properties fo:font-weight="bold" fo:font-style="normal"/><style:paragraph-properties fo:text-align="start"/></style:style><style:style style:name="Shared" style:family="paragraph"><style:paragraph-properties fo:break-after="auto"/></style:style></office:styles>"#,
            ),
        );
        let p = s.paragraph(Some("P1"));
        assert_eq!(p.text.bold, Some(true));
        assert_eq!(
            p.text.italic,
            Some(true),
            "the nearer style overrides the ancestor"
        );
        assert!(p.break_before && p.break_after && p.aligned && p.master_page);
        assert_eq!(p.halign, Align::Center);
        let values = [
            ("start", Align::Left),
            ("left", Align::Left),
            ("center", Align::Center),
            ("end", Align::Right),
            ("right", Align::Right),
            ("justify", Align::None),
        ];
        for (value, want) in values {
            assert_eq!(text_align(value), want, "{value}");
        }
        assert!(
            !s.paragraph(Some("Mid")).master_page,
            "master page is the style's own attribute"
        );
        assert!(
            s.paragraph(Some("Shared")).break_after,
            "content.xml wins over styles.xml"
        );
        assert_eq!(s.paragraph(None), ParaInfo::default());
        assert_eq!(s.paragraph(Some("Unknown")), ParaInfo::default());
        assert_eq!(
            s.paragraph(Some("Loop")).kind,
            ParaKind::Normal,
            "a parent cycle terminates"
        );
        assert!(s.table(Some("Tbl")).break_before);
        assert!(!s.table(Some("P1")).break_before, "families are separate");
        assert!(!s.table(None).break_before);
    }

    #[test]
    fn classifies_paragraph_styles() {
        let named = |name: &str, extra: &str| {
            format!(r#"<style:style style:name="{name}" style:family="paragraph" {extra}/>"#)
        };
        let all = [
            named("Heading_20_2", ""),
            named("Title", ""),
            named("Heading_20_9", ""),
            named("Heading_20_0", ""),
            named("Heading_20_x", ""),
            named("Custom", r#"style:default-outline-level="3""#),
            named("Zero", r#"style:default-outline-level="0""#),
            named("Deep", r#"style:default-outline-level="12""#),
            named("Heading_20_4", ""),
            named("Child", r#"style:parent-style-name="Heading_20_4""#),
            named("Local", r#"style:display-name="Heading 5""#),
            named("Preformatted_20_Text", ""),
            named("Source_Code", ""),
            named("Quotations", ""),
            named("QChild", r#"style:parent-style-name="Quotations""#),
            named("Horizontal_20_Line", ""),
            named("Text_20_body", ""),
        ]
        .concat();
        let s = styles(
            &format!("<office:automatic-styles>{all}</office:automatic-styles>"),
            None,
        );
        let h = |n: &str| s.paragraph(Some(n)).heading;
        assert_eq!(h("Heading_20_2"), Some(2));
        assert_eq!(h("Title"), Some(1));
        assert_eq!(h("Heading_20_9"), Some(6), "levels are clamped");
        assert_eq!(h("Heading_20_0"), None);
        assert_eq!(h("Heading_20_x"), None);
        assert_eq!(h("Custom"), Some(3));
        assert_eq!(h("Zero"), None);
        assert_eq!(h("Deep"), Some(6));
        assert_eq!(h("Child"), Some(4), "through the parent chain");
        assert_eq!(h("Local"), Some(5), "by display name");
        assert_eq!(h("Text_20_body"), None);
        let k = |n: &str| s.paragraph(Some(n)).kind;
        assert_eq!(k("Preformatted_20_Text"), ParaKind::Code);
        assert_eq!(k("Source_Code"), ParaKind::Code);
        assert_eq!(k("Quotations"), ParaKind::Quote);
        assert_eq!(k("QChild"), ParaKind::Quote);
        assert_eq!(k("Horizontal_20_Line"), ParaKind::Rule);
        assert_eq!(k("Text_20_body"), ParaKind::Normal);
    }

    #[test]
    fn code_character_styles_are_monospace() {
        let s = styles(
            r#"<office:automatic-styles><style:style style:name="Source_20_Text" style:family="text"/><style:style style:name="T9" style:family="text" style:parent-style-name="Source_20_Text"/><style:style style:name="Own" style:family="text" style:display-name="Code"><style:text-properties style:font-name="Arial"/></style:style><style:style style:name="Plain" style:family="text"/></office:automatic-styles>"#,
            None,
        );
        assert_eq!(s.text("Source_20_Text").mono, Some(true));
        assert_eq!(s.text("T9").mono, Some(true));
        assert_eq!(
            s.text("Own").mono,
            Some(false),
            "an explicit font wins over the name"
        );
        assert_eq!(s.text("Plain").mono, None);
        assert_eq!(s.text("Nothing"), TextProps::default());
    }

    #[test]
    fn list_styles_describe_each_level() {
        let s = styles(
            r#"<office:automatic-styles><text:list-style style:name="L1"><text:list-level-style-bullet text:level="1" text:bullet-char="&#8226;"/><text:list-level-style-number text:level="2" style:num-format="a" text:start-value="3"/><text:list-level-style-image text:level="3"/><text:list-level-style-number text:level="4" style:num-format=""/><text:list-level-style-number text:level="5" style:num-format="1"/><text:list-level-style-bullet text:level="6" text:bullet-char="&#9744;"/><text:list-level-style-bullet text:level="7" text:bullet-char="&#9745;"/><text:list-level-style-bullet text:level="8" text:bullet-char="&#9746;"/><text:list-level-style-number style:num-format="1"/><text:outline-level-style text:level="9"/></text:list-style><text:list-style/></office:automatic-styles>"#,
            None,
        );
        let l = |level| s.list_level(Some("L1"), level);
        assert_eq!(l(1), ListKind::Bullet);
        assert_eq!(
            l(2),
            ListKind::Number {
                decimal: false,
                start: 3
            }
        );
        assert_eq!(l(3), ListKind::Bullet);
        assert_eq!(l(4), ListKind::Bullet);
        assert_eq!(
            l(5),
            ListKind::Number {
                decimal: true,
                start: 1
            }
        );
        assert_eq!(l(6), ListKind::Task(false));
        assert_eq!(l(7), ListKind::Task(true));
        assert_eq!(l(8), ListKind::Task(true));
        assert_eq!(
            l(12),
            ListKind::Task(true),
            "deeper levels inherit the nearest lower one"
        );
        assert_eq!(s.list_level(Some("Nope"), 1), ListKind::Bullet);
        assert_eq!(s.list_level(None, 1), ListKind::Bullet);
        assert_eq!(s.list_level(Some("L1"), 0), ListKind::Bullet);
    }

    #[test]
    fn counts_page_furniture_and_columns() {
        let s = styles(
            r#"<office:automatic-styles><style:style style:name="Sect1" style:family="section"><style:section-properties><style:columns fo:column-count="2"/></style:section-properties></style:style><style:style style:name="Sect2" style:family="section" style:parent-style-name="Sect1"/><style:style style:name="Sect3" style:family="section"><style:section-properties/></style:style></office:automatic-styles>"#,
            Some(
                r#"<office:automatic-styles><style:page-layout style:name="pm1"><style:page-layout-properties><style:columns fo:column-count="1"/></style:page-layout-properties></style:page-layout><style:page-layout style:name="pm2"><style:page-layout-properties><style:columns fo:column-count="3"/></style:page-layout-properties></style:page-layout><style:page-layout style:name="pm3"/><style:page-layout/></office:automatic-styles><office:master-styles><style:master-page style:name="Standard" style:page-layout-name="pm1"><style:header><text:p>Head</text:p></style:header><style:footer><text:p>  </text:p></style:footer><style:footer-left><text:p><draw:frame/></text:p></style:footer-left><style:header-first><text:p><text:span>Deep</text:span></text:p></style:header-first></style:master-page><style:master-page style:name="Wide" style:page-layout-name="pm2"><style:header-left><text:p>L</text:p></style:header-left><style:footer-first><text:p>F</text:p></style:footer-first><style:other><text:p>X</text:p></style:other></style:master-page><style:master-page style:name="Odd" style:page-layout-name="pm3"/><style:master-page style:name="NoLayout"/></office:master-styles>"#,
            ),
        );
        assert_eq!(s.section_columns("Sect1"), 2);
        assert_eq!(s.section_columns("Sect2"), 2, "inherited");
        assert_eq!(s.section_columns("Sect3"), 1);
        assert_eq!(s.section_columns("Missing"), 1);
        assert_eq!(s.header_footers, 5);
        assert_eq!(s.column_pages, 1);
    }

    #[test]
    fn font_faces_decide_monospace_by_name_not_by_pitch() {
        let s = styles(
            r#"<office:font-face-decls><style:font-face style:name="A" svg:font-family="'Liberation Mono'"/><style:font-face style:name="B" svg:font-family="Foo" style:font-pitch="fixed"/><style:font-face style:name="Courier"/><style:font-face style:name="Sans" svg:font-family="Foo"/><style:font-face style:name="標楷體" svg:font-family="標楷體" style:font-family-generic="script" style:font-pitch="fixed"/><style:font-face/></office:font-face-decls>"#,
            None,
        );
        for (name, want) in [
            ("A", true),
            ("B", false),
            ("Courier", true),
            ("Sans", false),
            ("標楷體", false),
        ] {
            assert_eq!(s.fonts.get(name), Some(&want), "{name}");
        }
    }
}
