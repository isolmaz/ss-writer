//! Style sheet embedded in HTML and ePub exports: the editor's light theme, or its dark theme
//! when the export asks for it ([`Theme`]).

use std::sync::LazyLock;

use ssw_core::markdown::ast::{CalloutKind, HighlightColor};

/// Accent color of the light theme.
pub const ACCENT: &str = "#1874D3";

/// Background color of a highlight, as used both by the style sheet and by inline styles.
pub const fn highlight_hex(color: Option<HighlightColor>) -> &'static str {
    match color {
        None | Some(HighlightColor::Yellow) => "#FFF3A3",
        Some(HighlightColor::Red) => "#FFC9C9",
        Some(HighlightColor::Orange) => "#FFD8A8",
        Some(HighlightColor::Green) => "#C3F0C8",
        Some(HighlightColor::Blue) => "#C5DDFB",
        Some(HighlightColor::Purple) => "#E3D0F8",
    }
}

/// Accent color of a callout.
pub const fn callout_hex(kind: CalloutKind) -> &'static str {
    match kind {
        CalloutKind::Note => "#1874D3",
        CalloutKind::Tip => "#1E8A3E",
        CalloutKind::Important => "#8250DF",
        CalloutKind::Warning => "#BF7600",
        CalloutKind::Caution => "#CF222E",
    }
}

