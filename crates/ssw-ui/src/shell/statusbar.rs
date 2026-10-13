//! The status bar (spec §8): Ln/Col, word count, zoom, encoding, line ending and the language of
//! a plain-text document. The encoding, line-ending and language segments are buttons
//! ([`StatusEvent`]); the others are plain text.
//!
//! The bar is transparent (Mica shows through) with a hairline on top. Segments with empty text
//! (no document, or nothing to show) take no space and have no access node.

use ssw_core::i18n::{Arg, Localizer};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::{Canvas, Dash};
use crate::editor::highlight::Language;
use crate::geometry::{CornerRadii, Insets, Point, Rect, Size};
use crate::input::{Event, FocusEvent, vk};
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::anim::Motion;
use crate::widgets::{
    Control, Interaction, PointerOutcome, control_state, draw_centered, focus_ring, layout_line,
    outcome_result, perform_focus,
};

use super::layout::STATUS_BAR_HEIGHT;

/// Text size of the status bar.
const FONT: f32 = 12.0;
/// Horizontal padding inside a segment.
const SEGMENT_PADDING: f32 = 8.0;
/// Gap between segments.
const SEGMENT_GAP: f32 = 2.0;
/// Inset of the first and last segment from the bar's edges.
const EDGE_PADDING: f32 = 8.0;
/// Vertical margin of a segment inside the bar.
const SEGMENT_MARGIN: f32 = 2.0;

/// The segments of the bar, in visual order. The first two sit on the left, the rest on the right.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusSegment {
    Position,
    Words,
    Zoom,
    Encoding,
    LineEnding,
    Language,
}

impl StatusSegment {
    /// Visual order.
    pub const ALL: [StatusSegment; 6] = [
        StatusSegment::Position,
        StatusSegment::Words,
        StatusSegment::Zoom,
        StatusSegment::Encoding,
        StatusSegment::LineEnding,
        StatusSegment::Language,
    ];

    /// Whether clicking the segment does something.
    #[must_use]
    pub const fn is_clickable(self) -> bool {
        matches!(
            self,
            StatusSegment::Encoding | StatusSegment::LineEnding | StatusSegment::Language
        )
    }

    const fn is_left(self) -> bool {
        matches!(self, StatusSegment::Position | StatusSegment::Words)
    }

    /// What a click on the segment reports; `anchor` is the segment's rectangle.
    const fn event(self, anchor: Rect) -> Option<StatusEvent> {
        match self {
            StatusSegment::Encoding => Some(StatusEvent::EncodingClicked { anchor }),
            StatusSegment::LineEnding => Some(StatusEvent::LineEndingClicked { anchor }),
            StatusSegment::Language => Some(StatusEvent::LanguageClicked { anchor }),
            StatusSegment::Position | StatusSegment::Words | StatusSegment::Zoom => None,
        }
    }
}

/// Line ending style shown in the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
    Cr,
}

/// What the language segment shows for the active document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLanguage {
    /// Not a plain-text document (a Markdown or Mermaid one): the bar has no language segment.
    Hidden,
    /// A plain-text document that is not coloured.
    PlainText,
    /// A plain-text document coloured as this language.
    Language(Language),
}

impl StatusLanguage {
    /// What the segment shows for a document that is `plain` text and coloured as `language`
    /// (`None`: not coloured).
    #[must_use]
    pub const fn of(plain: bool, language: Option<Language>) -> Self {
        match (plain, language) {
            (false, _) => StatusLanguage::Hidden,
            (true, None) => StatusLanguage::PlainText,
            (true, Some(language)) => StatusLanguage::Language(language),
        }
    }
}

/// The display name of `language` (`JSON`, `C#`, `PowerShell`, …), "Plain text" for none: the
/// text of the language segment and the labels of its menu.
#[must_use]
pub fn language_label(localizer: &Localizer, language: Option<Language>) -> String {
    match language {
        Some(language) => localizer.text(&format!("statusbar-language-{}", language.name())),
        None => localizer.text("statusbar-language-plain"),
    }
}

