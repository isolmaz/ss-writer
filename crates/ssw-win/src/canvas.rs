//! [`D2dCanvas`]: the Direct2D implementation of [`ssw_ui::canvas::Canvas`].
//!
//! The canvas draws through the `ID2D1RenderTarget` base interface, so the same code serves a
//! window's device context and a WIC bitmap render target. Coordinates are DIPs: the target's
//! DPI does the pixel scaling. `Canvas` methods cannot fail, so the first error is remembered
//! and returned by [`D2dCanvas::finish`].

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use ssw_render::scene::Scene;
use ssw_ui::canvas::{Canvas, Dash, IconFont, ImageId, Path, PathVerb, SceneId};
use ssw_ui::geometry::{Color, CornerRadii, Point, Rect, Size, Transform2D};
use ssw_ui::text::TextLayout;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F,
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN, D2D1_FILL_MODE_WINDING,
    D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_ARC_SIZE_SMALL,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_FLAT,
    D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_DASH, D2D1_DASH_STYLE_DOT, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS, D2D1_LINE_JOIN_MITER,
    D2D1_QUADRATIC_BEZIER_SEGMENT, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE, ID2D1Bitmap,
    ID2D1Factory, ID2D1Factory1, ID2D1Geometry, ID2D1Layer, ID2D1RenderTarget,
    ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{DWRITE_TEXT_METRICS, IDWriteTextLayout};
use windows_numerics::{Matrix3x2, Vector2};

use crate::com::{OsResultExt, WinError};
use crate::dwrite::{DWriteEngine, DWriteLayout};
use crate::fault::check;
use crate::fonts::icon_family;
use crate::wic::{DecodedImage, Wic};

/// Cache sizes beyond which a cache is cleared (they hold GPU objects).
const MAX_BRUSHES: usize = 512;
const MAX_GEOMETRIES: usize = 256;

/// Draws a [`Scene`] with D2D; implemented by the render integration, injected into the canvas.
pub trait SceneRenderer {
    /// Draws `scene` scaled into `dest` (DIPs, in the target's current transform).
    ///
    /// # Errors
    /// Fails when drawing fails; the canvas reports the first failure from `finish`.
    fn draw(&self, target: &ID2D1RenderTarget, scene: &Scene, dest: Rect) -> Result<(), WinError>;
}

/// Decoded images by id; device bitmaps are created lazily per render target.
#[derive(Debug)]
pub struct ImageStore {
    wic: Wic,
    images: HashMap<ImageId, (u64, DecodedImage)>,
    revision: u64,
}

impl ImageStore {
    #[must_use]
    pub fn new(wic: Wic) -> Self {
        Self {
            wic,
            images: HashMap::new(),
            revision: 0,
        }
    }

    /// Decodes PNG / JPEG / GIF / BMP / ... bytes and registers them under `id`, replacing
    /// any previous image. Returns the pixel size.
    ///
    /// # Errors
    /// Fails when the bytes are not a supported image.
    pub fn register_encoded(&mut self, id: ImageId, bytes: &[u8]) -> Result<(u32, u32), WinError> {
        let image = self.wic.decode(bytes)?;
        let size = (image.width, image.height);
        self.insert(id, image);
        Ok(size)
    }

    /// Registers premultiplied BGRA pixels under `id`.
    ///
    /// # Errors
    /// Fails when the buffer does not match the size.
    pub fn register_bgra(
        &mut self,
        id: ImageId,
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    ) -> Result<(), WinError> {
        if bgra.len() != (width as usize) * (height as usize) * 4 || width == 0 || height == 0 {
            return Err(WinError::Invalid(
                "pixel buffer does not match the image size",
            ));
        }
        self.insert(
            id,
            DecodedImage {
                width,
                height,
                bgra,
            },
        );
        Ok(())
    }

    fn insert(&mut self, id: ImageId, image: DecodedImage) {
        self.revision += 1;
        self.images.insert(id, (self.revision, image));
    }

    /// Forgets an image; returns whether it existed.
    pub fn remove(&mut self, id: ImageId) -> bool {
        self.images.remove(&id).is_some()
    }

    /// Pixel size of a registered image.
    #[must_use]
    pub fn size(&self, id: ImageId) -> Option<(u32, u32)> {
        self.images
            .get(&id)
            .map(|(_, image)| (image.width, image.height))
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }
}

/// Everything a canvas draws from that is not tied to one render target.
pub struct CanvasAssets {
    pub images: ImageStore,
    scenes: HashMap<SceneId, Arc<Scene>>,
    renderer: Option<Box<dyn SceneRenderer>>,
    engine: DWriteEngine,
}

impl std::fmt::Debug for CanvasAssets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanvasAssets")
            .field("images", &self.images.len())
            .field("scenes", &self.scenes.len())
            .field("has_scene_renderer", &self.renderer.is_some())
            .finish_non_exhaustive()
    }
}

impl CanvasAssets {
    #[must_use]
    pub fn new(images: ImageStore, engine: DWriteEngine) -> Self {
        Self {
            images,
            scenes: HashMap::new(),
            renderer: None,
            engine,
        }
    }

    /// Installs the renderer used by `Canvas::draw_scene`.
    pub fn set_scene_renderer(&mut self, renderer: Box<dyn SceneRenderer>) {
        self.renderer = Some(renderer);
    }

    /// Registers a scene under `id`, replacing any previous one.
    pub fn register_scene(&mut self, id: SceneId, scene: Arc<Scene>) {
        self.scenes.insert(id, scene);
    }

    /// Forgets a scene; returns whether it existed.
    pub fn remove_scene(&mut self, id: SceneId) -> bool {
        self.scenes.remove(&id).is_some()
    }

    #[must_use]
    pub fn scene(&self, id: SceneId) -> Option<&Arc<Scene>> {
        self.scenes.get(&id)
    }
}

/// Device-dependent caches of one render target and device generation.
pub struct TargetResources {
    generation: u64,
    factory: ID2D1Factory1,
    brushes: HashMap<[u32; 4], ID2D1SolidColorBrush>,
    layers: Vec<ID2D1Layer>,
    geometries: HashMap<Vec<u32>, ID2D1Geometry>,
    bitmaps: HashMap<ImageId, (u64, ID2D1Bitmap)>,
    strokes: HashMap<Dash, ID2D1StrokeStyle>,
    icons: HashMap<(char, IconFont, u32), (IDWriteTextLayout, Size)>,
}

impl std::fmt::Debug for TargetResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetResources")
            .field("generation", &self.generation)
            .field("brushes", &self.brushes.len())
            .field("geometries", &self.geometries.len())
            .finish_non_exhaustive()
    }
}

impl TargetResources {
    /// Empty caches for a target built on devices of `generation`.
    #[must_use]
    pub fn new(factory: ID2D1Factory1, generation: u64) -> Self {
        Self {
            generation,
            factory,
            brushes: HashMap::new(),
            layers: Vec::new(),
            geometries: HashMap::new(),
            bitmaps: HashMap::new(),
            strokes: HashMap::new(),
            icons: HashMap::new(),
        }
    }

    /// The device generation the caches belong to.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of cached brushes.
    #[must_use]
    pub fn brush_count(&self) -> usize {
        self.brushes.len()
    }

    /// Number of cached geometries.
    #[must_use]
    pub fn geometry_count(&self) -> usize {
        self.geometries.len()
    }

    fn brush(
        &mut self,
        target: &ID2D1RenderTarget,
        color: Color,
    ) -> Result<ID2D1SolidColorBrush, WinError> {
        let key = [
            color.r.to_bits(),
            color.g.to_bits(),
            color.b.to_bits(),
            color.a.to_bits(),
        ];
        if let Some(brush) = self.brushes.get(&key) {
            return Ok(brush.clone());
        }
        if self.brushes.len() >= MAX_BRUSHES {
            self.brushes.clear();
        }
        let d2d = D2D1_COLOR_F {
            r: color.r,
            g: color.g,
            b: color.b,
            a: color.a,
        };
        // SAFETY: valid target and color pointer.
        let brush =
            check(unsafe { target.CreateSolidColorBrush(&d2d, None) }).ctx("create brush")?;
        self.brushes.insert(key, brush.clone());
        Ok(brush)
    }