const BASE: &str = "\
:root{color-scheme:light}
html{-webkit-text-size-adjust:100%}
body{margin:0;background:#fff;color:#1b1b1b;font-family:'Segoe UI Variable Text','Segoe UI',system-ui,sans-serif;font-size:16px;line-height:1.75}
main,article{max-width:46rem;margin:0 auto;padding:2.5rem 1.5rem}
h1,h2,h3,h4,h5,h6{font-weight:600;line-height:1.3;margin:1.6em 0 .6em}
h1{font-size:1.75em}h2{font-size:1.45em}h3{font-size:1.25em}h4{font-size:1.1em}h5,h6{font-size:1em}
p{margin:0 0 1em}
a{color:#1874D3;text-decoration:none}a:hover{text-decoration:underline}
code,pre{font-family:'Cascadia Mono',Consolas,monospace;font-size:.9em}
code{background:#f3f3f3;border-radius:4px;padding:.1em .35em}
pre{background:#f3f3f3;border-radius:6px;padding:.9em 1.1em;overflow:auto;line-height:1.5;margin:0 0 1em}
pre code{background:none;padding:0;border-radius:0}
blockquote{margin:0 0 1em;padding:0 0 0 1em;border-left:4px solid #d0d0d0;color:#555}
hr{border:0;border-top:1px solid #d9d9d9;margin:2em 0}
img,svg{max-width:100%;height:auto}
table{border-collapse:separate;border-spacing:0;border:1px solid #d9d9d9;border-radius:6px;margin:0 0 1em;overflow:hidden}
th,td{padding:.4em .8em;border-bottom:1px solid #e6e6e6;text-align:left}
th{font-weight:600;background:#f7f7f7}
tbody tr:nth-child(even) td{background:#fafafa}
tr:last-child td{border-bottom:0}
ul,ol{margin:0 0 1em;padding-left:1.6em}
li>p:last-child{margin-bottom:0}
li.task-list-item{list-style:none;margin-left:-1.4em}
li.task-list-item input{margin-right:.5em}
li.task-list-item.done{color:#6b6b6b}
mark{border-radius:4px;padding:.05em .2em;background:#FFF3A3;color:inherit}
mark.hl-red{background:#FFC9C9}mark.hl-orange{background:#FFD8A8}mark.hl-yellow{background:#FFF3A3}
mark.hl-green{background:#C3F0C8}mark.hl-blue{background:#C5DDFB}mark.hl-purple{background:#E3D0F8}
.callout{margin:0 0 1em;padding:.6em 1em;border-left:4px solid #1874D3;border-radius:4px;background:#f3f8fe}
.callout-title{margin:0 0 .3em;font-weight:600;color:#1874D3}
.callout-tip{border-left-color:#1E8A3E;background:#f2faf4}.callout-tip .callout-title{color:#1E8A3E}
.callout-important{border-left-color:#8250DF;background:#f7f3fd}.callout-important .callout-title{color:#8250DF}
.callout-warning{border-left-color:#BF7600;background:#fdf8ee}.callout-warning .callout-title{color:#BF7600}
.callout-caution{border-left-color:#CF222E;background:#fdf1f2}.callout-caution .callout-title{color:#CF222E}
.math.display{display:block;text-align:center;margin:0 0 1em;overflow-x:auto}
.math.inline svg{vertical-align:middle}
.mermaid-diagram{text-align:center;margin:0 0 1em}
.footnotes{margin-top:2.5em;border-top:1px solid #d9d9d9;font-size:.9em;color:#444}
.footnote-ref{font-size:.75em;line-height:0;vertical-align:super}
.raw-html{color:#777}
.page-break{break-after:page;page-break-after:always;height:0;margin:0;border:0}
@media print{body{font-size:11pt}main,article{max-width:none;padding:0}a{color:inherit}}
";

/// Which style sheet an HTML or ePub export carries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Theme {
    /// The editor's light theme.
    #[default]
    Light,
    /// The editor's dark theme: the same sheet with the colors of the dark surface.
    Dark,
}

/// Accent color of a callout in the dark theme.
pub const fn dark_callout_hex(kind: CalloutKind) -> &'static str {
    match kind {
        CalloutKind::Note => "#4493F8",
        CalloutKind::Tip => "#3FB950",
        CalloutKind::Important => "#A371F7",
        CalloutKind::Warning => "#D29922",
        CalloutKind::Caution => "#F85149",
    }
}

/// What the dark theme changes: the surface (`#272727`) and text (`#E4E4E4`) colors of the
/// editor's dark theme, its code, table, highlight and callout colors. It follows [`BASE`] in
/// the same sheet, so every rule it does not mention keeps working.
const DARK_OVERRIDES: &str = "\
:root{color-scheme:dark}
body{background:#272727;color:#E4E4E4}
a{color:#4493F8}
code,pre{background:#303134}
blockquote{border-left-color:#3D4045;color:#9E9E9E}
hr{border-top-color:#3A3A3A}
table{border-color:#3D4045}
th,td{border-bottom-color:#3D4045}
th{background:#2C2C2C}
tbody tr:nth-child(even) td{background:#2C2C2C}
li.task-list-item.done{color:#9E9E9E}
mark{background:#5E5420;color:#FFF3A3}
mark.hl-red{background:#6B2A2A;color:#FFD6D6}mark.hl-orange{background:#6B4620;color:#FFE2BF}mark.hl-yellow{background:#5E5420;color:#FFF3A3}
mark.hl-green{background:#2F5A25;color:#D6F8C5}mark.hl-blue{background:#254A75;color:#CFE4FF}mark.hl-purple{background:#5A2F66;color:#F3D5FA}
.callout{border-left-color:#4493F8;background:#2C3848}
.callout-title{color:#4493F8}
.callout-tip{border-left-color:#3FB950;background:#2B3E2E}.callout-tip .callout-title{color:#3FB950}
.callout-important{border-left-color:#A371F7;background:#3B3348}.callout-important .callout-title{color:#A371F7}
.callout-warning{border-left-color:#D29922;background:#423926}.callout-warning .callout-title{color:#D29922}
.callout-caution{border-left-color:#F85149;background:#482E2C}.callout-caution .callout-title{color:#F85149}
.footnotes{border-top-color:#3A3A3A;color:#BDBDBD}
.raw-html{color:#9E9E9E}
";

/// The complete light-theme style sheet (minified).
pub fn light_theme_css() -> &'static str {
    BASE
}

/// The complete dark-theme style sheet (minified): the light sheet with the dark colors after
/// it.
pub fn dark_theme_css() -> &'static str {
    static SHEET: LazyLock<String> = LazyLock::new(|| format!("{BASE}{DARK_OVERRIDES}"));
    SHEET.as_str()
}

/// The style sheet of `theme`.
pub fn theme_css(theme: Theme) -> &'static str {
    match theme {
        Theme::Light => light_theme_css(),
        Theme::Dark => dark_theme_css(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_highlight_color_has_a_distinct_rule_matching_its_hex() {
        let css = light_theme_css();
        for color in HighlightColor::ALL {
            let rule = format!(
                "mark.hl-{}{{background:{}}}",
                color.name(),
                highlight_hex(Some(color))
            );
            assert!(css.contains(&rule), "{rule}");
        }
        assert_eq!(
            highlight_hex(None),
            highlight_hex(Some(HighlightColor::Yellow))
        );
    }

    #[test]
    fn every_callout_kind_has_its_accent_in_the_sheet() {
        let css = light_theme_css();
        for kind in CalloutKind::ALL {
            let hex = callout_hex(kind);
            assert!(css.contains(hex), "{kind:?}");
        }
        assert!(css.contains(ACCENT));
    }

    #[test]
    fn page_break_uses_css_break_after() {
        assert!(light_theme_css().contains(".page-break{break-after:page"));
    }

    #[test]
    fn the_dark_sheet_is_the_light_sheet_with_the_dark_colors_after_it() {
        let dark = dark_theme_css();
        assert!(dark.starts_with(light_theme_css()));
        let overrides = &dark[light_theme_css().len()..];
        assert!(
            overrides.starts_with(":root{color-scheme:dark}"),
            "{overrides}"
        );
        assert!(overrides.contains("body{background:#272727;color:#E4E4E4}"));
        assert!(overrides.contains("code,pre{background:#303134}"));
        for kind in CalloutKind::ALL {
            let hex = dark_callout_hex(kind);
            assert!(
                overrides.contains(&format!("border-left-color:{hex};")),
                "{kind:?}"
            );
            assert!(overrides.contains(&format!("color:{hex}}}")), "{kind:?}");
        }
        for color in HighlightColor::ALL {
            let rule = format!("mark.hl-{}{{background:", color.name());
            assert!(overrides.contains(&rule), "{rule}");
        }
        // The light accents are gone from every rule the dark theme overrides.
        assert!(!overrides.contains(ACCENT));
    }

    #[test]
    fn a_theme_picks_its_sheet() {
        assert_eq!(Theme::default(), Theme::Light);
        assert_eq!(theme_css(Theme::Light), light_theme_css());
        assert_eq!(theme_css(Theme::Dark), dark_theme_css());
        assert_ne!(light_theme_css(), dark_theme_css());
    }
}
