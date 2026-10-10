//! Direct2D drawing of [`ssw_render::scene::Scene`]s.
//!
//! [`D2dSceneRenderer`] is the [`SceneRenderer`] the canvas draws math formulas, Mermaid
//! diagrams, SVG images and print pages with. Everything stays vector: paths become path
//! geometries, glyph runs become glyph outlines of the *embedded* font bytes (DirectWrite's
//! in-memory font loader, `GetGlyphRunOutline` + `FillGeometry`, so text is pixel-equivalent to
//! `typst-render`), groups become transforms plus layers. The renderer draws through
//! `ID2D1RenderTarget`, so one implementation serves a window, a WIC bitmap and a print page.
//!
//! # Caches
//!
//! - Font faces are device independent and cached per font file (address, length and face
//!   index of the shared bytes). The cache keeps the bytes alive and the in-memory loader holds
//!   its own reference, so DirectWrite never reads freed memory.
//! - Decoded images are cached per `Arc<ImageData>`. Their device bitmaps are re-validated at
//!   every use with `CreateSharedBitmap`, which Direct2D refuses for a bitmap that belongs to
//!   another device: after a device loss (or on another render target) the bitmap is simply
//!   created again from the decoded pixels. No cache entry is ever tied to the address of a
//!   render target.
//!
//! Geometries, brushes, layers and stroke styles are created per [`SceneRenderer::draw`] call
//! (stroke styles, layers and the solid brush are shared inside one call) and never outlive it.
//!
//! # What is not drawn
//!
//! An image whose bytes cannot be decoded is skipped (and counted, see
//! [`D2dSceneRenderer::skipped_image_count`]) instead of failing the whole scene. Gradient
//! stops are sorted and clamped to `0..=1`; a gradient with one stop, no extent or no stops is
//! drawn as its last color or not at all, as SVG specifies. Scene clips carry no fill rule and
//! are always non-zero.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Arc;

use ssw_render::scene::{
    Color, Dash, Fill, FillRule, FontData, Glyphs, GradientStop, Group, Image, ImageData, LineCap,
    LineJoin, LinearGradient, MIME_RGBA8, Paint, Path, PathVerb, Point, RadialGradient, Scene,
    SceneItem, SpreadMode, Stroke, Transform,
};
use ssw_ui::geometry::Rect;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_BEZIER_SEGMENT, D2D1_COLOR_F,
    D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED, D2D1_FIGURE_END_OPEN,
    D2D1_FILL_MODE_ALTERNATE, D2D1_FILL_MODE_WINDING, D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
    D2D1_BITMAP_PROPERTIES, D2D1_BRUSH_PROPERTIES, D2D1_CAP_STYLE, D2D1_CAP_STYLE_FLAT,
    D2D1_CAP_STYLE_ROUND, D2D1_CAP_STYLE_SQUARE, D2D1_DASH_STYLE_CUSTOM, D2D1_DASH_STYLE_SOLID,
    D2D1_EXTEND_MODE, D2D1_EXTEND_MODE_CLAMP, D2D1_EXTEND_MODE_MIRROR, D2D1_EXTEND_MODE_WRAP,
    D2D1_GAMMA_2_2, D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS, D2D1_LINE_JOIN_BEVEL,
    D2D1_LINE_JOIN_MITER_OR_BEVEL, D2D1_LINE_JOIN_ROUND, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES,
    D2D1_QUADRATIC_BEZIER_SEGMENT, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES,
    D2D1_STROKE_STYLE_PROPERTIES, ID2D1Bitmap, ID2D1Brush, ID2D1Factory, ID2D1Geometry,
    ID2D1GeometrySink, ID2D1GradientStopCollection, ID2D1Layer, ID2D1PathGeometry,
    ID2D1RenderTarget, ID2D1SolidColorBrush, ID2D1StrokeStyle,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_ISOLATED, DWRITE_FONT_FACE_TYPE_UNKNOWN, DWRITE_FONT_FILE_TYPE_UNKNOWN,
    DWRITE_FONT_SIMULATIONS_NONE, DWRITE_GLYPH_OFFSET, DWriteCreateFactory, IDWriteFactory5,
    IDWriteFontFace, IDWriteInMemoryFontFileLoader,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::core::{BOOL, IUnknown, Interface, implement};
use windows_numerics::{Matrix3x2, Vector2};

use crate::canvas::{CanvasAssets, ImageStore, SceneRenderer};
use crate::com::{OsResultExt, WinError};
use crate::dwrite::DWriteEngine;
use crate::fault::check;
use crate::wic::{DecodedImage, Wic};

/// Font faces kept before the cache is emptied.
const MAX_FACES: usize = 128;
/// Decoded images kept before the cache is emptied.
const MAX_IMAGES: usize = 64;
/// Decoded pixels kept before the image cache is emptied.
const MAX_IMAGE_BYTES: usize = 256 * 1024 * 1024;
/// Deepest group nesting that is drawn; deeper scenes are refused instead of overflowing the
/// stack of the UI thread.
const MAX_GROUP_DEPTH: usize = 256;
/// Miter limit of strokes whose limit is not a number (the SVG default).
const DEFAULT_MITER_LIMIT: f32 = 4.0;

/// Scene → Direct2D renderer; see the [module documentation](self).
#[derive(Debug)]
pub struct D2dSceneRenderer {
    wic: Wic,
    fonts: RefCell<FontCache>,
    images: RefCell<ImageCache>,
    skipped_images: Cell<usize>,
}