    fn stroke_style(&mut self, dash: Dash) -> Result<Option<ID2D1StrokeStyle>, WinError> {
        if dash == Dash::Solid {
            return Ok(None);
        }
        if let Some(style) = self.strokes.get(&dash) {
            return Ok(Some(style.clone()));
        }
        let (dash_style, dash_cap) = match dash {
            Dash::Dotted => (D2D1_DASH_STYLE_DOT, D2D1_CAP_STYLE_ROUND),
            Dash::Dashed | Dash::Solid => (D2D1_DASH_STYLE_DASH, D2D1_CAP_STYLE_FLAT),
        };
        let properties = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: dash_cap,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: dash_style,
            dashOffset: 0.0,
        };
        // `ID2D1Factory1` has its own `CreateStrokeStyle` (extended properties); the plain
        // version is wanted here.
        let factory: &ID2D1Factory = &self.factory;
        // SAFETY: valid factory; preset dash styles take no custom array.
        let style = check(unsafe { factory.CreateStrokeStyle(&properties, None) })
            .ctx("create stroke style")?;
        self.strokes.insert(dash, style.clone());
        Ok(Some(style))
    }

    fn layer(&mut self, target: &ID2D1RenderTarget) -> Result<ID2D1Layer, WinError> {
        if let Some(layer) = self.layers.pop() {
            return Ok(layer);
        }
        // SAFETY: valid target; `None` lets D2D size the layer automatically.
        check(unsafe { target.CreateLayer(None) }).ctx("create layer")
    }

    fn geometry(
        &mut self,
        key: Vec<u32>,
        build: impl FnOnce(&ID2D1Factory1) -> Result<ID2D1Geometry, WinError>,
    ) -> Result<ID2D1Geometry, WinError> {
        if let Some(geometry) = self.geometries.get(&key) {
            return Ok(geometry.clone());
        }
        if self.geometries.len() >= MAX_GEOMETRIES {
            self.geometries.clear();
        }
        let geometry = build(&self.factory)?;
        self.geometries.insert(key, geometry.clone());
        Ok(geometry)
    }

    fn bitmap(
        &mut self,
        target: &ID2D1RenderTarget,
        id: ImageId,
        revision: u64,
        image: &DecodedImage,
    ) -> Result<ID2D1Bitmap, WinError> {
        if let Some((cached, bitmap)) = self.bitmaps.get(&id)
            && *cached == revision
        {
            return Ok(bitmap.clone());
        }
        let properties = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        // SAFETY: the pixel buffer holds `stride * height` bytes and D2D copies it.
        let bitmap = check(unsafe {
            target.CreateBitmap(
                D2D_SIZE_U {
                    width: image.width,
                    height: image.height,
                },
                Some(image.bgra.as_ptr().cast()),
                image.stride(),
                &properties,
            )
        })
        .ctx("create bitmap")?;
        self.bitmaps.insert(id, (revision, bitmap.clone()));
        Ok(bitmap)
    }
}

fn matrix(transform: Transform2D) -> Matrix3x2 {
    Matrix3x2 {
        M11: transform.m11,
        M12: transform.m12,
        M21: transform.m21,
        M22: transform.m22,
        M31: transform.dx,
        M32: transform.dy,
    }
}

fn rect_f(rect: Rect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: rect.left(),
        top: rect.top(),
        right: rect.right(),
        bottom: rect.bottom(),
    }
}

fn vec2(point: Point) -> Vector2 {
    Vector2 {
        X: point.x,
        Y: point.y,
    }
}

/// A cache key for a path: verb tags and coordinate bits.
fn path_key(path: &Path) -> Vec<u32> {
    let mut key = Vec::with_capacity(path.verbs().len() * 4);
    for verb in path.verbs() {
        match *verb {
            PathVerb::MoveTo(to) => key.extend([1, to.x.to_bits(), to.y.to_bits()]),
            PathVerb::LineTo(to) => key.extend([2, to.x.to_bits(), to.y.to_bits()]),
            PathVerb::QuadTo { control, to } => key.extend([
                3,
                control.x.to_bits(),
                control.y.to_bits(),
                to.x.to_bits(),
                to.y.to_bits(),
            ]),
            PathVerb::CubicTo {
                control1,
                control2,
                to,
            } => key.extend([
                4,
                control1.x.to_bits(),
                control1.y.to_bits(),
                control2.x.to_bits(),
                control2.y.to_bits(),
                to.x.to_bits(),
                to.y.to_bits(),
            ]),
            PathVerb::ArcTo {
                to,
                radii,
                rotation_degrees,
                large_arc,
                sweep_clockwise,
            } => key.extend([
                5,
                to.x.to_bits(),
                to.y.to_bits(),
                radii.width.to_bits(),
                radii.height.to_bits(),
                rotation_degrees.to_bits(),
                u32::from(large_arc) | (u32::from(sweep_clockwise) << 1),
            ]),
            PathVerb::Close => key.push(6),
        }
    }
    key
}

/// The closed outline of a rounded rectangle with per-corner radii, built from arcs.
#[must_use]
pub fn rounded_rect_path(rect: Rect, radii: CornerRadii) -> Path {
    let r = radii.clamped(rect.size());
    let arc = |radius: f32, to: Point| (to, Size::new(radius, radius));
    let (left, top, right, bottom) = (rect.left(), rect.top(), rect.right(), rect.bottom());
    let mut path = Path::new().move_to(Point::new(left + r.top_left, top));
    path = path.line_to(Point::new(right - r.top_right, top));
    if r.top_right > 0.0 {
        let (to, radii) = arc(r.top_right, Point::new(right, top + r.top_right));
        path = path.arc_to(to, radii, 0.0, false, true);
    }
    path = path.line_to(Point::new(right, bottom - r.bottom_right));
    if r.bottom_right > 0.0 {
        let (to, radii) = arc(r.bottom_right, Point::new(right - r.bottom_right, bottom));
        path = path.arc_to(to, radii, 0.0, false, true);
    }
    path = path.line_to(Point::new(left + r.bottom_left, bottom));
    if r.bottom_left > 0.0 {
        let (to, radii) = arc(r.bottom_left, Point::new(left, bottom - r.bottom_left));
        path = path.arc_to(to, radii, 0.0, false, true);
    }
    path = path.line_to(Point::new(left, top + r.top_left));
    if r.top_left > 0.0 {
        let (to, radii) = arc(r.top_left, Point::new(left + r.top_left, top));
        path = path.arc_to(to, radii, 0.0, false, true);
    }
    path.close()
}

fn build_geometry(factory: &ID2D1Factory1, path: &Path) -> Result<ID2D1Geometry, WinError> {
    // SAFETY: the sink is opened, filled with balanced figures and closed before the geometry
    // is used.
    unsafe {
        let geometry = check(factory.CreatePathGeometry()).ctx("create path geometry")?;
        let sink = check(geometry.Open()).ctx("open geometry sink")?;
        sink.SetFillMode(D2D1_FILL_MODE_WINDING);
        let mut open = false;
        for verb in path.verbs() {
            match *verb {
                PathVerb::MoveTo(to) => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    sink.BeginFigure(vec2(to), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                }
                PathVerb::LineTo(to) => sink.AddLine(vec2(to)),
                PathVerb::QuadTo { control, to } => {
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: vec2(control),
                        point2: vec2(to),
                    });
                }
                PathVerb::CubicTo {
                    control1,
                    control2,
                    to,
                } => sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                    point1: vec2(control1),
                    point2: vec2(control2),
                    point3: vec2(to),
                }),
                PathVerb::ArcTo {
                    to,
                    radii,
                    rotation_degrees,
                    large_arc,
                    sweep_clockwise,
                } => sink.AddArc(&D2D1_ARC_SEGMENT {
                    point: vec2(to),
                    size: windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F {
                        width: radii.width,
                        height: radii.height,
                    },
                    rotationAngle: rotation_degrees,
                    sweepDirection: if sweep_clockwise {
                        D2D1_SWEEP_DIRECTION_CLOCKWISE
                    } else {
                        D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE
                    },
                    arcSize: if large_arc {
                        D2D1_ARC_SIZE_LARGE
                    } else {
                        D2D1_ARC_SIZE_SMALL
                    },
                }),
                // `Path` only records a close for an open subpath.
                PathVerb::Close => {
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                    open = false;
                }
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        check(sink.Close()).ctx("close geometry sink")?;
        Ok(geometry.into())
    }
}

