//! `word/styles.xml` for the importer: style names, `basedOn` chains, outline levels, list
//! properties and character-style run formatting.

use std::collections::HashMap;

use ssw_core::markdown::ast::HighlightColor;

use crate::ooxml::{Element, Ns, XML_DECL, on_off};

use super::write::ExportOptions;

/// Longest `basedOn` chain followed (guards against cycles).
const MAX_CHAIN: usize = 24;

/// Heading style-name prefixes of the languages Word ships in (matched case-insensitively,
/// followed by a level digit).
const HEADING_PREFIXES: [&str; 22] = [
    "heading",
    "überschrift",
    "titre",
    "título",
    "titulo",
    "başlık",
    "baslik",
    "заголовок",
    "nagłówek",
    "rubrik",
    "kop",
    "otsikko",
    "intestazione",
    "overskrift",
    "nadpis",
    "címsor",
    "επικεφαλίδα",
    "标题",
    "標題",
    "見出し",
    "제목",
    "cabeçalho",
];

/// Substrings of paragraph style names that mark preformatted code.
const CODE_STYLE_NAMES: [&str; 6] = [
    "code",
    "source code",
    "preformatted",
    "macro text",
    "verbatim",
    "listing",
];

/// Font names (lower case) that are monospace without containing "mono".
const MONO_FONTS: [&str; 6] = [
    "courier new",
    "courier",
    "consolas",
    "lucida console",
    "menlo",
    "fira code",
];

/// Direct or style-derived character formatting the importer understands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunProps {
    pub bold: Option<bool>,
    pub italic: Option<bool>,
    pub strike: Option<bool>,
    pub underline: Option<bool>,
    /// `w:vertAlign` superscript or subscript.
    pub sup_sub: Option<bool>,
    /// `w:vanish` or `w:specVanish`.
    pub hidden: Option<bool>,
    pub highlight: Option<HighlightColor>,
    /// Explicit text color other than automatic/black.
    pub colored: bool,
    /// Explicit shading fill other than automatic/white.
    pub shaded: bool,
    /// Monospace font.
    pub mono: bool,
    /// `w:rStyle`.
    pub style: Option<String>,
}

impl RunProps {
    /// Reads a `w:rPr` element.
    pub fn from_rpr(rpr: &Element) -> RunProps {
        let mut p = RunProps::default();
        for e in rpr.elements().filter(|e| e.ns == Ns::W) {
            let val = e.attr("val");
            match e.name.as_str() {
                "b" => p.bold = Some(on_off(val)),
                "i" => p.italic = Some(on_off(val)),
                "strike" | "dstrike" => p.strike = Some(on_off(val)),
                "u" => p.underline = Some(val.is_some_and(|v| v != "none")),
                "vertAlign" => {
                    p.sup_sub = Some(matches!(val, Some("superscript" | "subscript")));
                }
                "vanish" | "specVanish" => p.hidden = Some(on_off(val)),
                "highlight" => p.highlight = val.and_then(highlight_color),
                "color" => {
                    p.colored = val.is_some_and(|v| {
                        !v.eq_ignore_ascii_case("auto") && !v.eq_ignore_ascii_case("000000")
                    });
                }
                "shd" => {
                    p.shaded = e.attr("fill").is_some_and(|v| {
                        !v.eq_ignore_ascii_case("auto") && !v.eq_ignore_ascii_case("FFFFFF")
                    });
                }
                "rFonts" => {
                    p.mono = ["ascii", "hAnsi", "cs"]
                        .iter()
                        .filter_map(|a| e.attr(a))
                        .any(is_mono_font);
                }
                "rStyle" => p.style = val.map(str::to_string),
                _ => {}
            }
        }
        p
    }

    /// Overlays `other` (more specific) on `self`.
    pub fn overlay(&mut self, other: &RunProps) {
        self.bold = other.bold.or(self.bold);
        self.italic = other.italic.or(self.italic);
        self.strike = other.strike.or(self.strike);
        self.underline = other.underline.or(self.underline);
        self.sup_sub = other.sup_sub.or(self.sup_sub);
        self.hidden = other.hidden.or(self.hidden);
        self.highlight = other.highlight.or(self.highlight);
        self.colored |= other.colored;
        self.shaded |= other.shaded;
        self.mono |= other.mono;
        if other.style.is_some() {
            self.style.clone_from(&other.style);
        }
    }
}

