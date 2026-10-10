//! Office Math Markup Language (`m:oMath`) → LaTeX.
//!
//! Supported: fractions (`f`), scripts (`sSup`, `sSub`, `sSubSup`, `sPre`), radicals (`rad`),
//! n-ary operators (`nary`), delimiters (`d`), matrices (`m`), equation arrays (`eqArr`),
//! functions (`func`), accents (`acc`), bars (`bar`), group characters (`groupChr`), limits
//! (`limLow`, `limUpp`), boxes (`box`, `borderBox`), phantoms (`phant`) and math runs (`r`) with
//! `m:sty` / `m:scr` / `m:nor`. Any other element in the math namespace is an error so the
//! caller can report the equation as unsupported.

use crate::ooxml::{Element, Ns};

/// Why an equation could not be converted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OmmlError {
    #[error("unsupported math element: {0}")]
    Unsupported(String),
}

/// Converts an `m:oMath` or `m:oMathPara` element to LaTeX source (no `$` delimiters).
/// Paragraphs with several equations join them with `\\`.
pub fn to_latex(element: &Element) -> Result<String, OmmlError> {
    if element.is(Ns::M, "oMathPara") {
        let mut lines = Vec::new();
        for math in element.elements().filter(|e| e.is(Ns::M, "oMath")) {
            lines.push(content(math)?);
        }
        return Ok(lines.join(r" \\ "));
    }
    if element.is(Ns::M, "oMath") {
        return content(element);
    }
    Err(OmmlError::Unsupported(element.name.clone()))
}

/// Children of a container element, concatenated.
fn content(element: &Element) -> Result<String, OmmlError> {
    let mut out = String::new();
    for child in element.elements() {
        match child.ns {
            Ns::M => {
                if child.name.ends_with("Pr") {
                    continue;
                }
                push_piece(&mut out, &node(child)?);
            }
            Ns::W
                if matches!(
                    child.name.as_str(),
                    "ins" | "smartTag" | "sdt" | "sdtContent"
                ) =>
            {
                push_piece(&mut out, &content(child)?);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Appends `piece`, inserting a space when a trailing `\command` would otherwise run into the
/// first letter or digit of `piece`.
fn push_piece(out: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    let tail = out.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let ends_with_command = tail.len() < out.len() && tail.ends_with('\\');
    if ends_with_command && piece.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        out.push(' ');
    }
    out.push_str(piece);
}

/// Content of the child container `name`, or empty when absent.
fn arg(element: &Element, name: &str) -> Result<String, OmmlError> {
    match element.child(Ns::M, name) {
        Some(child) => content(child),
        None => Ok(String::new()),
    }
}

/// `m:val` of `m:<pr>/m:<prop>`.
fn prop<'a>(element: &'a Element, pr: &str, prop: &str) -> Option<&'a str> {
    element.child(Ns::M, pr)?.child(Ns::M, prop)?.attr("val")
}

fn is_single_token(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first == '\\' {
        let rest: Vec<char> = chars.collect();
        return match rest.first() {
            Some(c) if c.is_ascii_alphabetic() => rest.iter().all(char::is_ascii_alphabetic),
            Some(_) => rest.len() == 1,
            None => false,
        };
    }
    chars.next().is_none() && !matches!(first, '{' | '}' | '^' | '_' | ' ')
}

/// Wraps `s` in braces unless it is one token.
fn group(s: &str) -> String {
    if is_single_token(s) {
        s.to_string()
    } else {
        format!("{{{s}}}")
    }
}

fn node(element: &Element) -> Result<String, OmmlError> {
    match element.name.as_str() {
        "r" => run(element),
        "e" | "num" | "den" | "sub" | "sup" | "deg" | "lim" | "fName" | "oMath" => content(element),
        "f" => fraction(element),
        "sSup" => Ok(format!(
            "{}^{}",
            group(&arg(element, "e")?),
            group(&arg(element, "sup")?)
        )),
        "sSub" => Ok(format!(
            "{}_{}",
            group(&arg(element, "e")?),
            group(&arg(element, "sub")?)
        )),
        "sSubSup" => Ok(format!(
            "{}_{}^{}",
            group(&arg(element, "e")?),
            group(&arg(element, "sub")?),
            group(&arg(element, "sup")?)
        )),
        "sPre" => {
            let mut out = format!(
                "{{}}_{}^{}",
                group(&arg(element, "sub")?),
                group(&arg(element, "sup")?)
            );
            push_piece(&mut out, &group(&arg(element, "e")?));
            Ok(out)
        }
        "rad" => radical(element),
        "nary" => nary(element),
        "d" => delimiter(element),
        "m" => matrix(element),
        "eqArr" => equation_array(element),
        "func" => function(element),
        "acc" => accent(element),
        "bar" => {
            let body = arg(element, "e")?;
            if prop(element, "barPr", "pos") == Some("top") {
                Ok(format!(r"\overline{{{body}}}"))
            } else {
                Ok(format!(r"\underline{{{body}}}"))
            }
        }
        "groupChr" => group_char(element),
        "limLow" => limit(element, true),
        "limUpp" => limit(element, false),
        "borderBox" => Ok(format!(r"\boxed{{{}}}", arg(element, "e")?)),
        "box" | "phant" => arg(element, "e"),
        other => Err(OmmlError::Unsupported(other.to_string())),
    }
}

fn fraction(element: &Element) -> Result<String, OmmlError> {
    let num = arg(element, "num")?;
    let den = arg(element, "den")?;
    match prop(element, "fPr", "type") {
        Some("noBar") => Ok(format!(r"\binom{{{num}}}{{{den}}}")),
        Some("lin") => Ok(format!("{}/{}", group(&num), group(&den))),
        _ => Ok(format!(r"\frac{{{num}}}{{{den}}}")),
    }
}

fn radical(element: &Element) -> Result<String, OmmlError> {
    let body = arg(element, "e")?;
    let degree = arg(element, "deg")?;
    let hidden = prop(element, "radPr", "degHide").is_some_and(|v| crate::ooxml::on_off(Some(v)));
    if degree.is_empty() || hidden {
        Ok(format!(r"\sqrt{{{body}}}"))
    } else {
        Ok(format!(r"\sqrt[{degree}]{{{body}}}"))
    }
}

fn nary_operator(chr: &str) -> &'static str {
    match chr {
        "∑" => r"\sum",
        "∏" => r"\prod",
        "∐" => r"\coprod",
        "∬" => r"\iint",
        "∭" => r"\iiint",
        "∮" => r"\oint",
        "∯" => r"\oiint",
        "∰" => r"\oiiint",
        "⋃" => r"\bigcup",
        "⋂" => r"\bigcap",
        "⋁" => r"\bigvee",
        "⋀" => r"\bigwedge",
        "⨁" => r"\bigoplus",
        "⨂" => r"\bigotimes",
        "⨀" => r"\bigodot",
        "⨄" => r"\biguplus",
        _ => r"\int",
    }
}

