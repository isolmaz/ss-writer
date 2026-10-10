//! Mermaid diagram source → [`Scene`].
//!
//! The pipeline is `mermaid-rs-renderer` (vendored in `third_party/`, parse → layout → SVG)
//! followed by [`svg_to_scene`]. This module adds what the renderer lacks as a library:
//!
//! * `%%{init: …}%%` directives (also when spread over several lines) and the `config:` block of
//!   a YAML front matter are merged and applied: `theme`, `themeVariables`, `fontFamily`,
//!   `flowchart.nodeSpacing` and `flowchart.rankSpacing`. Directives win over the front matter.
//! * light and dark palettes in Segoe UI, with a readable pie palette;
//! * a guard around everything: the work runs on a worker thread with a large stack, panics are
//!   caught and a diagram that takes longer than [`RENDER_LIMIT`] is abandoned (the worker cannot
//!   be cancelled; it finishes in the background and its result is dropped);
//! * a fallback for diagram types whose output is known to be wrong
//!   (`sankey-beta`, `block`): the caller shows the source instead, see
//!   [`MermaidOutcome`].

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use mermaid_rs_renderer::{
    DiagramKind, LayoutConfig, Theme, compute_layout, parse_mermaid, render_svg,
};
use serde_json::{Map, Number, Value};
use thiserror::Error;

use crate::scene::Scene;
use crate::svg_to_scene::svg_to_scene;

/// Longest time [`render_mermaid`] waits for a diagram.
pub const RENDER_LIMIT: Duration = Duration::from_millis(500);

/// Font family of every diagram (a user `fontFamily` overrides it).
pub const FONT_FAMILY: &str = "\"Segoe UI\", sans-serif";

/// Base font size in CSS pixels (a user `fontSize` overrides it).
const FONT_SIZE: f32 = 14.0;

/// The layout of large diagrams is recursive: give the worker far more than the default 2 MiB.
const WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;

/// Slice colors of pie charts (Tableau 10 plus two), mid tones that read on white.
const PIE_LIGHT: [&str; 12] = [
    "#4C78A8", "#F58518", "#54A24B", "#E45756", "#72B7B2", "#B279A2", "#EECA3B", "#FF9DA6",
    "#9D755D", "#BAB0AC", "#6BAED6", "#A1D99B",
];

/// Brighter variants of [`PIE_LIGHT`] that keep their contrast on the dark background.
const PIE_DARK: [&str; 12] = [
    "#6FA8DC", "#F6A04D", "#7BC16F", "#EF7B7A", "#8FD0CB", "#C79AD0", "#F2D664", "#FFB6C1",
    "#B8957A", "#C8C2BE", "#8EC6EA", "#B5E3AB",
];

/// Which palette a diagram is drawn with unless its own configuration names a theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MermaidTheme {
    #[default]
    Light,
    Dark,
}

/// Why a diagram is not shown as a picture.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum UnsupportedReason {
    /// The diagram type is recognized but renders wrongly, so it is not drawn.
    #[error("{0} diagrams are not supported")]
    DiagramType(&'static str),
    /// The source is not valid Mermaid (message of the parser).
    #[error("invalid Mermaid source: {0}")]
    Syntax(String),
    /// The renderer produced an SVG that could not be converted (message of the SVG parser).
    #[error("the rendered SVG is unusable: {0}")]
    Svg(String),
    /// The renderer panicked (panic message).
    #[error("the diagram renderer failed: {0}")]
    Crashed(String),
    /// The renderer needed more than the given time.
    #[error("rendering took longer than {0:?}")]
    TimedOut(Duration),
    /// The worker thread could not be started (message of the OS).
    #[error("cannot start the diagram renderer: {0}")]
    Worker(String),
}

/// Result of [`render_mermaid`].
#[derive(Clone, Debug, PartialEq)]
pub enum MermaidOutcome {
    /// The diagram as a vector scene (CSS pixels) and the SVG it was made from.
    Diagram(Scene, String),
    /// The caller falls back to showing the source with a note.
    Unsupported { reason: UnsupportedReason },
}