fn is_mono_font(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("mono") || MONO_FONTS.contains(&lower.as_str())
}

/// Nearest of the six highlight colors to a Word highlight name; `None` for "none".
pub fn highlight_color(name: &str) -> Option<HighlightColor> {
    match name {
        "yellow" | "darkYellow" | "lightGray" | "darkGray" | "black" | "white" => {
            Some(HighlightColor::Yellow)
        }
        "green" | "darkGreen" => Some(HighlightColor::Green),
        "blue" | "darkBlue" | "cyan" | "darkCyan" => Some(HighlightColor::Blue),
        "magenta" | "darkMagenta" => Some(HighlightColor::Purple),
        "red" | "darkRed" => Some(HighlightColor::Red),
        _ => None,
    }
}

/// Paragraph-level list properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumRef {
    pub num_id: String,
    pub level: u8,
}

/// One style definition.
#[derive(Debug, Clone, Default)]
pub struct Style {
    pub name: String,
    pub based_on: Option<String>,
    pub character: bool,
    /// `w:outlineLvl` in the style's paragraph properties (0–9).
    pub outline: Option<u8>,
    pub num: Option<NumRef>,
    /// `w:numId` of `0`: the style explicitly removes numbering.
    pub no_num: bool,
    pub run: RunProps,
}

/// All styles of a document, by style id.
#[derive(Debug, Clone, Default)]
pub struct Styles {
    map: HashMap<String, Style>,
}

/// Reads `w:numPr` of a `w:pPr`: `Some(None)` for an explicit "no numbering", `Some(Some(_))`
/// for a list reference, `None` when absent.
pub fn read_num_pr(ppr: &Element) -> Option<Option<NumRef>> {
    let num_pr = ppr.child(Ns::W, "numPr")?;
    let num_id = num_pr.child(Ns::W, "numId").and_then(|e| e.attr("val"))?;
    if num_id == "0" {
        return Some(None);
    }
    let level = num_pr
        .child(Ns::W, "ilvl")
        .and_then(|e| e.attr("val"))
        .and_then(|v| v.parse::<u8>().ok())
        .unwrap_or(0)
        .min(8);
    Some(Some(NumRef {
        num_id: num_id.to_string(),
        level,
    }))
}

/// Maps `w:outlineLvl` to a Markdown heading level: 0–5 → 1–6, 6–8 → 6, 9 (body text) → none.
pub fn outline_to_level(outline: u8) -> Option<u8> {
    match outline {
        0..=5 => Some(outline + 1),
        6..=8 => Some(6),
        _ => None,
    }
}

/// Heading level encoded in a style name (`heading 2`, `Überschrift 3`, `Title`).
fn heading_level_from_name(name: &str) -> Option<u8> {
    let lower = name.trim().to_lowercase();
    if lower == "title" {
        return Some(1);
    }
    let (prefix, digits) = lower.split_at(lower.find(|c: char| c.is_ascii_digit())?);
    if digits.len() != 1 || !HEADING_PREFIXES.contains(&prefix.trim()) {
        return None;
    }
    let level: u8 = digits.parse().ok()?;
    (1..=9).contains(&level).then_some(level.min(6))
}

impl Styles {
    /// Parses a `w:styles` root.
    pub fn parse(root: &Element) -> Styles {
        let mut map = HashMap::new();
        for style in root.elements().filter(|e| e.is(Ns::W, "style")) {
            let Some(id) = style.attr_ns(Ns::W, "styleId") else {
                continue;
            };
            let mut s = Style {
                name: style
                    .child(Ns::W, "name")
                    .and_then(|e| e.attr("val"))
                    .unwrap_or(id)
                    .to_string(),
                based_on: style
                    .child(Ns::W, "basedOn")
                    .and_then(|e| e.attr("val"))
                    .map(str::to_string),
                character: style.attr_ns(Ns::W, "type") == Some("character"),
                ..Style::default()
            };
            if let Some(ppr) = style.child(Ns::W, "pPr") {
                s.outline = ppr
                    .child(Ns::W, "outlineLvl")
                    .and_then(|e| e.attr("val"))
                    .and_then(|v| v.parse().ok());
                match read_num_pr(ppr) {
                    Some(Some(num)) => s.num = Some(num),
                    Some(None) => s.no_num = true,
                    None => {}
                }
            }
            if let Some(rpr) = style.child(Ns::W, "rPr") {
                s.run = RunProps::from_rpr(rpr);
            }
            map.insert(id.to_string(), s);
        }
        Styles { map }
    }