/// What an entry of the clip/layer stack must be undone with.
enum StackEntry {
    AxisClip,
    Layer(ID2D1Layer),
}

/// A [`Canvas`] drawing with Direct2D.
pub struct D2dCanvas<'a> {
    target: ID2D1RenderTarget,
    assets: &'a CanvasAssets,
    resources: &'a mut TargetResources,
    transform: Transform2D,
    saved: Vec<Transform2D>,
    stack: Vec<StackEntry>,
    error: Option<WinError>,
}

impl std::fmt::Debug for D2dCanvas<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D2dCanvas")
            .field("transform", &self.transform)
            .field("stack", &self.stack.len())
            .field("failed", &self.error.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> D2dCanvas<'a> {
    /// A canvas drawing into `target` (inside `BeginDraw`/`EndDraw`) with the identity
    /// transform.
    #[must_use]
    pub fn new(
        target: &ID2D1RenderTarget,
        assets: &'a CanvasAssets,
        resources: &'a mut TargetResources,
    ) -> Self {
        let canvas = Self {
            target: target.clone(),
            assets,
            resources,
            transform: Transform2D::IDENTITY,
            saved: Vec::new(),
            stack: Vec::new(),
            error: None,
        };
        canvas.apply_transform();
        canvas
    }

    /// Ends the canvas, returning the first error that occurred (an unbalanced save, clip or
    /// layer counts as one).
    ///
    /// # Errors
    /// The first recorded drawing error.
    pub fn finish(mut self) -> Result<(), WinError> {
        if self.error.is_none() && (!self.saved.is_empty() || !self.stack.is_empty()) {
            self.error = Some(WinError::Invalid("unbalanced save, clip or layer"));
        }
        // Layers left open are closed by `Drop` when `self` goes out of scope below.
        match self.error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// The current transform.
    #[must_use]
    pub fn transform(&self) -> Transform2D {
        self.transform
    }

    fn apply_transform(&self) {
        let m = matrix(self.transform);
        // SAFETY: valid target and matrix pointer.
        unsafe { self.target.SetTransform(&m) };
    }

    fn fail(&mut self, error: WinError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    fn ok<T>(&mut self, result: Result<T, WinError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.fail(error);
                None
            }
        }
    }

    fn brush(&mut self, color: Color) -> Option<ID2D1SolidColorBrush> {
        let result = self.resources.brush(&self.target, color);
        self.ok(result)
    }

    fn geometry_for_path(&mut self, path: &Path) -> Option<ID2D1Geometry> {
        let result = self
            .resources
            .geometry(path_key(path), |factory| build_geometry(factory, path));
        self.ok(result)
    }

    fn pop_entry(&mut self, entry: StackEntry) {
        match entry {
            // SAFETY: pops the clip pushed by the matching `push_*` call.
            StackEntry::AxisClip => unsafe { self.target.PopAxisAlignedClip() },
            StackEntry::Layer(layer) => {
                // SAFETY: pops the layer pushed by the matching `push_*` call.
                unsafe { self.target.PopLayer() };
                self.resources.layers.push(layer);
            }
        }
    }

    fn push_layer_with(&mut self, mask: Option<ID2D1Geometry>, opacity: f32) {
        let Some(layer) = self.ok_layer() else { return };
        let mut parameters = D2D1_LAYER_PARAMETERS {
            contentBounds: D2D_RECT_F {
                left: f32::MIN,
                top: f32::MIN,
                right: f32::MAX,
                bottom: f32::MAX,
            },
            geometricMask: ManuallyDrop::new(mask),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: matrix(Transform2D::IDENTITY),
            opacity,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        // SAFETY: valid target, parameters and layer; the borrowed mask interface is released
        // exactly once right after the call (the struct holds it in `ManuallyDrop`).
        unsafe {
            self.target.PushLayer(&parameters, &layer);
            ManuallyDrop::drop(&mut parameters.geometricMask);
            ManuallyDrop::drop(&mut parameters.opacityBrush);
        }
        self.stack.push(StackEntry::Layer(layer));
    }

    fn ok_layer(&mut self) -> Option<ID2D1Layer> {
        let result = self.resources.layer(&self.target);
        self.ok(result)
    }

    fn axis_aligned(&self) -> bool {
        self.transform.m12 == 0.0 && self.transform.m21 == 0.0
    }

    fn fill_geometry(&mut self, geometry: &ID2D1Geometry, color: Color) {
        let Some(brush) = self.brush(color) else {
            return;
        };
        // SAFETY: valid geometry and brush for this target.
        unsafe {
            self.target.FillGeometry(
                geometry,
                &brush,
                None::<&windows::Win32::Graphics::Direct2D::ID2D1Brush>,
            )
        };
    }

    fn stroke_geometry(&mut self, geometry: &ID2D1Geometry, color: Color, width: f32, dash: Dash) {
        let Some(brush) = self.brush(color) else {
            return;
        };
        let style = self.resources.stroke_style(dash);
        let Some(style) = self.ok(style) else { return };
        // SAFETY: valid geometry, brush and optional stroke style.
        unsafe {
            self.target
                .DrawGeometry(geometry, &brush, width, style.as_ref())
        };
    }

    fn rounded_geometry(&mut self, rect: Rect, radii: CornerRadii) -> Option<ID2D1Geometry> {
        let path = rounded_rect_path(rect, radii);
        self.geometry_for_path(&path)
    }

    fn icon_layout(
        &mut self,
        glyph: char,
        font: IconFont,
        size: f32,
    ) -> Option<(IDWriteTextLayout, Size)> {
        let key = (glyph, font, size.to_bits());
        if let Some(found) = self.resources.icons.get(&key) {
            return Some(found.clone());
        }
        let engine = &self.assets.engine;
        let built = (|| -> Result<(IDWriteTextLayout, Size), WinError> {
            let format = engine.text_format(icon_family(font), size, 400, false)?;
            let mut units = [0u16; 2];
            let encoded = glyph.encode_utf16(&mut units);
            // SAFETY: valid factory and format; the metrics struct is a valid out pointer.
            unsafe {
                let layout = check(
                    engine
                        .factory()
                        .CreateTextLayout(encoded, &format, 10_000.0, 10_000.0),
                )
                .ctx("icon layout")?;
                let mut metrics = DWRITE_TEXT_METRICS::default();
                check(layout.GetMetrics(&mut metrics)).ctx("icon metrics")?;
                Ok((
                    layout,
                    Size::new(metrics.widthIncludingTrailingWhitespace, metrics.height),
                ))
            }
        })();
        let built = self.ok(built)?;
        self.resources.icons.insert(key, built.clone());
        Some(built)
    }
}

impl Drop for D2dCanvas<'_> {
    /// Clips and layers left open would corrupt the target (`EndDraw` fails with them pushed):
    /// close them, also when the drawing code unwinds before calling [`D2dCanvas::finish`].
    fn drop(&mut self) {
        while let Some(entry) = self.stack.pop() {
            self.pop_entry(entry);
        }
    }
}

