//! The inline objects of a preview: formulas and Mermaid diagrams typeset by `ssw-render`.
//! Images are never loaded: the preview has no document path, and Explorer previews must not
//! touch the network.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ssw_render::math::render_math;
use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, render_mermaid};
use ssw_render::scene::{Color as SceneColor, Scene};
use ssw_ui::canvas::SceneId;
use ssw_ui::editor::object::{
    InlineObjectProvider, ObjectInfo, ObjectKind, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::geometry::{Color, Size};

/// How long a preview may spend rendering diagrams in total. Mermaid renders synchronously on
/// the shell's UI thread; once the budget is spent the remaining diagrams stay source text.
pub(crate) const MERMAID_BUDGET: Duration = Duration::from_secs(3);

/// The last scene id issued. Ids are unique in the process, so a replaced provider can never
/// hand out an id that an older provider's scene is still registered under.
static LAST_SCENE_ID: AtomicU64 = AtomicU64::new(0);

/// A formula: its TeX source, display style, font size (bits) and RGBA color.
type MathKey = (String, bool, u32, [u8; 4]);

/// A typeset scene that was registered with the canvas, in the scene's own units.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Typeset {
    Ready {
        id: SceneId,
        size: Size,
        baseline: f32,
    },
    Failed,
}

impl Typeset {
    /// The editor's answer: the scene at its natural size, or (`fit`) shrunk in proportion to
    /// fit `max_width`.
    fn answer(self, fit: bool, max_width: f32) -> ObjectState {
        let Self::Ready { id, size, baseline } = self else {
            return ObjectState::Failed;
        };
        let scale = if fit && size.width > max_width && max_width > 0.0 {
            max_width / size.width
        } else {
            1.0
        };
        ObjectState::Ready(ObjectInfo {
            visual: ObjectVisual::Scene(id),
            size: Size::new(size.width * scale, size.height * scale),
            baseline: baseline * scale,
        })
    }
}

/// Answers the editor's questions about math, diagrams and images.
///
/// Every formula is typeset once per source, style, size and color, and every diagram once per
/// source; the scenes are handed out through [`PreviewObjects::take_fresh`] (or all of them
/// through [`PreviewObjects::all_scenes`]) so the canvas can register them before it draws.
/// Display math and diagrams wider than the text column are scaled down to it. Formulas and
/// diagrams that do not render answer [`ObjectState::Failed`] (the editor shows the source in
/// the error color); images answer [`ObjectState::Pending`] (the source stays visible, muted),
/// and so do diagrams once the render budget is spent.
#[derive(Debug)]
pub(crate) struct PreviewObjects {
    mermaid_theme: MermaidTheme,
    mermaid_budget: Duration,
    math: RefCell<HashMap<MathKey, Typeset>>,
    diagrams: RefCell<HashMap<String, Typeset>>,
    scenes: RefCell<Vec<(SceneId, Arc<Scene>)>>,
    handed_out: Cell<usize>,
    mermaid_spent: Cell<Duration>,
}

impl PreviewObjects {
    /// Objects for a preview whose theme is dark (`dark`) or light; diagrams are drawn with the
    /// matching palette.
    pub(crate) fn new(dark: bool) -> Self {
        Self::with_budget(dark, MERMAID_BUDGET)
    }

    /// Like [`PreviewObjects::new`] with an explicit diagram render budget.
    pub(crate) fn with_budget(dark: bool, mermaid_budget: Duration) -> Self {
        Self {
            mermaid_theme: if dark {
                MermaidTheme::Dark
            } else {
                MermaidTheme::Light
            },
            mermaid_budget,
            math: RefCell::default(),
            diagrams: RefCell::default(),
            scenes: RefCell::default(),
            handed_out: Cell::new(0),
            mermaid_spent: Cell::new(Duration::ZERO),
        }
    }

    /// True when diagrams are drawn with the dark palette.
    pub(crate) fn is_dark(&self) -> bool {
        self.mermaid_theme == MermaidTheme::Dark
    }

    /// The scenes typeset since the last call, with the ids the answers refer to.
    pub(crate) fn take_fresh(&self) -> Vec<(SceneId, Arc<Scene>)> {
        let scenes = self.scenes.borrow();
        let fresh = scenes[self.handed_out.get()..].to_vec();
        self.handed_out.set(scenes.len());
        fresh
    }

    /// True when scenes were typeset that [`PreviewObjects::take_fresh`] has not handed out.
    pub(crate) fn has_fresh(&self) -> bool {
        self.handed_out.get() < self.scenes.borrow().len()
    }

    /// Every scene typeset so far (for a canvas that starts empty).
    pub(crate) fn all_scenes(&self) -> Vec<(SceneId, Arc<Scene>)> {
        self.scenes.borrow().clone()
    }