impl D2dSceneRenderer {
    /// A renderer decoding images with `wic`. COM must be initialized on the calling thread.
    ///
    /// # Errors
    /// Fails when DirectWrite cannot create its factory or the in-memory font loader.
    pub fn new(wic: Wic) -> Result<Self, WinError> {
        Ok(Self {
            wic,
            fonts: RefCell::new(FontCache::new()?),
            images: RefCell::new(ImageCache::default()),
            skipped_images: Cell::new(0),
        })
    }

    /// Number of font faces created so far and still cached.
    #[must_use]
    pub fn cached_font_count(&self) -> usize {
        self.fonts.borrow().faces.len()
    }

    /// Number of images (decoded or found undecodable) in the cache.
    #[must_use]
    pub fn cached_image_count(&self) -> usize {
        self.images.borrow().entries.len()
    }

    /// How many times an image was skipped because its bytes could not be decoded.
    #[must_use]
    pub fn skipped_image_count(&self) -> usize {
        self.skipped_images.get()
    }

    /// Forgets every cached font face, decoded image and device bitmap.
    pub fn clear_caches(&self) {
        self.fonts.borrow_mut().faces.clear();
        *self.images.borrow_mut() = ImageCache::default();
    }

    /// The Direct2D bitmap of `image` for `target`, `None` when the bytes cannot be decoded.
    fn bitmap(
        &self,
        target: &ID2D1RenderTarget,
        image: &Arc<ImageData>,
    ) -> Result<Option<ID2D1Bitmap>, WinError> {
        let mut cache = self.images.borrow_mut();
        let entry = cache.entry(image, || decode_image(&self.wic, image));
        let Some(pixels) = &entry.pixels else {
            return Ok(None);
        };
        if let Some(cached) = &entry.bitmap
            && let Some(shared) = share_bitmap(target, cached)
        {
            return Ok(Some(shared));
        }
        let bitmap = create_bitmap(target, pixels)?;
        entry.bitmap = Some(bitmap.clone());
        Ok(Some(bitmap))
    }
}

impl SceneRenderer for D2dSceneRenderer {
    /// Draws `scene`, scaled from its size to `dest`, below the target's current transform
    /// (which is restored afterwards). Nothing is drawn when the scene or `dest` has no area.
    /// Content outside the scene's size is not clipped. Failures of Direct2D calls abort the
    /// drawing; open layers are always closed.
    fn draw(&self, target: &ID2D1RenderTarget, scene: &Scene, dest: Rect) -> Result<(), WinError> {
        let Some(fit) = fit_transform(scene, dest) else {
            return Ok(());
        };
        // SAFETY: valid target; the matrix is a valid out pointer.
        let (factory, base) = unsafe {
            let factory = check(target.GetFactory()).ctx("scene render target factory")?;
            let mut base = Matrix3x2::identity();
            target.GetTransform(&mut base);
            (factory, base)
        };
        let root = fit.then(&from_matrix(&base));
        if !invertible(&root) {
            return Ok(());
        }
        let mut session = Session {
            renderer: self,
            target,
            factory,
            scene,
            solid: None,
            layers: Vec::new(),
            strokes: HashMap::new(),
        };
        let drawn = session.items(&scene.items, &root, 0);
        // SAFETY: valid target and matrix pointer.
        unsafe { target.SetTransform(&base) };
        drawn
    }
}

impl CanvasAssets {
    /// Assets like [`CanvasAssets::new`] whose scene renderer is a [`D2dSceneRenderer`] that
    /// decodes images with `wic`.
    ///
    /// # Errors
    /// Fails when the renderer cannot be created.
    pub fn with_scene_renderer(
        images: ImageStore,
        engine: DWriteEngine,
        wic: Wic,
    ) -> Result<Self, WinError> {
        let mut assets = Self::new(images, engine);
        assets.set_scene_renderer(Box::new(D2dSceneRenderer::new(wic)?));
        Ok(assets)
    }
}

/// Where the text baseline of `scene` lies when the scene is drawn into `dest`, in the
/// coordinates of `dest`; `None` when the scene has no baseline or no area.
#[must_use]
pub fn baseline_in(scene: &Scene, dest: Rect) -> Option<f32> {
    let baseline = scene.baseline?;
    let fit = fit_transform(scene, dest)?;
    Some(fit.apply(Point::new(0.0, baseline)).y)
}

/// The transform that maps the scene's own units into `dest`.
fn fit_transform(scene: &Scene, dest: Rect) -> Option<Transform> {
    let positive = |value: f32| value.is_finite() && value > 0.0;
    let valid = positive(scene.size.width)
        && positive(scene.size.height)
        && positive(dest.width)
        && positive(dest.height)
        && dest.x.is_finite()
        && dest.y.is_finite();
    valid.then(|| {
        Transform::new(
            dest.width / scene.size.width,
            0.0,
            0.0,
            dest.height / scene.size.height,
            dest.x,
            dest.y,
        )
    })
}

/// True when `transform` is finite and maps the plane onto a plane (something can be seen).
fn invertible(transform: &Transform) -> bool {
    let determinant = transform.determinant();
    determinant.is_finite() && determinant != 0.0
}

fn matrix(transform: &Transform) -> Matrix3x2 {
    Matrix3x2 {
        M11: transform.sx,
        M12: transform.ky,
        M21: transform.kx,
        M22: transform.sy,
        M31: transform.tx,
        M32: transform.ty,
    }
}

fn from_matrix(matrix: &Matrix3x2) -> Transform {
    Transform::new(
        matrix.M11, matrix.M12, matrix.M21, matrix.M22, matrix.M31, matrix.M32,
    )
}