    /// Style with this id.
    pub fn get(&self, id: &str) -> Option<&Style> {
        self.map.get(id)
    }

    /// Display name of a style (`heading 1`), or the id when the style is unknown.
    pub fn name<'a>(&'a self, id: &'a str) -> &'a str {
        self.map.get(id).map_or(id, |s| s.name.as_str())
    }

    /// The style followed by its `basedOn` ancestors.
    fn chain(&self, id: &str) -> Vec<&Style> {
        let mut out = Vec::new();
        let mut next = Some(id);
        while let Some(current) = next {
            if out.len() >= MAX_CHAIN {
                break;
            }
            let Some(style) = self.map.get(current) else {
                break;
            };
            out.push(style);
            next = style.based_on.as_deref();
        }
        out
    }

    /// Heading level (1–6) of a paragraph style, from outline levels and localized names along
    /// the `basedOn` chain; `None` for body styles.
    pub fn heading_level(&self, id: &str) -> Option<u8> {
        for style in self.chain(id) {
            if let Some(outline) = style.outline {
                return outline_to_level(outline);
            }
            if let Some(level) = heading_level_from_name(&style.name) {
                return Some(level);
            }
        }
        None
    }

    /// List reference a paragraph style carries (`Some(None)`: the style removes numbering).
    pub fn num_ref(&self, id: &str) -> Option<Option<NumRef>> {
        for style in self.chain(id) {
            if let Some(num) = &style.num {
                return Some(Some(num.clone()));
            }
            if style.no_num {
                return Some(None);
            }
        }
        None
    }

    /// True when the paragraph style (or an ancestor) is a preformatted/code style.
    pub fn is_code_paragraph(&self, id: &str) -> bool {
        self.chain(id).iter().any(|s| {
            let lower = s.name.to_lowercase();
            CODE_STYLE_NAMES.iter().any(|c| lower.contains(c)) && !lower.contains("char")
        })
    }

    /// True when the paragraph style (or an ancestor) is a block quote style.
    pub fn is_quote_paragraph(&self, id: &str) -> bool {
        self.chain(id).iter().any(|s| {
            matches!(
                s.name.to_lowercase().as_str(),
                "quote" | "intense quote" | "block text" | "blockquote" | "block quotation"
            )
        })
    }

    /// Character formatting of a character style, ancestors first. The `Hyperlink` style
    /// contributes nothing (its underline is link chrome, not user formatting).
    pub fn char_props(&self, id: &str) -> RunProps {
        let mut props = RunProps::default();
        for style in self.chain(id).into_iter().rev() {
            if !style.character {
                continue;
            }
            if style.name.eq_ignore_ascii_case("hyperlink") {
                continue;
            }
            props.overlay(&style.run);
            let lower = style.name.to_lowercase();
            if lower.contains("code") || lower.contains("verbatim") {
                props.mono = true;
            }
        }
        props
    }
}