fn nary(element: &Element) -> Result<String, OmmlError> {
    let chr = prop(element, "naryPr", "chr").unwrap_or("∫");
    let flag =
        |name: &str| prop(element, "naryPr", name).is_some_and(|v| crate::ooxml::on_off(Some(v)));
    let mut out = nary_operator(chr).to_string();
    let sub = arg(element, "sub")?;
    if !flag("subHide") && !sub.is_empty() {
        out.push_str(&format!("_{}", group(&sub)));
    }
    let sup = arg(element, "sup")?;
    if !flag("supHide") && !sup.is_empty() {
        out.push_str(&format!("^{}", group(&sup)));
    }
    let body = arg(element, "e")?;
    if !body.is_empty() {
        out.push(' ');
        out.push_str(&body);
    }
    Ok(out)
}

fn delimiter_char(chr: &str) -> String {
    match chr {
        "" => ".".to_string(),
        "{" => r"\{".to_string(),
        "}" => r"\}".to_string(),
        "‖" | "∥" => r"\|".to_string(),
        "⟨" | "〈" | "⟪" => r"\langle".to_string(),
        "⟩" | "〉" | "⟫" => r"\rangle".to_string(),
        "⌊" => r"\lfloor".to_string(),
        "⌋" => r"\rfloor".to_string(),
        "⌈" => r"\lceil".to_string(),
        "⌉" => r"\rceil".to_string(),
        other => other.to_string(),
    }
}

fn delimiter(element: &Element) -> Result<String, OmmlError> {
    let open = delimiter_char(prop(element, "dPr", "begChr").unwrap_or("("));
    let close = delimiter_char(prop(element, "dPr", "endChr").unwrap_or(")"));
    let separator = prop(element, "dPr", "sepChr").unwrap_or("|");
    let joiner = match separator {
        "|" => r" \mid ".to_string(),
        "," | ";" => format!("{separator} "),
        other => format!(" {} ", map_text(other)),
    };
    let mut parts = Vec::new();
    for e in element.elements().filter(|e| e.is(Ns::M, "e")) {
        parts.push(content(e)?);
    }
    Ok(format!(
        r"\left{open} {} \right{close}",
        parts.join(&joiner)
    ))
}

fn matrix(element: &Element) -> Result<String, OmmlError> {
    let mut rows = Vec::new();
    for row in element.elements().filter(|e| e.is(Ns::M, "mr")) {
        let mut cells = Vec::new();
        for cell in row.elements().filter(|e| e.is(Ns::M, "e")) {
            cells.push(content(cell)?);
        }
        rows.push(cells.join(" & "));
    }
    Ok(format!(
        r"\begin{{matrix}} {} \end{{matrix}}",
        rows.join(r" \\ ")
    ))
}

fn equation_array(element: &Element) -> Result<String, OmmlError> {
    let mut rows = Vec::new();
    for row in element.elements().filter(|e| e.is(Ns::M, "e")) {
        rows.push(content(row)?);
    }
    Ok(format!(
        r"\begin{{aligned}} {} \end{{aligned}}",
        rows.join(r" \\ ")
    ))
}

const FUNCTION_NAMES: [&str; 31] = [
    "sin", "cos", "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh", "tanh",
    "coth", "log", "ln", "lg", "exp", "lim", "liminf", "limsup", "max", "min", "sup", "inf", "det",
    "dim", "gcd", "deg", "ker", "arg", "Pr",
];

fn function(element: &Element) -> Result<String, OmmlError> {
    let name_element = element.child(Ns::M, "fName");
    let plain = name_element.map(plain_text).unwrap_or_default();
    let mut out = if FUNCTION_NAMES.contains(&plain.as_str()) {
        format!(r"\{plain}")
    } else if !plain.is_empty() && plain.chars().all(char::is_alphabetic) {
        format!(r"\operatorname{{{plain}}}")
    } else {
        arg(element, "fName")?
    };
    let body = arg(element, "e")?;
    if !body.is_empty() {
        out.push(' ');
        out.push_str(&body);
    }
    Ok(out)
}

/// Text of a container made only of math runs (`None`-styled), else empty.
fn plain_text(element: &Element) -> String {
    let mut out = String::new();
    for child in element.elements() {
        if child.is(Ns::M, "r") {
            out.push_str(
                &child
                    .child(Ns::M, "t")
                    .map(Element::text)
                    .unwrap_or_default(),
            );
        } else if !child.name.ends_with("Pr") {
            return String::new();
        }
    }
    out
}

fn accent(element: &Element) -> Result<String, OmmlError> {
    let chr = prop(element, "accPr", "chr").unwrap_or("\u{302}");
    let command = match chr {
        "\u{302}" | "^" | "ˆ" => r"\hat",
        "\u{303}" | "~" | "˜" => r"\tilde",
        "\u{304}" | "¯" | "ˉ" | "\u{305}" => r"\bar",
        "\u{306}" | "˘" => r"\breve",
        "\u{307}" | "˙" => r"\dot",
        "\u{308}" | "¨" => r"\ddot",
        "\u{20DB}" => r"\dddot",
        "\u{30C}" | "ˇ" => r"\check",
        "\u{301}" | "´" => r"\acute",
        "\u{300}" | "`" => r"\grave",
        "\u{20D7}" | "→" => r"\vec",
        _ => r"\hat",
    };
    Ok(format!("{command}{{{}}}", arg(element, "e")?))
}