fn vec2(point: Point) -> Vector2 {
    Vector2 {
        X: point.x,
        Y: point.y,
    }
}

fn color_f(color: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: f32::from(color.r) / 255.0,
        g: f32::from(color.g) / 255.0,
        b: f32::from(color.b) / 255.0,
        a: color.alpha_f32(),
    }
}

/// `value` limited to `0..=1`; not-a-number becomes 0.
fn unit(value: f32) -> f32 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn extend_mode(spread: SpreadMode) -> D2D1_EXTEND_MODE {
    match spread {
        SpreadMode::Pad => D2D1_EXTEND_MODE_CLAMP,
        SpreadMode::Reflect => D2D1_EXTEND_MODE_MIRROR,
        SpreadMode::Repeat => D2D1_EXTEND_MODE_WRAP,
    }
}

/// Direct2D gradient stops: positions clamped to `0..=1` and sorted (equal positions keep their
/// order, so hard color changes survive). With `inner > 0` the colors start on a circle of
/// `inner` times the end radius: the stops are squeezed into `inner..=1` and the first color is
/// extended to the center.
fn gradient_stops(stops: &[GradientStop], inner: f32) -> Vec<D2D1_GRADIENT_STOP> {
    let mut converted = Vec::with_capacity(stops.len() + 1);
    if inner > 0.0
        && let Some(first) = stops.first()
    {
        converted.push(D2D1_GRADIENT_STOP {
            position: 0.0,
            color: color_f(first.color),
        });
    }
    converted.extend(stops.iter().map(|stop| D2D1_GRADIENT_STOP {
        position: inner + unit(stop.offset) * (1.0 - inner),
        color: color_f(stop.color),
    }));
    converted.sort_by(|a, b| a.position.total_cmp(&b.position));
    converted
}

/// The dash lengths of a stroke in units of its width (Direct2D's convention) and the phase in
/// the same unit. Patterns that cannot be drawn (empty, negative or non-finite lengths, no
/// positive length) mean a solid line; an odd count is repeated once, as in SVG.
fn dash_pattern(dash: Option<&Dash>, width: f32) -> (Vec<f32>, f32) {
    let Some(dash) = dash else {
        return (Vec::new(), 0.0);
    };
    let drawable = !dash.array.is_empty()
        && dash.array.iter().all(|v| v.is_finite() && *v >= 0.0)
        && dash.array.iter().sum::<f32>() > 0.0;
    if !drawable {
        return (Vec::new(), 0.0);
    }
    let repeats = if dash.array.len() % 2 == 1 { 2 } else { 1 };
    let lengths = dash
        .array
        .iter()
        .cycle()
        .take(dash.array.len() * repeats)
        .map(|length| length / width)
        .collect();
    let phase = if dash.phase.is_finite() {
        dash.phase / width
    } else {
        0.0
    };
    (lengths, phase)
}

/// Premultiplied BGRA pixels of an image, `None` when the bytes are not a drawable image.
fn decode_image(wic: &Wic, image: &ImageData) -> Option<DecodedImage> {
    if image.mime != MIME_RGBA8 {
        return wic.decode(&image.bytes).ok();
    }
    // Wide arithmetic: the byte count of any `u32` x `u32` image cannot overflow, so a size no
    // buffer can match is refused by the comparison.
    let expected = u128::from(image.width) * u128::from(image.height) * 4;
    if expected == 0 || image.bytes.len() as u128 != expected {
        return None;
    }
    let mut bgra = Vec::with_capacity(image.bytes.len());
    for pixel in image.bytes.as_chunks::<4>().0 {
        let alpha = u32::from(pixel[3]);
        let premultiply = |channel: u8| ((u32::from(channel) * alpha + 127) / 255) as u8;
        bgra.extend_from_slice(&[
            premultiply(pixel[2]),
            premultiply(pixel[1]),
            premultiply(pixel[0]),
            pixel[3],
        ]);
    }
    Some(DecodedImage {
        width: image.width,
        height: image.height,
        bgra,
    })
}

fn create_bitmap(
    target: &ID2D1RenderTarget,
    image: &DecodedImage,
) -> Result<ID2D1Bitmap, WinError> {
    let properties = D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    };
    // SAFETY: the pixel buffer holds `stride * height` bytes and Direct2D copies it.
    check(unsafe {
        target.CreateBitmap(
            D2D_SIZE_U {
                width: image.width,
                height: image.height,
            },
            Some(image.bgra.as_ptr().cast::<c_void>()),
            image.stride(),
            &properties,
        )
    })
    .ctx("create scene bitmap")
}

/// A bitmap for `target` that shares the pixels of `bitmap`; `None` when `bitmap` was created
/// by another device (after a device loss) or another resource domain.
fn share_bitmap(target: &ID2D1RenderTarget, bitmap: &ID2D1Bitmap) -> Option<ID2D1Bitmap> {
    let mut shared: Option<ID2D1Bitmap> = None;
    // SAFETY: `bitmap` is a live `ID2D1Bitmap` and the IID names that interface; `shared` is a
    // valid out pointer.
    let created =
        unsafe { target.CreateSharedBitmap(&ID2D1Bitmap::IID, bitmap.as_raw(), None, &mut shared) };
    created.ok().and(shared)
}

/// Decoded images by identity of their shared allocation.
#[derive(Debug, Default)]
struct ImageCache {
    entries: HashMap<usize, CachedImage>,
    /// Decoded size of all entries.
    bytes: usize,
}