/// Renders `src` (the content of a ```` ```mermaid ```` block) within [`RENDER_LIMIT`].
pub fn render_mermaid(src: &str, theme: MermaidTheme) -> MermaidOutcome {
    render_mermaid_within(src, theme, RENDER_LIMIT)
}

/// Like [`render_mermaid`] with an explicit time limit (export may allow more than an editor
/// preview).
pub fn render_mermaid_within(src: &str, theme: MermaidTheme, limit: Duration) -> MermaidOutcome {
    let src = src.to_owned();
    let builder = thread::Builder::new()
        .name("mermaid".to_owned())
        .stack_size(WORKER_STACK_BYTES);
    match run_guarded(builder, limit, move || render_unguarded(&src, theme)).and_then(|r| r) {
        Ok((scene, svg)) => MermaidOutcome::Diagram(scene, svg),
        Err(reason) => MermaidOutcome::Unsupported { reason },
    }
}

/// Runs `job` on a thread made by `builder`; panics and overruns of `limit` become errors.
fn run_guarded<T: Send + 'static>(
    builder: thread::Builder,
    limit: Duration,
    job: impl FnOnce() -> T + Send + 'static,
) -> Result<T, UnsupportedReason> {
    let (sender, receiver) = mpsc::channel();
    builder
        .spawn(move || {
            // The receiver is gone when the caller gave up waiting: nobody wants the result.
            let _ = sender.send(catch_unwind(AssertUnwindSafe(job)));
        })
        .map_err(|err| UnsupportedReason::Worker(err.to_string()))?;
    match receiver.recv_timeout(limit) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(payload)) => Err(UnsupportedReason::Crashed(panic_message(payload.as_ref()))),
        Err(_) => Err(UnsupportedReason::TimedOut(limit)),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Diagram types that parse but are drawn wrongly by the renderer.
fn unsupported_kind(kind: DiagramKind) -> Option<&'static str> {
    match kind {
        DiagramKind::Sankey => Some("sankey-beta"),
        DiagramKind::Block => Some("block"),
        _ => None,
    }
}

fn render_unguarded(src: &str, theme: MermaidTheme) -> Result<(Scene, String), UnsupportedReason> {
    let source = join_multiline_directives(src);
    let parsed =
        parse_mermaid(&source).map_err(|err| UnsupportedReason::Syntax(err.to_string()))?;
    if let Some(name) = unsupported_kind(parsed.graph.kind) {
        return Err(UnsupportedReason::DiagramType(name));
    }

    let mut config = front_matter_config(&source).unwrap_or(Value::Null);
    if let Some(init) = parsed.init_config {
        merge_json(&mut config, init);
    }
    let mut palette = base_theme(theme);
    let mut layout_config = LayoutConfig::default();
    apply_config(&mut palette, &mut layout_config, &config);

    let layout = compute_layout(&parsed.graph, &palette, &layout_config);
    scene_from_svg(render_svg(&layout, &palette, &layout_config))
}

/// The scene of the renderer's SVG, and the SVG itself.
fn scene_from_svg(svg: String) -> Result<(Scene, String), UnsupportedReason> {
    svg_to_scene(svg.as_bytes())
        .map(|scene| (scene, svg))
        .map_err(|err| UnsupportedReason::Svg(err.to_string()))
}

// ---------------------------------------------------------------------------------------------
// Directives and front matter
// ---------------------------------------------------------------------------------------------

/// The renderer only reads `%%{init: …}%%` when it is on one line; a directive written over
/// several lines (`%%{` … `}%%`) is joined into one.
fn join_multiline_directives(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut open = false;
    for line in src.lines() {
        let trimmed = line.trim();
        if open {
            out.push(' ');
            out.push_str(trimmed);
        } else {
            out.push_str(line);
            open = trimmed.starts_with("%%{");
        }
        if open && trimmed.contains("}%%") {
            open = false;
        }
        if !open {
            out.push('\n');
        }
    }
    out
}

