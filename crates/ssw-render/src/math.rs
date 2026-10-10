//! LaTeX math → MiTeX → Typst → [`Scene`].
//!
//! MiTeX (the Rust crate, not its wasm plugin) converts the LaTeX source to Typst math; the
//! result is evaluated with MiTeX's vendored command specification (`third_party/mitex-spec`,
//! served from memory by [`TypstWorld`]) and laid out by Typst on a page that shrinks to the
//! formula. The page frame becomes a [`Scene`].
//!
//! MiTeX's parser recovers silently from unbalanced input, so [`render_math`] checks braces,
//! `\left`/`\right` and `\begin`/`\end` itself and reports the problem as a [`MathError`].
//! Commands that need the not-yet-ported xarrow package (`\xrightarrow`, …) are errors too.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use thiserror::Error;

use crate::fonts::TEXT_FAMILY;
use crate::frame_to_scene::{BASELINE_MARKER, frame_to_scene};
use crate::scene::{Color, Scene};
use crate::typst_markup::string_literal;
use crate::world::{MITEX_DIR, TypstWorld, lock};

/// Formulas kept in the cache; the cache is emptied when it is full.
const MAX_CACHE_ENTRIES: usize = 512;

/// Largest accepted font size in points.
const MAX_FONT_SIZE_PT: f32 = 1000.0;

/// Commands of the vendored MiTeX specification that are built on the xarrow package.
const UNSUPPORTED_COMMANDS: [&str; 19] = [
    "xleftarrow",
    "xrightarrow",
    "xLeftarrow",
    "xRightarrow",
    "xleftrightarrow",
    "xLeftrightarrow",
    "xhookleftarrow",
    "xhookrightarrow",
    "xtwoheadleftarrow",
    "xtwoheadrightarrow",
    "xleftharpoonup",
    "xrightharpoonup",
    "xleftharpoondown",
    "xrightharpoondown",
    "xleftrightharpoons",
    "xrightleftharpoons",
    "xtofrom",
    "xmapsto",
    "xlongequal",
];

/// A formula that cannot be rendered. The message names the problem (unknown or unsupported
/// command, unbalanced braces, Typst error); callers show it next to the formula source.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct MathError {
    /// What is wrong with the formula.
    pub message: String,
}

impl MathError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Renders a LaTeX formula (without `$` delimiters) to a [`Scene`] in points.
///
/// `display` selects display style (block formula) over inline style. The scene is as large as
/// the formula; for inline formulas `Scene::baseline` is the distance from its top edge to the
/// text baseline. `color` paints every glyph and rule. Results are cached per
/// `(latex, display, font_size_pt, color)`; the first call in a process loads the fonts and
/// the MiTeX specification (about 13 ms), later ones take well under a millisecond when
/// cached.
pub fn render_math(
    latex: &str,
    display: bool,
    font_size_pt: f32,
    color: Color,
) -> Result<Scene, MathError> {
    shared().render(latex, display, font_size_pt, color)
}

/// The shared renderer behind [`render_math`] and the export formula check.
fn shared() -> &'static MathRenderer {
    static SHARED: LazyLock<MathRenderer> = LazyLock::new(MathRenderer::new);
    &SHARED
}

/// The Typst expression that typesets `latex`, for embedding in a Typst document that has
/// imported `mitex-scope` (see [`typst_import`]). The formula is rendered once to prove that
/// Typst accepts it, so an exported document never fails because of a bad formula.
pub(crate) fn export_expression(latex: &str, display: bool) -> Result<String, MathError> {
    let expression = typst_expression(latex, display)?;
    shared().render(latex, display, 12.0, Color::BLACK)?;
    Ok(expression)
}

/// The import line that brings `mitex-scope` into a Typst document.
pub(crate) fn typst_import() -> String {
    format!("#import \"{MITEX_DIR}/mod.typ\": mitex-scope\n")
}

/// A formula renderer with its own Typst world and cache.
#[derive(Debug)]
pub struct MathRenderer {
    world: TypstWorld,
    cache: Mutex<HashMap<CacheKey, Result<Scene, MathError>>>,
}

type CacheKey = (String, bool, u32, Color);

impl Default for MathRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl MathRenderer {
    /// A renderer with the bundled fonts and no file access.
    pub fn new() -> Self {
        Self {
            world: TypstWorld::new(None),
            cache: Mutex::default(),
        }
    }