#[derive(Debug)]
struct CachedImage {
    /// Keeps the allocation alive: its address is the cache key and must not be reused.
    _data: Arc<ImageData>,
    /// `None` when the bytes cannot be decoded.
    pixels: Option<DecodedImage>,
    /// The bitmap of the render target that drew the image last.
    bitmap: Option<ID2D1Bitmap>,
}

impl ImageCache {
    /// The entry of `image`, decoding it with `decode` when it is new. A full cache is emptied
    /// first.
    fn entry(
        &mut self,
        image: &Arc<ImageData>,
        decode: impl FnOnce() -> Option<DecodedImage>,
    ) -> &mut CachedImage {
        let key = Arc::as_ptr(image) as usize;
        if !self.entries.contains_key(&key) {
            let size = (image.width as usize)
                .saturating_mul(image.height as usize)
                .saturating_mul(4);
            if self.entries.len() >= MAX_IMAGES || self.bytes.saturating_add(size) > MAX_IMAGE_BYTES
            {
                self.entries.clear();
                self.bytes = 0;
            }
            self.bytes += size;
        }
        self.entries.entry(key).or_insert_with(|| CachedImage {
            _data: Arc::clone(image),
            pixels: decode(),
            bitmap: None,
        })
    }
}

/// Owns the font bytes for DirectWrite: the in-memory loader holds a reference to this object
/// for as long as it serves the font file.
#[implement()]
struct FontOwner {
    _font: Arc<FontData>,
}

/// Address, length and face index of the shared font bytes.
type FontKey = (usize, usize, u32);

#[derive(Debug)]
struct CachedFace {
    face: IDWriteFontFace,
    /// Keeps the bytes (and so their address, the cache key) alive.
    _font: Arc<FontData>,
}

/// Font faces of embedded font files. Own isolated factory: the loader registered on it, and
/// the bytes it reads, belong to this renderer alone.
#[derive(Debug)]
struct FontCache {
    /// Dropped first: faces refer to files of the loader.
    faces: HashMap<FontKey, CachedFace>,
    loader: IDWriteInMemoryFontFileLoader,
    factory: IDWriteFactory5,
}

impl FontCache {
    fn new() -> Result<Self, WinError> {
        // SAFETY: plain factory and loader creation; the loader is registered with the factory
        // that created it.
        unsafe {
            let factory: IDWriteFactory5 = check(DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED))
                .ctx("create DirectWrite factory")?;
            let loader = check(factory.CreateInMemoryFontFileLoader())
                .ctx("create in-memory font loader")?;
            check(factory.RegisterFontFileLoader(&loader)).ctx("register in-memory font loader")?;
            Ok(Self {
                faces: HashMap::new(),
                loader,
                factory,
            })
        }
    }

    /// The DirectWrite face of `font`, created on first use.
    fn face(&mut self, font: &Arc<FontData>) -> Result<IDWriteFontFace, WinError> {
        let bytes = font.bytes();
        let key = (bytes.as_ptr() as usize, bytes.len(), font.index);
        if let Some(cached) = self.faces.get(&key) {
            return Ok(cached.face.clone());
        }
        // Font files cannot exceed 4 GiB (sfnt offsets are 32 bit): a longer blob is no font
        // and DirectWrite rejects its first 4 GiB.
        let length = bytes.len() as u32;
        let owner: IUnknown = FontOwner {
            _font: Arc::clone(font),
        }
        .into();
        // SAFETY: the factory and loader are live; the font bytes stay valid for every read
        // DirectWrite makes because `owner` (a clone of `font`) is held by the file reference;
        // the out pointers are valid locals.
        let face = unsafe {
            let file = check(self.loader.CreateInMemoryFontFileReference(
                &self.factory,
                bytes.as_ptr().cast::<c_void>(),
                length,
                &owner,
            ))
            .ctx("create font file reference")?;
            let mut supported = BOOL(0);
            let mut file_type = DWRITE_FONT_FILE_TYPE_UNKNOWN;
            let mut face_type = DWRITE_FONT_FACE_TYPE_UNKNOWN;
            let mut faces = 0u32;
            check(file.Analyze(
                &mut supported,
                &mut file_type,
                Some(&mut face_type),
                &mut faces,
            ))
            .ctx("analyze font file")?;
            if !supported.as_bool() {
                return Err(WinError::Invalid("DirectWrite does not support this font"));
            }
            check(self.factory.CreateFontFace(
                face_type,
                &[Some(file)],
                font.index,
                DWRITE_FONT_SIMULATIONS_NONE,
            ))
            .ctx("create font face")?
        };
        if self.faces.len() >= MAX_FACES {
            self.faces.clear();
        }
        self.faces.insert(
            key,
            CachedFace {
                face: face.clone(),
                _font: Arc::clone(font),
            },
        );
        Ok(face)
    }
}

/// What distinguishes the stroke styles of one draw call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct StrokeKey {
    cap: u8,
    join: u8,
    miter_limit: u32,
    dash_phase: u32,
    dashes: Vec<u32>,
}

/// State of one [`SceneRenderer::draw`] call.
struct Session<'a> {
    renderer: &'a D2dSceneRenderer,
    target: &'a ID2D1RenderTarget,
    factory: ID2D1Factory,
    scene: &'a Scene,
    solid: Option<ID2D1SolidColorBrush>,
    /// Layers that are not pushed at the moment.
    layers: Vec<ID2D1Layer>,
    strokes: HashMap<StrokeKey, ID2D1StrokeStyle>,
}

