//! Offscreen rendering of the real window content into PNG bytes (`screenshot` steps of the
//! scripted harness, see [`super::test_script`]).
//!
//! [`Visuals`] draws what the window would draw (the same layout and paint passes, the same
//! Direct2D canvas, DirectWrite text and scene renderer) into a WIC bitmap, so nothing is ever
//! shown. Mica does not exist offscreen: callers paint the solid fallback.
//!
//! The editor asks for math, Mermaid diagrams and images while it lays out. The window answers
//! from worker threads; here [`OffscreenObjects`] typesets and decodes on the spot, and
//! [`Visuals::render`] lays the view out until no new object appears before it draws, so every
//! scene and image is registered with the canvas by then.
//!
//! COM must be initialized on the calling thread and outlive the [`Visuals`].

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::i18n::Localizer;
use ssw_render::math::render_math;
use ssw_render::mermaid::{MermaidOutcome, MermaidTheme, render_mermaid};
use ssw_render::scene::{Color as SceneColor, Scene};
use ssw_ui::canvas::{Canvas, ImageId, RecordingCanvas, SceneId};
use ssw_ui::editor::object::{
    InlineObjectProvider, ObjectInfo, ObjectKind, ObjectRequest, ObjectState, ObjectVisual,
};
use ssw_ui::editor::{EditorView, blocks_media, blocks_table, blocks_text};
use ssw_ui::geometry::{Color, Size};
use ssw_win::canvas::{CanvasAssets, D2dCanvas, ImageStore, TargetResources};
use ssw_win::com::WinError;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{WicTarget, d2d_factory};
use ssw_win::wic::{DecodedImage, Wic};

use super::platform::fit;
use super::shell_view::EditorSetup;

/// How often the view is laid out while it keeps asking for new objects.
const MAX_LAYOUT_PASSES: usize = 6;
/// The largest bitmap a screenshot may need (width x height in pixels).
const MAX_PIXELS: u64 = 64_000_000;

/// The size of a screenshot: DIPs and the DPI scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    width: u32,
    height: u32,
    scale: f32,
}

impl Geometry {
    /// A `width` x `height` DIP image at `scale` (2.0 is 192 DPI).
    ///
    /// # Errors
    /// The bitmap would exceed 64 million pixels.
    pub fn new(width: u32, height: u32, scale: f32) -> Result<Self, String> {
        let geometry = Self {
            width,
            height,
            scale,
        };
        let (w, h) = geometry.pixels();
        if u64::from(w) * u64::from(h) > MAX_PIXELS {
            Err(format!(
                "a {w}x{h} pixel screenshot is larger than {MAX_PIXELS} pixels"
            ))
        } else {
            Ok(geometry)
        }
    }

    /// The size in DIPs.
    #[must_use]
    pub fn size(self) -> Size {
        Size::new(self.width as f32, self.height as f32)
    }

    /// The DPI scale.
    #[must_use]
    pub fn scale(self) -> f32 {
        self.scale
    }

    /// The size of the bitmap in pixels.
    #[must_use]
    pub fn pixels(self) -> (u32, u32) {
        let pixels = |dips: u32| (dips as f32 * self.scale).round().max(1.0) as u32;
        (pixels(self.width), pixels(self.height))
    }
}

/// Everything the objects of one question share: what was asked and how it was answered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ObjectKey {
    kind: u8,
    source: String,
    font_bits: u32,
    color: [u8; 4],
    max_width_bits: u32,
    width: Option<u32>,
    height: Option<u32>,
    base: Option<PathBuf>,
}

/// Math, Mermaid and image objects, produced synchronously.
struct OffscreenObjects {
    wic: Wic,
    base: RefCell<Option<PathBuf>>,
    answers: RefCell<HashMap<ObjectKey, ObjectState>>,
    scenes: RefCell<Vec<(SceneId, Arc<Scene>)>>,
    images: RefCell<Vec<(ImageId, DecodedImage)>>,
    next_id: Cell<u64>,
}

impl fmt::Debug for OffscreenObjects {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OffscreenObjects")
            .field("scenes", &self.scenes.borrow().len())
            .field("images", &self.images.borrow().len())
            .finish_non_exhaustive()
    }
}