/// The `config:` object of a leading YAML front matter block (`---` … `---`), if any.
fn front_matter_config(src: &str) -> Option<Value> {
    let mut lines = src.lines().skip_while(|line| line.trim().is_empty());
    if lines.next()?.trim() != "---" {
        return None;
    }
    let mut block = Vec::new();
    let mut closed = false;
    for line in lines {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        let text = strip_yaml_comment(line);
        let content = text.trim();
        if !content.is_empty() {
            block.push((text.len() - text.trim_start().len(), content));
        }
    }
    if !closed {
        return None;
    }
    let mut position = 0;
    let mut root = parse_block(&block, &mut position, 0);
    root.remove("config").filter(Value::is_object)
}

/// `line` without a trailing YAML comment (`#` at the start or after whitespace, outside quotes).
fn strip_yaml_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut previous_is_space = true;
    for (index, ch) in line.char_indices() {
        match quote {
            Some(open) if ch == open => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '#' && previous_is_space => return &line[..index],
            None => {}
        }
        previous_is_space = ch.is_whitespace();
    }
    line
}

/// Parses the `key: value` lines of one indentation level (`lines` = indent, text) into an
/// object; deeper lines below a `key:` without value form a nested object, deeper lines below a
/// key with a value are the continuation of a multi-line scalar and are dropped.
fn parse_block(lines: &[(usize, &str)], position: &mut usize, indent: usize) -> Map<String, Value> {
    let mut map = Map::new();
    while let Some(&(line_indent, text)) = lines.get(*position) {
        if line_indent < indent {
            break;
        }
        *position += 1;
        let Some((key, value)) = text.split_once(':') else {
            continue;
        };
        let key = unquote(key.trim()).unwrap_or(key.trim()).to_owned();
        let value = value.trim();
        if value.is_empty() {
            let child_indent = lines
                .get(*position)
                .map(|line| line.0)
                .filter(|child| *child > line_indent);
            let child = match child_indent {
                Some(child) => parse_block(lines, position, child),
                None => Map::new(),
            };
            map.insert(key, Value::Object(child));
        } else {
            map.insert(key, scalar(value));
            while lines
                .get(*position)
                .is_some_and(|line| line.0 > line_indent)
            {
                *position += 1;
            }
        }
    }
    map
}

/// `text` without its surrounding quotes, when it is one quoted string.
fn unquote(text: &str) -> Option<&str> {
    ['"', '\''].into_iter().find_map(|quote| {
        text.strip_prefix(quote)?
            .strip_suffix(quote)
            .filter(|inner| !inner.contains(quote))
    })
}

fn scalar(text: &str) -> Value {
    if let Some(inner) = unquote(text) {
        return Value::String(inner.to_owned());
    }
    match text {
        "true" => return Value::Bool(true),
        "false" => return Value::Bool(false),
        "null" | "~" => return Value::Null,
        _ => {}
    }
    if let Ok(int) = text.parse::<i64>() {
        return Value::from(int);
    }
    if let Some(number) = text.parse::<f64>().ok().and_then(Number::from_f64) {
        return Value::Number(number);
    }
    if text.starts_with(['{', '['])
        && let Ok(value) = serde_json::from_str(text)
    {
        return value;
    }
    Value::String(text.to_owned())
}

/// Deep merge: objects are merged key by key, anything else in `over` replaces `base`.
fn merge_json(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(slot) => merge_json(slot, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (slot, over) => *slot = over,
    }
}

// ---------------------------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------------------------

/// The palette for `theme`: the renderer's modern (light) or dark preset with our font.
fn base_theme(theme: MermaidTheme) -> Theme {
    let (mut palette, pie) = match theme {
        MermaidTheme::Light => (Theme::modern(), PIE_LIGHT),
        MermaidTheme::Dark => (Theme::dark(), PIE_DARK),
    };
    palette.pie_colors = pie.map(String::from);
    with_default_font(palette)
}

fn with_default_font(mut palette: Theme) -> Theme {
    palette.font_family = FONT_FAMILY.to_owned();
    palette.font_size = FONT_SIZE;
    palette
}

