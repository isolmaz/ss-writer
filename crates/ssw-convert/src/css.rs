//! Style sheet embedded in HTML and ePub exports: the editor's light theme.

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

/// The complete light-theme style sheet (minified).
pub fn light_theme_css() -> &'static str {
    BASE
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
}