impl OffscreenObjects {
    fn new(wic: Wic) -> Self {
        Self {
            wic,
            base: RefCell::new(None),
            answers: RefCell::new(HashMap::new()),
            scenes: RefCell::new(Vec::new()),
            images: RefCell::new(Vec::new()),
            next_id: Cell::new(1),
        }
    }

    /// How many scenes and images were produced so far.
    fn count(&self) -> usize {
        self.scenes.borrow().len() + self.images.borrow().len()
    }

    /// Registers every scene and image with `assets`.
    fn register_all(&self, assets: &mut CanvasAssets) -> Result<(), WinError> {
        for (id, scene) in self.scenes.borrow().iter() {
            assets.register_scene(*id, Arc::clone(scene));
        }
        for (id, image) in self.images.borrow().iter() {
            assets
                .images
                .register_bgra(*id, image.width, image.height, image.bgra.clone())?;
        }
        Ok(())
    }

    fn take_id(&self) -> u64 {
        self.next_id.replace(self.next_id.get() + 1)
    }

    /// Stores `scene` and describes it, scaled down to `max_width` when wider.
    fn publish(&self, scene: Scene, max_width: f32) -> ObjectState {
        let (width, height) = fit(scene.size.width, scene.size.height, max_width);
        let baseline = scene.baseline.map_or(height, |baseline| {
            baseline * height / scene.size.height.max(f32::EPSILON)
        });
        let id = SceneId(self.take_id());
        self.scenes.borrow_mut().push((id, Arc::new(scene)));
        ObjectState::Ready(ObjectInfo {
            visual: ObjectVisual::Scene(id),
            size: Size::new(width, height),
            baseline,
        })
    }

    fn image(&self, request: &ObjectRequest<'_>) -> ObjectState {
        if super::remote_images::is_remote(request.source) {
            // The screenshots never touch the network.
            return ObjectState::Blocked;
        }
        let path = Path::new(request.source);
        let full = if path.is_absolute() {
            Some(path.to_path_buf())
        } else {
            self.base.borrow().as_ref().map(|base| base.join(path))
        };
        let decoded = full
            .and_then(|full| std::fs::read(full).ok())
            .and_then(|bytes| self.wic.decode(&bytes).ok());
        let Some(decoded) = decoded else {
            return ObjectState::Failed;
        };
        let (mut w, mut h) = (decoded.width as f32, decoded.height as f32);
        match (request.width, request.height) {
            (Some(rw), Some(rh)) => (w, h) = (rw as f32, rh as f32),
            (Some(rw), None) => (w, h) = (rw as f32, h * rw as f32 / w.max(1.0)),
            (None, Some(rh)) => (w, h) = (w * rh as f32 / h.max(1.0), rh as f32),
            (None, None) => {}
        }
        let (w, h) = fit(w, h, request.max_width);
        let id = ImageId(self.take_id());
        self.images.borrow_mut().push((id, decoded));
        ObjectState::Ready(ObjectInfo {
            visual: ObjectVisual::Image(id),
            size: Size::new(w, h),
            baseline: h,
        })
    }

    fn produce(&self, request: &ObjectRequest<'_>) -> ObjectState {
        match request.kind {
            ObjectKind::Math | ObjectKind::DisplayMath => {
                let display = request.kind == ObjectKind::DisplayMath;
                let [r, g, b, a] = request.text_color.to_rgba8();
                let color = SceneColor { r, g, b, a };
                match render_math(request.source, display, request.font_size, color) {
                    Ok(scene) => self.publish(scene, request.max_width),
                    Err(_) => ObjectState::Failed,
                }
            }
            ObjectKind::Mermaid => {
                let c = request.text_color;
                let theme = if (c.r + c.g + c.b) / 3.0 > 0.5 {
                    MermaidTheme::Dark
                } else {
                    MermaidTheme::Light
                };
                match render_mermaid(request.source, theme) {
                    MermaidOutcome::Diagram(scene, _) => self.publish(scene, request.max_width),
                    MermaidOutcome::Unsupported { .. } => ObjectState::Failed,
                }
            }
            ObjectKind::Image => self.image(request),
        }
    }
}