/// Applies a merged Mermaid configuration object.
fn apply_config(palette: &mut Theme, layout: &mut LayoutConfig, config: &Value) {
    if let Some(name) = config.get("theme").and_then(Value::as_str) {
        match name.trim().to_ascii_lowercase().as_str() {
            "modern" => *palette = base_theme(MermaidTheme::Light),
            "dark" => *palette = base_theme(MermaidTheme::Dark),
            _ => {
                if let Some(named) = Theme::from_name(name) {
                    *palette = with_default_font(named);
                }
            }
        }
    }
    if let Some(family) = config.get("fontFamily").and_then(Value::as_str) {
        palette.font_family = family.to_owned();
    }
    if let Some(variables) = config.get("themeVariables").and_then(Value::as_object) {
        for (key, value) in variables {
            apply_variable(palette, key, value);
        }
    }
    if let Some(flowchart) = config.get("flowchart") {
        let spacing = |key: &str| {
            flowchart
                .get(key)
                .and_then(Value::as_f64)
                .map(|value| (value as f32).clamp(1.0, 500.0))
        };
        if let Some(value) = spacing("nodeSpacing") {
            layout.node_spacing = value;
        }
        if let Some(value) = spacing("rankSpacing") {
            layout.rank_spacing = value;
        }
    }
}

fn apply_variable(palette: &mut Theme, key: &str, value: &Value) {
    if key == "fontSize" {
        let size = value.as_f64().or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().trim_end_matches("px").trim().parse().ok())
        });
        if let Some(size) = size.filter(|size| (1.0..=200.0).contains(size)) {
            palette.font_size = size as f32;
        }
    } else if let Some(text) = value.as_str()
        && let Some(slot) = string_slot(palette, key)
    {
        *slot = text.to_owned();
    }
}

/// The color/text field a `themeVariables` key stands for.
fn string_slot<'a>(palette: &'a mut Theme, key: &str) -> Option<&'a mut String> {
    Some(match key {
        "fontFamily" => &mut palette.font_family,
        "primaryColor" => &mut palette.primary_color,
        "primaryTextColor" => &mut palette.primary_text_color,
        "primaryBorderColor" => &mut palette.primary_border_color,
        "lineColor" => &mut palette.line_color,
        "secondaryColor" => &mut palette.secondary_color,
        "tertiaryColor" => &mut palette.tertiary_color,
        "textColor" => &mut palette.text_color,
        "background" => &mut palette.background,
        "edgeLabelBackground" => &mut palette.edge_label_background,
        "clusterBkg" => &mut palette.cluster_background,
        "clusterBorder" => &mut palette.cluster_border,
        "actorBkg" => &mut palette.sequence_actor_fill,
        "actorBorder" => &mut palette.sequence_actor_border,
        "actorLineColor" => &mut palette.sequence_actor_line,
        "noteBkgColor" => &mut palette.sequence_note_fill,
        "noteBorderColor" => &mut palette.sequence_note_border,
        "activationBkgColor" => &mut palette.sequence_activation_fill,
        "activationBorderColor" => &mut palette.sequence_activation_border,
        "commitLabelColor" => &mut palette.git_commit_label_color,
        "commitLabelBackground" => &mut palette.git_commit_label_background,
        "tagLabelColor" => &mut palette.git_tag_label_color,
        "tagLabelBackground" => &mut palette.git_tag_label_background,
        "tagLabelBorder" => &mut palette.git_tag_label_border,
        "pieTitleTextColor" => &mut palette.pie_title_text_color,
        "pieSectionTextColor" => &mut palette.pie_section_text_color,
        "pieLegendTextColor" => &mut palette.pie_legend_text_color,
        "pieStrokeColor" => &mut palette.pie_stroke_color,
        "pieOuterStrokeColor" => &mut palette.pie_outer_stroke_color,
        _ => return numbered_slot(palette, key),
    })
}