    /// See [`render_math`].
    pub fn render(
        &self,
        latex: &str,
        display: bool,
        font_size_pt: f32,
        color: Color,
    ) -> Result<Scene, MathError> {
        let size_ok = font_size_pt > 0.0 && font_size_pt <= MAX_FONT_SIZE_PT;
        if !size_ok {
            return Err(MathError::new(format!(
                "invalid font size {font_size_pt} (expected 0 < size <= {MAX_FONT_SIZE_PT})"
            )));
        }
        let key = (latex.to_owned(), display, font_size_pt.to_bits(), color);
        if let Some(hit) = lock(&self.cache).get(&key) {
            return hit.clone();
        }
        let result = self.render_uncached(latex, display, font_size_pt, color);
        let mut cache = lock(&self.cache);
        if cache.len() >= MAX_CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, result.clone());
        result
    }

    fn render_uncached(
        &self,
        latex: &str,
        display: bool,
        font_size_pt: f32,
        color: Color,
    ) -> Result<Scene, MathError> {
        let expression = typst_expression(latex, display)?;
        let source = page_source(&expression, display, font_size_pt, color);
        let document = self
            .world
            .compile(&source)
            .map_err(|e| MathError::new(e.message))?;
        // A formula always yields exactly one page; the default scene is never returned.
        Ok(document
            .pages()
            .first()
            .map(|page| frame_to_scene(&page.frame))
            .unwrap_or_default())
    }

    #[cfg(test)]
    fn cached_formulas(&self) -> usize {
        lock(&self.cache).len()
    }
}

/// Typst expression (without the leading `#`) for a formula.
fn typst_expression(latex: &str, display: bool) -> Result<String, MathError> {
    validate(latex)?;
    let converted = mitex::convert_math(latex, None).map_err(MathError::new)?;
    // `eval` in math mode returns an inline equation. A block equation (display style) is a
    // `$ … $` expression: the spaces after the opening and before the closing `$` make it one.
    Ok(if display {
        format!(
            "eval({}, scope: mitex-scope)",
            string_literal(&format!("$ {converted} $"))
        )
    } else {
        format!(
            "eval({}, mode: \"math\", scope: mitex-scope)",
            string_literal(&converted)
        )
    })
}

/// The Typst document whose single page is the formula. Inline formulas start with an invisible
/// zero-width run (fill [`BASELINE_MARKER`]) whose position is the baseline of the line.
fn page_source(expression: &str, display: bool, font_size_pt: f32, color: Color) -> String {
    let [mr, mg, mb, ma] = BASELINE_MARKER;
    let marker = if display {
        String::new()
    } else {
        format!("#box(width: 0pt, text(fill: rgb(\"#{mr:02x}{mg:02x}{mb:02x}{ma:02x}\"))[x])")
    };
    let Color { r, g, b, a } = color;
    format!(
        "{import}#set page(width: auto, height: auto, margin: 0pt)\n\
         #set text(font: {font}, size: {font_size_pt}pt, fill: rgb(\"#{r:02x}{g:02x}{b:02x}{a:02x}\"))\n\
         {marker}#{expression}\n",
        import = typst_import(),
        font = string_literal(TEXT_FAMILY),
    )
}

/// Structural checks MiTeX does not make. Returns the first problem found.
fn validate(latex: &str) -> Result<(), MathError> {
    if latex.trim().is_empty() {
        return Err(MathError::new("empty formula"));
    }
    let bytes = latex.as_bytes();
    let mut depth = 0usize;
    let mut left = 0usize;
    let mut environments: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                let name_len = bytes[i + 1..]
                    .iter()
                    .take_while(|b| b.is_ascii_alphabetic())
                    .count();
                if name_len == 0 {
                    // A control symbol (`\{`, `\\`, `\,`): the next character is not syntax.
                    i += 2;
                    continue;
                }
                let name = &latex[i + 1..i + 1 + name_len];
                i += 1 + name_len;
                match name {
                    "left" => left += 1,
                    "right" => {
                        left = left
                            .checked_sub(1)
                            .ok_or_else(|| MathError::new("\\right without a matching \\left"))?;
                    }
                    "begin" | "end" => {
                        let (environment, next) = environment_name(latex, i).ok_or_else(|| {
                            MathError::new(format!("\\{name} needs an environment name in braces"))
                        })?;
                        i = next;
                        if name == "begin" {
                            environments.push(environment);
                        } else {
                            match environments.pop() {
                                Some(open) if open == environment => {}
                                Some(open) => {
                                    return Err(MathError::new(format!(
                                        "\\end{{{environment}}} does not match \\begin{{{open}}}"
                                    )));
                                }
                                None => {
                                    return Err(MathError::new(format!(
                                        "\\end{{{environment}}} without a matching \\begin"
                                    )));
                                }
                            }
                        }
                    }
                    _ if UNSUPPORTED_COMMANDS.contains(&name) => {
                        return Err(MathError::new(format!(
                            "unsupported command: \\{name} (extensible arrows are not available)"
                        )));
                    }
                    _ => {}
                }
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| MathError::new("unmatched }"))?;
            }
            // A comment runs to the end of the line.
            b'%' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if depth > 0 {
        return Err(MathError::new("unclosed {"));
    }
    if left > 0 {
        return Err(MathError::new("\\left without a matching \\right"));
    }
    if let Some(open) = environments.pop() {
        return Err(MathError::new(format!(
            "\\begin{{{open}}} without a matching \\end"
        )));
    }
    Ok(())
}