impl Session<'_> {
    /// Draws `items` below `current` (scene units → target DIPs).
    fn items(
        &mut self,
        items: &[SceneItem],
        current: &Transform,
        depth: usize,
    ) -> Result<(), WinError> {
        for item in items {
            match item {
                SceneItem::Group(group) => self.group(group, current, depth)?,
                SceneItem::Fill(fill) => self.fill(fill, current)?,
                SceneItem::Stroke(stroke) => self.stroke(stroke, current)?,
                SceneItem::Glyphs(run) => self.glyphs(run, current)?,
                SceneItem::Image(image) => self.image(image, current)?,
            }
        }
        Ok(())
    }

    fn set_transform(&self, transform: &Transform) {
        let m = matrix(transform);
        // SAFETY: valid target and matrix pointer.
        unsafe { self.target.SetTransform(&m) };
    }

    fn group(&mut self, group: &Group, parent: &Transform, depth: usize) -> Result<(), WinError> {
        if depth >= MAX_GROUP_DEPTH {
            return Err(WinError::Invalid("scene groups are nested too deeply"));
        }
        let current = group.transform.then(parent);
        // Nothing is visible with a fully transparent group, a flattened transform or an empty
        // clip.
        if group.items.is_empty()
            || group.opacity.is_nan()
            || group.opacity <= 0.0
            || !invertible(&current)
            || group.clip.as_ref().is_some_and(Path::is_empty)
        {
            return Ok(());
        }
        let opacity = group.opacity.min(1.0);
        let mask = match &group.clip {
            Some(path) => Some(self.mask(path, &current)?),
            None => None,
        };
        if mask.is_none() && opacity >= 1.0 {
            return self.items(&group.items, &current, depth + 1);
        }
        let layer = self.push_layer(mask, opacity)?;
        let drawn = self.items(&group.items, &current, depth + 1);
        // SAFETY: pops the layer pushed above.
        unsafe { self.target.PopLayer() };
        self.layers.push(layer);
        drawn
    }

    /// A clip as device-space geometry: the layer mask ignores the target transform, so the
    /// whole matrix is baked into the geometry.
    fn mask(&self, path: &Path, current: &Transform) -> Result<ID2D1Geometry, WinError> {
        let geometry = build_geometry(&self.factory, path, FillRule::NonZero)?;
        let m = matrix(current);
        // SAFETY: valid factory, geometry and matrix pointer.
        let transformed = check(unsafe { self.factory.CreateTransformedGeometry(&geometry, &m) })
            .ctx("create clip geometry")?;
        Ok(transformed.into())
    }

    fn push_layer(
        &mut self,
        mask: Option<ID2D1Geometry>,
        opacity: f32,
    ) -> Result<ID2D1Layer, WinError> {
        let layer = match self.layers.pop() {
            Some(layer) => layer,
            // SAFETY: valid target; `None` lets Direct2D size the layer.
            None => check(unsafe { self.target.CreateLayer(None) }).ctx("create layer")?,
        };
        let mut parameters = D2D1_LAYER_PARAMETERS {
            contentBounds: D2D_RECT_F {
                left: f32::MIN,
                top: f32::MIN,
                right: f32::MAX,
                bottom: f32::MAX,
            },
            geometricMask: ManuallyDrop::new(mask),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity,
            opacityBrush: ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        self.set_transform(&Transform::IDENTITY);
        // SAFETY: valid target, parameters and layer; the interfaces the parameters borrow are
        // released exactly once right after the call (the struct holds them in `ManuallyDrop`).
        unsafe {
            self.target.PushLayer(&parameters, &layer);
            ManuallyDrop::drop(&mut parameters.geometricMask);
            ManuallyDrop::drop(&mut parameters.opacityBrush);
        }
        Ok(layer)
    }

    fn fill(&mut self, fill: &Fill, current: &Transform) -> Result<(), WinError> {
        if fill.path.is_empty() {
            return Ok(());
        }
        let Some(brush) = self.brush(&fill.paint)? else {
            return Ok(());
        };
        let geometry = build_geometry(&self.factory, &fill.path, fill.rule)?;
        self.set_transform(current);
        // SAFETY: valid geometry and brush of this target.
        unsafe {
            self.target
                .FillGeometry(&geometry, &brush, None::<&ID2D1Brush>);
        }
        Ok(())
    }

    fn stroke(&mut self, stroke: &Stroke, current: &Transform) -> Result<(), WinError> {
        if stroke.path.is_empty() || !stroke.width.is_finite() || stroke.width <= 0.0 {
            return Ok(());
        }
        let Some(brush) = self.brush(&stroke.paint)? else {
            return Ok(());
        };
        let geometry = build_geometry(&self.factory, &stroke.path, FillRule::NonZero)?;
        let style = self.stroke_style(stroke)?;
        self.set_transform(current);
        // SAFETY: valid geometry, brush and stroke style.
        unsafe {
            self.target
                .DrawGeometry(&geometry, &brush, stroke.width, &style);
        }
        Ok(())
    }

    fn stroke_style(&mut self, stroke: &Stroke) -> Result<ID2D1StrokeStyle, WinError> {
        let (dashes, phase) = dash_pattern(stroke.dash.as_ref(), stroke.width);
        let miter_limit = if stroke.miter_limit.is_finite() {
            stroke.miter_limit.max(1.0)
        } else {
            DEFAULT_MITER_LIMIT
        };
        let key = StrokeKey {
            cap: stroke.cap as u8,
            join: stroke.join as u8,
            miter_limit: miter_limit.to_bits(),
            dash_phase: phase.to_bits(),
            dashes: dashes.iter().map(|length| length.to_bits()).collect(),
        };
        if let Some(style) = self.strokes.get(&key) {
            return Ok(style.clone());
        }
        let cap: D2D1_CAP_STYLE = match stroke.cap {
            LineCap::Butt => D2D1_CAP_STYLE_FLAT,
            LineCap::Round => D2D1_CAP_STYLE_ROUND,
            LineCap::Square => D2D1_CAP_STYLE_SQUARE,
        };
        let properties = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: cap,
            endCap: cap,
            dashCap: cap,
            // `MITER_OR_BEVEL` is the SVG/Typst miter: a bevel once the limit is exceeded
            // (plain `MITER` clips the tip instead).
            lineJoin: match stroke.join {
                LineJoin::Miter => D2D1_LINE_JOIN_MITER_OR_BEVEL,
                LineJoin::Round => D2D1_LINE_JOIN_ROUND,
                LineJoin::Bevel => D2D1_LINE_JOIN_BEVEL,
            },
            miterLimit: miter_limit,
            dashStyle: if dashes.is_empty() {
                D2D1_DASH_STYLE_SOLID
            } else {
                D2D1_DASH_STYLE_CUSTOM
            },
            dashOffset: phase,
        };
        // SAFETY: valid factory and properties; the dash array is only read during the call.
        let style = check(unsafe {
            self.factory.CreateStrokeStyle(
                &properties,
                if dashes.is_empty() {
                    None
                } else {
                    Some(&dashes)
                },
            )
        })
        .ctx("create stroke style")?;
        self.strokes.insert(key, style.clone());
        Ok(style)
    }

    fn glyphs(&mut self, run: &Glyphs, current: &Transform) -> Result<(), WinError> {
        if run.glyphs.is_empty() || !run.size.is_finite() || run.size <= 0.0 {
            return Ok(());
        }
        let font = self
            .scene
            .fonts
            .get(run.font.0)
            .ok_or(WinError::Invalid("a glyph run refers to a missing font"))?;
        let Some(brush) = self.brush(&run.paint)? else {
            return Ok(());
        };
        let face = self.renderer.fonts.borrow_mut().face(font)?;
        let geometry = self.outline(&face, run)?;
        self.set_transform(current);
        // SAFETY: valid geometry and brush of this target.
        unsafe {
            self.target
                .FillGeometry(&geometry, &brush, None::<&ID2D1Brush>);
        }
        Ok(())
    }

    /// The outlines of a glyph run as one geometry. Every glyph is placed with an offset from
    /// the run origin (advances are zero): the scene stores absolute glyph positions.
    fn outline(&self, face: &IDWriteFontFace, run: &Glyphs) -> Result<ID2D1PathGeometry, WinError> {
        let indices: Vec<u16> = run.glyphs.iter().map(|glyph| glyph.id).collect();
        let advances = vec![0.0f32; indices.len()];
        // `ascenderOffset` is positive upwards, the scene's `y` grows downwards.
        let offsets: Vec<DWRITE_GLYPH_OFFSET> = run
            .glyphs
            .iter()
            .map(|glyph| DWRITE_GLYPH_OFFSET {
                advanceOffset: glyph.x,
                ascenderOffset: -glyph.y,
            })
            .collect();
        // SAFETY: the arrays hold `indices.len()` elements each (a run cannot hold 2^32
        // glyphs; a truncated count would only read fewer); the sink is opened, filled by
        // DirectWrite and closed before the geometry is used.
        unsafe {
            let geometry = check(self.factory.CreatePathGeometry()).ctx("create glyph geometry")?;
            let sink = check(geometry.Open()).ctx("open glyph geometry")?;
            sink.SetFillMode(D2D1_FILL_MODE_WINDING);
            check(face.GetGlyphRunOutline(
                run.size,
                indices.as_ptr(),
                Some(advances.as_ptr()),
                Some(offsets.as_ptr()),
                indices.len() as u32,
                false,
                false,
                &sink,
            ))
            .ctx("get glyph outline")?;
            check(sink.Close()).ctx("close glyph geometry")?;
            Ok(geometry)
        }
    }

    fn image(&mut self, image: &Image, current: &Transform) -> Result<(), WinError> {
        let rect = image.rect;
        let positive = |value: f32| value.is_finite() && value > 0.0;
        if !positive(rect.width) || !positive(rect.height) {
            return Ok(());
        }
        let Some(bitmap) = self.renderer.bitmap(self.target, &image.data)? else {
            self.renderer
                .skipped_images
                .set(self.renderer.skipped_images.get() + 1);
            return Ok(());
        };
        let area = D2D_RECT_F {
            left: rect.x,
            top: rect.y,
            right: rect.right(),
            bottom: rect.bottom(),
        };
        self.set_transform(current);
        // SAFETY: valid bitmap created for (or shared with) this target.
        unsafe {
            self.target.DrawBitmap(
                &bitmap,
                Some(&area),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
        Ok(())
    }

    /// The brush of `paint`; `None` when it paints nothing (a gradient without stops).
    fn brush(&mut self, paint: &Paint) -> Result<Option<ID2D1Brush>, WinError> {
        match paint {
            Paint::Solid(color) => self.solid(*color).map(Some),
            Paint::Linear(gradient) => self.linear(gradient),
            Paint::Radial(gradient) => self.radial(gradient),
        }
    }

    /// The session's solid brush, set to `color`.
    fn solid(&mut self, color: Color) -> Result<ID2D1Brush, WinError> {
        let d2d = color_f(color);
        let brush = match &self.solid {
            Some(brush) => brush.clone(),
            None => {
                // SAFETY: valid target and color pointer.
                let brush = check(unsafe { self.target.CreateSolidColorBrush(&d2d, None) })
                    .ctx("create scene brush")?;
                self.solid = Some(brush.clone());
                brush
            }
        };
        // SAFETY: valid brush and color pointer.
        unsafe { brush.SetColor(&d2d) };
        Ok(brush.into())
    }

    fn linear(&mut self, gradient: &LinearGradient) -> Result<Option<ID2D1Brush>, WinError> {
        let Some(last) = gradient.stops.last() else {
            return Ok(None);
        };
        // SVG: one stop, or no distance between start and end, paint the last color.
        if gradient.stops.len() == 1 || gradient.start == gradient.end {
            return self.solid(last.color).map(Some);
        }
        let stops = gradient_stops(&gradient.stops, 0.0);
        let collection = self.stop_collection(&stops, gradient.spread)?;
        let properties = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: vec2(gradient.start),
            endPoint: vec2(gradient.end),
        };
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: 1.0,
            transform: matrix(&gradient.transform),
        };
        // SAFETY: valid target, property pointers and stop collection.
        let brush = check(unsafe {
            self.target
                .CreateLinearGradientBrush(&properties, Some(&brush_properties), &collection)
        })
        .ctx("create linear gradient")?;
        Ok(Some(brush.into()))
    }

    fn radial(&mut self, gradient: &RadialGradient) -> Result<Option<ID2D1Brush>, WinError> {
        let Some(last) = gradient.stops.last() else {
            return Ok(None);
        };
        let inner = if gradient.radius > 0.0 {
            gradient.focal_radius.max(0.0) / gradient.radius
        } else {
            1.0
        };
        // SVG: one stop, or an empty gradient circle, paint the last color. Direct2D has no
        // start circle: a focal radius is emulated by squeezing the stops (exact when the
        // circles are concentric).
        if gradient.stops.len() == 1
            || !gradient.radius.is_finite()
            || inner.is_nan()
            || inner >= 1.0
        {
            return self.solid(last.color).map(Some);
        }
        let stops = gradient_stops(&gradient.stops, inner);
        let collection = self.stop_collection(&stops, gradient.spread)?;
        let properties = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
            center: vec2(gradient.center),
            gradientOriginOffset: Vector2 {
                X: gradient.focal.x - gradient.center.x,
                Y: gradient.focal.y - gradient.center.y,
            },
            radiusX: gradient.radius,
            radiusY: gradient.radius,
        };
        let brush_properties = D2D1_BRUSH_PROPERTIES {
            opacity: 1.0,
            transform: matrix(&gradient.transform),
        };
        // SAFETY: valid target, property pointers and stop collection.
        let brush = check(unsafe {
            self.target
                .CreateRadialGradientBrush(&properties, Some(&brush_properties), &collection)
        })
        .ctx("create radial gradient")?;
        Ok(Some(brush.into()))
    }

    fn stop_collection(
        &self,
        stops: &[D2D1_GRADIENT_STOP],
        spread: SpreadMode,
    ) -> Result<ID2D1GradientStopCollection, WinError> {
        // SAFETY: valid target; the stops are copied by the call.
        check(unsafe {
            self.target
                .CreateGradientStopCollection(stops, D2D1_GAMMA_2_2, extend_mode(spread))
        })
        .ctx("create gradient stops")
    }
}