/// `git0`…`git7`, `gitInv0`…, `gitBranchLabel0`… and `pie1`…`pie12`.
fn numbered_slot<'a>(palette: &'a mut Theme, key: &str) -> Option<&'a mut String> {
    let number = |prefix: &str| key.strip_prefix(prefix)?.parse::<usize>().ok();
    if let Some(index) = number("gitBranchLabel") {
        palette.git_branch_label_colors.get_mut(index)
    } else if let Some(index) = number("gitInv") {
        palette.git_inv_colors.get_mut(index)
    } else if let Some(index) = number("git") {
        palette.git_colors.get_mut(index)
    } else {
        palette.pie_colors.get_mut(number("pie")?.checked_sub(1)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(text: &str) -> Value {
        let lines: Vec<(usize, &str)> = text
            .lines()
            .map(strip_yaml_comment)
            .filter(|line| !line.trim().is_empty())
            .map(|line| (line.len() - line.trim_start().len(), line.trim()))
            .collect();
        let mut position = 0;
        Value::Object(parse_block(&lines, &mut position, 0))
    }

    #[test]
    fn yaml_subset_nesting_scalars_and_comments() {
        let value = parse(
            "# leading comment\n\
             title: Plan   # trailing\n\
             config:\n\
             \x20 theme: \"dark\"\n\
             \x20 themeVariables:\n\
             \x20   primaryColor: \"#ff0000\"\n\
             \x20   fontSize: 18\n\
             \x20   ratio: 1.5\n\
             \x20   flag: true\n\
             \x20   off: false\n\
             \x20   nothing: ~\n\
             \x20   inline: {\"a\": [1, 2]}\n\
             \x20   bare: #notacolor\n\
             \x20   word: some words here\n\
             \x20 'single': 'quoted: value'\n\
             other: x\n",
        );
        assert_eq!(
            value,
            json!({
                "title": "Plan",
                "config": {
                    "theme": "dark",
                    "themeVariables": {
                        "primaryColor": "#ff0000",
                        "fontSize": 18,
                        "ratio": 1.5,
                        "flag": true,
                        "off": false,
                        "nothing": null,
                        "inline": {"a": [1, 2]},
                        "bare": {},
                        "word": "some words here"
                    },
                    "single": "quoted: value"
                },
                "other": "x"
            })
        );
    }

    #[test]
    fn yaml_subset_skips_lines_without_key_and_multiline_scalars() {
        let value = parse(
            "config:\n\
             \x20 css: \" first line\n\
             \x20     rect[id^=x] { height: calc(1px) }\n\
             \x20     more: text\n\
             \x20 \"\n\
             \x20 theme: base\n\
             no colon here\n\
             empty:\n\
             last: 1\n",
        );
        assert_eq!(
            value,
            json!({"config": {"css": "\" first line", "theme": "base"}, "empty": {}, "last": 1})
        );
    }

    #[test]
    fn yaml_scalars_that_look_like_numbers_but_are_not() {
        assert_eq!(scalar("nan"), json!("nan"));
        assert_eq!(scalar("inf"), json!("inf"));
        assert_eq!(scalar("[unclosed"), json!("[unclosed"));
        assert_eq!(scalar("\"a\"b\""), json!("\"a\"b\""));
        assert_eq!(scalar("-7"), json!(-7));
    }

    #[test]
    fn comment_stripping_respects_quotes_and_word_boundaries() {
        assert_eq!(strip_yaml_comment("a: 1 # c"), "a: 1 ");
        assert_eq!(strip_yaml_comment("# all"), "");
        assert_eq!(strip_yaml_comment("a: \"x # y\" # z"), "a: \"x # y\" ");
        assert_eq!(strip_yaml_comment("a: 'x # y'"), "a: 'x # y'");
        assert_eq!(strip_yaml_comment("a: b#c"), "a: b#c");
    }

    #[test]
    fn front_matter_config_requires_closed_block_with_config_object() {
        let config = front_matter_config("\n---\ntitle: T\nconfig:\n  theme: forest\n---\npie\n");
        assert_eq!(config, Some(json!({"theme": "forest"})));
        // Blank lines and comment-only lines inside the block carry no content.
        let spaced = front_matter_config("---\nconfig:\n\n  # note\n  theme: forest\n---\npie");
        assert_eq!(spaced, Some(json!({"theme": "forest"})));
        assert_eq!(front_matter_config("pie\n  \"a\": 1"), None);
        assert_eq!(
            front_matter_config("---\nconfig:\n  theme: forest\npie"),
            None
        );
        assert_eq!(front_matter_config("---\ntitle: T\n---\npie"), None);
        assert_eq!(front_matter_config("---\nconfig: text\n---\npie"), None);
        assert_eq!(front_matter_config(""), None);
    }

    #[test]
    fn multiline_directives_become_one_line() {
        let joined = join_multiline_directives(
            "%%{\n  init: {\"theme\": \"dark\"}\n}%%\npie\n%%{init: {\"a\": 1}}%%\n  \"x\": 1\n",
        );
        assert_eq!(
            joined,
            "%%{ init: {\"theme\": \"dark\"} }%%\npie\n%%{init: {\"a\": 1}}%%\n  \"x\": 1\n"
        );
    }

    #[test]
    fn json_merge_is_deep_and_later_values_win() {
        let mut base = json!({"a": {"x": 1, "y": 2}, "b": 1});
        merge_json(&mut base, json!({"a": {"y": 3, "z": 4}, "c": [1]}));
        assert_eq!(
            base,
            json!({"a": {"x": 1, "y": 3, "z": 4}, "b": 1, "c": [1]})
        );
        let mut null = Value::Null;
        merge_json(&mut null, json!({"a": 1}));
        assert_eq!(null, json!({"a": 1}));
        let mut replaced = json!({"a": 1});
        merge_json(&mut replaced, json!(5));
        assert_eq!(replaced, json!(5));
    }

    #[test]
    fn every_theme_variable_reaches_its_field() {
        let mut theme = base_theme(MermaidTheme::Light);
        let keys = [
            "fontFamily",
            "primaryColor",
            "primaryTextColor",
            "primaryBorderColor",
            "lineColor",
            "secondaryColor",
            "tertiaryColor",
            "textColor",
            "background",
            "edgeLabelBackground",
            "clusterBkg",
            "clusterBorder",
            "actorBkg",
            "actorBorder",
            "actorLineColor",
            "noteBkgColor",
            "noteBorderColor",
            "activationBkgColor",
            "activationBorderColor",
            "commitLabelColor",
            "commitLabelBackground",
            "tagLabelColor",
            "tagLabelBackground",
            "tagLabelBorder",
            "pieTitleTextColor",
            "pieSectionTextColor",
            "pieLegendTextColor",
            "pieStrokeColor",
            "pieOuterStrokeColor",
        ];
        let mut seen = std::collections::HashSet::new();
        for key in keys {
            let marker = format!("marker-{key}");
            apply_variable(&mut theme, key, &json!(marker));
            let slot = string_slot(&mut theme, key).expect("known key");
            assert_eq!(*slot, marker, "{key}");
            // Every key owns a distinct field.
            assert!(
                seen.insert(slot as *mut String as usize),
                "{key} shares a field"
            );
        }
        for index in 0..8 {
            for prefix in ["git", "gitInv", "gitBranchLabel"] {
                let key = format!("{prefix}{index}");
                apply_variable(&mut theme, &key, &json!(format!("m-{key}")));
                let slot = string_slot(&mut theme, &key).expect("indexed key");
                assert_eq!(*slot, format!("m-{key}"));
                assert!(
                    seen.insert(slot as *mut String as usize),
                    "{key} shares a field"
                );
            }
        }
        for index in 1..=12 {
            let key = format!("pie{index}");
            apply_variable(&mut theme, &key, &json!(format!("m-{key}")));
            assert_eq!(theme.pie_colors[index - 1], format!("m-{key}"));
        }
    }

    #[test]
    fn unknown_or_out_of_range_variables_are_ignored() {
        let mut theme = base_theme(MermaidTheme::Light);
        let before = format!("{theme:?}");
        for key in [
            "nope",
            "git8",
            "gitInv9",
            "gitBranchLabel8",
            "pie0",
            "pie13",
            "pieX",
            "git",
        ] {
            assert!(string_slot(&mut theme, key).is_none(), "{key}");
            apply_variable(&mut theme, key, &json!("#123456"));
        }
        apply_variable(&mut theme, "primaryColor", &json!(5));
        apply_variable(&mut theme, "fontSize", &json!("huge"));
        apply_variable(&mut theme, "fontSize", &json!(0));
        apply_variable(&mut theme, "fontSize", &json!(5000));
        assert_eq!(format!("{theme:?}"), before);
    }

    #[test]
    fn font_size_accepts_numbers_and_pixel_strings() {
        let mut theme = base_theme(MermaidTheme::Light);
        apply_variable(&mut theme, "fontSize", &json!(20));
        assert_eq!(theme.font_size, 20.0);
        apply_variable(&mut theme, "fontSize", &json!(" 18px "));
        assert_eq!(theme.font_size, 18.0);
        apply_variable(&mut theme, "fontSize", &json!("12.5"));
        assert_eq!(theme.font_size, 12.5);
    }

    #[test]
    fn config_selects_theme_font_and_spacing() {
        let mut layout = LayoutConfig::default();
        let mut theme = base_theme(MermaidTheme::Light);

        apply_config(&mut theme, &mut layout, &json!({"theme": "dark"}));
        assert_eq!(theme.background, Theme::dark().background);
        assert_eq!(theme.pie_colors[0], PIE_DARK[0]);
        assert_eq!(theme.font_family, FONT_FAMILY);

        apply_config(&mut theme, &mut layout, &json!({"theme": " Modern "}));
        assert_eq!(theme.background, Theme::modern().background);
        assert_eq!(theme.pie_colors[0], PIE_LIGHT[0]);

        apply_config(&mut theme, &mut layout, &json!({"theme": "forest"}));
        assert_eq!(theme.primary_color, Theme::forest().primary_color);
        assert_eq!(theme.font_family, FONT_FAMILY);
        assert_eq!(theme.font_size, FONT_SIZE);

        let before = theme.primary_color.clone();
        apply_config(&mut theme, &mut layout, &json!({"theme": "no-such-theme"}));
        assert_eq!(theme.primary_color, before);

        apply_config(
            &mut theme,
            &mut layout,
            &json!({
                "fontFamily": "Cascadia Mono",
                "themeVariables": {"primaryColor": "#010203", "fontSize": "16px"},
                "flowchart": {"nodeSpacing": 80, "rankSpacing": 100000}
            }),
        );
        assert_eq!(theme.font_family, "Cascadia Mono");
        assert_eq!(theme.primary_color, "#010203");
        assert_eq!(theme.font_size, 16.0);
        assert_eq!(layout.node_spacing, 80.0);
        assert_eq!(layout.rank_spacing, 500.0);

        apply_config(
            &mut theme,
            &mut layout,
            &json!({"flowchart": {"nodeSpacing": 0}}),
        );
        assert_eq!(layout.node_spacing, 1.0);
        apply_config(&mut theme, &mut layout, &Value::Null);
        assert_eq!(layout.node_spacing, 1.0);

        // A flowchart block that names only one of the two spacings leaves the other alone.
        apply_config(
            &mut theme,
            &mut layout,
            &json!({"flowchart": {"nodeSpacing": 80}}),
        );
        apply_config(
            &mut theme,
            &mut layout,
            &json!({"flowchart": {"rankSpacing": 120}}),
        );
        assert_eq!(layout.node_spacing, 80.0);
        assert_eq!(layout.rank_spacing, 120.0);
    }

    #[test]
    fn base_themes_use_segoe_ui_and_distinct_pie_colors() {
        for theme in [MermaidTheme::Light, MermaidTheme::Dark] {
            let palette = base_theme(theme);
            assert_eq!(palette.font_family, FONT_FAMILY);
            assert_eq!(palette.font_size, FONT_SIZE);
            let mut colors = palette.pie_colors.to_vec();
            colors.sort();
            colors.dedup();
            assert_eq!(colors.len(), 12);
        }
        assert_ne!(
            base_theme(MermaidTheme::Light).background,
            base_theme(MermaidTheme::Dark).background
        );
        assert_eq!(MermaidTheme::default(), MermaidTheme::Light);
    }

    /// What the job of [`guarded`] does.
    enum Job {
        Value(u8),
        PanicStr,
        PanicString,
        PanicOther,
        Sleep,
    }

    /// Every test of the guard goes through this one closure, so a single instantiation of
    /// [`run_guarded`] sees all of its outcomes.
    fn guarded(
        builder: thread::Builder,
        limit: Duration,
        job: Job,
    ) -> Result<u8, UnsupportedReason> {
        run_guarded(builder, limit, move || match job {
            Job::Value(value) => value,
            Job::PanicStr => panic!("boom str"),
            Job::PanicString => panic!("{}", String::from("boom string")),
            Job::PanicOther => std::panic::panic_any(42_u8),
            Job::Sleep => {
                thread::sleep(Duration::from_millis(400));
                0
            }
        })
    }

    #[test]
    fn guard_reports_panics_overruns_and_spawn_failures() {
        let builder = || thread::Builder::new().stack_size(1 << 20);
        let patient = Duration::from_secs(5);
        assert_eq!(guarded(builder(), patient, Job::Value(7)), Ok(7));
        assert_eq!(
            guarded(builder(), patient, Job::PanicStr),
            Err(UnsupportedReason::Crashed("boom str".to_owned()))
        );
        assert_eq!(
            guarded(builder(), patient, Job::PanicString),
            Err(UnsupportedReason::Crashed("boom string".to_owned()))
        );
        assert_eq!(
            guarded(builder(), patient, Job::PanicOther),
            Err(UnsupportedReason::Crashed("unknown panic".to_owned()))
        );
        let limit = Duration::from_millis(20);
        assert_eq!(
            guarded(builder(), limit, Job::Sleep),
            Err(UnsupportedReason::TimedOut(limit))
        );
        // An address space reservation no machine can satisfy.
        let impossible = thread::Builder::new().stack_size(1 << 60);
        let failed = guarded(impossible, patient, Job::Value(7))
            .expect_err("no machine has that much address space");
        assert!(
            failed
                .to_string()
                .starts_with("cannot start the diagram renderer: "),
            "{failed}"
        );
    }

    #[test]
    fn rendered_svg_that_usvg_rejects_is_unusable() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"5\">\
                   <rect width=\"10\" height=\"5\"/></svg>";
        let (scene, kept) = scene_from_svg(svg.to_owned()).expect("valid svg");
        assert_eq!(kept, svg);
        assert_eq!((scene.size.width, scene.size.height), (10.0, 5.0));

        let reason = scene_from_svg("<svg".to_owned()).expect_err("truncated svg");
        assert!(
            reason
                .to_string()
                .starts_with("the rendered SVG is unusable: invalid SVG: "),
            "{reason}"
        );
    }

    #[test]
    fn unsupported_kinds_are_sankey_and_block() {
        assert_eq!(unsupported_kind(DiagramKind::Sankey), Some("sankey-beta"));
        assert_eq!(unsupported_kind(DiagramKind::Block), Some("block"));
        for kind in [
            DiagramKind::Flowchart,
            DiagramKind::Sequence,
            DiagramKind::Gantt,
            DiagramKind::Pie,
        ] {
            assert_eq!(unsupported_kind(kind), None);
        }
    }

    #[test]
    fn reasons_have_readable_messages() {
        assert_eq!(
            UnsupportedReason::DiagramType("block").to_string(),
            "block diagrams are not supported"
        );
        assert!(
            UnsupportedReason::Syntax("x".into())
                .to_string()
                .contains("invalid Mermaid")
        );
        assert!(
            UnsupportedReason::Svg("x".into())
                .to_string()
                .contains("SVG")
        );
        assert!(
            UnsupportedReason::TimedOut(Duration::from_millis(500))
                .to_string()
                .contains("500ms")
        );
        assert!(
            UnsupportedReason::Worker("x".into())
                .to_string()
                .contains("cannot start")
        );
    }
}