/// What the bar shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusInfo {
    /// 1-based line of the caret.
    pub line: usize,
    /// 1-based column of the caret.
    pub column: usize,
    /// Words in the document.
    pub words: usize,
    /// Words in the selection, when something is selected.
    pub selected_words: Option<usize>,
    pub zoom_percent: u32,
    /// Display name of the encoding (`UTF-8`, `UTF-8 with BOM`, `Windows-1254`…).
    pub encoding: String,
    pub eol: Eol,
    /// The language segment: shown for a plain-text document only.
    pub language: StatusLanguage,
}

/// Localized strings of the bar that need no arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusStrings {
    /// Accessible name of the bar.
    pub label: String,
    /// Description of the encoding button.
    pub encoding_hint: String,
    /// Description of the line-ending button.
    pub eol_hint: String,
    /// Description of the language button.
    pub language_hint: String,
}

impl StatusStrings {
    /// Looks the strings up in `shell.ftl`.
    #[must_use]
    pub fn from_localizer(localizer: &Localizer) -> Self {
        Self {
            label: localizer.text("statusbar-label"),
            encoding_hint: localizer.text("statusbar-encoding-hint"),
            eol_hint: localizer.text("statusbar-eol-hint"),
            language_hint: localizer.text("statusbar-language-hint"),
        }
    }

    /// What a click on `kind` does; empty for the text segments.
    fn hint(&self, kind: StatusSegment) -> &str {
        match kind {
            StatusSegment::Encoding => &self.encoding_hint,
            StatusSegment::LineEnding => &self.eol_hint,
            StatusSegment::Language => &self.language_hint,
            StatusSegment::Position | StatusSegment::Words | StatusSegment::Zoom => "",
        }
    }
}

/// What the user clicked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StatusEvent {
    /// The encoding segment: reopen with / save with another encoding. `anchor` is the segment.
    EncodingClicked { anchor: Rect },
    /// The line-ending segment.
    LineEndingClicked { anchor: Rect },
    /// The language segment of a plain-text document: pick the language it is coloured as.
    LanguageClicked { anchor: Rect },
}

#[derive(Debug, Clone)]
struct Segment {
    kind: StatusSegment,
    id: WidgetId,
    text: String,
    width: f32,
    bounds: Rect,
    interaction: Interaction,
    activated: u32,
}

impl Segment {
    fn new(kind: StatusSegment) -> Self {
        Self {
            kind,
            id: WidgetId::allocate(),
            text: String::new(),
            width: 0.0,
            bounds: Rect::ZERO,
            interaction: Interaction::new(Motion::full()),
            activated: 0,
        }
    }

    fn is_shown(&self) -> bool {
        !self.text.is_empty()
    }

    fn is_active(&self) -> bool {
        self.kind.is_clickable() && self.is_shown()
    }
}

/// The status bar.
#[derive(Debug, Clone)]
pub struct StatusBar {
    id: WidgetId,
    strings: StatusStrings,
    segments: Vec<Segment>,
    bounds: Rect,
    events: Vec<StatusEvent>,
}