impl InlineObjectProvider for OffscreenObjects {
    fn object(&self, request: &ObjectRequest<'_>) -> ObjectState {
        let key = ObjectKey {
            kind: request.kind as u8,
            source: request.source.to_owned(),
            font_bits: request.font_size.to_bits(),
            color: request.text_color.to_rgba8(),
            max_width_bits: request.max_width.to_bits(),
            width: request.width,
            height: request.height,
            // A relative path depends on the document's folder.
            base: (request.kind == ObjectKind::Image)
                .then(|| self.base.borrow().clone())
                .flatten(),
        };
        let known = self.answers.borrow().get(&key).copied();
        known.unwrap_or_else(|| {
            let state = self.produce(request);
            self.answers.borrow_mut().insert(key, state);
            state
        })
    }
}

/// The offscreen renderer: WIC, DirectWrite and the objects of the editors.
#[derive(Debug)]
pub struct Visuals {
    wic: Wic,
    text: DWriteEngine,
    objects: Rc<OffscreenObjects>,
}

impl Visuals {
    /// The renderer; COM must be initialized on this thread.
    ///
    /// # Errors
    /// WIC or DirectWrite are unavailable.
    pub fn new() -> Result<Self, String> {
        Self::create().map_err(|error| format!("cannot start the screenshot renderer: {error}"))
    }

    fn create() -> Result<Self, WinError> {
        let wic = Wic::new()?;
        let text = DWriteEngine::new()?;
        let objects = Rc::new(OffscreenObjects::new(wic.clone()));
        Ok(Self { wic, text, objects })
    }

    /// The text engine the views are laid out with.
    #[must_use]
    pub fn text(&self) -> &DWriteEngine {
        &self.text
    }

    /// Relative image paths are resolved against `base` (the folder of the document on show).
    pub fn set_base(&self, base: Option<PathBuf>) {
        *self.objects.base.borrow_mut() = base;
    }

    /// The editor configuration of a window: the block renderers (text, media, tables) and the
    /// objects of this renderer.
    #[must_use]
    pub fn editor_setup(&self, i18n: &Rc<Localizer>) -> EditorSetup {
        let i18n = Rc::clone(i18n);
        let objects = Rc::clone(&self.objects);
        Rc::new(move |editor: &mut EditorView| {
            let registry = editor.registry_mut();
            blocks_text::register_with(registry, blocks_text::BlockStrings::new(&i18n));
            let media = blocks_media::MediaServices {
                strings: blocks_media::MediaStrings::new(Rc::clone(&i18n)),
                ..Default::default()
            };
            blocks_media::register_with(registry, media);
            let table = blocks_table::register_with(
                registry,
                blocks_table::TableStrings::new(Rc::clone(&i18n)),
            );
            editor.set_object_provider(Rc::clone(&objects) as Rc<dyn InlineObjectProvider>);
            Some(table)
        })
    }

    /// Draws what `paint` paints into a bitmap of `geometry` cleared to `background` and
    /// returns it as PNG. `paint` runs several times: once per layout pass (onto a recording
    /// canvas, until no new object appears) and once on the real canvas.
    ///
    /// # Errors
    /// Direct2D, WIC or the scene renderer fail, or the objects never settle.
    pub fn render(
        &self,
        geometry: Geometry,
        background: Color,
        paint: &mut dyn FnMut(&mut dyn Canvas),
    ) -> Result<Vec<u8>, String> {
        self.render_png(geometry, background, paint)
            .map_err(|error| format!("cannot render the screenshot: {error}"))
    }