/// `word/styles.xml` written by the exporter.
pub fn styles_xml(options: &ExportOptions, ns: &str) -> String {
    let mono = r#"<w:rFonts w:ascii="Cascadia Mono" w:hAnsi="Cascadia Mono" w:eastAsia="Cascadia Mono" w:cs="Cascadia Mono"/>"#;
    let size = options.font_half_points;
    let heading_color = if options.black_text {
        "000000"
    } else {
        "1F3864"
    };
    let quote_color = if options.black_text {
        "000000"
    } else {
        "595959"
    };
    let mut s = format!(
        r#"{XML_DECL}<w:styles {ns}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii="Calibri" w:hAnsi="Calibri" w:eastAsia="Calibri" w:cs="Calibri"/><w:sz w:val="{size}"/><w:szCs w:val="{size}"/><w:lang w:val="en-US"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after="120" w:line="264" w:lineRule="auto"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/><w:qFormat/></w:style><w:style w:type="character" w:default="1" w:styleId="DefaultParagraphFont"><w:name w:val="Default Paragraph Font"/><w:uiPriority w:val="1"/><w:semiHidden/><w:unhideWhenUsed/></w:style><w:style w:type="table" w:default="1" w:styleId="TableNormal"><w:name w:val="Normal Table"/><w:uiPriority w:val="99"/><w:semiHidden/><w:unhideWhenUsed/><w:tblPr><w:tblInd w:w="0" w:type="dxa"/><w:tblCellMar><w:top w:w="0" w:type="dxa"/><w:left w:w="108" w:type="dxa"/><w:bottom w:w="0" w:type="dxa"/><w:right w:w="108" w:type="dxa"/></w:tblCellMar></w:tblPr></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="10"/><w:qFormat/><w:pPr><w:spacing w:after="240"/></w:pPr><w:rPr><w:b/><w:bCs/><w:color w:val="{heading_color}"/><w:sz w:val="52"/><w:szCs w:val="52"/></w:rPr></w:style>"#
    );
    for (n, sz) in [(1, 40), (2, 34), (3, 30), (4, 26), (5, 24), (6, 22)] {
        s.push_str(&format!(
            r#"<w:style w:type="paragraph" w:styleId="Heading{n}"><w:name w:val="heading {n}"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="9"/><w:qFormat/><w:pPr><w:keepNext/><w:keepLines/><w:spacing w:before="240" w:after="80"/><w:outlineLvl w:val="{}"/></w:pPr><w:rPr><w:b/><w:bCs/><w:color w:val="{heading_color}"/><w:sz w:val="{sz}"/><w:szCs w:val="{sz}"/></w:rPr></w:style>"#,
            n - 1
        ));
    }
    s.push_str(r#"<w:style w:type="paragraph" w:styleId="TableHeader"><w:name w:val="Table Header"/><w:basedOn w:val="Normal"/><w:qFormat/><w:rPr><w:b/><w:bCs/></w:rPr></w:style>"#);
    s.push_str(&format!(
        r#"<w:style w:type="paragraph" w:styleId="Quote"><w:name w:val="Quote"/><w:basedOn w:val="Normal"/><w:next w:val="Normal"/><w:uiPriority w:val="29"/><w:qFormat/><w:pPr><w:pBdr><w:left w:val="single" w:sz="18" w:space="8" w:color="A6A6A6"/></w:pBdr><w:ind w:left="567" w:right="567"/></w:pPr><w:rPr><w:i/><w:iCs/><w:color w:val="{quote_color}"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="CodeBlock"><w:name w:val="Code Block"/><w:basedOn w:val="Normal"/><w:qFormat/><w:pPr><w:pBdr><w:left w:val="single" w:sz="4" w:space="4" w:color="D9D9D9"/></w:pBdr><w:shd w:val="clear" w:color="auto" w:fill="F2F2F2"/><w:spacing w:before="0" w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr>{mono}<w:sz w:val="19"/><w:szCs w:val="19"/></w:rPr></w:style><w:style w:type="character" w:styleId="InlineCode"><w:name w:val="Inline Code"/><w:qFormat/><w:rPr>{mono}<w:color w:val="C7254E"/><w:sz w:val="20"/><w:szCs w:val="20"/></w:rPr></w:style><w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:uiPriority w:val="99"/><w:unhideWhenUsed/><w:rPr><w:color w:val="0563C1"/><w:u w:val="single"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="ListParagraph"><w:name w:val="List Paragraph"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="34"/><w:qFormat/><w:pPr><w:spacing w:after="40"/><w:contextualSpacing/></w:pPr></w:style><w:style w:type="paragraph" w:styleId="FootnoteText"><w:name w:val="footnote text"/><w:basedOn w:val="Normal"/><w:uiPriority w:val="99"/><w:unhideWhenUsed/><w:pPr><w:spacing w:after="0" w:line="240" w:lineRule="auto"/></w:pPr><w:rPr><w:sz w:val="18"/><w:szCs w:val="18"/></w:rPr></w:style><w:style w:type="character" w:styleId="FootnoteReference"><w:name w:val="footnote reference"/><w:uiPriority w:val="99"/><w:unhideWhenUsed/><w:rPr><w:vertAlign w:val="superscript"/></w:rPr></w:style></w:styles>"#
    ));
    s
}

/// `word/settings.xml` written by the exporter.
pub fn settings_xml(ns: &str) -> String {
    format!(
        r#"{XML_DECL}<w:settings {ns}><w:defaultTabStop w:val="720"/><w:footnotePr><w:footnote w:id="-1"/><w:footnote w:id="0"/></w:footnotePr><w:compat><w:compatSetting w:name="compatibilityMode" w:uri="http://schemas.microsoft.com/office/word" w:val="15"/></w:compat></w:settings>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::parse_xml;

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn styles(body: &str) -> Styles {
        let xml = format!("<w:styles {W}>{body}</w:styles>");
        Styles::parse(&parse_xml(xml.as_bytes()).unwrap())
    }

    fn rpr(inner: &str) -> RunProps {
        let xml = format!("<w:rPr {W}>{inner}</w:rPr>");
        RunProps::from_rpr(&parse_xml(xml.as_bytes()).unwrap())
    }

    #[test]
    fn heading_levels_from_names_outlines_and_chains() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="H1"><w:name w:val="heading 1"/></w:style>
<w:style w:type="paragraph" w:styleId="Ueb"><w:name w:val="Überschrift 3"/></w:style>
<w:style w:type="paragraph" w:styleId="Tr"><w:name w:val="Başlık 2"/></w:style>
<w:style w:type="paragraph" w:styleId="Deep"><w:name w:val="heading 9"/></w:style>
<w:style w:type="paragraph" w:styleId="Child"><w:name w:val="My Chapter"/><w:basedOn w:val="H1"/></w:style>
<w:style w:type="paragraph" w:styleId="Outl"><w:name w:val="Custom"/><w:pPr><w:outlineLvl w:val="1"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="OutlDeep"><w:name w:val="Custom2"/><w:pPr><w:outlineLvl w:val="7"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="TocH"><w:name w:val="TOC Heading"/><w:basedOn w:val="H1"/><w:pPr><w:outlineLvl w:val="9"/></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/></w:style>
<w:style w:type="paragraph" w:styleId="Body"><w:name w:val="Body Text"/></w:style>
<w:style w:type="paragraph" w:styleId="Odd"><w:name w:val="Foo 12"/></w:style>
<w:style w:type="paragraph" w:styleId="Odd2"><w:name w:val="Foo 2"/></w:style>
<w:style w:type="paragraph" w:styleId="Zero"><w:name w:val="heading 0"/></w:style>
<w:style w:type="paragraph" w:styleId="NoName"><w:basedOn w:val="Missing"/></w:style>
<w:style w:type="paragraph"><w:name w:val="no id"/></w:style>"#,
        );
        let cases = [
            ("H1", Some(1)),
            ("Ueb", Some(3)),
            ("Tr", Some(2)),
            ("Deep", Some(6)),
            ("Child", Some(1)),
            ("Outl", Some(2)),
            ("OutlDeep", Some(6)),
            ("TocH", None),
            ("Title", Some(1)),
            ("Body", None),
            ("Odd", None),
            ("Odd2", None),
            ("Zero", None),
            ("NoName", None),
            ("Unknown", None),
        ];
        for (id, want) in cases {
            assert_eq!(s.heading_level(id), want, "{id}");
        }
        assert_eq!(s.name("H1"), "heading 1");
        assert_eq!(s.name("Nope"), "Nope");
        assert_eq!(s.name("NoName"), "NoName");
        assert!(s.get("H1").is_some());
        assert!(s.get("nope").is_none());
    }

    #[test]
    fn style_chain_stops_on_cycles() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="A"><w:name w:val="A"/><w:basedOn w:val="B"/></w:style>
<w:style w:type="paragraph" w:styleId="B"><w:name w:val="B"/><w:basedOn w:val="A"/></w:style>"#,
        );
        assert_eq!(s.heading_level("A"), None);
    }

    #[test]
    fn numbering_references_follow_the_chain() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="LB"><w:name w:val="List Bullet"/><w:pPr><w:numPr><w:ilvl w:val="2"/><w:numId w:val="7"/></w:numPr></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="LB2"><w:name w:val="List Bullet 2"/><w:basedOn w:val="LB"/></w:style>