/// `path` as a Direct2D geometry. Drawing verbs without a preceding move start a figure at the
/// current point (the start of the subpath after a close), as in SVG.
fn build_geometry(
    factory: &ID2D1Factory,
    path: &Path,
    rule: FillRule,
) -> Result<ID2D1PathGeometry, WinError> {
    /// Starts a figure at `at` unless one is open.
    ///
    /// # Safety
    /// `sink` must be an open geometry sink.
    unsafe fn ensure_figure(sink: &ID2D1GeometrySink, open: &mut bool, at: Point) {
        if !*open {
            // SAFETY: see the function contract.
            unsafe { sink.BeginFigure(vec2(at), D2D1_FIGURE_BEGIN_FILLED) };
            *open = true;
        }
    }

    // SAFETY: the sink is opened, filled with balanced figures and closed before the geometry is
    // used.
    unsafe {
        let geometry = check(factory.CreatePathGeometry()).ctx("create path geometry")?;
        let sink = check(geometry.Open()).ctx("open geometry sink")?;
        sink.SetFillMode(match rule {
            FillRule::NonZero => D2D1_FILL_MODE_WINDING,
            FillRule::EvenOdd => D2D1_FILL_MODE_ALTERNATE,
        });
        let mut open = false;
        let mut start = Point::default();
        let mut current = Point::default();
        for verb in &path.verbs {
            match *verb {
                PathVerb::Move(to) => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    sink.BeginFigure(vec2(to), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                    start = to;
                    current = to;
                }
                PathVerb::Line(to) => {
                    ensure_figure(&sink, &mut open, current);
                    sink.AddLine(vec2(to));
                    current = to;
                }
                PathVerb::Quad(control, to) => {
                    ensure_figure(&sink, &mut open, current);
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: vec2(control),
                        point2: vec2(to),
                    });
                    current = to;
                }
                PathVerb::Cubic(control1, control2, to) => {
                    ensure_figure(&sink, &mut open, current);
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: vec2(control1),
                        point2: vec2(control2),
                        point3: vec2(to),
                    });
                    current = to;
                }
                PathVerb::Close => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    current = start;
                }
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        check(sink.Close()).ctx("close geometry sink")?;
        Ok(geometry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssw_render::scene::Size;

    fn stop(offset: f32, r: u8) -> GradientStop {
        GradientStop {
            offset,
            color: Color::rgb(r, 0, 0),
        }
    }

    fn scene_of(width: f32, height: f32) -> Scene {
        Scene::new(Size::new(width, height))
    }

    #[test]
    fn scene_is_fitted_into_the_destination() {
        let scene = scene_of(20.0, 10.0);
        let fit = fit_transform(&scene, Rect::new(5.0, 7.0, 40.0, 30.0)).unwrap();
        assert_eq!(fit, Transform::new(2.0, 0.0, 0.0, 3.0, 5.0, 7.0));
        for (scene_size, dest) in [
            ((0.0, 10.0), (1.0, 1.0)),
            ((20.0, f32::NAN), (1.0, 1.0)),
            ((20.0, 10.0), (0.0, 1.0)),
            ((20.0, 10.0), (1.0, -1.0)),
            ((20.0, 10.0), (f32::INFINITY, 1.0)),
        ] {
            let scene = scene_of(scene_size.0, scene_size.1);
            assert!(fit_transform(&scene, Rect::new(0.0, 0.0, dest.0, dest.1)).is_none());
        }
        assert!(fit_transform(&scene, Rect::new(f32::NAN, 0.0, 1.0, 1.0)).is_none());
    }

    #[test]
    fn baseline_is_scaled_into_the_destination() {
        let mut scene = scene_of(20.0, 10.0);
        let dest = Rect::new(0.0, 100.0, 40.0, 30.0);
        assert_eq!(baseline_in(&scene, dest), None);
        scene.baseline = Some(8.0);
        assert_eq!(baseline_in(&scene, dest), Some(124.0));
        assert_eq!(baseline_in(&scene, Rect::new(0.0, 0.0, 0.0, 5.0)), None);
    }

    #[test]
    fn flat_transforms_are_not_drawable() {
        assert!(invertible(&Transform::IDENTITY));
        assert!(!invertible(&Transform::scale(0.0, 1.0)));
        assert!(!invertible(&Transform::scale(f32::NAN, 1.0)));
    }

    #[test]
    fn gradient_stops_are_clamped_and_sorted_keeping_hard_edges() {
        let stops = gradient_stops(
            &[
                stop(0.8, 1),
                stop(f32::NAN, 2),
                stop(0.5, 3),
                stop(0.5, 4),
                stop(7.0, 5),
            ],
            0.0,
        );
        let positions: Vec<f32> = stops.iter().map(|s| s.position).collect();
        assert_eq!(positions, [0.0, 0.5, 0.5, 0.8, 1.0]);
        let reds: Vec<f32> = stops.iter().map(|s| s.color.r).collect();
        let expected: Vec<f32> = [2u8, 3, 4, 1, 5]
            .iter()
            .map(|r| f32::from(*r) / 255.0)
            .collect();
        assert_eq!(reds, expected);
    }

    #[test]
    fn focal_radius_squeezes_the_stops_and_extends_the_first_color() {
        let stops = gradient_stops(&[stop(0.0, 10), stop(1.0, 20)], 0.5);
        let positions: Vec<f32> = stops.iter().map(|s| s.position).collect();
        assert_eq!(positions, [0.0, 0.5, 1.0]);
        assert_eq!(stops[0].color.r, stops[1].color.r);
        assert!(stops[2].color.r > stops[1].color.r);
        assert!(gradient_stops(&[], 0.5).is_empty());
    }

    #[test]
    fn dashes_are_in_units_of_the_stroke_width() {
        let dash = Dash {
            array: vec![4.0, 2.0],
            phase: 1.0,
        };
        assert_eq!(dash_pattern(Some(&dash), 2.0), (vec![2.0, 1.0], 0.5));
        let odd = Dash {
            array: vec![3.0, 1.0, 2.0],
            phase: f32::NAN,
        };
        assert_eq!(
            dash_pattern(Some(&odd), 1.0),
            (vec![3.0, 1.0, 2.0, 3.0, 1.0, 2.0], 0.0)
        );
        assert_eq!(dash_pattern(None, 1.0), (Vec::new(), 0.0));
        for array in [vec![], vec![0.0, 0.0], vec![1.0, -1.0], vec![f32::NAN, 1.0]] {
            let dash = Dash { array, phase: 2.0 };
            assert_eq!(dash_pattern(Some(&dash), 1.0), (Vec::new(), 0.0));
        }
    }

    #[test]
    fn raw_images_are_swizzled_and_premultiplied() {
        // Declared first, so it drops after the WIC factory.
        let _com = crate::com::ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        // One half-transparent red pixel (straight RGBA).
        let image = ImageData::rgba8(1, 1, vec![255, 0, 0, 128]);
        let decoded = decode_image(&wic, &image).unwrap();
        assert_eq!(decoded.bgra, [0, 0, 128, 128]);
        // Wrong buffer length, zero size and undecodable bytes.
        assert!(decode_image(&wic, &ImageData::rgba8(2, 1, vec![0; 4])).is_none());
        assert!(decode_image(&wic, &ImageData::rgba8(0, 0, Vec::new())).is_none());
        // Sizes whose byte count does not fit a machine word are not images either (and do
        // not wrap around to a count that a buffer could match).
        let huge = ImageData::rgba8(u32::MAX, u32::MAX, vec![0; 4]);
        assert!(decode_image(&wic, &huge).is_none());
        assert!(decode_image(&wic, &ImageData::rgba8(u32::MAX, 1, vec![0; 4])).is_none());
        let mut text = ImageData::rgba8(1, 1, b"not an image".to_vec());
        text.mime = "image/png".to_owned();
        assert!(decode_image(&wic, &text).is_none());
    }
}