    fn render_png(
        &self,
        geometry: Geometry,
        background: Color,
        paint: &mut dyn FnMut(&mut dyn Canvas),
    ) -> Result<Vec<u8>, WinError> {
        let (width_px, height_px) = geometry.pixels();
        let factory = d2d_factory(false)?;
        let target = WicTarget::new(
            &factory,
            &self.wic,
            width_px,
            height_px,
            96.0 * geometry.scale(),
        )?;
        let mut assets = CanvasAssets::with_scene_renderer(
            ImageStore::new(self.wic.clone()),
            self.text.clone(),
            self.wic.clone(),
        )?;
        let mut resources = TargetResources::new(factory, 1);
        let mut settled = false;
        for _ in 0..MAX_LAYOUT_PASSES {
            let before = self.objects.count();
            paint(&mut RecordingCanvas::default());
            if self.objects.count() == before {
                settled = true;
                break;
            }
        }
        if !settled {
            return Err(WinError::Invalid("the objects of the view keep changing"));
        }
        self.objects.register_all(&mut assets)?;
        target.paint(Some(background), |render_target| {
            let mut canvas = D2dCanvas::new(render_target, &assets, &mut resources);
            paint(&mut canvas);
            canvas.finish()
        })?;
        self.wic.encode_png(&target.pixels()?)
    }
}

#[cfg(test)]
mod tests {
    use ssw_core::text::TextRange;
    use ssw_ui::geometry::Rect;
    use ssw_win::com::ComGuard;

    use super::*;