    fn math(&self, request: &ObjectRequest<'_>, display: bool) -> ObjectState {
        let color = scene_color(request.text_color);
        let key = (
            request.source.to_owned(),
            display,
            request.font_size.to_bits(),
            [color.r, color.g, color.b, color.a],
        );
        let cached = self.math.borrow().get(&key).copied();
        let typeset = cached.unwrap_or_else(|| {
            let typeset = match render_math(request.source, display, request.font_size, color) {
                Ok(scene) => self.publish(scene),
                Err(_) => Typeset::Failed,
            };
            self.math.borrow_mut().insert(key, typeset);
            typeset
        });
        typeset.answer(display, request.max_width)
    }

    fn mermaid(&self, request: &ObjectRequest<'_>) -> ObjectState {
        let cached = self.diagrams.borrow().get(request.source).copied();
        let typeset = match cached {
            Some(typeset) => typeset,
            None => {
                if self.mermaid_spent.get() >= self.mermaid_budget {
                    return ObjectState::Pending;
                }
                let started = Instant::now();
                let outcome = render_mermaid(request.source, self.mermaid_theme);
                self.mermaid_spent
                    .set(self.mermaid_spent.get() + started.elapsed());
                let typeset = match outcome {
                    MermaidOutcome::Diagram(scene, _svg) => self.publish(scene),
                    MermaidOutcome::Unsupported { .. } => Typeset::Failed,
                };
                self.diagrams
                    .borrow_mut()
                    .insert(request.source.to_owned(), typeset);
                typeset
            }
        };
        typeset.answer(true, request.max_width)
    }

    /// Queues `scene` for registration and describes it.
    fn publish(&self, scene: Scene) -> Typeset {
        let id = SceneId(LAST_SCENE_ID.fetch_add(1, Ordering::Relaxed) + 1);
        let size = Size::new(scene.size.width, scene.size.height);
        let baseline = scene.baseline.unwrap_or(size.height);
        self.scenes.borrow_mut().push((id, Arc::new(scene)));
        Typeset::Ready { id, size, baseline }
    }
}

impl InlineObjectProvider for PreviewObjects {
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState {
        match request.kind {
            ObjectKind::Math => self.math(request, false),
            ObjectKind::DisplayMath => self.math(request, true),
            ObjectKind::Mermaid => self.mermaid(request),
            ObjectKind::Image => ObjectState::Pending,
        }
    }
}

/// `color` as the 8-bit straight RGBA color scenes use.
pub(crate) fn scene_color(color: Color) -> SceneColor {
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    SceneColor::rgba(byte(color.r), byte(color.g), byte(color.b), byte(color.a))
}

#[cfg(test)]
mod tests {
    use ssw_core::text::TextRange;

    use super::*;

    const FLOWCHART: &str = "graph TD\n  A[Start] --> B[End]\n";