impl StatusBar {
    /// An empty bar.
    #[must_use]
    pub fn new(strings: StatusStrings) -> Self {
        Self {
            id: WidgetId::allocate(),
            strings,
            segments: StatusSegment::ALL.into_iter().map(Segment::new).collect(),
            bounds: Rect::ZERO,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    #[must_use]
    pub fn with_motion(mut self, motion: Motion) -> Self {
        for segment in &mut self.segments {
            segment.interaction.set_motion(motion);
        }
        self
    }

    /// Formats `info` with `localizer` into the segment texts.
    pub fn set_info(&mut self, info: &StatusInfo, localizer: &Localizer) {
        let line = info.line.to_string();
        let column = info.column.to_string();
        let position = localizer.format(
            "statusbar-position",
            &[("line", Arg::Str(&line)), ("column", Arg::Str(&column))],
        );
        let words = clamp_i64(info.words);
        let words = match info.selected_words {
            Some(selected) => localizer.format(
                "statusbar-words-selected",
                &[
                    ("selected", Arg::Int(clamp_i64(selected))),
                    ("count", Arg::Int(words)),
                ],
            ),
            None => localizer.format("statusbar-words", &[("count", Arg::Int(words))]),
        };
        let zoom = localizer.format(
            "statusbar-zoom",
            &[("percent", Arg::Int(i64::from(info.zoom_percent)))],
        );
        let eol = localizer.text(match info.eol {
            Eol::Lf => "statusbar-eol-lf",
            Eol::CrLf => "statusbar-eol-crlf",
            Eol::Cr => "statusbar-eol-cr",
        });
        self.set_text(StatusSegment::Position, position);
        self.set_text(StatusSegment::Words, words);
        self.set_text(StatusSegment::Zoom, zoom);
        self.set_text(StatusSegment::Encoding, info.encoding.clone());
        self.set_text(StatusSegment::LineEnding, eol);
        let language = match info.language {
            StatusLanguage::Hidden => String::new(),
            StatusLanguage::PlainText => language_label(localizer, None),
            StatusLanguage::Language(language) => language_label(localizer, Some(language)),
        };
        self.set_text(StatusSegment::Language, language);
    }

    /// Empties every segment (no document is active).
    pub fn clear(&mut self) {
        for segment in &mut self.segments {
            segment.text.clear();
        }
    }

    fn set_text(&mut self, kind: StatusSegment, text: String) {
        // `segments` follows `StatusSegment::ALL`, so a kind's discriminant is its slot.
        self.segments[kind as usize].text = text;
    }

    /// The text of `kind`; empty when the segment is not shown.
    #[must_use]
    pub fn text(&self, kind: StatusSegment) -> &str {
        self.segments
            .iter()
            .find(|segment| segment.kind == kind)
            .map_or("", |segment| segment.text.as_str())
    }

    /// The rectangle of `kind` (valid after arrange); empty when the segment is not shown.
    #[must_use]
    pub fn segment_rect(&self, kind: StatusSegment) -> Rect {
        self.segments
            .iter()
            .find(|segment| segment.kind == kind)
            .map_or(Rect::ZERO, |segment| segment.bounds)
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<StatusEvent> {
        std::mem::take(&mut self.events)
    }

    /// The clickable segment under `point` and what a click does, the text of its tooltip.
    #[must_use]
    pub fn tooltip_at(&self, point: Point) -> Option<(Rect, &str)> {
        let segment = self
            .segments
            .iter()
            .filter(|segment| segment.is_active())
            .find(|segment| segment.bounds.contains(point))?;
        Some((segment.bounds, self.strings.hint(segment.kind)))
    }

    fn collect(&mut self) {
        for segment in &mut self.segments {
            if std::mem::take(&mut segment.activated) == 0 {
                continue;
            }
            // Only the clickable segments are ever activated (`is_active`).
            if let Some(event) = segment.kind.event(segment.bounds) {
                self.events.push(event);
            }
        }
    }

    /// Carries out an access action on the segment whose access node has id `item`.
    pub fn perform_item(
        &mut self,
        cx: &mut EventCx,
        item: WidgetId,
        action: &AccessAction,
    ) -> bool {
        let Some(segment) = self
            .segments
            .iter_mut()
            .find(|segment| segment.id == item && segment.is_active())
        else {
            return false;
        };
        let done = match action {
            AccessAction::Invoke => {
                segment.activated += 1;
                true
            }
            AccessAction::Focus => perform_focus(&mut cx.for_widget(item)),
            _ => false,
        };
        self.collect();
        done
    }
}

fn clamp_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

impl Control for StatusBar {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for StatusBar {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            0.0
        };
        Size::new(width, STATUS_BAR_HEIGHT)
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        for segment in &mut self.segments {
            segment.bounds = Rect::ZERO;
            segment.width = if segment.is_shown() {
                layout_line(cx.text, cx.theme, &segment.text, FONT)
                    .size()
                    .width
                    + 2.0 * SEGMENT_PADDING
            } else {
                0.0
            };
        }
        let height = (bounds.height - 2.0 * SEGMENT_MARGIN).max(0.0);
        let top = bounds.y + (bounds.height - height) / 2.0;
        let mut left = bounds.x + EDGE_PADDING;
        for segment in self
            .segments
            .iter_mut()
            .filter(|s| s.kind.is_left() && s.is_shown())
        {
            segment.bounds = Rect::new(left, top, segment.width, height);
            left += segment.width + SEGMENT_GAP;
        }
        let mut right = bounds.right() - EDGE_PADDING;
        for segment in self
            .segments
            .iter_mut()
            .rev()
            .filter(|s| !s.kind.is_left() && s.is_shown())
        {
            right -= segment.width;
            segment.bounds = Rect::new(right, top, segment.width, height);
            right -= SEGMENT_GAP;
        }
        for segment in self.segments.iter().filter(|s| s.is_active()) {
            cx.for_widget(segment.id).add_tab_stop();
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        canvas.stroke_line(
            Point::new(self.bounds.x, self.bounds.y + 0.5),
            Point::new(self.bounds.right(), self.bounds.y + 0.5),
            tokens.hairline,
            1.0,
            Dash::Solid,
        );
        for segment in self.segments.iter().filter(|s| s.is_shown()) {
            let radii = CornerRadii::uniform(tokens.metrics.control_radius);
            let mut color = tokens.text_muted;
            if segment.kind.is_clickable() {
                let fill = segment.interaction.blend(
                    tokens.hover_bg.with_alpha(0.0),
                    tokens.hover_bg,
                    tokens.pressed_bg,
                );
                canvas.fill_rounded_rect(segment.bounds, radii, fill);
                if segment.interaction.hovered() {
                    color = tokens.text;
                }
            }
            let layout = layout_line(cx.text, cx.theme, &segment.text, FONT);
            draw_centered(canvas, layout.as_ref(), segment.bounds, color);
            if cx.for_widget(segment.id).is_focused() {
                focus_ring::paint(canvas, segment.bounds, radii.top_left, cx.theme);
            }
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let mut result = EventResult::ignored();
        for segment in self.segments.iter_mut().filter(|s| s.is_active()) {
            let mut child = cx.for_widget(segment.id);
            match e {
                Event::Pointer(event) => {
                    let outcome =
                        segment
                            .interaction
                            .pointer(&mut child, segment.bounds, event, true);
                    if outcome == PointerOutcome::Clicked {
                        segment.activated += 1;
                    }
                    result = result.or(outcome_result(outcome));
                }
                Event::Key(key)
                    if key.is_down
                        && child.is_focused()
                        && matches!(key.vk, vk::SPACE | vk::RETURN)
                        && key.mods.is_empty() =>
                {
                    if !key.repeat {
                        segment.activated += 1;
                    }
                    result = result.or(EventResult::handled());
                }
                Event::Focus(FocusEvent::Gained | FocusEvent::Lost) => {
                    child.invalidate(segment.bounds.outset(Insets::uniform(2.0)));
                }
                Event::AnimationFrame(now) if segment.interaction.animate(&mut child, *now) => {
                    child.invalidate(segment.bounds);
                }
                _ => {}
            }
        }
        self.collect();
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::new();
        for segment in self.segments.iter().filter(|s| s.is_shown()) {
            let role = if segment.kind.is_clickable() {
                Role::Button
            } else {
                Role::Text
            };
            let mut node = AccessNode::new(segment.id, role, segment.text.clone(), segment.bounds);
            if segment.kind.is_clickable() {
                let child = cx.for_widget(segment.id);
                node = node.with_state(control_state(&child, true));
                node.patterns.invoke = true;
                node.description = self.strings.hint(segment.kind).to_owned();
            }
            out.insert(node);
            children.push(segment.id);
        }
        out.insert(
            AccessNode::new(
                self.id,
                Role::StatusBar,
                self.strings.label.clone(),
                self.bounds,
            )
            .with_children(children),
        );
    }

    fn perform(&mut self, _cx: &mut EventCx, _a: &AccessAction) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::input::{KeyEvent, Modifiers};
    use crate::widgets::testkit::Harness;

    const SHELL_FTL: &str = include_str!("../../../../assets/i18n/en/shell.ftl");

    fn localizer() -> Localizer {
        let localizer = Localizer::new("en", &[("en", SHELL_FTL)]);
        let diagnostics = localizer.diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        localizer
    }

    fn info() -> StatusInfo {
        StatusInfo {
            line: 12,
            column: 5,
            words: 340,
            selected_words: None,
            zoom_percent: 100,
            encoding: "UTF-8".to_owned(),
            eol: Eol::CrLf,
            language: StatusLanguage::Hidden,
        }
    }

    fn setup(info: &StatusInfo) -> (Harness, StatusBar) {
        let localizer = localizer();
        let mut harness = Harness::new();
        let mut bar = StatusBar::new(StatusStrings::from_localizer(&localizer))
            .with_motion(Motion::reduced());
        bar.set_info(info, &localizer);
        harness.arrange(&mut bar, Rect::new(0.0, 576.0, 800.0, 24.0));
        (harness, bar)
    }

    #[test]
    fn info_is_formatted_with_the_localizer() {
        let (_, bar) = setup(&info());
        assert_eq!(bar.text(StatusSegment::Position), "Ln 12, Col 5");
        assert_eq!(bar.text(StatusSegment::Words), "340 words");
        assert_eq!(bar.text(StatusSegment::Zoom), "100%");
        assert_eq!(bar.text(StatusSegment::Encoding), "UTF-8");
        assert_eq!(bar.text(StatusSegment::LineEnding), "CRLF");
    }

    #[test]
    fn words_follow_plural_rules_and_the_selection() {
        let localizer = localizer();
        let mut bar = StatusBar::new(StatusStrings::from_localizer(&localizer));
        let one = StatusInfo {
            words: 1,
            eol: Eol::Lf,
            ..info()
        };
        bar.set_info(&one, &localizer);
        assert_eq!(bar.text(StatusSegment::Words), "1 word");
        assert_eq!(bar.text(StatusSegment::LineEnding), "LF");
        let selected = StatusInfo {
            selected_words: Some(12),
            eol: Eol::Cr,
            ..info()
        };
        bar.set_info(&selected, &localizer);
        assert_eq!(bar.text(StatusSegment::Words), "12 of 340 words");
        assert_eq!(bar.text(StatusSegment::LineEnding), "CR");
        let selected_one = StatusInfo {
            selected_words: Some(1),
            words: 1,
            ..info()
        };
        bar.set_info(&selected_one, &localizer);
        assert_eq!(bar.text(StatusSegment::Words), "1 of 1 word");
    }

    #[test]
    fn segments_sit_left_and_right_of_the_bar() {
        let (_, bar) = setup(&info());
        assert_eq!(bar.bounds(), Rect::new(0.0, 576.0, 800.0, 24.0));
        // 8 DIPs per glyph at any size, plus 16 padding; 20 DIPs tall inside the 24 DIP bar.
        assert_eq!(
            bar.segment_rect(StatusSegment::Position),
            Rect::new(8.0, 578.0, 112.0, 20.0)
        );
        assert_eq!(
            bar.segment_rect(StatusSegment::Words),
            Rect::new(122.0, 578.0, 88.0, 20.0)
        );
        assert_eq!(
            bar.segment_rect(StatusSegment::LineEnding),
            Rect::new(744.0, 578.0, 48.0, 20.0)
        );
        assert_eq!(
            bar.segment_rect(StatusSegment::Encoding),
            Rect::new(686.0, 578.0, 56.0, 20.0)
        );
        assert_eq!(
            bar.segment_rect(StatusSegment::Zoom),
            Rect::new(636.0, 578.0, 48.0, 20.0)
        );
    }

    #[test]
    fn clearing_hides_every_segment() {
        let (mut harness, mut bar) = setup(&info());
        bar.clear();
        harness.arrange(&mut bar, Rect::new(0.0, 576.0, 800.0, 24.0));
        for kind in StatusSegment::ALL {
            assert_eq!(bar.text(kind), "");
            assert_eq!(bar.segment_rect(kind), Rect::ZERO);
        }
        assert!(harness.focus.tab_order().is_empty());
        let tree = harness.tree(&bar);
        assert_eq!(tree.len(), 1);
        let canvas = harness.paint(&bar);
        assert!(canvas.texts().is_empty());
        // A click anywhere does nothing.
        harness.click(&mut bar, Point::new(770.0, 588.0));
        assert!(bar.take_events().is_empty());
    }

    #[test]
    fn clicking_encoding_and_line_ending_emits_events_with_their_rects() {
        let (mut harness, mut bar) = setup(&info());
        let encoding = bar.segment_rect(StatusSegment::Encoding);
        let eol = bar.segment_rect(StatusSegment::LineEnding);
        harness.click(&mut bar, encoding.center());
        harness.click(&mut bar, eol.center());
        assert_eq!(
            bar.take_events(),
            vec![
                StatusEvent::EncodingClicked { anchor: encoding },
                StatusEvent::LineEndingClicked { anchor: eol },
            ]
        );
        // The text segments are not clickable.
        for kind in [
            StatusSegment::Position,
            StatusSegment::Words,
            StatusSegment::Zoom,
        ] {
            let center = bar.segment_rect(kind).center();
            let result = harness.click(&mut bar, center);
            assert!(!result.handled);
        }
        assert!(bar.take_events().is_empty());
    }

    #[test]
    fn clickable_segments_ask_for_the_hand_and_text_segments_for_nothing() {
        use crate::input::{Cursor, PointerEventKind};
        let (mut harness, mut bar) = setup(&info());
        let mut cursor_at = |bar: &mut StatusBar, at: Point| {
            harness.requests.take_cursor();
            harness.pointer(bar, PointerEventKind::Move, None, at);
            harness.requests.take_cursor()
        };
        let encoding = bar.segment_rect(StatusSegment::Encoding).center();
        let position = bar.segment_rect(StatusSegment::Position).center();
        assert_eq!(cursor_at(&mut bar, encoding), Some(Cursor::Hand));
        assert_eq!(cursor_at(&mut bar, position), None);
    }

    #[test]
    fn only_the_clickable_segments_are_tab_stops_and_respond_to_the_keyboard() {
        let (mut harness, mut bar) = setup(&info());
        assert_eq!(harness.focus.tab_order().len(), 2);
        let encoding_id = bar.segments[3].id;
        harness.focus.focus(encoding_id);
        assert!(harness.key(&mut bar, vk::RETURN).handled);
        let repeat = KeyEvent {
            repeat: true,
            ..KeyEvent::press(vk::SPACE, Modifiers::NONE)
        };
        assert!(harness.send(&mut bar, Event::Key(repeat)).handled);
        assert!(!harness.key(&mut bar, vk::ESCAPE).handled);
        assert_eq!(
            bar.take_events(),
            vec![StatusEvent::EncodingClicked {
                anchor: bar.segment_rect(StatusSegment::Encoding)
            }]
        );
        harness.focus.focus_next();
        assert!(harness.key(&mut bar, vk::SPACE).handled);
        assert!(matches!(
            bar.take_events().as_slice(),
            [StatusEvent::LineEndingClicked { .. }]
        ));
    }

    #[test]
    fn access_tree_describes_text_and_buttons() {
        let (harness, bar) = setup(&info());
        let tree = harness.tree(&bar);
        let root = tree.root().expect("status bar node");
        assert_eq!(root.role, Role::StatusBar);
        assert_eq!(root.name, "Status bar");
        let nodes: Vec<_> = tree.children(bar.id()).collect();
        let summary: Vec<(Role, &str)> = nodes
            .iter()
            .map(|node| (node.role, node.name.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Role::Text, "Ln 12, Col 5"),
                (Role::Text, "340 words"),
                (Role::Text, "100%"),
                (Role::Button, "UTF-8"),
                (Role::Button, "CRLF"),
            ]
        );
        assert_eq!(nodes[3].description, "Change encoding");
        assert_eq!(nodes[4].description, "Change line endings");
        assert!(nodes[3].patterns.invoke && nodes[3].state.focusable);
        assert!(!nodes[0].patterns.invoke);
    }

    #[test]
    fn access_actions_invoke_and_focus_the_buttons() {
        let (mut harness, mut bar) = setup(&info());
        let encoding = bar.segments[3].id;
        let words = bar.segments[1].id;
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, encoding, &AccessAction::Invoke));
        let mut cx = harness.event_cx(bar.id());
        assert!(!bar.perform_item(&mut cx, words, &AccessAction::Invoke));
        let mut cx = harness.event_cx(bar.id());
        assert!(!bar.perform_item(&mut cx, encoding, &AccessAction::Expand));
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, encoding, &AccessAction::Focus));
        assert!(harness.focus.is_focused(encoding));
        assert!(matches!(
            bar.take_events().as_slice(),
            [StatusEvent::EncodingClicked { .. }]
        ));
        let mut cx = harness.event_cx(bar.id());
        assert!(!bar.perform(&mut cx, &AccessAction::Focus));
    }

    #[test]
    fn painting_draws_a_hairline_texts_and_the_hover_state() {
        let (mut harness, mut bar) = setup(&info());
        let encoding = bar.segment_rect(StatusSegment::Encoding).center();
        harness.hover(&mut bar, encoding);
        harness.frame(&mut bar, 16);
        harness.focus.focus(bar.segments[4].id);
        harness.send(&mut bar, Event::Focus(FocusEvent::Gained));
        harness.send(&mut bar, Event::Focus(FocusEvent::Lost));
        let canvas = harness.paint(&bar);
        assert_eq!(
            canvas.texts(),
            vec!["Ln 12, Col 5", "340 words", "100%", "UTF-8", "CRLF"]
        );
        let tokens = harness.theme.tokens;
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::StrokeLine { from, color, .. } if from.y == 576.5 && *color == tokens.hairline
        )));
        // The hovered encoding text is full strength, the others muted.
        let color_of = |wanted: &str| {
            canvas.ops().find_map(|op| match op {
                DrawOp::Text { text, color, .. } if text == wanted => Some(*color),
                _ => None,
            })
        };
        assert_eq!(color_of("UTF-8"), Some(tokens.text));
        assert_eq!(color_of("CRLF"), Some(tokens.text_muted));
        assert_eq!(color_of("100%"), Some(tokens.text_muted));
    }

    #[test]
    fn measuring_uses_the_status_bar_height() {
        let (mut harness, mut bar) = setup(&info());
        let bounds = harness.place(&mut bar, Point::ZERO);
        assert_eq!(bounds.size(), Size::new(0.0, STATUS_BAR_HEIGHT));
    }

    #[test]
    fn a_custom_id_names_the_bar() {
        let id = WidgetId::allocate();
        let bar = StatusBar::new(StatusStrings::from_localizer(&localizer())).with_id(id);
        assert_eq!(bar.id(), id);
        assert_eq!(Harness::new().node(&bar).id, id);
    }

    #[test]
    fn the_bar_measures_the_width_it_is_given_and_zero_without_one() {
        let (mut harness, mut bar) = setup(&info());
        let id = bar.id();
        let mut cx = LayoutCx::new(
            id,
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            bar.measure(&mut cx, Size::new(640.0, 100.0)),
            Size::new(640.0, STATUS_BAR_HEIGHT)
        );
        assert_eq!(
            bar.measure(&mut cx, Size::UNBOUNDED),
            Size::new(0.0, STATUS_BAR_HEIGHT)
        );
    }

    #[test]
    fn nothing_is_painted_outside_the_damaged_area() {
        let (mut harness, bar) = setup(&info());
        let away = Rect::new(0.0, 0.0, 100.0, 100.0);
        assert_eq!(harness.paint_damaged(&bar, Some(away)).ops().count(), 0);
        assert!(
            harness
                .paint_damaged(&bar, Some(bar.bounds()))
                .ops()
                .count()
                > 0
        );
    }

    #[test]
    fn clicking_a_read_only_segment_reports_nothing() {
        let (mut harness, mut bar) = setup(&info());
        for kind in [
            StatusSegment::Position,
            StatusSegment::Words,
            StatusSegment::Zoom,
        ] {
            let at = bar.segment_rect(kind).center();
            assert!(!harness.click(&mut bar, at).handled, "{kind:?}");
            assert!(bar.take_events().is_empty(), "{kind:?}");
        }
    }

    fn with_language(language: StatusLanguage) -> StatusInfo {
        StatusInfo { language, ..info() }
    }

    #[test]
    fn the_language_segment_is_shown_for_plain_documents_only() {
        assert_eq!(
            StatusLanguage::of(false, Some(Language::Json)),
            StatusLanguage::Hidden
        );
        assert_eq!(StatusLanguage::of(false, None), StatusLanguage::Hidden);
        assert_eq!(StatusLanguage::of(true, None), StatusLanguage::PlainText);
        assert_eq!(
            StatusLanguage::of(true, Some(Language::Json)),
            StatusLanguage::Language(Language::Json)
        );

        let localizer = localizer();
        let (mut harness, mut bar) = setup(&info());
        for (language, text) in [
            (StatusLanguage::Language(Language::Json), "JSON"),
            (StatusLanguage::PlainText, "Plain text"),
            (StatusLanguage::Language(Language::CSharp), "C#"),
            (StatusLanguage::Hidden, ""),
        ] {
            bar.set_info(&with_language(language), &localizer);
            harness.arrange(&mut bar, Rect::new(0.0, 576.0, 800.0, 24.0));
            assert_eq!(bar.text(StatusSegment::Language), text);
            assert_eq!(
                bar.segment_rect(StatusSegment::Language) == Rect::ZERO,
                text.is_empty(),
                "{text:?}"
            );
        }
        // No document: the segment goes with the others.
        bar.set_info(
            &with_language(StatusLanguage::Language(Language::Json)),
            &localizer,
        );
        bar.clear();
        assert_eq!(bar.text(StatusSegment::Language), "");
    }

    #[test]
    fn every_language_has_a_readable_label() {
        let localizer = localizer();
        assert_eq!(language_label(&localizer, None), "Plain text");
        let labels: Vec<String> = Language::ALL
            .iter()
            .map(|&language| language_label(&localizer, Some(language)))
            .collect();
        assert_eq!(
            labels,
            [
                "Rust",
                "C",
                "C++",
                "C#",
                "Java",
                "JavaScript",
                "TypeScript",
                "JSON",
                "Python",
                "Go",
                "Bash",
                "PowerShell",
                "HTML",
                "XML",
                "CSS",
                "SQL",
                "YAML",
                "TOML",
                "Markdown",
                "Diff",
            ]
        );
    }

    #[test]
    fn the_language_segment_is_the_rightmost_button() {
        let (mut harness, mut bar) =
            setup(&with_language(StatusLanguage::Language(Language::Json)));
        let language = bar.segment_rect(StatusSegment::Language);
        assert_eq!(language, Rect::new(744.0, 578.0, 48.0, 20.0));
        assert_eq!(
            bar.segment_rect(StatusSegment::LineEnding),
            Rect::new(694.0, 578.0, 48.0, 20.0)
        );
        assert_eq!(harness.focus.tab_order().len(), 3);

        harness.click(&mut bar, language.center());
        assert_eq!(
            bar.take_events(),
            vec![StatusEvent::LanguageClicked { anchor: language }]
        );
        let id = bar.segments[5].id;
        harness.focus.focus(id);
        assert!(harness.key(&mut bar, vk::RETURN).handled);
        let mut cx = harness.event_cx(bar.id());
        assert!(bar.perform_item(&mut cx, id, &AccessAction::Invoke));
        assert_eq!(
            bar.take_events(),
            vec![
                StatusEvent::LanguageClicked { anchor: language },
                StatusEvent::LanguageClicked { anchor: language },
            ]
        );
        assert_eq!(
            bar.tooltip_at(language.center()),
            Some((language, "Change language"))
        );

        let tree = harness.tree(&bar);
        let nodes: Vec<_> = tree.children(bar.id()).collect();
        let last = nodes.last().expect("the language node");
        assert_eq!(last.role, Role::Button);
        assert_eq!(last.name, "JSON");
        assert_eq!(last.description, "Change language");
        assert!(last.patterns.invoke && last.state.focusable);
    }

    #[test]
    fn hover_eases_in_frame_by_frame() {
        let localizer = localizer();
        let mut harness = Harness::new();
        let mut bar = StatusBar::new(StatusStrings::from_localizer(&localizer));
        bar.set_info(&info(), &localizer);
        harness.arrange(&mut bar, Rect::new(0.0, 576.0, 800.0, 24.0));
        let segment = bar.segment_rect(StatusSegment::Encoding);
        harness.hover(&mut bar, segment.center());
        assert!(harness.requests.animation_frame_requested());
        harness.requests.take_invalidation();
        // The first frame fixes the start of the easing; the next one moves it and repaints.
        harness.frame(&mut bar, 16);
        harness.requests.take_invalidation();
        harness.frame(&mut bar, 50);
        assert_eq!(
            harness.requests.take_invalidation(),
            crate::widget::Invalidation::Rect(segment)
        );
        harness.run_animations(&mut bar);
    }
}