fn group_char(element: &Element) -> Result<String, OmmlError> {
    let chr = prop(element, "groupChrPr", "chr").unwrap_or("⏟");
    let top = prop(element, "groupChrPr", "pos") == Some("top") || chr == "⏞";
    let body = arg(element, "e")?;
    let command = match (chr, top) {
        ("→", true) => r"\overrightarrow",
        ("←", true) => r"\overleftarrow",
        ("↔", true) => r"\overleftrightarrow",
        ("→", false) => r"\underrightarrow",
        ("←", false) => r"\underleftarrow",
        ("↔", false) => r"\underleftrightarrow",
        (_, true) => r"\overbrace",
        (_, false) => r"\underbrace",
    };
    Ok(format!("{command}{{{body}}}"))
}

fn limit(element: &Element, lower: bool) -> Result<String, OmmlError> {
    let base_element = element.child(Ns::M, "e");
    let base_plain = base_element.map(plain_text).unwrap_or_default();
    let base = arg(element, "e")?;
    let lim = arg(element, "lim")?;
    let is_operator = FUNCTION_NAMES.contains(&base_plain.as_str());
    let command_base = if is_operator {
        format!(r"\{base_plain}")
    } else {
        base.clone()
    };
    Ok(match (lower, is_operator) {
        (true, true) => format!("{command_base}_{}", group(&lim)),
        (false, true) => format!("{command_base}^{}", group(&lim)),
        (true, false) => format!(r"\underset{{{lim}}}{{{command_base}}}"),
        (false, false) => format!(r"\overset{{{lim}}}{{{command_base}}}"),
    })
}

fn run(element: &Element) -> Result<String, OmmlError> {
    let mut text = String::new();
    for child in element.elements() {
        if child.is(Ns::M, "t") || child.is(Ns::W, "t") {
            text.push_str(&child.text());
        }
    }
    let properties = element.child(Ns::M, "rPr");
    let flag = |name: &str| {
        properties
            .and_then(|p| p.child(Ns::M, name))
            .is_some_and(|e| crate::ooxml::on_off(e.attr("val")))
    };
    let value = |name: &str| {
        properties
            .and_then(|p| p.child(Ns::M, name))
            .and_then(|e| e.attr("val"))
    };
    if flag("nor") {
        return Ok(format!(r"\text{{{}}}", escape_text_mode(&text)));
    }
    let mapped = map_text(&text);
    let wrapped = match value("scr") {
        Some("script") => wrap_alnum(&mapped, r"\mathcal"),
        Some("fraktur") => wrap_alnum(&mapped, r"\mathfrak"),
        Some("double-struck") => wrap_alnum(&mapped, r"\mathbb"),
        Some("sans-serif") => wrap_alnum(&mapped, r"\mathsf"),
        Some("monospace") => wrap_alnum(&mapped, r"\mathtt"),
        _ => mapped,
    };
    Ok(match value("sty") {
        Some("p") => wrap_alnum(&wrapped, r"\mathrm"),
        Some("b") => wrap_alnum(&wrapped, r"\mathbf"),
        Some("bi") => wrap_alnum(&wrapped, r"\boldsymbol"),
        _ => wrapped,
    })
}

/// Wraps `s` in `\command{…}` when it is non-empty plain text without commands.
fn wrap_alnum(s: &str, command: &str) -> String {
    if s.is_empty() || s.contains('\\') || !s.chars().all(|c| c.is_alphanumeric()) {
        return s.to_string();
    }
    format!("{command}{{{s}}}")
}

