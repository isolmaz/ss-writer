//! `word/numbering.xml` for the importer: which `numId`/`ilvl` pairs are bullet or ordered
//! lists, their start values, and the running counters that give each item its number.

use std::collections::HashMap;

use crate::ooxml::{Element, Ns, XML_DECL};

use super::styles::Styles;

/// Deepest list level Word supports (levels 0–8).
pub const LEVELS: usize = 9;

/// How a list level is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Bullet,
    Ordered,
    /// `numFmt` `none`: the paragraph is indented text, not a list item.
    None,
}

/// One level of a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Level {
    pub kind: Kind,
    /// Ordered levels only: the format is plain decimal (otherwise letters/roman numerals are
    /// flattened to decimal and reported).
    pub decimal: bool,
    pub start: u32,
    /// `w:lvlText`.
    pub text: String,
}

impl Level {
    /// Task checkbox state when the level's bullet glyph is a ballot box.
    pub fn task(&self) -> Option<bool> {
        if self.kind != Kind::Bullet {
            return None;
        }
        match self.text.as_str() {
            "\u{2610}" => Some(false),
            "\u{2611}" | "\u{2612}" => Some(true),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Abstract {
    levels: HashMap<u8, Level>,
    /// Number of the list the `numStyleLink` style points to.
    linked_num: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LevelOverride {
    start: Option<u32>,
    level: Option<Level>,
}

#[derive(Debug, Clone, Default)]
struct Num {
    abstract_id: String,
    overrides: HashMap<u8, LevelOverride>,
}

/// All list definitions of a document.
#[derive(Debug, Clone, Default)]
pub struct Numbering {
    abstracts: HashMap<String, Abstract>,
    nums: HashMap<String, Num>,
}

fn val<'a>(parent: &'a Element, name: &str) -> Option<&'a str> {
    parent.child(Ns::W, name)?.attr("val")
}

fn parse_level(lvl: &Element) -> Level {
    let format = val(lvl, "numFmt").unwrap_or("decimal");
    let kind = match format {
        "bullet" => Kind::Bullet,
        "none" => Kind::None,
        _ => Kind::Ordered,
    };
    Level {
        kind,
        decimal: matches!(format, "decimal" | "decimalZero"),
        start: val(lvl, "start").and_then(|v| v.parse().ok()).unwrap_or(1),
        text: val(lvl, "lvlText").unwrap_or("").to_string(),
    }
}

fn level_index(lvl: &Element) -> Option<u8> {
    lvl.attr_ns(Ns::W, "ilvl")?.parse().ok()
}

impl Numbering {
    /// Parses a `w:numbering` root. `styles` resolves `numStyleLink` indirections.
    pub fn parse(root: &Element, styles: &Styles) -> Numbering {
        let mut numbering = Numbering::default();
        for abs in root.elements().filter(|e| e.is(Ns::W, "abstractNum")) {
            let Some(id) = abs.attr_ns(Ns::W, "abstractNumId") else {
                continue;
            };
            let mut entry = Abstract::default();
            for lvl in abs.elements().filter(|e| e.is(Ns::W, "lvl")) {
                if let Some(index) = level_index(lvl) {
                    entry.levels.insert(index, parse_level(lvl));
                }
            }
            if let Some(link) = val(abs, "numStyleLink")
                && let Some(Some(reference)) = styles.num_ref(link)
            {
                entry.linked_num = Some(reference.num_id);
            }
            numbering.abstracts.insert(id.to_string(), entry);
        }
        for num in root.elements().filter(|e| e.is(Ns::W, "num")) {
            let (Some(id), Some(abstract_id)) =
                (num.attr_ns(Ns::W, "numId"), val(num, "abstractNumId"))
            else {
                continue;
            };
            let mut entry = Num {
                abstract_id: abstract_id.to_string(),
                overrides: HashMap::new(),
            };
            for over in num.elements().filter(|e| e.is(Ns::W, "lvlOverride")) {
                if let Some(index) = level_index(over) {
                    entry.overrides.insert(
                        index,
                        LevelOverride {
                            start: val(over, "startOverride").and_then(|v| v.parse().ok()),
                            level: over.child(Ns::W, "lvl").map(parse_level),
                        },
                    );
                }
            }
            numbering.nums.insert(id.to_string(), entry);
        }
        numbering
    }

    /// The effective definition of list `num_id` at `level` (start overrides applied).
    pub fn level(&self, num_id: &str, level: u8) -> Option<Level> {
        self.level_at_depth(num_id, level, 0)
    }

    fn level_at_depth(&self, num_id: &str, level: u8, depth: u8) -> Option<Level> {
        if depth > 4 {
            return None;
        }
        let num = self.nums.get(num_id)?;
        let abs = self.abstracts.get(&num.abstract_id)?;
        let over = num.overrides.get(&level);
        let defined = over
            .and_then(|o| o.level.clone())
            .or_else(|| abs.levels.get(&level).cloned());
        let Some(mut found) = defined else {
            let linked = abs.linked_num.as_deref()?;
            if !abs.levels.is_empty() || linked == num_id {
                return None;
            }
            return self.level_at_depth(linked, level, depth + 1);
        };
        if let Some(start) = over.and_then(|o| o.start) {
            found.start = start;
        }
        Some(found)
    }
}

/// Running item numbers of every list in document order.
#[derive(Debug, Default)]
pub struct Counters {
    by_num: HashMap<String, [Option<u32>; LEVELS]>,
}

impl Counters {
    /// Advances `num_id` at `level` and returns the number the item shows: `start` for the
    /// first item, then one more each time. Deeper levels restart.
    pub fn next(&mut self, num_id: &str, level: u8, start: u32) -> u32 {
        let counters = self.by_num.entry(num_id.to_string()).or_default();
        let index = usize::from(level).min(LEVELS - 1);
        let number = counters[index].map_or(start, |n| n.saturating_add(1));
        counters[index] = Some(number);
        for deeper in &mut counters[index + 1..] {
            *deeper = None;
        }
        number
    }
}

/// `word/numbering.xml` written by the exporter: bullets (`numId` 1), open and done tasks
/// (3, 4) and one decimal instance per ordered list (`numId` `100 + i`, starting at the given
/// number on the given level).
pub fn numbering_xml(ordered: &[(u32, u8)], ns: &str) -> String {
    let level = |i: usize, format: &str, text: &str, font: Option<&str>| {
        let rpr = font
            .map(|f| {
                format!(
                    r#"<w:rPr><w:rFonts w:ascii="{f}" w:hAnsi="{f}" w:hint="default"/></w:rPr>"#
                )
            })
            .unwrap_or_default();
        format!(
            r#"<w:lvl w:ilvl="{i}"><w:start w:val="1"/><w:numFmt w:val="{format}"/><w:lvlText w:val="{text}"/><w:lvlJc w:val="left"/><w:pPr><w:ind w:left="{}" w:hanging="360"/></w:pPr>{rpr}</w:lvl>"#,
            720 * (i + 1)
        )
    };
    let abstract_num = |id: u32, levels: String| {
        format!(
            r#"<w:abstractNum w:abstractNumId="{id}"><w:multiLevelType w:val="hybridMultilevel"/>{levels}</w:abstractNum>"#
        )
    };
    let bullets: String = (0..LEVELS)
        .map(|i| {
            level(
                i,
                "bullet",
                ["\u{2022}", "\u{25E6}", "\u{25AA}"][i % 3],
                None,
            )
        })
        .collect();
    let decimals: String = (0..LEVELS)
        .map(|i| level(i, "decimal", &format!("%{}.", i + 1), None))
        .collect();
    let open: String = (0..LEVELS)
        .map(|i| level(i, "bullet", "\u{2610}", Some("Segoe UI Symbol")))
        .collect();
    let done: String = (0..LEVELS)
        .map(|i| level(i, "bullet", "\u{2611}", Some("Segoe UI Symbol")))
        .collect();
    let mut s = format!(r#"{XML_DECL}<w:numbering {ns}>"#);
    s.push_str(&abstract_num(1, bullets));
    s.push_str(&abstract_num(2, decimals));
    s.push_str(&abstract_num(3, open));
    s.push_str(&abstract_num(4, done));
    s.push_str(r#"<w:num w:numId="1"><w:abstractNumId w:val="1"/></w:num><w:num w:numId="3"><w:abstractNumId w:val="3"/></w:num><w:num w:numId="4"><w:abstractNumId w:val="4"/></w:num>"#);
    for (i, (start, lvl)) in ordered.iter().enumerate() {
        s.push_str(&format!(
            r#"<w:num w:numId="{}"><w:abstractNumId w:val="2"/><w:lvlOverride w:ilvl="{lvl}"><w:startOverride w:val="{start}"/></w:lvlOverride></w:num>"#,
            100 + i
        ));
    }
    s.push_str("</w:numbering>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::parse_xml;

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn lists(body: &str, styles: &str) -> Numbering {
        let styles_xml = format!("<w:styles {W}>{styles}</w:styles>");
        let styles = Styles::parse(&parse_xml(styles_xml.as_bytes()).unwrap());
        let xml = format!("<w:numbering {W}>{body}</w:numbering>");
        Numbering::parse(&parse_xml(xml.as_bytes()).unwrap(), &styles)
    }

    const BODY: &str = r#"
<w:abstractNum w:abstractNumId="0">
  <w:lvl w:ilvl="0"><w:start w:val="1"/><w:numFmt w:val="bullet"/><w:lvlText w:val="&#xF0B7;"/></w:lvl>
  <w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#x2610;"/></w:lvl>
  <w:lvl w:ilvl="2"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#x2611;"/></w:lvl>
  <w:lvl w:ilvl="3"><w:numFmt w:val="bullet"/><w:lvlText w:val="&#x2612;"/></w:lvl>
  <w:lvl><w:numFmt w:val="bullet"/></w:lvl>
</w:abstractNum>
<w:abstractNum w:abstractNumId="1">
  <w:lvl w:ilvl="0"><w:start w:val="3"/><w:numFmt w:val="decimal"/><w:lvlText w:val="%1."/></w:lvl>
  <w:lvl w:ilvl="1"><w:numFmt w:val="lowerLetter"/><w:lvlText w:val="%2)"/></w:lvl>
  <w:lvl w:ilvl="2"><w:numFmt w:val="none"/></w:lvl>
  <w:lvl w:ilvl="3"><w:numFmt w:val="decimalZero"/><w:start w:val="x"/></w:lvl>
  <w:lvl w:ilvl="4"/>
</w:abstractNum>
<w:abstractNum/>
<w:num w:numId="1"><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="2"><w:abstractNumId w:val="1"/></w:num>
<w:num w:numId="3"><w:abstractNumId w:val="1"/>
  <w:lvlOverride w:ilvl="0"><w:startOverride w:val="9"/></w:lvlOverride>
  <w:lvlOverride w:ilvl="1"><w:lvl w:ilvl="1"><w:numFmt w:val="bullet"/><w:lvlText w:val="-"/></w:lvl></w:lvlOverride>
  <w:lvlOverride><w:startOverride w:val="5"/></w:lvlOverride>
</w:num>
<w:num w:numId="4"><w:abstractNumId w:val="99"/></w:num>
<w:num><w:abstractNumId w:val="0"/></w:num>
<w:num w:numId="5"/>"#;

    #[test]
    fn bullet_and_ordered_levels() {
        let n = lists(BODY, "");
        let bullet = n.level("1", 0).unwrap();
        assert_eq!(
            (bullet.kind, bullet.decimal, bullet.start),
            (Kind::Bullet, false, 1)
        );
        assert_eq!(bullet.task(), None);
        assert_eq!(n.level("1", 1).unwrap().task(), Some(false));
        assert_eq!(n.level("1", 2).unwrap().task(), Some(true));
        assert_eq!(n.level("1", 3).unwrap().task(), Some(true));
        assert!(n.level("1", 4).is_none());
        let decimal = n.level("2", 0).unwrap();
        assert_eq!(
            (decimal.kind, decimal.decimal, decimal.start),
            (Kind::Ordered, true, 3)
        );
        assert_eq!(decimal.text, "%1.");
        assert_eq!(decimal.task(), None);
        let letters = n.level("2", 1).unwrap();
        assert_eq!((letters.kind, letters.decimal), (Kind::Ordered, false));
        assert_eq!(n.level("2", 2).unwrap().kind, Kind::None);
        let zero = n.level("2", 3).unwrap();
        assert!(zero.decimal);
        assert_eq!(zero.start, 1);
        let defaulted = n.level("2", 4).unwrap();
        assert_eq!((defaulted.kind, defaulted.decimal), (Kind::Ordered, true));
    }

    #[test]
    fn overrides_and_dangling_references() {
        let n = lists(BODY, "");
        assert_eq!(n.level("3", 0).unwrap().start, 9);
        let replaced = n.level("3", 1).unwrap();
        assert_eq!((replaced.kind, replaced.text.as_str()), (Kind::Bullet, "-"));
        assert_eq!(n.level("3", 2).unwrap().kind, Kind::None);
        assert!(n.level("4", 0).is_none());
        assert!(n.level("5", 0).is_none());
        assert!(n.level("missing", 0).is_none());
    }

    #[test]
    fn numbering_style_links_resolve_through_styles() {
        let styles = r#"<w:style w:type="paragraph" w:styleId="LS"><w:name w:val="List Style"/><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="Self"><w:name w:val="Self"/><w:pPr><w:numPr><w:numId w:val="7"/></w:numPr></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="NoNum"><w:name w:val="NoNum"/></w:style>"#;
        let body = format!(
            r#"{BODY}
<w:abstractNum w:abstractNumId="10"><w:numStyleLink w:val="LS"/></w:abstractNum>
<w:abstractNum w:abstractNumId="11"><w:numStyleLink w:val="Self"/></w:abstractNum>
<w:abstractNum w:abstractNumId="12"><w:numStyleLink w:val="NoNum"/></w:abstractNum>
<w:abstractNum w:abstractNumId="13"><w:numStyleLink w:val="LS"/><w:lvl w:ilvl="0"><w:numFmt w:val="bullet"/></w:lvl></w:abstractNum>
<w:num w:numId="6"><w:abstractNumId w:val="10"/></w:num>
<w:num w:numId="7"><w:abstractNumId w:val="11"/></w:num>
<w:num w:numId="8"><w:abstractNumId w:val="12"/></w:num>
<w:num w:numId="9"><w:abstractNumId w:val="13"/></w:num>"#
        );
        let n = lists(&body, styles);
        assert_eq!(n.level("6", 0).unwrap().start, 3);
        assert!(n.level("7", 0).is_none());
        assert!(n.level("8", 0).is_none());
        // A definition with its own levels does not follow the link for missing levels.
        assert_eq!(n.level("9", 0).unwrap().kind, Kind::Bullet);
        assert!(n.level("9", 1).is_none());
    }

    #[test]
    fn link_chains_are_bounded() {
        let styles = r#"<w:style w:type="paragraph" w:styleId="A"><w:name w:val="A"/><w:pPr><w:numPr><w:numId w:val="2"/></w:numPr></w:pPr></w:style>
<w:style w:type="paragraph" w:styleId="B"><w:name w:val="B"/><w:pPr><w:numPr><w:numId w:val="1"/></w:numPr></w:pPr></w:style>"#;
        let body = r#"<w:abstractNum w:abstractNumId="1"><w:numStyleLink w:val="A"/></w:abstractNum>
<w:abstractNum w:abstractNumId="2"><w:numStyleLink w:val="B"/></w:abstractNum>
<w:num w:numId="1"><w:abstractNumId w:val="1"/></w:num>
<w:num w:numId="2"><w:abstractNumId w:val="2"/></w:num>"#;
        let n = lists(body, styles);
        assert!(n.level("1", 0).is_none());
    }

    #[test]
    fn counters_count_and_restart_deeper_levels() {
        let mut c = Counters::default();
        assert_eq!(c.next("a", 0, 1), 1);
        assert_eq!(c.next("a", 0, 1), 2);
        assert_eq!(c.next("a", 1, 1), 1);
        assert_eq!(c.next("a", 1, 1), 2);
        assert_eq!(c.next("a", 0, 1), 3);
        assert_eq!(c.next("a", 1, 5), 5);
        assert_eq!(c.next("b", 0, 7), 7);
        assert_eq!(c.next("a", 0, 1), 4);
        assert_eq!(c.next("b", 0, 7), 8);
        assert_eq!(c.next("a", 30, 1), 1);
        assert_eq!(c.next("z", 0, 0), 0);
        assert_eq!(c.next("z", 0, 0), 1);
        assert_eq!(c.next("max", 0, u32::MAX), u32::MAX);
        assert_eq!(c.next("max", 0, u32::MAX), u32::MAX);
    }
}