    fn request(kind: ObjectKind, source: &str, max_width: f32) -> ObjectRequest<'_> {
        ObjectRequest {
            kind,
            source,
            alt: "",
            width: None,
            height: None,
            font_size: 16.0,
            max_width,
            text_color: Color::BLACK,
            range: TextRange::new(0, source.len()),
        }
    }

    fn ready(state: ObjectState) -> ObjectInfo {
        match state {
            ObjectState::Ready(info) => info,
            other => panic!("expected a ready object, got {other:?}"),
        }
    }

    #[test]
    fn scene_colors_are_rounded_and_clamped_to_bytes() {
        assert_eq!(
            scene_color(Color::new(1.0, 0.5, 0.0, 1.0)),
            SceneColor::rgba(255, 128, 0, 255)
        );
        assert_eq!(
            scene_color(Color::new(2.0, -1.0, 0.2, 0.0)),
            SceneColor::rgba(255, 0, 51, 0)
        );
    }

    #[test]
    fn a_formula_is_typeset_once_and_aligned_to_the_text() {
        let objects = PreviewObjects::new(false);
        let math = request(ObjectKind::Math, "x^2 + y^2", 400.0);
        let first = ready(objects.object(&math));
        assert!(
            first.size.width > 10.0 && first.size.height > 5.0,
            "{first:?}"
        );
        assert!(first.baseline > 0.0 && first.baseline <= first.size.height);
        let ObjectVisual::Scene(id) = first.visual else {
            panic!("a formula is a scene");
        };
        let fresh = objects.take_fresh();
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].0, id);
        assert_eq!(fresh[0].1.size.width, first.size.width);
        assert_eq!(
            ready(objects.object(&math)),
            first,
            "the second answer is the cached one"
        );
        assert!(objects.take_fresh().is_empty(), "nothing is typeset twice");
    }

    #[test]
    fn size_and_color_make_different_formulas() {
        let objects = PreviewObjects::new(false);
        let small = ready(objects.object(&request(ObjectKind::Math, "a+b", 400.0)));
        let mut bigger = request(ObjectKind::Math, "a+b", 400.0);
        bigger.font_size = 32.0;
        let big = ready(objects.object(&bigger));
        assert!(big.size.width > small.size.width * 1.5);
        let mut red = request(ObjectKind::Math, "a+b", 400.0);
        red.text_color = Color::new(1.0, 0.0, 0.0, 1.0);
        let recolored = ready(objects.object(&red));
        assert_ne!(recolored.visual, small.visual);
        assert_eq!(objects.take_fresh().len(), 3);
    }

    #[test]
    fn a_broken_formula_fails_and_stays_failed() {
        let objects = PreviewObjects::new(false);
        let broken = request(ObjectKind::Math, "\\frac{1", 400.0);
        assert_eq!(objects.object(&broken), ObjectState::Failed);
        assert_eq!(objects.object(&broken), ObjectState::Failed);
        assert!(objects.take_fresh().is_empty());
    }

    #[test]
    fn display_math_shrinks_to_the_column_but_inline_math_does_not() {
        let objects = PreviewObjects::new(false);
        let long = "a_1 + a_2 + a_3 + a_4 + a_5 + a_6 + a_7 + a_8 + a_9 + a_{10} + a_{11} + a_{12}";
        let natural = ready(objects.object(&request(ObjectKind::DisplayMath, long, 10_000.0)));
        assert!(natural.size.width > 100.0);
        let narrow = ready(objects.object(&request(ObjectKind::DisplayMath, long, 100.0)));
        assert!((narrow.size.width - 100.0).abs() < 0.01, "{narrow:?}");
        let ratio = narrow.size.width / natural.size.width;
        assert!((narrow.size.height / natural.size.height - ratio).abs() < 1e-4);
        assert_eq!(
            narrow.visual, natural.visual,
            "one scene, drawn at two sizes"
        );
        let inline = ready(objects.object(&request(ObjectKind::Math, long, 100.0)));
        assert!(inline.size.width > 100.0, "inline math keeps its size");
    }

    #[test]
    fn a_diagram_is_rendered_once_and_scaled_to_the_column() {
        let objects = PreviewObjects::new(true);
        let wide = ready(objects.object(&request(ObjectKind::Mermaid, FLOWCHART, 10_000.0)));
        assert!(
            wide.size.width > 20.0 && wide.size.height > 20.0,
            "{wide:?}"
        );
        let narrow = ready(objects.object(&request(ObjectKind::Mermaid, FLOWCHART, 40.0)));
        assert!((narrow.size.width - 40.0).abs() < 0.01, "{narrow:?}");
        assert_eq!(narrow.visual, wide.visual);
        assert_eq!(objects.take_fresh().len(), 1);
    }

    #[test]
    fn diagrams_that_do_not_render_fail() {
        let objects = PreviewObjects::new(false);
        let unsupported = request(ObjectKind::Mermaid, "sankey-beta\nA,B,1\n", 400.0);
        assert_eq!(objects.object(&unsupported), ObjectState::Failed);
        let nonsense = request(ObjectKind::Mermaid, "this is not mermaid", 400.0);
        assert_eq!(objects.object(&nonsense), ObjectState::Failed);
        assert!(objects.take_fresh().is_empty());
    }

    #[test]
    fn a_spent_budget_leaves_new_diagrams_as_source_text() {
        let objects = PreviewObjects::with_budget(false, Duration::ZERO);
        let diagram = request(ObjectKind::Mermaid, FLOWCHART, 400.0);
        assert_eq!(objects.object(&diagram), ObjectState::Pending);
        assert_eq!(objects.object(&diagram), ObjectState::Pending);
        assert!(objects.take_fresh().is_empty());
    }

    #[test]
    fn diagrams_already_rendered_survive_a_spent_budget() {
        let objects = PreviewObjects::with_budget(false, Duration::from_nanos(1));
        let diagram = request(ObjectKind::Mermaid, FLOWCHART, 400.0);
        let first = ready(objects.object(&diagram));
        assert_eq!(ready(objects.object(&diagram)), first);
        let other = request(ObjectKind::Mermaid, "graph LR\n  X --> Y\n", 400.0);
        assert_eq!(
            objects.object(&other),
            ObjectState::Pending,
            "the budget is spent"
        );
    }

    #[test]
    fn images_are_never_loaded() {
        let objects = PreviewObjects::new(false);
        assert_eq!(
            objects.object(&request(
                ObjectKind::Image,
                "https://example.com/a.png",
                400.0
            )),
            ObjectState::Pending
        );
    }
}