<w:style w:type="paragraph" w:styleId="NoList"><w:name w:val="No"/><w:basedOn w:val="LB"/><w:pPr><w:numPr><w:numId w:val="0"/></w:numPr></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="Plain"><w:name w:val="Plain"/></w:style>"#,
        );
        assert_eq!(
            s.num_ref("LB2"),
            Some(Some(NumRef {
                num_id: "7".into(),
                level: 2
            }))
        );
        assert_eq!(s.num_ref("NoList"), Some(None));
        assert_eq!(s.num_ref("Plain"), None);
        assert_eq!(s.num_ref("Unknown"), None);
    }

    #[test]
    fn code_and_quote_styles() {
        let s = styles(
            r#"<w:style w:type="paragraph" w:styleId="SC"><w:name w:val="Source Code"/></w:style>
<w:style w:type="paragraph" w:styleId="HP"><w:name w:val="HTML Preformatted"/></w:style>
<w:style w:type="character" w:styleId="SCC"><w:name w:val="Source Code Char"/></w:style>
<w:style w:type="paragraph" w:styleId="SC2"><w:name w:val="Mine"/><w:basedOn w:val="SC"/></w:style>
<w:style w:type="paragraph" w:styleId="Q"><w:name w:val="Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="IQ"><w:name w:val="Intense Quote"/></w:style>
<w:style w:type="paragraph" w:styleId="QC"><w:name w:val="Mine2"/><w:basedOn w:val="Q"/></w:style>
<w:style w:type="paragraph" w:styleId="N"><w:name w:val="Normal"/></w:style>"#,
        );
        for (id, code) in [
            ("SC", true),
            ("HP", true),
            ("SCC", false),
            ("SC2", true),
            ("N", false),
        ] {
            assert_eq!(s.is_code_paragraph(id), code, "{id}");
        }
        for (id, quote) in [
            ("Q", true),
            ("IQ", true),
            ("QC", true),
            ("N", false),
            ("SC", false),
        ] {
            assert_eq!(s.is_quote_paragraph(id), quote, "{id}");
        }
    }

    #[test]
    fn character_style_props_resolve_ancestors_first() {
        let s = styles(
            r#"<w:style w:type="character" w:styleId="Strong"><w:name w:val="Strong"/><w:rPr><w:b/></w:rPr></w:style>
<w:style w:type="character" w:styleId="StrongEm"><w:name w:val="Strong Emphasis"/><w:basedOn w:val="Strong"/><w:rPr><w:b w:val="0"/><w:i/></w:rPr></w:style>
<w:style w:type="character" w:styleId="Hyperlink"><w:name w:val="Hyperlink"/><w:rPr><w:u w:val="single"/><w:color w:val="0563C1"/></w:rPr></w:style>
<w:style w:type="character" w:styleId="IC"><w:name w:val="Inline Code"/></w:style>
<w:style w:type="character" w:styleId="Verb"><w:name w:val="VerbatimChar"/></w:style>
<w:style w:type="paragraph" w:styleId="P"><w:name w:val="P"/><w:rPr><w:b/></w:rPr></w:style>"#,
        );
        assert_eq!(s.char_props("Strong").bold, Some(true));
        let both = s.char_props("StrongEm");
        assert_eq!(both.bold, Some(false));
        assert_eq!(both.italic, Some(true));
        let link = s.char_props("Hyperlink");
        assert_eq!(link.underline, None);
        assert!(!link.colored);
        assert!(s.char_props("IC").mono);
        assert!(s.char_props("Verb").mono);
        assert_eq!(s.char_props("P"), RunProps::default());
        assert_eq!(s.char_props("missing"), RunProps::default());
    }

    #[test]
    fn run_properties() {
        let p = rpr(
            r#"<w:rStyle w:val="Emph"/><w:b/><w:i w:val="0"/><w:strike/><w:u w:val="single"/><w:vertAlign w:val="superscript"/><w:vanish/><w:highlight w:val="magenta"/><w:color w:val="FF0000"/><w:shd w:val="clear" w:fill="FFFF00"/><w:rFonts w:ascii="Consolas"/>"#,
        );
        assert_eq!(p.style.as_deref(), Some("Emph"));
        assert_eq!(p.bold, Some(true));
        assert_eq!(p.italic, Some(false));
        assert_eq!(p.strike, Some(true));
        assert_eq!(p.underline, Some(true));
        assert_eq!(p.sup_sub, Some(true));
        assert_eq!(p.hidden, Some(true));
        assert_eq!(p.highlight, Some(HighlightColor::Purple));
        assert!(p.colored && p.shaded && p.mono);

        let q = rpr(
            r#"<w:u w:val="none"/><w:vertAlign w:val="baseline"/><w:dstrike w:val="false"/><w:highlight w:val="none"/><w:color w:val="auto"/><w:shd w:val="clear" w:fill="auto"/><w:rFonts w:ascii="Calibri" w:hAnsi="Cascadia Mono"/><w:unknown/><w:specVanish w:val="0"/>"#,
        );
        assert_eq!(q.underline, Some(false));
        assert_eq!(q.sup_sub, Some(false));
        assert_eq!(q.strike, Some(false));
        assert_eq!(q.highlight, None);
        assert_eq!(q.hidden, Some(false));
        assert!(!q.colored && !q.shaded);
        assert!(q.mono);

        let r = rpr(
            r#"<w:u/><w:color w:val="000000"/><w:shd w:fill="FFFFFF"/><w:rFonts w:ascii="Calibri"/>"#,
        );
        assert_eq!(r.underline, Some(false));
        assert!(!r.colored && !r.shaded && !r.mono);
    }

    #[test]
    fn overlay_prefers_the_more_specific_values() {
        let mut base = rpr(r#"<w:b/><w:i/><w:color w:val="FF0000"/>"#);
        let top = rpr(
            r#"<w:b w:val="0"/><w:rStyle w:val="X"/><w:rFonts w:ascii="Courier"/><w:highlight w:val="red"/>"#,
        );
        base.overlay(&top);
        assert_eq!(base.bold, Some(false));
        assert_eq!(base.italic, Some(true));
        assert_eq!(base.highlight, Some(HighlightColor::Red));
        assert_eq!(base.style.as_deref(), Some("X"));
        assert!(base.colored && base.mono);
        base.overlay(&RunProps::default());
        assert_eq!(base.style.as_deref(), Some("X"));
    }

    #[test]
    fn highlight_names_map_to_the_six_colors() {
        let cases = [
            ("yellow", Some(HighlightColor::Yellow)),
            ("darkYellow", Some(HighlightColor::Yellow)),
            ("lightGray", Some(HighlightColor::Yellow)),
            ("darkGray", Some(HighlightColor::Yellow)),
            ("black", Some(HighlightColor::Yellow)),
            ("white", Some(HighlightColor::Yellow)),
            ("green", Some(HighlightColor::Green)),
            ("darkGreen", Some(HighlightColor::Green)),
            ("blue", Some(HighlightColor::Blue)),
            ("darkBlue", Some(HighlightColor::Blue)),
            ("cyan", Some(HighlightColor::Blue)),
            ("darkCyan", Some(HighlightColor::Blue)),
            ("magenta", Some(HighlightColor::Purple)),
            ("darkMagenta", Some(HighlightColor::Purple)),
            ("red", Some(HighlightColor::Red)),
            ("darkRed", Some(HighlightColor::Red)),
            ("none", None),
        ];
        for (name, want) in cases {
            assert_eq!(highlight_color(name), want, "{name}");
        }
    }

    #[test]
    fn numbering_property_reader() {
        let parse = |inner: &str| {
            let xml = format!("<w:pPr {W}>{inner}</w:pPr>");
            read_num_pr(&parse_xml(xml.as_bytes()).unwrap())
        };
        assert_eq!(parse(""), None);
        assert_eq!(parse("<w:numPr><w:ilvl w:val=\"1\"/></w:numPr>"), None);
        assert_eq!(
            parse("<w:numPr><w:numId w:val=\"0\"/></w:numPr>"),
            Some(None)
        );
        assert_eq!(
            parse("<w:numPr><w:numId w:val=\"3\"/></w:numPr>"),
            Some(Some(NumRef {
                num_id: "3".into(),
                level: 0
            }))
        );
        assert_eq!(
            parse("<w:numPr><w:ilvl w:val=\"12\"/><w:numId w:val=\"3\"/></w:numPr>"),
            Some(Some(NumRef {
                num_id: "3".into(),
                level: 8
            }))
        );
        assert_eq!(
            parse("<w:numPr><w:ilvl w:val=\"x\"/><w:numId w:val=\"3\"/></w:numPr>"),
            Some(Some(NumRef {
                num_id: "3".into(),
                level: 0
            }))
        );
        assert_eq!(outline_to_level(0), Some(1));
        assert_eq!(outline_to_level(5), Some(6));
        assert_eq!(outline_to_level(8), Some(6));
        assert_eq!(outline_to_level(9), None);
    }
}