/// The `{name}` that follows `\begin` / `\end` at byte offset `from`, and the offset after it.
fn environment_name(latex: &str, from: usize) -> Option<(&str, usize)> {
    let rest = &latex[from..];
    let body = rest.trim_start().strip_prefix('{')?;
    let close = body.find('}')?;
    let consumed = rest.len() - body.len() + close + 1;
    Some((body[..close].trim(), from + consumed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_table() {
        let ok = [
            r"\frac{a+b}{c}",
            r"\left( \frac{1}{2} \right)^{\!2}",
            r"\left. x \right|_0^1",
            r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
            r"\begin{array}{cc} 1 & 2 \end{array}",
            r"\{ x \}",
            r"\\ \, \; \%",
            "a % } ignored { comment\n+ b",
            r"\leftarrow \rightarrow",
            r"é + \alpha",
            "\\begin {aligned} a &= b \\end{aligned}",
        ];
        for latex in ok {
            assert_eq!(validate(latex), Ok(()), "{latex}");
        }
        let bad = [
            ("", "empty formula"),
            ("  \n ", "empty formula"),
            (r"\frac{a}{", "unclosed {"),
            (r"a}", "unmatched }"),
            (r"\left( x", "\\left without a matching \\right"),
            (r"x \right)", "\\right without a matching \\left"),
            (
                r"\begin{pmatrix} a",
                "\\begin{pmatrix} without a matching \\end",
            ),
            (
                r"a \end{pmatrix}",
                "\\end{pmatrix} without a matching \\begin",
            ),
            (
                r"\begin{pmatrix} a \end{bmatrix}",
                "\\end{bmatrix} does not match \\begin{pmatrix}",
            ),
            (r"\begin pmatrix", "\\begin needs an environment name"),
            (r"\begin{pmatrix", "\\begin needs an environment name"),
            (r"A \xrightarrow{f} B", "unsupported command: \\xrightarrow"),
            (r"\xmapsto{g}", "unsupported command: \\xmapsto"),
        ];
        for (latex, expected) in bad {
            let err = validate(latex).expect_err(latex);
            assert!(err.message.contains(expected), "{latex}: {err}");
        }
        // A command that merely starts with an unsupported name is fine.
        assert_eq!(validate(r"\xrightarrowx"), Ok(()));
    }

    #[test]
    fn every_unsupported_command_is_in_the_spec_and_rejected() {
        let spec = include_str!("../../../third_party/mitex-spec/latex/standard.typ");
        for name in UNSUPPORTED_COMMANDS {
            assert!(
                spec.contains(&format!("{name}: arrow-handle(")),
                "{name} is not an xarrow command of the vendored spec"
            );
            let err = validate(&format!("\\{name}{{f}}")).expect_err(name);
            assert!(err.message.contains(name), "{err}");
        }
        let handles = spec.matches(": arrow-handle(").count();
        assert_eq!(handles, UNSUPPORTED_COMMANDS.len());
    }

    #[test]
    fn environment_names_are_trimmed_and_positioned() {
        assert_eq!(
            environment_name(r"\begin{ aligned }x", 6),
            Some(("aligned", 17))
        );
        assert_eq!(environment_name("  {a}", 0), Some(("a", 5)));
        assert_eq!(environment_name("x{a}", 0), None);
        assert_eq!(environment_name("{a", 0), None);
    }

    #[test]
    fn expression_embeds_the_converted_formula_as_a_string() {
        let display = typst_expression(r"\frac{a}{b}", true).expect("valid");
        assert!(display.starts_with("eval(\"$ "), "{display}");
        assert!(display.ends_with(" $\", scope: mitex-scope)"), "{display}");
        let inline = typst_expression("x", false).expect("valid");
        assert!(inline.starts_with("eval(\""), "{inline}");
        assert!(
            inline.ends_with("\", mode: \"math\", scope: mitex-scope)"),
            "{inline}"
        );
        let err = typst_expression(r"\foobar{x}", false).expect_err("unknown command");
        assert!(err.message.contains("foobar"), "{err}");
        assert!(typst_expression(r"\frac{a}{", false).is_err());
        assert_eq!(typst_import(), "#import \"mitex/mod.typ\": mitex-scope\n");
    }

    #[test]
    fn page_source_marks_inline_formulas_only() {
        let inline = page_source("EXPR", false, 12.5, Color::rgba(255, 0, 0, 128));
        assert!(inline.contains("#box(width: 0pt, text(fill: rgb(\"#01020304\"))[x])#EXPR"));
        assert!(inline.contains("size: 12.5pt, fill: rgb(\"#ff000080\")"));
        assert!(inline.contains("font: \"New Computer Modern\""));
        let display = page_source("EXPR", true, 16.0, Color::BLACK);
        assert!(!display.contains("#01020304"));
        assert!(display.ends_with("\n#EXPR\n"));
        assert!(display.contains("size: 16pt, fill: rgb(\"#000000ff\")"));
    }

    #[test]
    fn results_and_errors_are_cached() {
        let renderer = MathRenderer::default();
        assert_eq!(renderer.cached_formulas(), 0);
        let first = renderer
            .render("x^2", false, 14.0, Color::BLACK)
            .expect("renders");
        assert_eq!(renderer.cached_formulas(), 1);
        let again = renderer
            .render("x^2", false, 14.0, Color::BLACK)
            .expect("cached");
        assert_eq!(first, again);
        assert_eq!(renderer.cached_formulas(), 1);
        // Every key component distinguishes entries.
        renderer
            .render("x^2", true, 14.0, Color::BLACK)
            .expect("renders");
        renderer
            .render("x^2", false, 15.0, Color::BLACK)
            .expect("renders");
        renderer
            .render("x^2", false, 14.0, Color::WHITE)
            .expect("renders");
        assert_eq!(renderer.cached_formulas(), 4);
        // Errors are cached as well and stay errors.
        let err = renderer
            .render(r"\frac{", false, 14.0, Color::BLACK)
            .expect_err("bad");
        assert_eq!(renderer.cached_formulas(), 5);
        let err_again = renderer
            .render(r"\frac{", false, 14.0, Color::BLACK)
            .expect_err("bad");
        assert_eq!(err, err_again);
        assert_eq!(renderer.cached_formulas(), 5);
    }

    #[test]
    fn full_cache_is_emptied_before_inserting() {
        let renderer = MathRenderer::new();
        for n in 0..MAX_CACHE_ENTRIES {
            // Invalid formulas cost no Typst compile but occupy a cache slot.
            let latex = format!("{{{n}");
            renderer
                .render(&latex, false, 12.0, Color::BLACK)
                .expect_err("unclosed");
        }
        assert_eq!(renderer.cached_formulas(), MAX_CACHE_ENTRIES);
        renderer
            .render("}", false, 12.0, Color::BLACK)
            .expect_err("unmatched");
        assert_eq!(renderer.cached_formulas(), 1);
    }

    #[test]
    fn invalid_font_sizes_are_rejected() {
        let renderer = MathRenderer::new();
        for size in [0.0, -1.0, f32::NAN, f32::INFINITY, MAX_FONT_SIZE_PT + 1.0] {
            let err = renderer
                .render("x", false, size, Color::BLACK)
                .expect_err("size");
            assert!(err.message.contains("invalid font size"), "{size}: {err}");
        }
        assert_eq!(renderer.cached_formulas(), 0);
        assert!(
            renderer
                .render("x", false, MAX_FONT_SIZE_PT, Color::BLACK)
                .is_ok()
        );
    }

    #[test]
    fn export_expression_checks_the_formula_with_typst() {
        let expression = export_expression(r"\sum_{i=1}^n i", true).expect("valid");
        assert!(expression.starts_with("eval(\"$ "));
        assert!(
            export_expression("x", false)
                .expect("valid")
                .starts_with("eval(")
        );
        assert!(export_expression(r"\xrightarrow{f}", false).is_err());
        assert!(export_expression("", false).is_err());
    }
}