fn escape_text_mode(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match c {
            '\\' => out.push_str(r"\textbackslash{}"),
            '{' | '}' | '#' | '%' | '$' | '_' | '&' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// LaTeX for one Unicode character, or `None` when it passes through unchanged.
fn symbol(c: char) -> Option<&'static str> {
    Some(match c {
        'α' => r"\alpha",
        'β' => r"\beta",
        'γ' => r"\gamma",
        'δ' => r"\delta",
        'ε' => r"\epsilon",
        'ϵ' => r"\varepsilon",
        'ζ' => r"\zeta",
        'η' => r"\eta",
        'θ' => r"\theta",
        'ϑ' => r"\vartheta",
        'ι' => r"\iota",
        'κ' => r"\kappa",
        'λ' => r"\lambda",
        'μ' | 'µ' => r"\mu",
        'ν' => r"\nu",
        'ξ' => r"\xi",
        'π' => r"\pi",
        'ϖ' => r"\varpi",
        'ρ' => r"\rho",
        'ϱ' => r"\varrho",
        'σ' => r"\sigma",
        'ς' => r"\varsigma",
        'τ' => r"\tau",
        'υ' => r"\upsilon",
        'φ' => r"\varphi",
        'ϕ' => r"\phi",
        'χ' => r"\chi",
        'ψ' => r"\psi",
        'ω' => r"\omega",
        'Γ' => r"\Gamma",
        'Δ' => r"\Delta",
        'Θ' => r"\Theta",
        'Λ' => r"\Lambda",
        'Ξ' => r"\Xi",
        'Π' => r"\Pi",
        'Σ' => r"\Sigma",
        'Υ' => r"\Upsilon",
        'Φ' => r"\Phi",
        'Ψ' => r"\Psi",
        'Ω' => r"\Omega",
        '×' => r"\times",
        '÷' => r"\div",
        '±' => r"\pm",
        '∓' => r"\mp",
        '·' | '⋅' => r"\cdot",
        '∘' => r"\circ",
        '∙' => r"\bullet",
        '∗' => r"\ast",
        '⊕' => r"\oplus",
        '⊗' => r"\otimes",
        '⊙' => r"\odot",
        '≤' => r"\le",
        '≥' => r"\ge",
        '≠' => r"\ne",
        '≈' => r"\approx",
        '≡' => r"\equiv",
        '∼' => r"\sim",
        '≃' => r"\simeq",
        '≅' => r"\cong",
        '∝' => r"\propto",
        '≪' => r"\ll",
        '≫' => r"\gg",
        '≺' => r"\prec",
        '≻' => r"\succ",
        '∈' => r"\in",
        '∉' => r"\notin",
        '∋' => r"\ni",
        '⊂' => r"\subset",
        '⊃' => r"\supset",
        '⊆' => r"\subseteq",
        '⊇' => r"\supseteq",
        '⊄' => r"\not\subset",
        '∪' => r"\cup",
        '∩' => r"\cap",
        '∖' => r"\setminus",
        '∅' => r"\emptyset",
        '∀' => r"\forall",
        '∃' => r"\exists",
        '∄' => r"\nexists",
        '¬' => r"\neg",
        '∧' => r"\wedge",
        '∨' => r"\vee",
        '∞' => r"\infty",
        '∂' => r"\partial",
        '∇' => r"\nabla",
        '√' => r"\surd",
        '∑' => r"\sum",
        '∏' => r"\prod",
        '∫' => r"\int",
        '∮' => r"\oint",
        '→' => r"\to",
        '←' => r"\leftarrow",
        '↔' => r"\leftrightarrow",
        '⇒' => r"\Rightarrow",
        '⇐' => r"\Leftarrow",
        '⇔' => r"\Leftrightarrow",
        '↦' => r"\mapsto",
        '↑' => r"\uparrow",
        '↓' => r"\downarrow",
        '⟶' => r"\longrightarrow",
        '⟵' => r"\longleftarrow",
        '…' => r"\ldots",
        '⋯' => r"\cdots",
        '⋮' => r"\vdots",
        '⋱' => r"\ddots",
        '∴' => r"\therefore",
        '∵' => r"\because",
        '∠' => r"\angle",
        '⊥' => r"\perp",
        '∥' => r"\parallel",
        '⊤' => r"\top",
        '⊢' => r"\vdash",
        '⊨' => r"\models",
        'ℓ' => r"\ell",
        'ℏ' | 'ħ' => r"\hbar",
        'ℵ' => r"\aleph",
        'ℝ' => r"\mathbb{R}",
        'ℂ' => r"\mathbb{C}",
        'ℕ' => r"\mathbb{N}",
        'ℤ' => r"\mathbb{Z}",
        'ℚ' => r"\mathbb{Q}",
        '′' => "'",
        '″' => "''",
        '°' => r"^\circ",
        '−' => "-",
        '{' => r"\{",
        '}' => r"\}",
        '\\' => r"\backslash",
        '#' => r"\#",
        '%' => r"\%",
        '$' => r"\$",
        '_' => r"\_",
        '^' => r"\^{}",
        '~' => r"\sim",
        '\u{2009}' | '\u{200A}' | '\u{2006}' => r"\,",
        '\u{2005}' | '\u{2004}' => r"\;",
        '\u{00A0}' | '\u{2002}' | '\u{2003}' => r"\ ",
        '\u{2061}'..='\u{2064}' => "",
        _ => return None,
    })
}

/// Maps the text of a math run to LaTeX, spacing `\commands` from following letters.
fn map_text(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        match symbol(c) {
            Some(latex) => push_piece(&mut out, latex),
            None => {
                let mut buffer = [0u8; 4];
                push_piece(&mut out, c.encode_utf8(&mut buffer));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ooxml::parse_xml;

    const M_NS: &str = r#"xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math" xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main""#;

    fn math(inner: &str) -> Result<String, OmmlError> {
        let xml = format!("<m:oMath {M_NS}>{inner}</m:oMath>");
        to_latex(&parse_xml(xml.as_bytes()).unwrap())
    }

    fn r(text: &str) -> String {
        format!("<m:r><m:t>{text}</m:t></m:r>")
    }

    #[test]
    fn fractions() {
        let cases = [
            (
                format!(
                    "<m:f><m:num>{}</m:num><m:den>{}</m:den></m:f>",
                    r("a"),
                    r("b")
                ),
                r"\frac{a}{b}",
            ),
            (
                format!(
                    r#"<m:f><m:fPr><m:type m:val="noBar"/></m:fPr><m:num>{}</m:num><m:den>{}</m:den></m:f>"#,
                    r("n"),
                    r("k")
                ),
                r"\binom{n}{k}",
            ),
            (
                format!(
                    r#"<m:f><m:fPr><m:type m:val="lin"/></m:fPr><m:num>{}</m:num><m:den>{}</m:den></m:f>"#,
                    r("12"),
                    r("b")
                ),
                "{12}/b",
            ),
            (
                format!(
                    r#"<m:f><m:fPr><m:type m:val="skw"/></m:fPr><m:num>{}</m:num><m:den>{}</m:den></m:f>"#,
                    r("a"),
                    r("b")
                ),
                r"\frac{a}{b}",
            ),
        ];
        for (xml, want) in cases {
            assert_eq!(math(&xml).unwrap(), want);
        }
    }

    #[test]
    fn scripts() {
        let cases = [
            (
                format!(
                    "<m:sSup><m:e>{}</m:e><m:sup>{}</m:sup></m:sSup>",
                    r("x"),
                    r("2")
                ),
                "x^2",
            ),
            (
                format!(
                    "<m:sSup><m:e>{}</m:e><m:sup>{}</m:sup></m:sSup>",
                    r("x"),
                    r("10")
                ),
                "x^{10}",
            ),
            (
                format!(
                    "<m:sSub><m:e>{}</m:e><m:sub>{}</m:sub></m:sSub>",
                    r("a"),
                    r("i")
                ),
                "a_i",
            ),
            (
                format!(
                    "<m:sSubSup><m:e>{}</m:e><m:sub>{}</m:sub><m:sup>{}</m:sup></m:sSubSup>",
                    r("x"),
                    r("i"),
                    r("n+1")
                ),
                "x_i^{n+1}",
            ),
            (
                format!(
                    "<m:sPre><m:sub>{}</m:sub><m:sup>{}</m:sup><m:e>{}</m:e></m:sPre>",
                    r("1"),
                    r("2"),
                    r("X")
                ),
                "{}_1^2X",
            ),
            (
                format!(
                    "<m:sSup><m:e>{}</m:e><m:sup>{}</m:sup></m:sSup>",
                    r("ab"),
                    r("α")
                ),
                r"{ab}^\alpha",
            ),
            (
                format!(
                    "<m:sSup><m:e>{}</m:e><m:sup>{}</m:sup></m:sSup>",
                    r("x"),
                    r("{")
                ),
                r"x^\{",
            ),
        ];
        for (xml, want) in cases {
            assert_eq!(math(&xml).unwrap(), want, "{xml}");
        }
    }

    #[test]
    fn radicals() {
        let plain = format!("<m:rad><m:deg/><m:e>{}</m:e></m:rad>", r("x"));
        assert_eq!(math(&plain).unwrap(), r"\sqrt{x}");
        let cube = format!(
            "<m:rad><m:deg>{}</m:deg><m:e>{}</m:e></m:rad>",
            r("3"),
            r("x")
        );
        assert_eq!(math(&cube).unwrap(), r"\sqrt[3]{x}");
        let hidden = format!(
            r#"<m:rad><m:radPr><m:degHide m:val="1"/></m:radPr><m:deg>{}</m:deg><m:e>{}</m:e></m:rad>"#,
            r("3"),
            r("x")
        );
        assert_eq!(math(&hidden).unwrap(), r"\sqrt{x}");
        let missing = format!("<m:rad><m:e>{}</m:e></m:rad>", r("y"));
        assert_eq!(math(&missing).unwrap(), r"\sqrt{y}");
    }

    #[test]
    fn nary_operators() {
        let sum = format!(
            r#"<m:nary><m:naryPr><m:chr m:val="∑"/></m:naryPr><m:sub>{}</m:sub><m:sup>{}</m:sup><m:e>{}</m:e></m:nary>"#,
            r("i=1"),
            r("n"),
            r("i")
        );
        assert_eq!(math(&sum).unwrap(), r"\sum_{i=1}^n i");
        let integral = format!(
            "<m:nary><m:sub>{}</m:sub><m:sup>{}</m:sup><m:e>{}</m:e></m:nary>",
            r("0"),
            r("1"),
            r("f")
        );
        assert_eq!(math(&integral).unwrap(), r"\int_0^1 f");
        let hidden = format!(
            r#"<m:nary><m:naryPr><m:chr m:val="∏"/><m:subHide m:val="1"/><m:supHide m:val="on"/></m:naryPr><m:sub>{}</m:sub><m:sup>{}</m:sup><m:e>{}</m:e></m:nary>"#,
            r("a"),
            r("b"),
            r("x")
        );
        assert_eq!(math(&hidden).unwrap(), r"\prod x");
        let chars = [
            ("∐", r"\coprod"),
            ("∬", r"\iint"),
            ("∭", r"\iiint"),
            ("∮", r"\oint"),
            ("∯", r"\oiint"),
            ("∰", r"\oiiint"),
            ("⋃", r"\bigcup"),
            ("⋂", r"\bigcap"),
            ("⋁", r"\bigvee"),
            ("⋀", r"\bigwedge"),
            ("⨁", r"\bigoplus"),
            ("⨂", r"\bigotimes"),
            ("⨀", r"\bigodot"),
            ("⨄", r"\biguplus"),
            ("?", r"\int"),
        ];
        for (chr, want) in chars {
            let xml = format!(
                r#"<m:nary><m:naryPr><m:chr m:val="{chr}"/></m:naryPr><m:sub/><m:sup/><m:e/></m:nary>"#
            );
            assert_eq!(math(&xml).unwrap(), want);
        }
    }

    #[test]
    fn delimiters() {
        let cases = [
            (
                format!("<m:d><m:e>{}</m:e></m:d>", r("x")),
                r"\left( x \right)",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:sepChr m:val="/"/></m:dPr><m:e>{}</m:e><m:e>{}</m:e></m:d>"#,
                    r("a"),
                    r("b")
                ),
                r"\left( a / b \right)",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:begChr m:val="["/><m:endChr m:val="]"/><m:sepChr m:val=","/></m:dPr><m:e>{}</m:e><m:e>{}</m:e></m:d>"#,
                    r("a"),
                    r("b")
                ),
                r"\left[ a, b \right]",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:begChr m:val="{{"/><m:endChr m:val=""/></m:dPr><m:e>{}</m:e></m:d>"#,
                    r("x")
                ),
                r"\left\{ x \right.",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:begChr m:val="⟨"/><m:endChr m:val="⟩"/></m:dPr><m:e>{}</m:e><m:e>{}</m:e></m:d>"#,
                    r("a"),
                    r("b")
                ),
                r"\left\langle a \mid b \right\rangle",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:begChr m:val="⌊"/><m:endChr m:val="⌋"/></m:dPr><m:e>{}</m:e></m:d>"#,
                    r("x")
                ),
                r"\left\lfloor x \right\rfloor",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:begChr m:val="⌈"/><m:endChr m:val="⌉"/></m:dPr><m:e>{}</m:e></m:d>"#,
                    r("x")
                ),
                r"\left\lceil x \right\rceil",
            ),
            (
                format!(
                    r#"<m:d><m:dPr><m:begChr m:val="‖"/><m:endChr m:val="}}"/></m:dPr><m:e>{}</m:e></m:d>"#,
                    r("x")
                ),
                r"\left\| x \right\}",
            ),
        ];
        for (xml, want) in cases {
            assert_eq!(math(&xml).unwrap(), want, "{xml}");
        }
    }

    #[test]
    fn matrices_and_equation_arrays() {
        let matrix = format!(
            "<m:m><m:mr><m:e>{}</m:e><m:e>{}</m:e></m:mr><m:mr><m:e>{}</m:e><m:e>{}</m:e></m:mr></m:m>",
            r("a"),
            r("b"),
            r("c"),
            r("d")
        );
        assert_eq!(
            math(&matrix).unwrap(),
            r"\begin{matrix} a & b \\ c & d \end{matrix}"
        );
        let array = format!(
            "<m:eqArr><m:e>{}</m:e><m:e>{}</m:e></m:eqArr>",
            r("x=1"),
            r("y=2")
        );
        assert_eq!(
            math(&array).unwrap(),
            r"\begin{aligned} x=1 \\ y=2 \end{aligned}"
        );
    }

    #[test]
    fn functions_and_limits() {
        let sin = format!(
            "<m:func><m:fName>{}</m:fName><m:e>{}</m:e></m:func>",
            r("sin"),
            r("x")
        );
        assert_eq!(math(&sin).unwrap(), r"\sin x");
        // run properties inside the name do not make it more than plain text
        let with_properties = format!(
            "<m:func><m:fName><m:ctrlPr/>{}</m:fName><m:e>{}</m:e></m:func>",
            r("sin"),
            r("x")
        );
        assert_eq!(math(&with_properties).unwrap(), r"\sin x");
        let custom = format!(
            "<m:func><m:fName>{}</m:fName><m:e>{}</m:e></m:func>",
            r("sgn"),
            r("x")
        );
        assert_eq!(math(&custom).unwrap(), r"\operatorname{sgn} x");
        let odd = format!(
            "<m:func><m:fName>{}</m:fName><m:e>{}</m:e></m:func>",
            r("f1"),
            r("x")
        );
        assert_eq!(math(&odd).unwrap(), "f1 x");
        let lim = format!(
            "<m:func><m:fName><m:limLow><m:e>{}</m:e><m:lim>{}</m:lim></m:limLow></m:fName><m:e>{}</m:e></m:func>",
            r("lim"),
            r("x→0"),
            r("f")
        );
        assert_eq!(math(&lim).unwrap(), r"\lim_{x\to 0} f");
        let under = format!(
            "<m:limLow><m:e>{}</m:e><m:lim>{}</m:lim></m:limLow>",
            r("X"),
            r("k")
        );
        assert_eq!(math(&under).unwrap(), r"\underset{k}{X}");
        let upper_op = format!(
            "<m:limUpp><m:e>{}</m:e><m:lim>{}</m:lim></m:limUpp>",
            r("max"),
            r("k")
        );
        assert_eq!(math(&upper_op).unwrap(), r"\max^k");
        let over = format!(
            "<m:limUpp><m:e>{}</m:e><m:lim>{}</m:lim></m:limUpp>",
            r("X"),
            r("k")
        );
        assert_eq!(math(&over).unwrap(), r"\overset{k}{X}");
    }

    #[test]
    fn accents_bars_and_group_characters() {
        let accents = [
            ("\u{302}", r"\hat"),
            ("~", r"\tilde"),
            ("¯", r"\bar"),
            ("\u{306}", r"\breve"),
            ("\u{307}", r"\dot"),
            ("\u{308}", r"\ddot"),
            ("\u{20DB}", r"\dddot"),
            ("\u{30C}", r"\check"),
            ("\u{301}", r"\acute"),
            ("\u{300}", r"\grave"),
            ("\u{20D7}", r"\vec"),
            ("?", r"\hat"),
        ];
        for (chr, command) in accents {
            let xml = format!(
                r#"<m:acc><m:accPr><m:chr m:val="{chr}"/></m:accPr><m:e>{}</m:e></m:acc>"#,
                r("x")
            );
            assert_eq!(math(&xml).unwrap(), format!("{command}{{x}}"));
        }
        let default_accent = format!("<m:acc><m:e>{}</m:e></m:acc>", r("x"));
        assert_eq!(math(&default_accent).unwrap(), r"\hat{x}");
        let under = format!("<m:bar><m:e>{}</m:e></m:bar>", r("x"));
        assert_eq!(math(&under).unwrap(), r"\underline{x}");
        let over = format!(
            r#"<m:bar><m:barPr><m:pos m:val="top"/></m:barPr><m:e>{}</m:e></m:bar>"#,
            r("x")
        );
        assert_eq!(math(&over).unwrap(), r"\overline{x}");
        let groups = [
            ("", "", r"\underbrace"),
            ("⏞", "", r"\overbrace"),
            ("⏟", "top", r"\overbrace"),
            ("→", "top", r"\overrightarrow"),
            ("←", "top", r"\overleftarrow"),
            ("↔", "top", r"\overleftrightarrow"),
            ("→", "bot", r"\underrightarrow"),
            ("←", "bot", r"\underleftarrow"),
            ("↔", "bot", r"\underleftrightarrow"),
        ];
        for (chr, pos, command) in groups {
            let chr_xml = if chr.is_empty() {
                String::new()
            } else {
                format!(r#"<m:chr m:val="{chr}"/>"#)
            };
            let pos_xml = if pos.is_empty() {
                String::new()
            } else {
                format!(r#"<m:pos m:val="{pos}"/>"#)
            };
            let xml = format!(
                "<m:groupChr><m:groupChrPr>{chr_xml}{pos_xml}</m:groupChrPr><m:e>{}</m:e></m:groupChr>",
                r("x")
            );
            assert_eq!(
                math(&xml).unwrap(),
                format!("{command}{{x}}"),
                "{chr} {pos}"
            );
        }
    }

    #[test]
    fn boxes_and_phantoms() {
        let border = format!("<m:borderBox><m:e>{}</m:e></m:borderBox>", r("x"));
        assert_eq!(math(&border).unwrap(), r"\boxed{x}");
        let plain_box = format!("<m:box><m:e>{}</m:e></m:box>", r("y"));
        assert_eq!(math(&plain_box).unwrap(), "y");
        let phantom = format!("<m:phant><m:e>{}</m:e></m:phant>", r("z"));
        assert_eq!(math(&phantom).unwrap(), "z");
    }

    #[test]
    fn run_styles_and_text_mapping() {
        let cases = [
            (
                r#"<m:r><m:rPr><m:sty m:val="p"/></m:rPr><m:t>sin</m:t></m:r>"#,
                r"\mathrm{sin}",
            ),
            (
                r#"<m:r><m:rPr><m:sty m:val="b"/></m:rPr><m:t>v</m:t></m:r>"#,
                r"\mathbf{v}",
            ),
            (
                r#"<m:r><m:rPr><m:sty m:val="bi"/></m:rPr><m:t>v</m:t></m:r>"#,
                r"\boldsymbol{v}",
            ),
            (
                r#"<m:r><m:rPr><m:sty m:val="i"/></m:rPr><m:t>v</m:t></m:r>"#,
                "v",
            ),
            (
                r#"<m:r><m:rPr><m:scr m:val="double-struck"/></m:rPr><m:t>R</m:t></m:r>"#,
                r"\mathbb{R}",
            ),
            (
                r#"<m:r><m:rPr><m:scr m:val="script"/></m:rPr><m:t>L</m:t></m:r>"#,
                r"\mathcal{L}",
            ),
            (
                r#"<m:r><m:rPr><m:scr m:val="fraktur"/></m:rPr><m:t>g</m:t></m:r>"#,
                r"\mathfrak{g}",
            ),
            (
                r#"<m:r><m:rPr><m:scr m:val="sans-serif"/></m:rPr><m:t>s</m:t></m:r>"#,
                r"\mathsf{s}",
            ),
            (
                r#"<m:r><m:rPr><m:scr m:val="monospace"/></m:rPr><m:t>m</m:t></m:r>"#,
                r"\mathtt{m}",
            ),
            (
                r#"<m:r><m:rPr><m:scr m:val="roman"/></m:rPr><m:t>r</m:t></m:r>"#,
                "r",
            ),
            (
                r#"<m:r><m:rPr><m:nor/></m:rPr><m:t>if {x} #1</m:t></m:r>"#,
                r"\text{if \{x\} \#1}",
            ),
            (
                r#"<m:r><m:rPr><m:nor m:val="0"/></m:rPr><m:t>a</m:t></m:r>"#,
                "a",
            ),
            (
                r#"<m:r><m:rPr><m:nor/></m:rPr><m:t>a\b</m:t></m:r>"#,
                r"\text{a\textbackslash{}b}",
            ),
            (
                r#"<m:r><m:rPr><m:sty m:val="p"/></m:rPr><m:t>a b</m:t></m:r>"#,
                "a b",
            ),
            (
                r#"<m:r><m:rPr><m:sty m:val="p"/></m:rPr><m:t>α</m:t></m:r>"#,
                r"\alpha",
            ),
            (r#"<m:r><m:rPr><m:sty m:val="p"/></m:rPr><m:t/></m:r>"#, ""),
            ("<m:r><w:rPr/><w:t>q</w:t></m:r>", "q"),
            ("<m:r><m:t>αβ x</m:t></m:r>", r"\alpha\beta x"),
            ("<m:r><m:t>x−y≤z</m:t></m:r>", r"x-y\le z"),
            ("<m:r><m:t>90°</m:t></m:r>", r"90^\circ"),
        ];
        for (xml, want) in cases {
            assert_eq!(math(xml).unwrap(), want, "{xml}");
        }
    }

    #[test]
    fn symbol_table_entries() {
        let samples = [
            ('α', r"\alpha"),
            ('β', r"\beta"),
            ('γ', r"\gamma"),
            ('δ', r"\delta"),
            ('ε', r"\epsilon"),
            ('ϵ', r"\varepsilon"),
            ('ζ', r"\zeta"),
            ('η', r"\eta"),
            ('θ', r"\theta"),
            ('ϑ', r"\vartheta"),
            ('ι', r"\iota"),
            ('κ', r"\kappa"),
            ('λ', r"\lambda"),
            ('μ', r"\mu"),
            ('µ', r"\mu"),
            ('ν', r"\nu"),
            ('ξ', r"\xi"),
            ('π', r"\pi"),
            ('ϖ', r"\varpi"),
            ('ρ', r"\rho"),
            ('ϱ', r"\varrho"),
            ('σ', r"\sigma"),
            ('ς', r"\varsigma"),
            ('τ', r"\tau"),
            ('υ', r"\upsilon"),
            ('φ', r"\varphi"),
            ('ϕ', r"\phi"),
            ('χ', r"\chi"),
            ('ψ', r"\psi"),
            ('ω', r"\omega"),
            ('Γ', r"\Gamma"),
            ('Δ', r"\Delta"),
            ('Θ', r"\Theta"),
            ('Λ', r"\Lambda"),
            ('Ξ', r"\Xi"),
            ('Π', r"\Pi"),
            ('Σ', r"\Sigma"),
            ('Υ', r"\Upsilon"),
            ('Φ', r"\Phi"),
            ('Ψ', r"\Psi"),
            ('Ω', r"\Omega"),
            ('×', r"\times"),
            ('÷', r"\div"),
            ('±', r"\pm"),
            ('∓', r"\mp"),
            ('·', r"\cdot"),
            ('⋅', r"\cdot"),
            ('∘', r"\circ"),
            ('∙', r"\bullet"),
            ('∗', r"\ast"),
            ('⊕', r"\oplus"),
            ('⊗', r"\otimes"),
            ('⊙', r"\odot"),
            ('≤', r"\le"),
            ('≥', r"\ge"),
            ('≠', r"\ne"),
            ('≈', r"\approx"),
            ('≡', r"\equiv"),
            ('∼', r"\sim"),
            ('≃', r"\simeq"),
            ('≅', r"\cong"),
            ('∝', r"\propto"),
            ('≪', r"\ll"),
            ('≫', r"\gg"),
            ('≺', r"\prec"),
            ('≻', r"\succ"),
            ('∈', r"\in"),
            ('∉', r"\notin"),
            ('∋', r"\ni"),
            ('⊂', r"\subset"),
            ('⊃', r"\supset"),
            ('⊆', r"\subseteq"),
            ('⊇', r"\supseteq"),
            ('⊄', r"\not\subset"),
            ('∪', r"\cup"),
            ('∩', r"\cap"),
            ('∖', r"\setminus"),
            ('∅', r"\emptyset"),
            ('∀', r"\forall"),
            ('∃', r"\exists"),
            ('∄', r"\nexists"),
            ('¬', r"\neg"),
            ('∧', r"\wedge"),
            ('∨', r"\vee"),
            ('∞', r"\infty"),
            ('∂', r"\partial"),
            ('∇', r"\nabla"),
            ('√', r"\surd"),
            ('∑', r"\sum"),
            ('∏', r"\prod"),
            ('∫', r"\int"),
            ('∮', r"\oint"),
            ('→', r"\to"),
            ('←', r"\leftarrow"),
            ('↔', r"\leftrightarrow"),
            ('⇒', r"\Rightarrow"),
            ('⇐', r"\Leftarrow"),
            ('⇔', r"\Leftrightarrow"),
            ('↦', r"\mapsto"),
            ('↑', r"\uparrow"),
            ('↓', r"\downarrow"),
            ('⟶', r"\longrightarrow"),
            ('⟵', r"\longleftarrow"),
            ('…', r"\ldots"),
            ('⋯', r"\cdots"),
            ('⋮', r"\vdots"),
            ('⋱', r"\ddots"),
            ('∴', r"\therefore"),
            ('∵', r"\because"),
            ('∠', r"\angle"),
            ('⊥', r"\perp"),
            ('∥', r"\parallel"),
            ('⊤', r"\top"),
            ('⊢', r"\vdash"),
            ('⊨', r"\models"),
            ('ℓ', r"\ell"),
            ('ℏ', r"\hbar"),
            ('ħ', r"\hbar"),
            ('ℵ', r"\aleph"),
            ('ℝ', r"\mathbb{R}"),
            ('ℂ', r"\mathbb{C}"),
            ('ℕ', r"\mathbb{N}"),
            ('ℤ', r"\mathbb{Z}"),
            ('ℚ', r"\mathbb{Q}"),
            ('′', "'"),
            ('″', "''"),
            ('°', r"^\circ"),
            ('−', "-"),
            ('{', r"\{"),
            ('}', r"\}"),
            ('\\', r"\backslash"),
            ('#', r"\#"),
            ('%', r"\%"),
            ('$', r"\$"),
            ('_', r"\_"),
            ('^', r"\^{}"),
            ('~', r"\sim"),
            ('\u{2009}', r"\,"),
            ('\u{200A}', r"\,"),
            ('\u{2006}', r"\,"),
            ('\u{2005}', r"\;"),
            ('\u{2004}', r"\;"),
            ('\u{00A0}', r"\ "),
            ('\u{2002}', r"\ "),
            ('\u{2003}', r"\ "),
            ('\u{2061}', ""),
            ('\u{2062}', ""),
            ('\u{2063}', ""),
            ('\u{2064}', ""),
        ];
        for (c, want) in samples {
            assert_eq!(symbol(c), Some(want), "{c}");
        }
        assert_eq!(symbol('x'), None);
        assert_eq!(symbol('é'), None);
    }

    #[test]
    fn commands_are_separated_from_following_letters() {
        let xml = format!("{}{}{}", r("α"), r("x"), r("2"));
        assert_eq!(math(&xml).unwrap(), r"\alpha x2");
        let tight = format!("{}{}", r("∑"), r("("));
        assert_eq!(math(&tight).unwrap(), r"\sum(");
    }

    #[test]
    fn math_paragraphs_and_non_math_content() {
        let xml = format!(
            "<m:oMathPara {M_NS}><m:oMath>{}</m:oMath><m:oMath>{}</m:oMath></m:oMathPara>",
            r("a"),
            r("b")
        );
        assert_eq!(
            to_latex(&parse_xml(xml.as_bytes()).unwrap()).unwrap(),
            r"a \\ b"
        );
        let single = format!(
            "<m:oMathPara {M_NS}><m:oMathParaPr/><m:oMath>{}</m:oMath></m:oMathPara>",
            r("a")
        );
        assert_eq!(
            to_latex(&parse_xml(single.as_bytes()).unwrap()).unwrap(),
            "a"
        );
        let tracked = format!("<w:ins>{}</w:ins><w:bookmarkStart/>{}", r("a"), r("b"));
        assert_eq!(math(&tracked).unwrap(), "ab");
        let nested = format!("<m:oMath>{}</m:oMath>", r("z"));
        assert_eq!(math(&nested).unwrap(), "z");
        let property_only = "<m:ctrlPr/>";
        assert_eq!(math(property_only).unwrap(), "");
    }

    #[test]
    fn unknown_elements_are_errors() {
        assert_eq!(
            math("<m:zigzag/>").unwrap_err(),
            OmmlError::Unsupported("zigzag".to_string())
        );
        assert_eq!(
            math(&format!(
                "<m:f><m:num><m:blob/></m:num><m:den>{}</m:den></m:f>",
                r("b")
            ))
            .unwrap_err(),
            OmmlError::Unsupported("blob".to_string())
        );
        let not_math = parse_xml(format!("<w:p {M_NS}/>").as_bytes()).unwrap();
        assert_eq!(
            to_latex(&not_math).unwrap_err(),
            OmmlError::Unsupported("p".to_string())
        );
        assert!(OmmlError::Unsupported("x".into()).to_string().contains("x"));
    }

    #[test]
    fn single_token_detection() {
        for s in ["x", "2", r"\alpha", r"\{", "+"] {
            assert!(is_single_token(s), "{s}");
        }
        for s in [
            "",
            "xy",
            "{",
            "^",
            "_",
            " ",
            r"\",
            r"\al1",
            r"\{x",
            r"\alpha x",
        ] {
            assert!(!is_single_token(s), "{s:?}");
        }
        assert_eq!(group(""), "{}");
        assert_eq!(group("a+b"), "{a+b}");
    }
}