    fn request(kind: ObjectKind, source: &str) -> ObjectRequest<'_> {
        ObjectRequest {
            kind,
            source,
            alt: "",
            width: None,
            height: None,
            font_size: 16.0,
            max_width: 400.0,
            text_color: Color::BLACK,
            range: TextRange::new(0, source.len()),
        }
    }

    fn ready_info(state: ObjectState) -> Option<ObjectInfo> {
        match state {
            ObjectState::Ready(info) => Some(info),
            _ => None,
        }
    }

    fn ready(state: ObjectState) -> ObjectInfo {
        ready_info(state).expect("a ready object")
    }

    #[test]
    fn geometry_rounds_to_pixels_and_refuses_huge_bitmaps() {
        let geometry = Geometry::new(1440, 900, 1.5).unwrap();
        assert_eq!(geometry.pixels(), (2160, 1350));
        assert_eq!(geometry.size(), Size::new(1440.0, 900.0));
        assert_eq!(geometry.scale(), 1.5);
        assert_eq!(Geometry::new(1, 1, 0.5).unwrap().pixels(), (1, 1));
        let error = Geometry::new(8192, 8192, 2.0).unwrap_err();
        assert!(error.contains("16384x16384"), "{error}");
    }

    #[test]
    fn math_and_diagrams_are_typeset_once_and_failures_are_reported() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let math = request(ObjectKind::DisplayMath, "x^2 + y^2");
        let first = ready(visuals.objects.object(&math));
        assert!(
            first.size.width > 10.0 && first.size.height > 5.0,
            "{first:?}"
        );
        assert_eq!(
            ready(visuals.objects.object(&math)),
            first,
            "answered from the cache"
        );
        assert_eq!(visuals.objects.count(), 1);
        let diagram = request(ObjectKind::Mermaid, "graph TD\n  A[Start] --> B[End]\n");
        assert_eq!(
            ready(visuals.objects.object(&diagram)).visual,
            ObjectVisual::Scene(SceneId(2))
        );
        assert_eq!(visuals.objects.count(), 2);
        let broken = request(ObjectKind::Math, "\\frac{1");
        assert_eq!(visuals.objects.object(&broken), ObjectState::Failed);
        assert_eq!(ready_info(visuals.objects.object(&broken)), None);
        let nonsense = request(ObjectKind::Mermaid, "this is not mermaid");
        assert_eq!(visuals.objects.object(&nonsense), ObjectState::Failed);
        assert_eq!(visuals.objects.count(), 2);
    }

    #[test]
    fn images_come_from_the_folder_of_the_document_and_never_from_the_network() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let png = visuals
            .wic
            .encode_png(&DecodedImage {
                width: 4,
                height: 2,
                bgra: vec![255; 4 * 2 * 4],
            })
            .unwrap();
        std::fs::write(tmp.path().join("pic.png"), png).unwrap();
        let pic = request(ObjectKind::Image, "pic.png");
        assert_eq!(
            visuals.objects.object(&pic),
            ObjectState::Failed,
            "no base yet"
        );
        visuals.set_base(Some(tmp.path().to_path_buf()));
        let natural = ready(visuals.objects.object(&pic));
        assert_eq!(natural.size, Size::new(4.0, 2.0));
        assert_eq!(natural.visual, ObjectVisual::Image(ImageId(1)));
        for (width, height, expected) in [
            (Some(8), None, Size::new(8.0, 4.0)),
            (None, Some(6), Size::new(12.0, 6.0)),
            (Some(10), Some(10), Size::new(10.0, 10.0)),
        ] {
            let sized = ObjectRequest {
                width,
                height,
                ..request(ObjectKind::Image, "pic.png")
            };
            assert_eq!(ready(visuals.objects.object(&sized)).size, expected);
        }
        let absolute = tmp.path().join("pic.png");
        let absolute = absolute.to_string_lossy();
        assert!(
            ready_info(
                visuals
                    .objects
                    .object(&request(ObjectKind::Image, &absolute))
            )
            .is_some()
        );
        let remote = request(ObjectKind::Image, "https://example.com/a.png");
        assert_eq!(visuals.objects.object(&remote), ObjectState::Blocked);
        let missing = request(ObjectKind::Image, "missing.png");
        assert_eq!(visuals.objects.object(&missing), ObjectState::Failed);
    }

    #[test]
    fn rendering_draws_the_registered_scene_and_the_background_into_a_png() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let objects = Rc::clone(&visuals.objects);
        let mut draws = 0;
        let png = visuals
            .render(
                Geometry::new(60, 40, 2.0).unwrap(),
                Color::WHITE,
                &mut |canvas: &mut dyn Canvas| {
                    draws += 1;
                    let state = objects.object(&request(ObjectKind::DisplayMath, "x^2"));
                    let info = ready(state);
                    // The first (and only) object is the scene number one.
                    assert_eq!(info.visual, ObjectVisual::Scene(SceneId(1)));
                    canvas.draw_scene(SceneId(1), Rect::new(2.0, 2.0, 50.0, 30.0));
                    canvas.fill_rect(
                        Rect::new(0.0, 36.0, 60.0, 4.0),
                        Color::from_rgb_u32(0xFF0000),
                    );
                },
            )
            .unwrap();
        assert_eq!(
            draws, 3,
            "two layout passes (the second one settles) and the real draw"
        );
        let image = visuals.wic.decode(&png).unwrap();
        assert_eq!((image.width, image.height), (120, 80));
        assert_eq!(image.pixel(1, 1), Some([255, 255, 255, 255]), "background");
        assert_eq!(
            image.pixel(60, 78),
            Some([0, 0, 255, 255]),
            "the red bar (BGRA)"
        );
        let dark_pixels = (0..image.height)
            .flat_map(|y| (0..image.width).map(move |x| (x, y)))
            .filter(|(x, y)| {
                image
                    .pixel(*x, *y)
                    .is_some_and(|p| p[0] < 128 && p[1] < 128 && p[2] < 128)
            })
            .count();
        assert!(dark_pixels > 20, "the formula was drawn: {dark_pixels}");
    }

    #[test]
    fn objects_that_never_settle_fail_the_render() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let objects = Rc::clone(&visuals.objects);
        let mut counter = 0;
        let error = visuals
            .render(
                Geometry::new(20, 20, 1.0).unwrap(),
                Color::WHITE,
                &mut |_: &mut dyn Canvas| {
                    counter += 1;
                    let tex = format!("x_{counter}");
                    objects.object(&request(ObjectKind::Math, &tex));
                },
            )
            .unwrap_err();
        assert!(error.contains("keep changing"), "{error}");
    }

    #[test]
    fn the_objects_describe_how_many_scenes_and_images_they_hold() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        assert_eq!(
            format!("{:?}", visuals.objects),
            "OffscreenObjects { scenes: 0, images: 0, .. }"
        );
        ready(visuals.objects.object(&request(ObjectKind::Math, "x")));
        assert_eq!(
            format!("{:?}", visuals.objects),
            "OffscreenObjects { scenes: 1, images: 0, .. }"
        );
    }

    #[test]
    fn diagrams_are_typeset_for_the_text_color_of_the_theme() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let source = "graph TD\n  A[Start] --> B[End]\n";
        let dark_text = ObjectRequest {
            text_color: Color::WHITE,
            ..request(ObjectKind::Mermaid, source)
        };
        let dark = ready(visuals.objects.object(&dark_text));
        let light = ready(
            visuals
                .objects
                .object(&request(ObjectKind::Mermaid, source)),
        );
        assert_eq!(dark.visual, ObjectVisual::Scene(SceneId(1)));
        assert_eq!(light.visual, ObjectVisual::Scene(SceneId(2)));
        assert_eq!(visuals.objects.count(), 2, "one scene per theme");
    }

    #[test]
    fn registered_images_are_drawn_and_a_damaged_one_fails_the_render() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let geometry = Geometry::new(20, 20, 1.0).unwrap();
        visuals.objects.images.borrow_mut().push((
            ImageId(7),
            DecodedImage {
                width: 2,
                height: 2,
                bgra: vec![255; 16],
            },
        ));
        let png = visuals
            .render(geometry, Color::WHITE, &mut |canvas: &mut dyn Canvas| {
                canvas.draw_image(ImageId(7), Rect::new(0.0, 0.0, 10.0, 10.0), 1.0);
            })
            .unwrap();
        assert_eq!(visuals.wic.decode(&png).unwrap().width, 20);
        // The buffer of the image does not fit its size: the bitmap cannot be registered.
        visuals.objects.images.borrow_mut().push((
            ImageId(8),
            DecodedImage {
                width: 4,
                height: 4,
                bgra: vec![0; 3],
            },
        ));
        let error = visuals
            .render(geometry, Color::WHITE, &mut |_: &mut dyn Canvas| {})
            .unwrap_err();
        assert!(
            error.starts_with("cannot render the screenshot:"),
            "{error}"
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_renderer_that_cannot_start_reports_why() {
        let _com = ComGuard::apartment_threaded().unwrap();
        // Every system call of the start fails in turn: the WIC factory first, then the steps
        // of the DirectWrite engine.
        let outcomes = ssw_win::fault::sweep(Visuals::new);
        let calls = outcomes.len();
        assert!(calls > 2, "the start makes several system calls: {calls}");
        for (call, outcome) in outcomes.iter().enumerate() {
            let error = outcome.as_ref().unwrap_err();
            assert!(
                error.starts_with("cannot start the screenshot renderer: "),
                "call {call}: {error}"
            );
        }
        let first = outcomes[0].as_ref().unwrap_err();
        assert!(first.contains("create WIC factory"), "{first}");
        let last = outcomes.last().unwrap().as_ref().unwrap_err();
        assert!(
            !last.contains("WIC"),
            "the last call belongs to DirectWrite: {last}"
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_system_call_of_a_render_can_fail_and_the_renderer_stays_usable() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let visuals = Visuals::new().unwrap();
        let geometry = Geometry::new(24, 16, 1.0).unwrap();
        let mut draw = |canvas: &mut dyn Canvas| {
            canvas.fill_rect(
                Rect::new(0.0, 0.0, 12.0, 8.0),
                Color::from_rgb_u32(0xFF0000),
            );
        };
        // The Direct2D factory, the bitmap and its render target, the scene renderer, the
        // brush of the drawing, the end of the frame, the copy of the pixels and the PNG
        // encoder each fail in turn.
        let outcomes = ssw_win::fault::sweep(|| visuals.render(geometry, Color::WHITE, &mut draw));
        let calls = outcomes.len();
        assert!(calls >= 8, "the render makes many system calls: {calls}");
        for (call, outcome) in outcomes.iter().enumerate() {
            let error = outcome.as_ref().unwrap_err();
            assert!(
                error.starts_with("cannot render the screenshot: "),
                "call {call}: {error}"
            );
        }
        let first = outcomes[0].as_ref().unwrap_err();
        assert!(first.contains("create D2D factory"), "{first}");
        // Nothing of the failed renders is left behind: the next one draws.
        let png = visuals.render(geometry, Color::WHITE, &mut draw).unwrap();
        let image = visuals.wic.decode(&png).unwrap();
        assert_eq!((image.width, image.height), (24, 16));
        assert_eq!(
            image.pixel(1, 1),
            Some([0, 0, 255, 255]),
            "the red square (BGRA)"
        );
        assert_eq!(
            image.pixel(20, 12),
            Some([255, 255, 255, 255]),
            "the background"
        );
    }
}