impl Canvas for D2dCanvas<'_> {
    fn save(&mut self) {
        self.saved.push(self.transform);
    }

    fn restore(&mut self) {
        match self.saved.pop() {
            Some(transform) => {
                self.transform = transform;
                self.apply_transform();
            }
            None => self.fail(WinError::Invalid("restore without save")),
        }
    }

    fn concat_transform(&mut self, transform: Transform2D) {
        self.transform = transform.compose(self.transform);
        self.apply_transform();
    }

    fn push_clip_rect(&mut self, rect: Rect) {
        if self.axis_aligned() {
            let clip = rect_f(rect);
            // SAFETY: valid target; popped by `pop_clip`.
            unsafe {
                self.target
                    .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            }
            self.stack.push(StackEntry::AxisClip);
        } else {
            let geometry = self.geometry_for_path(&Path::from_rect(rect));
            if let Some(geometry) = geometry {
                self.push_layer_with(Some(geometry), 1.0);
            }
        }
    }

    fn push_clip_rounded_rect(&mut self, rect: Rect, radii: CornerRadii) {
        if radii.is_zero() {
            self.push_clip_rect(rect);
            return;
        }
        if let Some(geometry) = self.rounded_geometry(rect, radii) {
            self.push_layer_with(Some(geometry), 1.0);
        }
    }

    fn pop_clip(&mut self) {
        match self.stack.pop() {
            Some(entry @ (StackEntry::AxisClip | StackEntry::Layer(_))) => self.pop_entry(entry),
            None => self.fail(WinError::Invalid("pop_clip without a clip")),
        }
    }

    fn push_layer(&mut self, opacity: f32) {
        self.push_layer_with(None, opacity.clamp(0.0, 1.0));
    }

    fn pop_layer(&mut self) {
        match self.stack.pop() {
            Some(entry) => self.pop_entry(entry),
            None => self.fail(WinError::Invalid("pop_layer without a layer")),
        }
    }

    fn fill_rect(&mut self, rect: Rect, color: Color) {
        let Some(brush) = self.brush(color) else {
            return;
        };
        let area = rect_f(rect);
        // SAFETY: valid rectangle and brush.
        unsafe { self.target.FillRectangle(&area, &brush) };
    }

    fn fill_rounded_rect(&mut self, rect: Rect, radii: CornerRadii, color: Color) {
        if radii.is_zero() {
            self.fill_rect(rect, color);
        } else if radii.is_uniform() {
            let Some(brush) = self.brush(color) else {
                return;
            };
            let radius = radii.clamped(rect.size()).top_left;
            let rounded = D2D1_ROUNDED_RECT {
                rect: rect_f(rect),
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: valid rounded rectangle and brush.
            unsafe { self.target.FillRoundedRectangle(&rounded, &brush) };
        } else if let Some(geometry) = self.rounded_geometry(rect, radii) {
            self.fill_geometry(&geometry, color);
        }
    }

    fn stroke_rounded_rect(&mut self, rect: Rect, radii: CornerRadii, color: Color, width: f32) {
        if radii.is_zero() || radii.is_uniform() {
            let Some(brush) = self.brush(color) else {
                return;
            };
            let radius = radii.clamped(rect.size()).top_left;
            let rounded = D2D1_ROUNDED_RECT {
                rect: rect_f(rect),
                radiusX: radius,
                radiusY: radius,
            };
            // SAFETY: valid rounded rectangle and brush; no stroke style.
            unsafe {
                self.target.DrawRoundedRectangle(
                    &rounded,
                    &brush,
                    width,
                    None::<&ID2D1StrokeStyle>,
                );
            }
        } else if let Some(geometry) = self.rounded_geometry(rect, radii) {
            self.stroke_geometry(&geometry, color, width, Dash::Solid);
        }
    }

    fn stroke_line(&mut self, from: Point, to: Point, color: Color, width: f32, dash: Dash) {
        let Some(brush) = self.brush(color) else {
            return;
        };
        let style = self.resources.stroke_style(dash);
        let Some(style) = self.ok(style) else { return };
        // SAFETY: valid points, brush and optional stroke style.
        unsafe {
            self.target
                .DrawLine(vec2(from), vec2(to), &brush, width, style.as_ref());
        }
    }

    fn fill_path(&mut self, path: &Path, color: Color) {
        if path.is_empty() {
            return;
        }
        if let Some(geometry) = self.geometry_for_path(path) {
            self.fill_geometry(&geometry, color);
        }
    }

    fn stroke_path(&mut self, path: &Path, color: Color, width: f32, dash: Dash) {
        if path.is_empty() {
            return;
        }
        if let Some(geometry) = self.geometry_for_path(path) {
            self.stroke_geometry(&geometry, color, width, dash);
        }
    }

    fn draw_text_layout(&mut self, layout: &dyn TextLayout, origin: Point, color: Color) {
        let Some(layout) = layout.as_any().downcast_ref::<DWriteLayout>() else {
            // A fallback layout from a failed request has nothing to draw; any other type was
            // not created by this platform layer.
            if layout
                .as_any()
                .downcast_ref::<crate::dwrite::FailedLayout>()
                .is_none()
            {
                self.fail(WinError::Invalid(
                    "text layout was not created by DWriteEngine",
                ));
            }
            return;
        };
        for (rect, background) in layout.backgrounds() {
            self.fill_rect(rect.translate(origin.x, origin.y), *background);
        }
        let target = self.target.clone();
        let resources = &mut *self.resources;
        let result = layout.draw(&target, origin, color, &mut |c| resources.brush(&target, c));
        self.ok(result);
    }

    fn draw_image(&mut self, image: ImageId, dest: Rect, opacity: f32) {
        let Some((revision, decoded)) = self.assets.images.images.get(&image) else {
            self.fail(WinError::Invalid("image is not registered"));
            return;
        };
        let bitmap = self
            .resources
            .bitmap(&self.target, image, *revision, decoded);
        let Some(bitmap) = self.ok(bitmap) else {
            return;
        };
        let area = rect_f(dest);
        // SAFETY: valid bitmap created for this target and destination rectangle.
        unsafe {
            self.target.DrawBitmap(
                &bitmap,
                Some(&area),
                opacity.clamp(0.0, 1.0),
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
    }

    fn draw_scene(&mut self, scene: SceneId, dest: Rect) {
        let Some(registered) = self.assets.scenes.get(&scene) else {
            self.fail(WinError::Invalid("scene is not registered"));
            return;
        };
        let Some(renderer) = self.assets.renderer.as_deref() else {
            self.fail(WinError::Invalid("no scene renderer is installed"));
            return;
        };
        let result = renderer.draw(&self.target, registered, dest);
        self.ok(result);
    }

    fn draw_icon(&mut self, glyph: char, font: IconFont, size: f32, color: Color, center: Point) {
        let Some((layout, extent)) = self.icon_layout(glyph, font, size) else {
            return;
        };
        let Some(brush) = self.brush(color) else {
            return;
        };
        let origin = Vector2 {
            X: center.x - extent.width / 2.0,
            Y: center.y - extent.height / 2.0,
        };
        // SAFETY: valid layout and brush for this target.
        unsafe {
            self.target
                .DrawTextLayout(origin, &layout, &brush, D2D1_DRAW_TEXT_OPTIONS_NONE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;
    use crate::gfx::{WicTarget, d2d_factory};
    use ssw_ui::text::{TextLayoutEngine, TextStyle};

    struct Rig {
        wic: Wic,
        target: WicTarget,
        assets: CanvasAssets,
        resources: TargetResources,
        engine: DWriteEngine,
        /// Declared last so it drops last: COM objects must be released before COM is torn
        /// down (releasing WIC / D2D objects afterwards crashes).
        _com: ComGuard,
    }

    const WHITE: Color = Color::WHITE;
    const RED: Color = Color::from_rgb_u32(0xFF0000);
    const BLUE: Color = Color::from_rgb_u32(0x0000FF);

    fn rig(width: u32, height: u32) -> Rig {
        let com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let factory = d2d_factory(false).unwrap();
        let target = WicTarget::new(&factory, &wic, width, height, 96.0).unwrap();
        let engine = DWriteEngine::new().unwrap();
        let assets = CanvasAssets::new(ImageStore::new(wic.clone()), engine.clone());
        let resources = TargetResources::new(factory, 1);
        Rig {
            _com: com,
            wic,
            target,
            assets,
            resources,
            engine,
        }
    }

    impl Rig {
        fn draw(
            &mut self,
            background: Option<Color>,
            f: impl FnOnce(&mut D2dCanvas<'_>),
        ) -> Result<DecodedImage, WinError> {
            let Rig {
                target,
                assets,
                resources,
                ..
            } = self;
            target.paint(background, |rt| {
                let mut canvas = D2dCanvas::new(rt, assets, resources);
                f(&mut canvas);
                canvas.finish()
            })?;
            self.target.pixels()
        }
    }

    fn bgra(image: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        image.pixel(x, y).unwrap()
    }

    #[test]
    fn fills_clips_and_transforms_paint_expected_pixels() {
        let mut rig = rig(64, 64);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.fill_rect(Rect::new(4.0, 4.0, 10.0, 10.0), RED);
                canvas.save();
                canvas.translate(20.0, 0.0);
                canvas.fill_rect(Rect::new(4.0, 4.0, 10.0, 10.0), BLUE);
                canvas.restore();
                canvas.push_clip_rect(Rect::new(40.0, 0.0, 10.0, 10.0));
                canvas.fill_rect(Rect::new(30.0, 0.0, 60.0, 30.0), RED);
                canvas.pop_clip();
                canvas.save();
                canvas.concat_transform(Transform2D::scale(2.0, 2.0));
                canvas.fill_rect(Rect::new(2.0, 20.0, 5.0, 5.0), BLUE);
                canvas.restore();
            })
            .unwrap();
        assert_eq!(bgra(&image, 8, 8), [0, 0, 255, 255]);
        assert_eq!(bgra(&image, 30, 8), [255, 0, 0, 255]);
        assert_eq!(bgra(&image, 2, 2), [255, 255, 255, 255]);
        assert_eq!(bgra(&image, 45, 5), [0, 0, 255, 255]);
        assert_eq!(bgra(&image, 55, 5), [255, 255, 255, 255]);
        assert_eq!(bgra(&image, 45, 15), [255, 255, 255, 255]);
        assert_eq!(bgra(&image, 8, 45), [255, 0, 0, 255]);
        assert_eq!(bgra(&image, 20, 60), [255, 255, 255, 255]);
    }

    #[test]
    fn rounded_shapes_cut_their_corners() {
        let mut rig = rig(64, 64);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.fill_rounded_rect(
                    Rect::new(0.0, 0.0, 30.0, 30.0),
                    CornerRadii::uniform(12.0),
                    RED,
                );
                canvas.fill_rounded_rect(
                    Rect::new(32.0, 0.0, 30.0, 30.0),
                    CornerRadii::new(14.0, 0.0, 14.0, 0.0),
                    BLUE,
                );
                canvas.fill_rounded_rect(Rect::new(0.0, 32.0, 30.0, 30.0), CornerRadii::ZERO, RED);
                // Strokes are 2 DIPs wide and centered on whole-pixel lines, so the two pixel rows
                // next to the line are fully covered (no anti-aliased half coverage).
                canvas.stroke_rounded_rect(
                    Rect::new(35.0, 35.0, 24.0, 24.0),
                    CornerRadii::uniform(6.0),
                    BLUE,
                    2.0,
                );
                // Per-corner radii take the path route; drawn over the blue shape above.
                canvas.stroke_rounded_rect(
                    Rect::new(36.0, 4.0, 22.0, 22.0),
                    CornerRadii::new(0.0, 8.0, 0.0, 8.0),
                    RED,
                    2.0,
                );
                canvas.push_clip_rounded_rect(Rect::new(0.0, 0.0, 64.0, 64.0), CornerRadii::ZERO);
                canvas.pop_clip();
            })
            .unwrap();
        assert_eq!(
            bgra(&image, 0, 0),
            [255, 255, 255, 255],
            "uniform corner cut"
        );
        assert_eq!(bgra(&image, 15, 15), [0, 0, 255, 255]);
        assert_eq!(bgra(&image, 32, 0), [255, 255, 255, 255], "top-left cut");
        assert_eq!(bgra(&image, 40, 12), [255, 0, 0, 255]);
        assert_eq!(bgra(&image, 1, 33), [0, 0, 255, 255]);
        // Uniform stroke: both rows around y = 35 are blue, the outer corner is cut, the
        // interior stays background.
        assert_eq!(
            bgra(&image, 47, 34),
            [255, 0, 0, 255],
            "stroke row above the line"
        );
        assert_eq!(
            bgra(&image, 47, 35),
            [255, 0, 0, 255],
            "stroke row below the line"
        );
        assert_eq!(
            bgra(&image, 34, 34),
            [255, 255, 255, 255],
            "rounded stroke corner"
        );
        assert_eq!(
            bgra(&image, 47, 47),
            [255, 255, 255, 255],
            "stroke interior untouched"
        );
        // Per-corner stroke: red on the top edge, the shape's blue fill inside it.
        assert_eq!(bgra(&image, 43, 3), [0, 0, 255, 255], "path stroke");
        assert_eq!(bgra(&image, 43, 4), [0, 0, 255, 255], "path stroke");
        assert_eq!(
            bgra(&image, 43, 10),
            [255, 0, 0, 255],
            "fill inside the path stroke"
        );
    }

    #[test]
    fn rounded_clips_layers_and_opacity() {
        let mut rig = rig(64, 64);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.push_clip_rounded_rect(
                    Rect::new(0.0, 0.0, 30.0, 30.0),
                    CornerRadii::uniform(15.0),
                );
                canvas.fill_rect(Rect::new(0.0, 0.0, 64.0, 64.0), RED);
                canvas.pop_clip();
                canvas.push_layer(0.5);
                canvas.fill_rect(Rect::new(32.0, 0.0, 20.0, 20.0), BLUE);
                canvas.pop_layer();
                canvas.save();
                canvas.concat_transform(Transform2D::rotation(std::f32::consts::FRAC_PI_4));
                canvas.push_clip_rect(Rect::new(30.0, 0.0, 10.0, 10.0));
                canvas.fill_rect(Rect::new(0.0, 0.0, 200.0, 200.0), BLUE);
                canvas.pop_clip();
                canvas.restore();
            })
            .unwrap();
        assert_eq!(
            bgra(&image, 0, 0),
            [255, 255, 255, 255],
            "round clip corner"
        );
        assert_eq!(bgra(&image, 15, 15), [0, 0, 255, 255]);
        let half = bgra(&image, 40, 10);
        assert!(
            (i32::from(half[0]) - 255).abs() <= 2 && (i32::from(half[2]) - 128).abs() <= 2,
            "{half:?}"
        );
        assert_eq!(bgra(&image, 60, 60), [255, 255, 255, 255]);
    }

    #[test]
    fn lines_paths_and_dashes() {
        let mut rig = rig(64, 64);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.stroke_line(
                    Point::new(0.0, 5.5),
                    Point::new(63.0, 5.5),
                    RED,
                    1.0,
                    Dash::Solid,
                );
                canvas.stroke_line(
                    Point::new(0.0, 10.5),
                    Point::new(63.0, 10.5),
                    RED,
                    1.0,
                    Dash::Dashed,
                );
                canvas.stroke_line(
                    Point::new(0.0, 15.5),
                    Point::new(63.0, 15.5),
                    RED,
                    2.0,
                    Dash::Dotted,
                );
                let triangle = Path::new()
                    .move_to(Point::new(10.0, 60.0))
                    .line_to(Point::new(30.0, 60.0))
                    .quad_to(Point::new(35.0, 45.0), Point::new(30.0, 30.0))
                    .cubic_to(
                        Point::new(25.0, 25.0),
                        Point::new(15.0, 25.0),
                        Point::new(10.0, 30.0),
                    )
                    .close();
                canvas.fill_path(&triangle, BLUE);
                let arc = Path::new()
                    .move_to(Point::new(40.0, 50.0))
                    .arc_to(
                        Point::new(60.0, 50.0),
                        Size::new(10.0, 10.0),
                        0.0,
                        false,
                        true,
                    )
                    .arc_to(
                        Point::new(40.0, 50.0),
                        Size::new(10.0, 10.0),
                        0.0,
                        true,
                        false,
                    );
                canvas.stroke_path(&arc, RED, 2.0, Dash::Solid);
                canvas.fill_path(&Path::new(), RED);
                canvas.stroke_path(&Path::new(), RED, 1.0, Dash::Dashed);
            })
            .unwrap();
        let red_count = |y: u32| {
            (0..64)
                .filter(|&x| bgra(&image, x, y) == [0, 0, 255, 255])
                .count()
        };
        assert_eq!(red_count(5), 63, "solid line");
        let dashed = red_count(10);
        assert!(dashed > 10 && dashed < 60, "dashed {dashed}");
        let dotted = (0..64)
            .filter(|&x| bgra(&image, x, 15) != [255, 255, 255, 255])
            .count();
        assert!(dotted > 5 && dotted < 60, "dotted {dotted}");
        assert_eq!(bgra(&image, 20, 50), [255, 0, 0, 255], "path interior");
        assert_eq!(bgra(&image, 3, 50), [255, 255, 255, 255]);
        assert_ne!(bgra(&image, 50, 40), [255, 255, 255, 255], "arc stroke");
        assert_eq!(
            rig.resources.geometry_count(),
            2,
            "empty paths are skipped, others cached"
        );
    }

    #[test]
    fn geometry_and_brush_caches_are_reused() {
        let mut rig = rig(32, 32);
        rig.draw(Some(WHITE), |canvas| {
            let path = Path::from_rect(Rect::new(1.0, 1.0, 5.0, 5.0));
            for _ in 0..3 {
                canvas.fill_path(&path, RED);
                canvas.fill_rect(Rect::new(10.0, 10.0, 4.0, 4.0), RED);
            }
        })
        .unwrap();
        assert_eq!(rig.resources.geometry_count(), 1);
        assert_eq!(rig.resources.brush_count(), 1);
        assert_eq!(rig.resources.generation(), 1);
    }

    #[test]
    fn text_icons_and_backgrounds_draw_pixels() {
        let mut rig = rig(160, 80);
        let engine = rig.engine.clone();
        let style = TextStyle::new("Segoe UI", 20.0);
        let ranges = [ssw_ui::text::StyledRange {
            range: 0..2,
            style: TextStyle::new("Segoe UI", 20.0)
                .with_color(RED)
                .with_background(Color::from_rgb_u32(0xFFFF00)),
        }];
        let plain = engine.layout("MMMM", &style, &[], &[], f32::INFINITY);
        let styled = engine.layout("MMMM", &style, &ranges, &[], f32::INFINITY);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.draw_text_layout(&*plain, Point::new(4.0, 4.0), Color::BLACK);
                canvas.draw_text_layout(&*styled, Point::new(4.0, 36.0), Color::BLACK);
                canvas.draw_icon(
                    '\u{E921}',
                    IconFont::SegoeFluent,
                    24.0,
                    Color::BLACK,
                    Point::new(130.0, 20.0),
                );
                canvas.draw_icon(
                    '\u{E921}',
                    IconFont::SegoeFluent,
                    24.0,
                    Color::BLACK,
                    Point::new(130.0, 50.0),
                );
            })
            .unwrap();
        let dark = |x0: u32, y0: u32, x1: u32, y1: u32| {
            (y0..y1)
                .flat_map(|y| (x0..x1).map(move |x| (x, y)))
                .filter(|&(x, y)| bgra(&image, x, y)[0] < 100)
                .count()
        };
        assert!(dark(0, 0, 100, 34) > 100, "plain text is black");
        let (red_text, yellow) = (
            (0..100u32).any(|x| {
                (36..70u32).any(|y| {
                    let p = bgra(&image, x, y);
                    p[2] > 200 && p[1] < 60 && p[0] < 60
                })
            }),
            (0..100u32).any(|x| (36..70u32).any(|y| bgra(&image, x, y) == [0, 255, 255, 255])),
        );
        assert!(red_text, "styled run uses its own color");
        assert!(yellow, "background is painted behind the run");
        assert!(dark(116, 6, 146, 36) > 5, "icon glyph drawn");
        rig.draw(Some(WHITE), |canvas| {
            canvas.draw_icon(
                '\u{E921}',
                IconFont::SegoeFluent,
                24.0,
                RED,
                Point::new(10.0, 10.0),
            );
            canvas.draw_icon(
                '\u{E921}',
                IconFont::SegoeFluent,
                24.0,
                RED,
                Point::new(40.0, 10.0),
            );
        })
        .unwrap();
        assert_eq!(
            rig.resources.icons.len(),
            1,
            "icon layouts are cached by glyph, font and size"
        );
    }

    #[test]
    fn baseline_shift_moves_a_run_up() {
        let mut rig = rig(120, 80);
        let engine = rig.engine.clone();
        let style = TextStyle::new("Segoe UI", 20.0);
        let ranges = [ssw_ui::text::StyledRange {
            range: 1..2,
            style: TextStyle::new("Segoe UI", 20.0)
                .with_color(RED)
                .with_baseline_shift(12.0),
        }];
        let flat = engine.layout("IIII", &style, &[], &[], f32::INFINITY);
        let raised = engine.layout("IIII", &style, &ranges, &[], f32::INFINITY);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.draw_text_layout(&*flat, Point::new(4.0, 30.0), Color::BLACK);
                canvas.draw_text_layout(&*raised, Point::new(4.0, 30.0), Color::BLACK);
            })
            .unwrap();
        let red_rows: Vec<u32> = (0..80u32)
            .filter(|&y| {
                (0..120u32).any(|x| {
                    let p = bgra(&image, x, y);
                    p[2] > 200 && p[1] < 80 && p[0] < 80
                })
            })
            .collect();
        assert!(!red_rows.is_empty(), "raised run is drawn in its color");
        let black_top = (0..80u32)
            .find(|&y| {
                (0..120u32).any(|x| bgra(&image, x, y)[0] < 60 && bgra(&image, x, y)[2] < 60)
            })
            .unwrap();
        assert!(red_rows[0] + 4 <= black_top, "{red_rows:?} {black_top}");
    }

    #[test]
    fn images_scale_and_report_errors() {
        let mut rig = rig(32, 32);
        let png = {
            let image = DecodedImage {
                width: 2,
                height: 1,
                bgra: vec![0, 0, 255, 255, 255, 0, 0, 255],
            };
            rig.wic.encode_png(&image).unwrap()
        };
        let id = ImageId(7);
        assert_eq!(
            rig.assets.images.register_encoded(id, &png).unwrap(),
            (2, 1)
        );
        assert_eq!(rig.assets.images.size(id), Some((2, 1)));
        assert_eq!(rig.assets.images.len(), 1);
        assert!(!rig.assets.images.is_empty());
        assert!(
            rig.assets
                .images
                .register_encoded(ImageId(8), b"junk")
                .is_err()
        );
        assert!(
            rig.assets
                .images
                .register_bgra(ImageId(9), 2, 2, vec![0; 4])
                .is_err()
        );
        assert!(
            rig.assets
                .images
                .register_bgra(ImageId(9), 0, 0, vec![])
                .is_err()
        );
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.draw_image(id, Rect::new(0.0, 0.0, 32.0, 16.0), 1.0);
                canvas.draw_image(id, Rect::new(0.0, 16.0, 32.0, 16.0), 0.5);
            })
            .unwrap();
        assert_eq!(bgra(&image, 4, 4), [0, 0, 255, 255]);
        assert_eq!(bgra(&image, 28, 4), [255, 0, 0, 255]);
        let faded = bgra(&image, 4, 24);
        assert!(
            faded[2] > 200 && faded[1] > 100 && faded[1] < 160,
            "{faded:?}"
        );
        // Re-registering under the same id refreshes the cached device bitmap.
        rig.assets
            .images
            .register_bgra(id, 1, 1, vec![0, 255, 0, 255])
            .unwrap();
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.draw_image(id, Rect::new(0.0, 0.0, 32.0, 32.0), 1.0)
            })
            .unwrap();
        assert_eq!(bgra(&image, 16, 16), [0, 255, 0, 255]);
        assert!(rig.assets.images.remove(id));
        assert!(!rig.assets.images.remove(id));
        let error = rig
            .draw(Some(WHITE), |canvas| canvas.draw_image(id, Rect::ZERO, 1.0))
            .unwrap_err();
        assert_eq!(error.to_string(), "image is not registered");
    }

    #[test]
    fn misuse_is_reported_by_finish() {
        let mut rig = rig(16, 16);
        type Misuse = fn(&mut D2dCanvas<'_>);
        let cases: [(&str, Misuse); 7] = [
            ("restore without save", |c| c.restore()),
            ("pop_clip without a clip", |c| c.pop_clip()),
            ("pop_layer without a layer", |c| c.pop_layer()),
            ("unbalanced save, clip or layer", |c| c.save()),
            ("unbalanced save, clip or layer", |c| {
                c.push_clip_rect(Rect::new(0.0, 0.0, 4.0, 4.0))
            }),
            ("scene is not registered", |c| {
                c.draw_scene(SceneId(1), Rect::ZERO)
            }),
            ("text layout was not created by DWriteEngine", |c| {
                let foreign = ssw_ui::text::MonoTextEngine::default().layout(
                    "x",
                    &TextStyle::new("A", 10.0),
                    &[],
                    &[],
                    100.0,
                );
                c.draw_text_layout(&*foreign, Point::ZERO, Color::BLACK);
            }),
        ];
        for (message, misuse) in cases {
            let error = rig.draw(None, misuse).unwrap_err();
            assert_eq!(error.to_string(), message);
        }
        // The first error wins and drawing continues afterwards.
        let error = rig
            .draw(None, |canvas| {
                canvas.restore();
                canvas.pop_clip();
                canvas.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), RED);
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "restore without save");
        let failed = TextStyle::new("Segoe UI", -1.0);
        let engine = rig.engine.clone();
        let layout = engine.layout("abc", &failed, &[], &[], 10.0);
        rig.draw(None, |canvas| {
            canvas.draw_text_layout(&*layout, Point::ZERO, RED)
        })
        .unwrap();
    }

    #[test]
    fn a_canvas_dropped_without_finish_leaves_the_target_balanced() {
        let mut rig = rig(16, 16);
        let Rig {
            target,
            assets,
            resources,
            ..
        } = &mut rig;
        // `EndDraw` fails while a clip or layer is still pushed.
        target
            .paint(Some(WHITE), |rt| {
                let mut canvas = D2dCanvas::new(rt, assets, resources);
                canvas.push_clip_rect(Rect::new(0.0, 0.0, 8.0, 8.0));
                canvas.push_clip_rounded_rect(
                    Rect::new(0.0, 0.0, 8.0, 8.0),
                    CornerRadii::uniform(3.0),
                );
                canvas.push_layer(0.5);
                drop(canvas);
                Ok(())
            })
            .unwrap();
        // The same when the drawing code panics halfway.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = target.paint(None, |rt| -> Result<(), WinError> {
                let mut canvas = D2dCanvas::new(rt, assets, resources);
                canvas.push_layer(0.5);
                panic!("drawing code failed");
            });
        }));
        assert!(outcome.is_err());
        let image = rig
            .draw(Some(WHITE), |canvas| {
                canvas.fill_rect(Rect::new(0.0, 0.0, 4.0, 4.0), RED)
            })
            .unwrap();
        assert_eq!(bgra(&image, 2, 2), [0, 0, 255, 255]);
    }

    #[test]
    fn rounded_rect_paths_are_closed_outlines() {
        let radii = CornerRadii::new(4.0, 0.0, 2.0, 100.0);
        let rect = Rect::new(0.0, 0.0, 20.0, 10.0);
        let path = rounded_rect_path(rect, radii);
        let verbs = path.verbs();
        // The radii are scaled to fit the rectangle; the outline starts after the top-left arc.
        let top_left = radii.clamped(rect.size()).top_left;
        assert!(top_left > 0.0 && top_left < 4.0, "{top_left}");
        assert_eq!(
            verbs.first(),
            Some(&PathVerb::MoveTo(Point::new(top_left, 0.0)))
        );
        assert_eq!(verbs.last(), Some(&PathVerb::Close));
        let arcs = verbs
            .iter()
            .filter(|verb| matches!(verb, PathVerb::ArcTo { .. }))
            .count();
        assert_eq!(arcs, 3, "the square corner has no arc");
        let square = rounded_rect_path(Rect::new(0.0, 0.0, 20.0, 10.0), CornerRadii::ZERO);
        assert!(
            !square
                .verbs()
                .iter()
                .any(|verb| matches!(verb, PathVerb::ArcTo { .. }))
        );
        assert_ne!(path_key(&path), path_key(&square));
        assert_eq!(path_key(&path), path_key(&path.clone()));
    }

    #[test]
    fn assets_resources_and_canvases_describe_their_state_when_debugged() {
        let mut rig = rig(16, 16);
        let empty = format!("{:?}", rig.assets);
        for expected in ["images: 0", "scenes: 0", "has_scene_renderer: false"] {
            assert!(empty.contains(expected), "{empty}");
        }
        rig.assets
            .images
            .register_bgra(ImageId(1), 1, 1, vec![0, 0, 0, 255])
            .unwrap();
        rig.assets.register_scene(
            SceneId(1),
            Arc::new(Scene::new(ssw_render::scene::Size::new(4.0, 4.0))),
        );
        let renderer = crate::scene_render::D2dSceneRenderer::new(rig.wic.clone()).unwrap();
        rig.assets.set_scene_renderer(Box::new(renderer));
        let filled = format!("{:?}", rig.assets);
        for expected in ["images: 1", "scenes: 1", "has_scene_renderer: true"] {
            assert!(filled.contains(expected), "{filled}");
        }

        let mut seen = Vec::new();
        let error = rig
            .draw(Some(WHITE), |canvas| {
                assert_eq!(canvas.transform(), Transform2D::IDENTITY);
                canvas.fill_rect(Rect::new(0.0, 0.0, 2.0, 2.0), RED);
                canvas.fill_path(&Path::from_rect(Rect::new(0.0, 0.0, 1.0, 1.0)), BLUE);
                canvas.push_clip_rect(Rect::new(0.0, 0.0, 4.0, 4.0));
                seen.push(format!("{canvas:?}"));
                canvas.pop_clip();
                canvas.translate(3.0, 4.0);
                let moved = canvas.transform();
                assert_eq!((moved.dx, moved.dy), (3.0, 4.0));
                // A restore without a save is the canvas' first error.
                canvas.restore();
                seen.push(format!("{canvas:?}"));
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "restore without save");
        let (clipped, failed) = (&seen[0], &seen[1]);
        for expected in ["stack: 1", "failed: false"] {
            assert!(clipped.contains(expected), "{clipped}");
        }
        for expected in ["stack: 0", "failed: true"] {
            assert!(failed.contains(expected), "{failed}");
        }
        let resources = format!("{:?}", rig.resources);
        for expected in ["generation: 1", "brushes: 2", "geometries: 1"] {
            assert!(resources.contains(expected), "{resources}");
        }
    }

    #[test]
    fn the_brush_and_geometry_caches_are_emptied_when_they_are_full() {
        let mut rig = rig(16, 16);
        rig.draw(Some(WHITE), |canvas| {
            for i in 0..=MAX_BRUSHES {
                let shade = Color::new(i as f32 / 1024.0, 0.0, 0.0, 1.0);
                canvas.fill_rect(Rect::new(0.0, 0.0, 1.0, 1.0), shade);
            }
        })
        .unwrap();
        assert_eq!(
            rig.resources.brush_count(),
            1,
            "the colour after the 512th found the cache full and emptied it"
        );
        rig.draw(Some(WHITE), |canvas| {
            for i in 0..=MAX_GEOMETRIES {
                let square = Rect::new(i as f32 * 0.01, 0.0, 1.0, 1.0);
                canvas.fill_path(&Path::from_rect(square), RED);
            }
        })
        .unwrap();
        assert_eq!(
            rig.resources.geometry_count(),
            1,
            "the shape after the 256th found the cache full and emptied it"
        );
    }

    #[test]
    fn dash_styles_are_created_once_per_pattern() {
        let mut rig = rig(32, 8);
        let image = rig
            .draw(Some(WHITE), |canvas| {
                for y in [1.5, 3.5] {
                    let (from, to) = (Point::new(0.0, y), Point::new(31.0, y));
                    canvas.stroke_line(from, to, RED, 1.0, Dash::Dashed);
                }
                let (from, to) = (Point::new(0.0, 6.5), Point::new(31.0, 6.5));
                canvas.stroke_line(from, to, RED, 1.0, Dash::Dotted);
            })
            .unwrap();
        assert_eq!(rig.resources.strokes.len(), 2, "dashed and dotted");
        let row = |y: u32| -> Vec<[u8; 4]> { (0..32).map(|x| bgra(&image, x, y)).collect() };
        assert_eq!(row(1), row(3), "the second dashed line reuses the style");
        assert!(row(1).contains(&[0, 0, 255, 255]));
        assert!(row(1).contains(&[255, 255, 255, 255]));
    }

    #[test]
    fn paths_fill_every_subpath_they_start() {
        let mut rig = rig(32, 12);
        // Two triangles; the first is left open when the second one starts.
        let two = Path::new()
            .move_to(Point::new(2.0, 2.0))
            .line_to(Point::new(10.0, 2.0))
            .line_to(Point::new(10.0, 10.0))
            .move_to(Point::new(20.0, 2.0))
            .line_to(Point::new(28.0, 2.0))
            .line_to(Point::new(28.0, 10.0));
        let image = rig
            .draw(Some(WHITE), |canvas| canvas.fill_path(&two, RED))
            .unwrap();
        assert_eq!(bgra(&image, 9, 4), [0, 0, 255, 255], "first triangle");
        assert_eq!(bgra(&image, 27, 4), [0, 0, 255, 255], "second triangle");
        assert_eq!(
            bgra(&image, 3, 9),
            [255, 255, 255, 255],
            "outside the first"
        );
        assert_eq!(
            bgra(&image, 22, 9),
            [255, 255, 255, 255],
            "outside the second"
        );
    }

    #[test]
    fn a_rejected_icon_size_is_reported_by_finish() {
        let mut rig = rig(16, 16);
        let error = rig
            .draw(None, |canvas| {
                canvas.draw_icon('\u{E921}', IconFont::SegoeFluent, 0.0, RED, Point::ZERO)
            })
            .unwrap_err();
        assert!(
            error.to_string().starts_with("create text format failed:"),
            "{error}"
        );
        assert!(rig.resources.icons.is_empty(), "failures are not cached");
    }

    /// Fault injection: every checked system call of an operation fails in turn.
    #[cfg(feature = "fault-injection")]
    mod faults {
        use super::*;
        use crate::fault;

        const AREA: Rect = Rect::new(2.0, 2.0, 20.0, 20.0);
        const SAME: CornerRadii = CornerRadii::uniform(4.0);
        const MIXED: CornerRadii = CornerRadii::new(6.0, 0.0, 6.0, 0.0);

        /// The operation a failed call names ("{context} failed: {source}").
        fn context(error: &WinError) -> String {
            let text = error.to_string();
            text.split(" failed: ").next().unwrap().to_owned()
        }

        /// Draws `operation` on canvases with empty caches, once per checked system call, with
        /// that call failing: the failure is what `finish` returns. Returns what the failures
        /// named, in call order; the draw after the last one is not affected.
        fn failures(rig: &mut Rig, operation: impl Fn(&mut D2dCanvas<'_>)) -> Vec<String> {
            let factory = rig.resources.factory.clone();
            let mut named = Vec::new();
            let mut n = 0;
            loop {
                rig.resources = TargetResources::new(factory.clone(), 1);
                let Rig {
                    target,
                    assets,
                    resources,
                    ..
                } = &mut *rig;
                let mut reached = false;
                let outcome = target.paint(Some(WHITE), |render_target| {
                    let mut canvas = D2dCanvas::new(render_target, assets, resources);
                    fault::arm(n);
                    operation(&mut canvas);
                    reached = fault::tripped();
                    fault::disarm();
                    canvas.finish()
                });
                if !reached {
                    outcome.unwrap();
                    return named;
                }
                named.push(context(&outcome.unwrap_err()));
                n += 1;
            }
        }

        #[test]
        fn every_failing_call_is_the_first_error_of_the_canvas() {
            let mut rig = rig(32, 32);
            rig.assets
                .images
                .register_bgra(ImageId(5), 2, 2, vec![255; 16])
                .unwrap();
            /// The calls that build a path geometry, then `rest`.
            fn geometry_then(rest: &[&'static str]) -> Vec<&'static str> {
                let build = [
                    "create path geometry",
                    "open geometry sink",
                    "close geometry sink",
                ];
                [&build[..], rest].concat()
            }
            type Operation = fn(&mut D2dCanvas<'_>);
            let cases: [(Operation, Vec<&str>); 13] = [
                (|c| c.fill_rect(AREA, RED), vec!["create brush"]),
                (
                    |c| c.fill_rounded_rect(AREA, SAME, RED),
                    vec!["create brush"],
                ),
                (
                    |c| c.fill_rounded_rect(AREA, MIXED, RED),
                    geometry_then(&["create brush"]),
                ),
                (
                    |c| c.stroke_rounded_rect(AREA, SAME, RED, 1.0),
                    vec!["create brush"],
                ),
                (
                    |c| c.stroke_rounded_rect(AREA, MIXED, RED, 1.0),
                    geometry_then(&["create brush"]),
                ),
                (
                    |c| {
                        let (from, to) = (Point::new(0.0, 5.5), Point::new(31.0, 5.5));
                        c.stroke_line(from, to, RED, 1.0, Dash::Dashed);
                    },
                    vec!["create brush", "create stroke style"],
                ),
                (
                    |c| c.fill_path(&Path::from_rect(AREA), RED),
                    geometry_then(&["create brush"]),
                ),
                (
                    |c| c.stroke_path(&Path::from_rect(AREA), RED, 1.0, Dash::Dashed),
                    geometry_then(&["create brush", "create stroke style"]),
                ),
                (
                    |c| {
                        c.concat_transform(Transform2D::rotation(0.5));
                        c.push_clip_rect(AREA);
                        c.pop_clip();
                    },
                    geometry_then(&["create layer"]),
                ),
                (
                    |c| {
                        c.push_clip_rounded_rect(AREA, SAME);
                        c.pop_clip();
                    },
                    geometry_then(&["create layer"]),
                ),
                (
                    |c| {
                        c.push_layer(0.5);
                        c.pop_layer();
                    },
                    vec!["create layer"],
                ),
                (
                    |c| c.draw_icon('\u{E921}', IconFont::SegoeFluent, 12.0, RED, Point::ZERO),
                    vec![
                        "create text format",
                        "icon layout",
                        "icon metrics",
                        "create brush",
                    ],
                ),
                (
                    |c| c.draw_image(ImageId(5), AREA, 1.0),
                    vec!["create bitmap"],
                ),
            ];
            for (index, (operation, expected)) in cases.into_iter().enumerate() {
                assert_eq!(failures(&mut rig, operation), expected, "operation {index}");
            }
        }

        #[test]
        fn failing_brushes_and_effects_while_drawing_text_are_reported() {
            let mut rig = rig(160, 40);
            let engine = rig.engine.clone();
            let ranges = [ssw_ui::text::StyledRange {
                range: 0..2,
                style: TextStyle::new("Segoe UI", 20.0)
                    .with_color(RED)
                    .with_background(Color::from_rgb_u32(0xFFFF00)),
            }];
            let layout = engine.layout(
                "MMMM",
                &TextStyle::new("Segoe UI", 20.0),
                &ranges,
                &[],
                f32::INFINITY,
            );
            let named = failures(&mut rig, |canvas| {
                canvas.draw_text_layout(&*layout, Point::new(4.0, 4.0), Color::BLACK);
            });
            // The background's brush, the default and the transparent brush, the reset of the
            // effects, the run's brush and its effect.
            assert_eq!(
                named,
                [
                    "create brush",
                    "create brush",
                    "create brush",
                    "set drawing effect",
                    "create brush",
                    "set drawing effect",
                ]
            );
        }
    }
}
