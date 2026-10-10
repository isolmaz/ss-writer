//! DirectWrite text layout: [`DWriteEngine`] implements [`TextLayoutEngine`].
//!
//! Every offset at the [`TextLayout`] API is a UTF-8 byte offset; DirectWrite works in UTF-16,
//! so each layout keeps a [`Utf16Map`] of its text. Styled ranges become DirectWrite range
//! properties; run colors and baseline shifts are applied as drawing effects at draw time (see
//! [`DWriteLayout::draw`]); backgrounds are computed as rectangles the canvas paints first.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use ssw_core::text::Utf16Map;
use ssw_ui::geometry::{Color, Point, Rect, Size};
use ssw_ui::text::{
    HitTest, InlineObject, LineHeight, LineMetrics, StyledRange, TextLayout, TextLayoutEngine,
    TextStyle,
};
use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::{
    D2D1_ANTIALIAS_MODE_ALIASED, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, ID2D1RenderTarget,
    ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_BREAK_CONDITION, DWRITE_BREAK_CONDITION_NEUTRAL, DWRITE_CLUSTER_METRICS,
    DWRITE_FACTORY_TYPE_ISOLATED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT, DWRITE_HIT_TEST_METRICS,
    DWRITE_INLINE_OBJECT_METRICS, DWRITE_LINE_METRICS, DWRITE_LINE_SPACING_METHOD_UNIFORM,
    DWRITE_OVERHANG_METRICS, DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_WORD_WRAPPING_NO_WRAP,
    DWriteCreateFactory, IDWriteFactory3, IDWriteFactory5, IDWriteFontCollection,
    IDWriteInMemoryFontFileLoader, IDWriteInlineObject, IDWriteInlineObject_Impl,
    IDWriteTextFormat, IDWriteTextLayout, IDWriteTextRenderer,
};
use windows::core::{BOOL, HRESULT, HSTRING, IUnknown, implement};
use windows_numerics::Vector2;

use crate::com::{OsResultExt, WinError};
use crate::fault::check;
use crate::fonts::{self, CASCADIA_MONO};

/// `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)`: what DirectWrite answers to a "count only"
/// probe whose output buffer is too small.
const E_NOT_SUFFICIENT_BUFFER: HRESULT = HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0);

/// Layout width used when the caller asks for no wrapping.
const UNBOUNDED_WIDTH: f32 = 1_000_000.0;
/// Layout height; text is never clipped vertically.
const LAYOUT_HEIGHT: f32 = 1_000_000.0;
/// Width added to a range rectangle that covers a hard line break, as a fraction of the base
/// font size.
const NEWLINE_PAD_EM: f32 = 0.35;
/// Tolerance when matching a DirectWrite rectangle to a line, in DIPs.
const LINE_EPSILON: f32 = 0.5;

/// Font bytes handed to the in-memory loader.
#[derive(Debug, Clone)]
pub enum FontBytes {
    /// Bytes compiled into the executable.
    Static(&'static [u8]),
    /// Bytes owned at runtime.
    Shared(Arc<[u8]>),
}

impl FontBytes {
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            FontBytes::Static(bytes) => bytes,
            FontBytes::Shared(bytes) => bytes,
        }
    }
}

struct EngineInner {
    factory: IDWriteFactory5,
    collection: IDWriteFontCollection,
    /// Keeps the loader registered with the factory alive.
    _loader: IDWriteInMemoryFontFileLoader,
    /// The loader does not copy font data: the bytes must outlive every layout.
    _font_data: Vec<FontBytes>,
    bundled: usize,
    resolved: RefCell<HashMap<String, String>>,
}

impl std::fmt::Debug for EngineInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DWriteEngine")
            .field("bundled_fonts", &self.bundled)
            .finish_non_exhaustive()
    }
}

impl EngineInner {
    fn has_family(&self, name: &str) -> bool {
        let mut index = 0u32;
        let mut exists = BOOL(0);
        // SAFETY: valid out pointers; the name is a valid HSTRING.
        let found = unsafe {
            self.collection
                .FindFamilyName(&HSTRING::from(name), &mut index, &mut exists)
        };
        found.is_ok() && exists.as_bool()
    }

    fn resolve(&self, requested: &str) -> String {
        if let Some(known) = self.resolved.borrow().get(requested) {
            return known.clone();
        }
        let resolved = fonts::resolve_family(requested, |name| self.has_family(name));
        self.resolved
            .borrow_mut()
            .insert(requested.to_owned(), resolved.clone());
        resolved
    }
}

/// Most layouts [`DWriteEngine`] keeps for requests that may come again.
const LAYOUT_CACHE_LIMIT: usize = 512;
/// Longest text (bytes) whose layout [`DWriteEngine`] keeps: a long block is the editor's to
/// cache, it would only push the short texts of the chrome out.
const LAYOUT_CACHE_MAX_TEXT: usize = 4096;

/// A request a cached layout answers, with the layout.
#[derive(Debug)]
struct CacheEntry {
    base: TextStyle,
    ranges: Vec<StyledRange>,
    objects: Vec<InlineObject>,
    width_bits: u32,
    /// The value of [`LayoutCache::clock`] when the entry was made or last used.
    used: u64,
    layout: DWriteLayout,
}

/// The layouts of the latest requests, by a hash of the whole request. The window chrome asks
/// for the same few dozen texts every frame; shaping them again each time costs milliseconds
/// per frame. Lives next to (not inside) [`EngineInner`]: the layouts keep that alive.
#[derive(Debug, Default)]
struct LayoutCache {
    entries: HashMap<u64, CacheEntry>,
    clock: u64,
}

fn hash_color(color: Option<Color>, hasher: &mut impl Hasher) {
    color
        .map(|c| [c.r.to_bits(), c.g.to_bits(), c.b.to_bits(), c.a.to_bits()])
        .hash(hasher);
}

fn hash_style(style: &TextStyle, hasher: &mut impl Hasher) {
    style.family.hash(hasher);
    style.size.to_bits().hash(hasher);
    style.weight.hash(hasher);
    (style.italic, style.underline, style.strikethrough).hash(hasher);
    hash_color(style.color, hasher);
    hash_color(style.background, hasher);
    style.baseline_shift.to_bits().hash(hasher);
    style
        .line_height
        .map(|height| match height {
            LineHeight::Multiple(factor) => (0u8, factor.to_bits()),
            LineHeight::Absolute(height) => (1u8, height.to_bits()),
        })
        .hash(hasher);
}

/// A hash of everything a layout depends on.
fn request_key(
    text: &str,
    base: &TextStyle,
    ranges: &[StyledRange],
    objects: &[InlineObject],
    max_width: f32,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hash_style(base, &mut hasher);
    ranges.len().hash(&mut hasher);
    for styled in ranges {
        (styled.range.start, styled.range.end).hash(&mut hasher);
        hash_style(&styled.style, &mut hasher);
    }
    objects.len().hash(&mut hasher);
    for object in objects {
        (object.at, object.id).hash(&mut hasher);
        (
            object.size.width.to_bits(),
            object.size.height.to_bits(),
            object.baseline.to_bits(),
        )
            .hash(&mut hasher);
    }
    max_width.to_bits().hash(&mut hasher);
    hasher.finish()
}

impl LayoutCache {
    /// The layout kept for exactly this request, if any.
    fn get(
        &mut self,
        key: u64,
        text: &str,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Option<DWriteLayout> {
        let entry = self.entries.get_mut(&key)?;
        let same = entry.layout.text == text
            && entry.base == *base
            && entry.ranges == ranges
            && entry.objects == objects
            && entry.width_bits == max_width.to_bits();
        if !same {
            return None;
        }
        self.clock += 1;
        entry.used = self.clock;
        Some(entry.layout.clone())
    }

    /// Keeps `layout`, the answer to this request (the oldest quarter makes room when full).
    fn insert(
        &mut self,
        key: u64,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
        layout: &DWriteLayout,
    ) {
        if layout.text.len() > LAYOUT_CACHE_MAX_TEXT {
            return;
        }
        if self.entries.len() >= LAYOUT_CACHE_LIMIT {
            let mut used: Vec<u64> = self.entries.values().map(|entry| entry.used).collect();
            used.sort_unstable();
            let cutoff = used[used.len() / 4];
            self.entries.retain(|_, entry| entry.used > cutoff);
        }
        self.clock += 1;
        self.entries.insert(
            key,
            CacheEntry {
                base: base.clone(),
                ranges: ranges.to_vec(),
                objects: objects.to_vec(),
                width_bits: max_width.to_bits(),
                used: self.clock,
                layout: layout.clone(),
            },
        );
    }
}

/// The DirectWrite [`TextLayoutEngine`]. Cloning is cheap and shares the font collection and
/// the layouts it kept (see [`DWriteEngine::cached_layouts`]).
#[derive(Debug, Clone)]
pub struct DWriteEngine {
    inner: Rc<EngineInner>,
    cache: Rc<RefCell<LayoutCache>>,
}

impl DWriteEngine {
    /// An engine whose collection is the system fonts plus the bundled Cascadia Mono.
    ///
    /// # Errors
    /// Fails when DirectWrite cannot be initialized.
    pub fn new() -> Result<Self, WinError> {
        Self::with_fonts(vec![FontBytes::Static(CASCADIA_MONO)])
    }

    /// An engine whose collection is the system fonts plus `fonts`. Font data DirectWrite
    /// rejects is skipped (see [`DWriteEngine::bundled_font_count`]).
    ///
    /// # Errors
    /// Fails when DirectWrite cannot be initialized.
    pub fn with_fonts(fonts: Vec<FontBytes>) -> Result<Self, WinError> {
        // SAFETY: all calls pass valid interface pointers; the in-memory loader reads from
        // `fonts`, which the engine keeps alive for as long as the collection can be used.
        unsafe {
            // An isolated factory: the loader registered below, and the font bytes it points
            // at, belong to this engine alone. On the process-wide shared factory every
            // engine (every test thread) would register another loader there, and that
            // loader would stay registered after the engine and its font bytes are gone.
            let factory: IDWriteFactory5 = check(DWriteCreateFactory(DWRITE_FACTORY_TYPE_ISOLATED))
                .ctx("create DirectWrite factory")?;
            let loader = check(factory.CreateInMemoryFontFileLoader())
                .ctx("create in-memory font loader")?;
            check(factory.RegisterFontFileLoader(&loader)).ctx("register in-memory font loader")?;
            let builder = check(factory.CreateFontSetBuilder()).ctx("create font set builder")?;
            let base: &IDWriteFactory3 = (&factory).into();
            let system = check(base.GetSystemFontSet()).ctx("system font set")?;
            check(builder.AddFontSet(&system)).ctx("add system fonts")?;
            let mut bundled = 0;
            for font in &fonts {
                let bytes = font.as_slice();
                // Font files cannot exceed 4 GiB (sfnt offsets are 32 bit): DirectWrite is
                // shown, and rejects, the first 4 GiB of a longer blob.
                let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                let file = loader.CreateInMemoryFontFileReference(
                    &factory,
                    bytes.as_ptr().cast::<c_void>(),
                    length,
                    None::<&IUnknown>,
                );
                let added = file.and_then(|file| builder.AddFontFile(&file));
                if added.is_ok() {
                    bundled += 1;
                }
            }
            let set = check(builder.CreateFontSet()).ctx("create font set")?;
            let collection: IDWriteFontCollection =
                check(base.CreateFontCollectionFromFontSet(&set))
                    .ctx("create font collection")?
                    .into();
            Ok(Self {
                inner: Rc::new(EngineInner {
                    factory,
                    collection,
                    _loader: loader,
                    _font_data: fonts,
                    bundled,
                    resolved: RefCell::new(HashMap::new()),
                }),
                cache: Rc::default(),
            })
        }
    }

    /// How many layouts the engine keeps for requests that may come again.
    #[must_use]
    pub fn cached_layouts(&self) -> usize {
        self.cache.borrow().entries.len()
    }

    /// The DirectWrite factory.
    #[must_use]
    pub fn factory(&self) -> &IDWriteFactory5 {
        &self.inner.factory
    }

    /// The collection of system fonts plus bundled fonts.
    #[must_use]
    pub fn collection(&self) -> &IDWriteFontCollection {
        &self.inner.collection
    }

    /// How many of the provided fonts DirectWrite accepted.
    #[must_use]
    pub fn bundled_font_count(&self) -> usize {
        self.inner.bundled
    }

    /// True when the collection has a family called `name`.
    #[must_use]
    pub fn has_family(&self, name: &str) -> bool {
        self.inner.has_family(name)
    }

    /// The installed family that stands in for `requested` (see [`fonts::resolve_family`]).
    #[must_use]
    pub fn resolve_family(&self, requested: &str) -> String {
        self.inner.resolve(requested)
    }

    /// A text format for a single-style run (icons, measuring).
    ///
    /// # Errors
    /// Fails when DirectWrite rejects the arguments.
    pub fn text_format(
        &self,
        family: &str,
        size: f32,
        weight: u16,
        italic: bool,
    ) -> Result<IDWriteTextFormat, WinError> {
        let family = HSTRING::from(self.resolve_family(family));
        // SAFETY: valid factory, collection and NUL-terminated names.
        unsafe {
            check(self.inner.factory.CreateTextFormat(
                &family,
                &self.inner.collection,
                DWRITE_FONT_WEIGHT(i32::from(weight)),
                if italic {
                    DWRITE_FONT_STYLE_ITALIC
                } else {
                    DWRITE_FONT_STYLE_NORMAL
                },
                DWRITE_FONT_STRETCH_NORMAL,
                size,
                &HSTRING::from("en-us"),
            ))
            .ctx("create text format")
        }
    }

    /// Lays out `text`; see [`TextLayoutEngine::layout`].
    ///
    /// # Errors
    /// Fails when DirectWrite rejects the style (for example a non-positive font size).
    pub fn create_layout(
        &self,
        text: &str,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Result<DWriteLayout, WinError> {
        DWriteLayout::build(self, text, base, ranges, objects, max_width)
    }
}

impl TextLayoutEngine for DWriteEngine {
    fn layout(
        &self,
        text: &str,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Box<dyn TextLayout> {
        let key = request_key(text, base, ranges, objects, max_width);
        let kept = self
            .cache
            .borrow_mut()
            .get(key, text, base, ranges, objects, max_width);
        if let Some(layout) = kept {
            return Box::new(layout);
        }
        match self.create_layout(text, base, ranges, objects, max_width) {
            Ok(layout) => {
                self.cache
                    .borrow_mut()
                    .insert(key, base, ranges, objects, max_width, &layout);
                Box::new(layout)
            }
            Err(_) => Box::new(FailedLayout::new(text)),
        }
    }
}

/// The layout returned when DirectWrite rejected the request: it keeps the text but has no
/// extent, so callers keep working and draw nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedLayout {
    text: String,
}

impl FailedLayout {
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
        }
    }
}

impl TextLayout for FailedLayout {
    fn text(&self) -> &str {
        &self.text
    }

    fn size(&self) -> Size {
        Size::ZERO
    }

    fn line_count(&self) -> usize {
        1
    }

    fn line_metrics(&self, index: usize) -> Option<LineMetrics> {
        (index == 0).then_some(LineMetrics {
            top: 0.0,
            height: 0.0,
            baseline: 0.0,
            start: 0,
            end: self.text.len(),
            newline_len: 0,
        })
    }

    fn hit_test_point(&self, _point: Point) -> HitTest {
        HitTest {
            offset: 0,
            trailing: false,
            inside: false,
            length: 0,
        }
    }

    fn caret_rect(&self, _offset: usize, _trailing: bool) -> Rect {
        Rect::ZERO
    }

    fn range_rects(&self, _range: Range<usize>) -> Vec<Rect> {
        Vec::new()
    }

    fn inline_object_rect(&self, _id: u64) -> Option<Rect> {
        None
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A DirectWrite inline object that only reserves space; the caller paints the object.
#[implement(IDWriteInlineObject)]
struct InlineSlot {
    width: f32,
    height: f32,
    baseline: f32,
}

#[allow(non_snake_case)]
impl IDWriteInlineObject_Impl for InlineSlot_Impl {
    fn Draw(
        &self,
        _clientdrawingcontext: *const c_void,
        _renderer: windows::core::Ref<'_, IDWriteTextRenderer>,
        _originx: f32,
        _originy: f32,
        _issideways: BOOL,
        _isrighttoleft: BOOL,
        _clientdrawingeffect: windows::core::Ref<'_, IUnknown>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn GetMetrics(&self) -> windows::core::Result<DWRITE_INLINE_OBJECT_METRICS> {
        Ok(DWRITE_INLINE_OBJECT_METRICS {
            width: self.width,
            height: self.height,
            baseline: self.baseline,
            supportsSideways: BOOL(0),
        })
    }

    fn GetOverhangMetrics(&self) -> windows::core::Result<DWRITE_OVERHANG_METRICS> {
        Ok(DWRITE_OVERHANG_METRICS::default())
    }

    fn GetBreakConditions(
        &self,
        breakconditionbefore: *mut DWRITE_BREAK_CONDITION,
        breakconditionafter: *mut DWRITE_BREAK_CONDITION,
    ) -> windows::core::Result<()> {
        // SAFETY: DirectWrite passes valid (or null) out pointers.
        unsafe {
            if let Some(before) = breakconditionbefore.as_mut() {
                *before = DWRITE_BREAK_CONDITION_NEUTRAL;
            }
            if let Some(after) = breakconditionafter.as_mut() {
                *after = DWRITE_BREAK_CONDITION_NEUTRAL;
            }
        }
        Ok(())
    }
}

/// An inline object whose placeholder is a cluster of the text.
#[derive(Debug, Clone, Copy)]
struct AcceptedObject {
    object: InlineObject,
    /// UTF-16 range of the placeholder cluster.
    at16: usize,
    end16: usize,
}

/// A run of text with the paint properties applied at draw time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct RunPaint {
    /// UTF-16 range.
    start: u32,
    length: u32,
    color: Option<Color>,
    shift: f32,
}

/// A DirectWrite layout with UTF-8 offsets. A clone shares the DirectWrite layout object: the
/// drawing effects [`DWriteLayout::draw`] sets are always set afresh before every pass.
#[derive(Clone)]
pub struct DWriteLayout {
    /// Keeps the font data and collection alive.
    _engine: Rc<EngineInner>,
    layout: IDWriteTextLayout,
    text: String,
    map: Utf16Map,
    size: Size,
    lines: Vec<LineMetrics>,
    runs: Vec<RunPaint>,
    backgrounds: Vec<(Rect, Color)>,
    objects: Vec<(u64, Rect)>,
    newline_pad: f32,
}

impl std::fmt::Debug for DWriteLayout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DWriteLayout")
            .field("text", &self.text)
            .field("size", &self.size)
            .field("lines", &self.lines.len())
            .finish_non_exhaustive()
    }
}

fn range16(start: usize, end: usize) -> DWRITE_TEXT_RANGE {
    DWRITE_TEXT_RANGE {
        startPosition: start as u32,
        length: (end - start) as u32,
    }
}

fn is_hard_break(rest: &str) -> bool {
    rest.starts_with(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
}

impl DWriteLayout {
    fn build(
        engine: &DWriteEngine,
        text: &str,
        base: &TextStyle,
        ranges: &[StyledRange],
        objects: &[InlineObject],
        max_width: f32,
    ) -> Result<Self, WinError> {
        let map = Utf16Map::new(text);
        let units: Vec<u16> = text.encode_utf16().collect();
        let unlimited = !max_width.is_finite();
        let width = if unlimited {
            UNBOUNDED_WIDTH
        } else {
            max_width.max(0.0)
        };
        let format = engine.text_format(&base.family, base.size, base.weight.0, base.italic)?;
        // SAFETY: all DirectWrite calls receive valid interface pointers, ranges inside the
        // UTF-16 text, and out buffers sized as the API requires.
        unsafe {
            if unlimited {
                check(format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP))
                    .ctx("disable word wrapping")?;
            }
            let layout = check(engine.factory().CreateTextLayout(
                &units,
                &format,
                width,
                LAYOUT_HEIGHT,
            ))
            .ctx("create text layout")?;
            let total = units.len();
            if total > 0 {
                if base.underline {
                    check(layout.SetUnderline(true, range16(0, total))).ctx("base underline")?;
                }
                if base.strikethrough {
                    check(layout.SetStrikethrough(true, range16(0, total)))
                        .ctx("base strikethrough")?;
                }
            }
            for styled in ranges {
                let start = map.byte_to_utf16_floor(styled.range.start);
                let end = map.byte_to_utf16_floor(styled.range.end);
                if start >= end {
                    continue;
                }
                let span = range16(start, end);
                let style = &styled.style;
                check(
                    layout.SetFontFamilyName(
                        &HSTRING::from(engine.resolve_family(&style.family)),
                        span,
                    ),
                )
                .ctx("range family")?;
                check(layout.SetFontSize(style.size, span)).ctx("range size")?;
                check(layout.SetFontWeight(DWRITE_FONT_WEIGHT(i32::from(style.weight.0)), span))
                    .ctx("range weight")?;
                check(layout.SetFontStyle(
                    if style.italic {
                        DWRITE_FONT_STYLE_ITALIC
                    } else {
                        DWRITE_FONT_STYLE_NORMAL
                    },
                    span,
                ))
                .ctx("range style")?;
                check(layout.SetUnderline(style.underline, span)).ctx("range underline")?;
                check(layout.SetStrikethrough(style.strikethrough, span))
                    .ctx("range strikethrough")?;
            }

            let cluster_starts = cluster_starts(&layout, total)?;
            let mut accepted: Vec<AcceptedObject> = Vec::new();
            for object in objects {
                let Some(at16) = map.byte_to_utf16(object.at) else {
                    continue;
                };
                let Ok(index) = cluster_starts.binary_search(&at16) else {
                    continue;
                };
                if at16 >= total {
                    continue;
                }
                let end16 = cluster_starts.get(index + 1).copied().unwrap_or(total);
                accepted.push(AcceptedObject {
                    object: *object,
                    at16,
                    end16,
                });
            }
            // The natural line metrics are read before the objects are set, so that objects
            // do not inflate them.
            if let Some(line_height) = base.line_height {
                set_uniform_line_height(&layout, base.size, line_height, &accepted)?;
            }
            for placed in &accepted {
                let slot: IDWriteInlineObject = InlineSlot {
                    width: placed.object.size.width,
                    height: placed.object.size.height,
                    baseline: placed.object.baseline,
                }
                .into();
                check(layout.SetInlineObject(&slot, range16(placed.at16, placed.end16)))
                    .ctx("set inline object")?;
            }

            let mut metrics = DWRITE_TEXT_METRICS::default();
            check(layout.GetMetrics(&mut metrics)).ctx("text metrics")?;
            let lines = read_lines(&layout, &map, total, metrics.height)?;

            let mut built = Self {
                _engine: Rc::clone(&engine.inner),
                layout,
                text: text.to_owned(),
                map,
                size: Size::new(metrics.width, metrics.height),
                lines,
                runs: Vec::new(),
                backgrounds: Vec::new(),
                objects: Vec::new(),
                newline_pad: base.size * NEWLINE_PAD_EM,
            };
            built.resolve_styles(base, ranges);
            built.place_objects(&accepted);
            Ok(built)
        }
    }

    /// Collapses the styled ranges (later ranges win) into paint runs and background
    /// rectangles.
    fn resolve_styles(&mut self, base: &TextStyle, ranges: &[StyledRange]) {
        #[derive(Clone, Copy, PartialEq)]
        struct Paint {
            color: Option<Color>,
            background: Option<Color>,
            shift: f32,
        }
        let total = self.map.utf16_len();
        let default = Paint {
            color: base.color,
            background: base.background,
            shift: base.baseline_shift,
        };
        let mut paints = vec![default; total];
        for styled in ranges {
            let start = self.map.byte_to_utf16_floor(styled.range.start);
            let end = self.map.byte_to_utf16_floor(styled.range.end);
            let paint = Paint {
                color: styled.style.color,
                background: styled.style.background,
                shift: styled.style.baseline_shift,
            };
            if start < end {
                paints[start..end].fill(paint);
            }
        }
        let mut start = 0;
        while start < total {
            let paint = paints[start];
            let mut end = start + 1;
            while end < total && paints[end] == paint {
                end += 1;
            }
            if paint.color.is_some() || paint.shift != 0.0 {
                self.runs.push(RunPaint {
                    start: start as u32,
                    length: (end - start) as u32,
                    color: paint.color,
                    shift: paint.shift,
                });
            }
            if let Some(background) = paint.background {
                let from = self.map.utf16_to_byte_floor(start);
                let to = self.map.utf16_to_byte_floor(end);
                for rect in self.range_rects(from..to) {
                    self.backgrounds.push((rect, background));
                }
            }
            start = end;
        }
    }

    fn place_objects(&mut self, accepted: &[AcceptedObject]) {
        for AcceptedObject { object, at16, .. } in accepted {
            let Some((x, y, _)) = self.position16(*at16, false) else {
                continue;
            };
            let line = self.lines[self.line_at(y)];
            self.objects.push((
                object.id,
                Rect::new(
                    x,
                    line.baseline - object.baseline,
                    object.size.width,
                    object.size.height,
                ),
            ));
        }
    }

    /// `(x, y, height)` of the caret at a UTF-16 position.
    fn position16(&self, position: usize, trailing: bool) -> Option<(f32, f32, f32)> {
        let (mut x, mut y) = (0.0f32, 0.0f32);
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: valid out pointers for the live layout.
        check(unsafe {
            self.layout
                .HitTestTextPosition(position as u32, trailing, &mut x, &mut y, &mut metrics)
        })
        .ok()?;
        Some((x, y, metrics.height))
    }

    /// Rectangles drawn behind the text: one per line of every run with a background.
    #[must_use]
    pub fn backgrounds(&self) -> &[(Rect, Color)] {
        &self.backgrounds
    }

    /// The underlying DirectWrite layout.
    #[must_use]
    pub fn raw(&self) -> &IDWriteTextLayout {
        &self.layout
    }

    fn set_effect(
        &self,
        brush: Option<&ID2D1SolidColorBrush>,
        start: u32,
        length: u32,
    ) -> Result<(), WinError> {
        let effect: Option<&IUnknown> = brush.map(Into::into);
        // SAFETY: valid layout; the range lies inside the text.
        check(unsafe {
            self.layout.SetDrawingEffect(
                effect,
                DWRITE_TEXT_RANGE {
                    startPosition: start,
                    length,
                },
            )
        })
        .ctx("set drawing effect")
    }

    /// Draws the text with its top-left at `origin` (backgrounds are the caller's job).
    ///
    /// Runs without a color use `default_color`. Runs with a baseline shift are drawn in an
    /// extra pass at the shifted origin, clipped to the run, while every other run is hidden
    /// with a transparent brush in that pass.
    ///
    /// # Errors
    /// Fails when a brush cannot be created or DirectWrite rejects an effect.
    pub fn draw(
        &self,
        target: &ID2D1RenderTarget,
        origin: Point,
        default_color: Color,
        brush_for: &mut dyn FnMut(Color) -> Result<ID2D1SolidColorBrush, WinError>,
    ) -> Result<(), WinError> {
        let default_brush = brush_for(default_color)?;
        if self.runs.is_empty() {
            self.draw_once(target, origin, &default_brush);
            return Ok(());
        }
        let hidden = brush_for(Color::TRANSPARENT)?;
        let mut shifts: Vec<f32> = Vec::new();
        for run in &self.runs {
            if run.shift != 0.0 && !shifts.contains(&run.shift) {
                shifts.push(run.shift);
            }
        }
        // Pass 0: unshifted runs at the origin.
        self.apply_runs(0.0, &hidden, brush_for)?;
        self.draw_once(target, origin, &default_brush);
        for shift in shifts {
            self.apply_runs(shift, &hidden, brush_for)?;
            let shifted = Point::new(origin.x, origin.y - shift);
            let mut bounds = Rect::ZERO;
            for run in self.runs.iter().filter(|run| run.shift == shift) {
                let from = self.map.utf16_to_byte_floor(run.start as usize);
                let to = self
                    .map
                    .utf16_to_byte_floor((run.start + run.length) as usize);
                for rect in self.range_rects(from..to) {
                    bounds = bounds.union(rect);
                }
            }
            let clip = D2D_RECT_F {
                left: shifted.x + bounds.left(),
                top: shifted.y + bounds.top(),
                right: shifted.x + bounds.right(),
                bottom: shifted.y + bounds.bottom(),
            };
            // SAFETY: balanced push/pop on the live render target.
            unsafe { target.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED) };
            self.draw_once(target, shifted, &default_brush);
            // SAFETY: pops the clip pushed above.
            unsafe { target.PopAxisAlignedClip() };
        }
        Ok(())
    }

    /// Sets the drawing effects so only runs of `shift` are visible.
    fn apply_runs(
        &self,
        shift: f32,
        hidden: &ID2D1SolidColorBrush,
        brush_for: &mut dyn FnMut(Color) -> Result<ID2D1SolidColorBrush, WinError>,
    ) -> Result<(), WinError> {
        self.set_effect(None, 0, self.map.utf16_len() as u32)?;
        for run in &self.runs {
            if run.shift == shift {
                if let Some(color) = run.color {
                    let brush = brush_for(color)?;
                    self.set_effect(Some(&brush), run.start, run.length)?;
                }
            } else {
                self.set_effect(Some(hidden), run.start, run.length)?;
            }
        }
        Ok(())
    }

    fn draw_once(&self, target: &ID2D1RenderTarget, origin: Point, brush: &ID2D1SolidColorBrush) {
        // SAFETY: valid target, layout and brush created for this target's device.
        unsafe {
            target.DrawTextLayout(
                Vector2 {
                    X: origin.x,
                    Y: origin.y,
                },
                &self.layout,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
        }
    }

    /// The line a vertical position belongs to: the last line that starts at most
    /// [`LINE_EPSILON`] below it (the first line for positions above the text). Every layout has
    /// at least one line.
    fn line_at(&self, y: f32) -> usize {
        self.lines
            .iter()
            .rposition(|line| line.top <= y + LINE_EPSILON)
            .unwrap_or(0)
    }

    /// The caret rectangle of a UTF-16 position: its x from DirectWrite, its vertical extent the
    /// box of the line it is on (the full, uniform line height).
    fn caret_at16(&self, position: usize, trailing: bool) -> Option<Rect> {
        let (x, y, height) = self.position16(position, trailing)?;
        let line = self.lines[self.line_at(y + height / 2.0)];
        Some(Rect::new(x, line.top, 0.0, line.height))
    }
}

/// UTF-16 start positions of every cluster plus the text length as a terminator.
///
/// # Safety
/// `layout` must be a valid layout of `total` UTF-16 units.
unsafe fn cluster_starts(layout: &IDWriteTextLayout, total: usize) -> Result<Vec<usize>, WinError> {
    let mut count = 0u32;
    // SAFETY: see the function contract; a null buffer asks only for the count.
    let probe = check(unsafe { layout.GetClusterMetrics(None, &mut count) });
    if let Err(error) = probe
        && error.code() != E_NOT_SUFFICIENT_BUFFER
    {
        return Err(WinError::from_os("count clusters", error));
    }
    let mut clusters = vec![DWRITE_CLUSTER_METRICS::default(); count as usize];
    // SAFETY: the buffer holds `count` entries.
    check(unsafe { layout.GetClusterMetrics(Some(&mut clusters), &mut count) })
        .ctx("cluster metrics")?;
    let mut starts = Vec::with_capacity(clusters.len() + 1);
    let mut position = 0usize;
    for cluster in clusters.iter().take(count as usize) {
        starts.push(position);
        position += usize::from(cluster.length);
    }
    debug_assert!(position <= total);
    starts.push(total);
    Ok(starts)
}

/// The raw DirectWrite metrics of every line of `layout`.
///
/// # Safety
/// `layout` must be a valid layout.
unsafe fn raw_lines(layout: &IDWriteTextLayout) -> Result<Vec<DWRITE_LINE_METRICS>, WinError> {
    let mut count = 0u32;
    // SAFETY: see the function contract; a null buffer asks only for the count.
    let probe = check(unsafe { layout.GetLineMetrics(None, &mut count) });
    if let Err(error) = probe
        && error.code() != E_NOT_SUFFICIENT_BUFFER
    {
        return Err(WinError::from_os("count lines", error));
    }
    let mut raw = vec![DWRITE_LINE_METRICS::default(); count as usize];
    // SAFETY: the buffer holds `count` entries.
    check(unsafe { layout.GetLineMetrics(Some(&mut raw), &mut count) }).ctx("line metrics")?;
    raw.truncate(count as usize);
    Ok(raw)
}

/// Makes every line of `layout` exactly `line_height` tall (`DWRITE_LINE_SPACING_METHOD_UNIFORM`).
///
/// The font's own ascent and descent (the largest over the layout's lines, read before any
/// inline object is set) sit centred in the line: half of the extra space goes above and half
/// below, like CSS `line-height`. The baseline is therefore `ascent + (height − ascent −
/// descent) / 2` from the top of the line. Uniform spacing never grows a line, so when an
/// inline object needs more room above or below the baseline than that, the spacing grows to
/// the object's extent instead (see [`LineHeight`]).
///
/// # Safety
/// `layout` must be a valid layout without inline objects.
unsafe fn set_uniform_line_height(
    layout: &IDWriteTextLayout,
    size: f32,
    line_height: LineHeight,
    objects: &[AcceptedObject],
) -> Result<(), WinError> {
    let height = line_height.resolve(size);
    // SAFETY: see the function contract.
    let natural = unsafe { raw_lines(layout) }?;
    let (ascent, descent) = natural
        .iter()
        .fold((0.0f32, 0.0f32), |(ascent, descent), line| {
            (
                ascent.max(line.baseline),
                descent.max(line.height - line.baseline),
            )
        });
    let leading = (height - ascent - descent) / 2.0;
    let (mut above, mut below) = (ascent + leading, descent + leading);
    for placed in objects {
        above = above.max(placed.object.baseline);
        below = below.max(placed.object.size.height - placed.object.baseline);
    }
    // SAFETY: a valid layout; the method and sizes are plain values.
    check(unsafe {
        layout.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, above + below, above)
    })
    .ctx("set line spacing")
}

/// The lines of `layout` (see [`convert_lines`]).
///
/// # Safety
/// `layout` must be a valid layout of `total` UTF-16 units.
unsafe fn read_lines(
    layout: &IDWriteTextLayout,
    map: &Utf16Map,
    total: usize,
    layout_height: f32,
) -> Result<Vec<LineMetrics>, WinError> {
    // SAFETY: see the function contract.
    let raw = unsafe { raw_lines(layout) }?;
    Ok(convert_lines(&raw, map, total, layout_height))
}

/// Converts DirectWrite line metrics to byte offsets, adding the empty last line after a
/// trailing hard break and the one line of an empty text when DirectWrite reports none, so a
/// layout always has a line to put a caret on.
fn convert_lines(
    raw: &[DWRITE_LINE_METRICS],
    map: &Utf16Map,
    total: usize,
    layout_height: f32,
) -> Vec<LineMetrics> {
    let mut lines = Vec::with_capacity(raw.len() + 1);
    let (mut top, mut start16) = (0.0f32, 0usize);
    for line in raw {
        let end16 = start16 + line.length as usize;
        let start = map.utf16_to_byte_floor(start16);
        let end = map.utf16_to_byte_floor(end16);
        let content_end = map.utf16_to_byte_floor(end16 - line.newlineLength as usize);
        lines.push(LineMetrics {
            top,
            height: line.height,
            baseline: top + line.baseline,
            start,
            end,
            newline_len: end - content_end,
        });
        top += line.height;
        start16 = end16;
    }
    if let Some(last) = lines.last().copied().filter(|line| line.newline_len > 0) {
        lines.push(LineMetrics {
            top: last.top + last.height,
            height: last.height,
            baseline: last.top + last.height + (last.baseline - last.top),
            start: last.end,
            end: last.end,
            newline_len: 0,
        });
    }
    if lines.is_empty() {
        lines.push(LineMetrics {
            top: 0.0,
            height: layout_height,
            baseline: layout_height,
            start: 0,
            end: map.utf16_to_byte_floor(total),
            newline_len: 0,
        });
    }
    lines
}

impl TextLayout for DWriteLayout {
    fn text(&self) -> &str {
        &self.text
    }

    fn size(&self) -> Size {
        self.size
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_metrics(&self, index: usize) -> Option<LineMetrics> {
        self.lines.get(index).copied()
    }

    fn hit_test_point(&self, point: Point) -> HitTest {
        let mut trailing = BOOL(0);
        let mut inside = BOOL(0);
        let mut metrics = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: valid out pointers for the live layout.
        let hit = check(unsafe {
            self.layout
                .HitTestPoint(point.x, point.y, &mut trailing, &mut inside, &mut metrics)
        });
        let inside = inside.as_bool();
        if hit.is_err() {
            return HitTest {
                offset: 0,
                trailing: false,
                inside: false,
                length: 0,
            };
        }
        let start = self.map.utf16_to_byte_floor(metrics.textPosition as usize);
        let end = self
            .map
            .utf16_to_byte_floor((metrics.textPosition + metrics.length) as usize);
        if start >= self.text.len() {
            return HitTest {
                offset: self.text.len(),
                trailing: false,
                inside,
                length: 0,
            };
        }
        // The caret never belongs after a hard line break (it would be on the next line).
        let on_break = is_hard_break(&self.text[start..]);
        HitTest {
            offset: start,
            trailing: trailing.as_bool() && !on_break,
            inside,
            length: end - start,
        }
    }

    fn caret_rect(&self, offset: usize, trailing: bool) -> Rect {
        let offset = offset.min(self.text.len());
        let last = self.lines.len() - 1;
        let at_end = |line: usize| {
            let line = self.lines[line];
            Rect::new(0.0, line.top, 0.0, line.height)
        };
        if self.text.is_empty() {
            return at_end(0);
        }
        if offset >= self.text.len() {
            // After the last cluster, or on the empty line after a trailing hard break.
            if self.lines[last].start == self.text.len() && self.lines[last].start > 0 {
                return at_end(last);
            }
            let tail = self
                .text
                .char_indices()
                .next_back()
                .map_or(0, |(index, _)| index);
            let position = self.map.byte_to_utf16_floor(tail);
            return self
                .caret_at16(position, true)
                .unwrap_or_else(|| at_end(last));
        }
        let position = self.map.byte_to_utf16_floor(offset);
        if trailing && is_hard_break(&self.text[offset..]) {
            let line = self.line_for_offset(offset);
            return at_end((line + 1).min(last));
        }
        self.caret_at16(position, trailing)
            .unwrap_or_else(|| at_end(0))
    }

    fn range_rects(&self, range: Range<usize>) -> Vec<Rect> {
        let len = self.text.len();
        let (start, end) = (range.start.min(len), range.end.min(len));
        if start >= end {
            return Vec::new();
        }
        let from = self.map.byte_to_utf16_floor(start);
        let to = self.map.byte_to_utf16_floor(end);
        if from >= to {
            return Vec::new();
        }
        let mut count = 0u32;
        // SAFETY: a null buffer asks only for the count.
        let probe = check(unsafe {
            self.layout.HitTestTextRange(
                from as u32,
                (to - from) as u32,
                0.0,
                0.0,
                None,
                &mut count,
            )
        });
        if let Err(error) = probe
            && error.code() != E_NOT_SUFFICIENT_BUFFER
        {
            return Vec::new();
        }
        let mut metrics = vec![DWRITE_HIT_TEST_METRICS::default(); count as usize];
        // SAFETY: the buffer holds `count` entries.
        let filled = check(unsafe {
            self.layout.HitTestTextRange(
                from as u32,
                (to - from) as u32,
                0.0,
                0.0,
                Some(&mut metrics),
                &mut count,
            )
        });
        if filled.is_err() {
            return Vec::new();
        }
        let mut per_line: Vec<(usize, Rect)> = Vec::new();
        for metric in metrics.iter().take(count as usize) {
            let rect = Rect::new(metric.left, metric.top, metric.width, metric.height);
            let line = self.line_at(metric.top + metric.height / 2.0);
            match per_line.iter_mut().find(|(index, _)| *index == line) {
                Some((_, existing)) => *existing = existing.union(rect),
                None => per_line.push((line, rect)),
            }
        }
        per_line.sort_by_key(|(line, _)| *line);
        per_line
            .into_iter()
            .map(|(line, rect)| {
                let metrics = self.lines[line];
                let mut rect = Rect::new(rect.x, metrics.top, rect.width, metrics.height);
                if metrics.newline_len > 0 && end > metrics.content_end() && start < metrics.end {
                    rect.width += self.newline_pad;
                }
                rect
            })
            .collect()
    }

    fn inline_object_rect(&self, id: u64) -> Option<Rect> {
        self.objects
            .iter()
            .find(|(object, _)| *object == id)
            .map(|(_, rect)| *rect)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;
    use crate::gfx::{WicTarget, d2d_factory};
    use crate::wic::Wic;
    use ssw_ui::text::FontWeight;
    use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
    use windows::Win32::Graphics::DirectWrite::DWRITE_BREAK_CONDITION_MUST_BREAK;
    use windows::core::Interface;

    fn engine() -> (ComGuard, DWriteEngine) {
        let com = ComGuard::apartment_threaded().unwrap();
        (com, DWriteEngine::new().unwrap())
    }

    fn style(size: f32) -> TextStyle {
        TextStyle::new("Segoe UI", size)
    }

    fn lay(engine: &DWriteEngine, text: &str, width: f32) -> Box<dyn TextLayout> {
        engine.layout(text, &style(16.0), &[], &[], width)
    }

    /// Byte offsets of the grapheme-ish clusters used by the round-trip tests.
    fn assert_round_trip(layout: &dyn TextLayout, starts: &[usize]) {
        let text = layout.text();
        for &offset in starts {
            let caret = layout.caret_rect(offset, false);
            let probe = Point::new(caret.x + 1.0, caret.y + caret.height / 2.0);
            let hit = layout.hit_test_point(probe);
            assert_eq!(hit.offset, offset, "offset {offset} in {text:?}");
            assert!(hit.inside);
            assert!(hit.length > 0);
        }
    }

    #[test]
    fn bundled_font_is_available_and_fallbacks_resolve() {
        let (_com, engine) = engine();
        assert_eq!(engine.bundled_font_count(), 1);
        assert!(engine.has_family("Cascadia Mono"));
        assert_eq!(engine.resolve_family("Cascadia Mono"), "Cascadia Mono");
        assert!(!engine.has_family("No Such Family 12345"));
        // An unknown family resolves to an installed stand-in.
        let stand_in = engine.resolve_family("No Such Family 12345");
        assert!(engine.has_family(&stand_in), "{stand_in}");
        let rejected =
            DWriteEngine::with_fonts(vec![FontBytes::Shared(Arc::from(&b"not a font"[..]))])
                .unwrap();
        assert_eq!(rejected.bundled_font_count(), 0);
    }

    #[test]
    fn ascii_layout_has_metrics_and_round_trips() {
        let (_com, engine) = engine();
        let layout = lay(&engine, "Hello world", f32::INFINITY);
        assert_eq!(layout.text(), "Hello world");
        assert_eq!(layout.line_count(), 1);
        assert!(layout.size().width > 40.0 && layout.size().height > 10.0);
        let line = layout.line_metrics(0).unwrap();
        assert_eq!((line.start, line.end, line.newline_len), (0, 11, 0));
        assert!(line.baseline > 0.0 && line.baseline < line.height);
        assert!(layout.line_metrics(1).is_none());
        assert_round_trip(&*layout, &[0, 1, 5, 6, 10]);
        let hit = layout.hit_test_point(Point::new(10_000.0, 5.0));
        assert_eq!((hit.offset, hit.trailing, hit.inside), (10, true, false));
        assert_eq!(layout.hit_test_point(Point::new(-50.0, 5.0)).offset, 0);
    }

    #[test]
    fn turkish_text_round_trips_through_utf16() {
        let (_com, engine) = engine();
        let text = "İıŞşĞğ çay";
        let layout = lay(&engine, text, f32::INFINITY);
        let starts: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
        assert_round_trip(&*layout, &starts[..starts.len() - 1]);
        let caret_end = layout.caret_rect(text.len(), false);
        assert!(caret_end.x > layout.caret_rect(starts[3], false).x);
        let hit = layout.hit_test_point(Point::new(1.0, 5.0));
        assert_eq!(hit.length, "İ".len());
    }

    #[test]
    fn emoji_and_cjk_use_whole_clusters() {
        let (_com, engine) = engine();
        let text = "a😀日本語b";
        let layout = lay(&engine, text, f32::INFINITY);
        let starts = [0, 1, 5, 8, 11];
        assert_round_trip(&*layout, &starts);
        let emoji = layout.hit_test_point(Point::new(
            layout.caret_rect(1, false).x + 2.0,
            layout.size().height / 2.0,
        ));
        assert_eq!((emoji.offset, emoji.length), (1, 4));
        // Offsets past the end clamp to the end of the text.
        assert_eq!(
            layout.caret_rect(999, false),
            layout.caret_rect(text.len(), false)
        );
    }

    #[test]
    fn combining_marks_stay_in_their_cluster() {
        let (_com, engine) = engine();
        let text = "e\u{301}x";
        let layout = lay(&engine, text, f32::INFINITY);
        let hit = layout.hit_test_point(Point::new(
            layout.caret_rect(0, false).x + 1.0,
            layout.size().height / 2.0,
        ));
        assert_eq!((hit.offset, hit.length), (0, 3));
        // A byte offset inside the cluster's second scalar maps to that scalar's position.
        let inside = layout.caret_rect(1, false);
        assert!(inside.x >= 0.0);
        let after = layout.caret_rect(0, true);
        assert!(after.x > layout.caret_rect(0, false).x);
        assert_eq!(after, layout.caret_rect(3, false));
    }

    #[test]
    fn hard_breaks_make_lines_and_an_empty_last_line() {
        let (_com, engine) = engine();
        let layout = lay(&engine, "ab\ncd\n", f32::INFINITY);
        assert_eq!(layout.line_count(), 3);
        let first = layout.line_metrics(0).unwrap();
        assert_eq!((first.start, first.end, first.newline_len), (0, 3, 1));
        assert_eq!(first.content_end(), 2);
        let last = layout.line_metrics(2).unwrap();
        assert_eq!((last.start, last.end, last.newline_len), (6, 6, 0));
        assert!(last.top > first.top);
        // The caret after a hard break is at the start of the next line.
        let after_break = layout.caret_rect(2, true);
        assert_eq!(
            (after_break.x, after_break.y),
            (0.0, first.top + first.height)
        );
        assert_eq!(layout.caret_rect(6, false).y, last.top);
        // Clicking right of a line that ends in a break lands before the break.
        let hit = layout.hit_test_point(Point::new(500.0, first.top + 2.0));
        assert_eq!((hit.offset, hit.trailing), (2, false));
        assert_eq!(layout.line_for_offset(4), 1);
        let empty_after_break = layout.hit_test_point(Point::new(5.0, last.top + 2.0));
        assert_eq!((empty_after_break.offset, empty_after_break.length), (6, 0));
    }

    #[test]
    fn empty_text_has_one_line() {
        let (_com, engine) = engine();
        let layout = lay(&engine, "", f32::INFINITY);
        assert_eq!(layout.line_count(), 1);
        let caret = layout.caret_rect(0, false);
        assert_eq!(caret.width, 0.0);
        assert!(caret.height > 0.0);
        assert!(layout.range_rects(0..0).is_empty());
        let hit = layout.hit_test_point(Point::new(3.0, 3.0));
        assert_eq!((hit.offset, hit.length), (0, 0));
    }

    #[test]
    fn wrapping_splits_lines_and_range_rects_follow() {
        let (_com, engine) = engine();
        let text = "alpha beta gamma delta epsilon";
        let layout = lay(&engine, text, 90.0);
        assert!(layout.line_count() >= 3);
        let rects = layout.range_rects(0..text.len());
        assert_eq!(rects.len(), layout.line_count());
        for pair in rects.windows(2) {
            assert!(pair[1].y > pair[0].y);
        }
        assert!(layout.range_rects(3..3).is_empty());
        assert!(layout.range_rects(50..60).is_empty());
        assert_eq!(layout.range_rects(0..999).len(), rects.len());
        let one = layout.range_rects(0..5);
        assert_eq!(one.len(), 1);
        assert!(one[0].width > 20.0);
        // The range-rect of a prefix ends where the caret of its end sits.
        let caret = layout.caret_rect(5, false);
        assert!((one[0].right() - caret.x).abs() < 1.5, "{one:?} {caret:?}");
    }

    #[test]
    fn range_over_a_break_is_padded() {
        let (_com, engine) = engine();
        let layout = lay(&engine, "ab\ncd", f32::INFINITY);
        let without = layout.range_rects(0..2);
        let with = layout.range_rects(0..3);
        assert_eq!(without.len(), 1);
        assert_eq!(with.len(), 1);
        assert!(with[0].width > without[0].width + 1.0);
        let two_lines = layout.range_rects(1..4);
        assert_eq!(two_lines.len(), 2);
    }

    #[test]
    fn styled_ranges_change_metrics_and_collect_backgrounds() {
        let (_com, engine) = engine();
        let big = StyledRange {
            range: 2..4,
            style: TextStyle::new("Segoe UI", 40.0)
                .with_weight(FontWeight::BOLD)
                .with_italic(true)
                .with_underline(true)
                .with_strikethrough(true)
                .with_background(Color::from_rgb_u32(0xFFFF00))
                .with_color(Color::from_rgb_u32(0xFF0000)),
        };
        let plain = lay(&engine, "abcdef", f32::INFINITY);
        let styled = engine.layout(
            "abcdef",
            &style(16.0).with_underline(true).with_strikethrough(true),
            &[
                big.clone(),
                StyledRange {
                    range: 3..3,
                    style: big.style.clone(),
                },
            ],
            &[],
            f32::INFINITY,
        );
        assert!(styled.size().width > plain.size().width + 10.0);
        assert!(styled.size().height > plain.size().height + 10.0);
        let concrete = engine
            .create_layout("abcdef", &style(16.0), &[big], &[], f32::INFINITY)
            .unwrap();
        assert_eq!(concrete.backgrounds().len(), 1);
        assert_eq!(concrete.backgrounds()[0].1, Color::from_rgb_u32(0xFFFF00));
        assert!(concrete.backgrounds()[0].0.width > 20.0);
        assert!(concrete.raw().cast::<IDWriteTextLayout>().is_ok());
    }

    #[test]
    fn inline_objects_reserve_space_and_report_rects() {
        let (_com, engine) = engine();
        let text = "a\u{FFFC}b";
        let object = InlineObject {
            at: 1,
            size: Size::new(30.0, 40.0),
            baseline: 30.0,
            id: 7,
        };
        // Without the object, U+FFFC is drawn as a glyph of its own; the reference is the text
        // without the placeholder: the object adds exactly its own width.
        let without = lay(&engine, "ab", f32::INFINITY);
        let layout = engine.layout(text, &style(16.0), &[], &[object], f32::INFINITY);
        let added = layout.size().width - without.size().width;
        assert!(
            (added - 30.0).abs() < 1.5,
            "the object reserves 30 DIPs, added {added}"
        );
        let rect = layout.inline_object_rect(7).unwrap();
        assert_eq!((rect.width, rect.height), (30.0, 40.0));
        let line = layout.line_metrics(0).unwrap();
        assert!((rect.y - (line.baseline - 30.0)).abs() < 0.01);
        assert!((rect.x - layout.caret_rect(1, false).x).abs() < 0.01);
        assert!(layout.size().height >= 40.0 - 30.0 + 30.0 - 0.01);
        assert!(layout.inline_object_rect(8).is_none());
        // Offsets that are not the start of a cluster are ignored.
        let ignored = engine.layout(
            "e\u{301}x",
            &style(16.0),
            &[],
            &[InlineObject {
                at: 1,
                size: Size::new(30.0, 10.0),
                baseline: 8.0,
                id: 1,
            }],
            f32::INFINITY,
        );
        assert!(ignored.inline_object_rect(1).is_none());
        let outside = engine.layout(
            "ab",
            &style(16.0),
            &[],
            &[InlineObject {
                at: 2,
                size: Size::new(30.0, 10.0),
                baseline: 8.0,
                id: 2,
            }],
            f32::INFINITY,
        );
        assert!(outside.inline_object_rect(2).is_none());
        let inside_scalar = engine.layout(
            "é",
            &style(16.0),
            &[],
            &[InlineObject {
                at: 1,
                size: Size::new(30.0, 10.0),
                baseline: 8.0,
                id: 3,
            }],
            f32::INFINITY,
        );
        assert!(inside_scalar.inline_object_rect(3).is_none());
    }

    #[test]
    fn rejected_requests_fall_back_to_an_empty_layout() {
        let (_com, engine) = engine();
        let failed = engine.layout("keep me", &style(-1.0), &[], &[], 100.0);
        assert_eq!(failed.text(), "keep me");
        assert_eq!(failed.size(), Size::ZERO);
        assert_eq!(failed.line_count(), 1);
        assert_eq!(failed.line_metrics(0).unwrap().end, 7);
        assert!(failed.line_metrics(1).is_none());
        assert_eq!(failed.hit_test_point(Point::new(1.0, 1.0)).length, 0);
        assert_eq!(failed.caret_rect(3, true), Rect::ZERO);
        assert!(failed.range_rects(0..3).is_empty());
        assert!(failed.inline_object_rect(1).is_none());
        assert!(failed.as_any().downcast_ref::<FailedLayout>().is_some());
        assert!(
            engine
                .create_layout("x", &style(-1.0), &[], &[], 10.0)
                .is_err()
        );
    }

    #[test]
    fn uniform_line_height_is_exact_and_centres_the_text() {
        let (_com, engine) = engine();
        let natural = lay(&engine, "a\nb\nc", f32::INFINITY);
        let natural_line = natural.line_metrics(0).unwrap();
        let tall = engine.layout(
            "a\nb\nc",
            &style(16.0).with_line_height(LineHeight::Multiple(1.75)),
            &[],
            &[],
            f32::INFINITY,
        );
        assert_eq!(tall.line_count(), 3);
        for index in 0..3 {
            let line = tall.line_metrics(index).unwrap();
            assert!((line.height - 28.0).abs() < 0.05, "{line:?}");
            assert!((line.top - 28.0 * index as f32).abs() < 0.2, "{line:?}");
        }
        let size = tall.size();
        assert!((size.height - 84.0).abs() < 0.2, "{size:?}");
        // Half of the extra space goes above the text.
        let expected = natural_line.baseline + (28.0 - natural_line.height) / 2.0;
        let line = tall.line_metrics(0).unwrap();
        assert!(
            (line.baseline - expected).abs() < 0.2,
            "{line:?} {expected}"
        );
        // Carets and selections span the whole line box.
        let caret = tall.caret_rect(2, false);
        assert!((caret.height - 28.0).abs() < 0.05 && (caret.y - 28.0).abs() < 0.2);
        let rects = tall.range_rects(0..5);
        assert_eq!(rects.len(), 3);
        assert!(rects.iter().all(|rect| (rect.height - 28.0).abs() < 0.05));
        // An absolute height.
        let fixed = engine.layout(
            "a\nb",
            &style(16.0).with_line_height(LineHeight::Absolute(40.0)),
            &[],
            &[],
            f32::INFINITY,
        );
        assert!((fixed.size().height - 80.0).abs() < 0.2);
        // An inline object taller than the line makes every line grow to it.
        let object = InlineObject {
            at: 1,
            size: Size::new(10.0, 50.0),
            baseline: 40.0,
            id: 1,
        };
        let grown = engine.layout(
            "a\u{FFFC}b\nc",
            &style(16.0).with_line_height(LineHeight::Multiple(1.75)),
            &[],
            &[object],
            f32::INFINITY,
        );
        assert!((grown.line_metrics(0).unwrap().height - 50.0).abs() < 0.2);
        assert!((grown.line_metrics(1).unwrap().height - 50.0).abs() < 0.2);
    }

    #[test]
    fn the_same_request_gets_the_kept_layout_and_any_change_makes_a_new_one() {
        let (_com, engine) = engine();
        assert_eq!(engine.cached_layouts(), 0);
        let first = lay(&engine, "Hello world", 90.0);
        assert_eq!(engine.cached_layouts(), 1);
        let again = lay(&engine, "Hello world", 90.0);
        assert_eq!(engine.cached_layouts(), 1, "answered from the cache");
        assert_eq!(again.text(), first.text());
        assert_eq!(again.size(), first.size());
        assert_eq!(again.line_count(), first.line_count());
        assert_eq!(again.range_rects(0..5), first.range_rects(0..5));
        // A clone of the engine shares what it kept.
        let shared = engine.clone();
        assert_eq!(shared.cached_layouts(), 1);
        let _ = lay(&shared, "Hello world", 90.0);
        assert_eq!(engine.cached_layouts(), 1);
        // Text, width, base style, ranges and objects each tell requests apart.
        let ranges = [StyledRange {
            range: 0..5,
            style: style(16.0).with_weight(ssw_ui::text::FontWeight::BOLD),
        }];
        let object = InlineObject {
            at: 0,
            size: Size::new(10.0, 10.0),
            baseline: 8.0,
            id: 1,
        };
        let base = style(16.0)
            .with_color(Color::BLACK)
            .with_background(Color::WHITE)
            .with_line_height(LineHeight::Multiple(1.5));
        let absolute = style(16.0).with_line_height(LineHeight::Absolute(30.0));
        let relative = style(16.0).with_line_height(LineHeight::Multiple(30.0));
        let plain = style(16.0);
        let ask = |text: &str,
                   base: &TextStyle,
                   ranges: &[StyledRange],
                   objects: &[InlineObject],
                   width: f32| {
            let _ = engine.layout(text, base, ranges, objects, width);
            engine.cached_layouts()
        };
        assert_eq!(ask("Hello there", &plain, &[], &[], 90.0), 2);
        assert_eq!(ask("Hello world", &plain, &[], &[], 91.0), 3);
        assert_eq!(ask("Hello world", &style(17.0), &[], &[], 90.0), 4);
        assert_eq!(ask("Hello world", &plain, &ranges, &[], 90.0), 5);
        assert_eq!(ask("Hello world", &plain, &[], &[object], 90.0), 6);
        assert_eq!(ask("Hello world", &base, &[], &[], 90.0), 7);
        assert_eq!(ask("Hello world", &absolute, &[], &[], 90.0), 8);
        assert_eq!(ask("Hello world", &relative, &[], &[], 90.0), 9);
        // Every one of them is answered from the cache the second time.
        assert_eq!(ask("Hello world", &relative, &[], &[], 90.0), 9);
        assert_eq!(ask("Hello world", &plain, &ranges, &[], 90.0), 9);
    }

    #[test]
    fn a_kept_layout_draws_as_often_as_it_is_asked_to() {
        let (_com, engine) = engine();
        let wic = Wic::new().unwrap();
        let factory = d2d_factory(false).unwrap();
        let ranges = [StyledRange {
            range: 0..1,
            style: style(20.0).with_color(RED),
        }];
        let draw_once = || {
            let layout = engine.layout("MMMM", &style(20.0), &ranges, &[], f32::INFINITY);
            let target = WicTarget::new(&factory, &wic, 80, 40, 96.0).unwrap();
            target
                .paint(Some(Color::WHITE), |render_target| {
                    let layout = layout.as_any().downcast_ref::<DWriteLayout>().unwrap();
                    let mut brushes = |color: Color| solid_brush(render_target, color);
                    layout.draw(
                        render_target,
                        Point::new(2.0, 2.0),
                        Color::BLACK,
                        &mut brushes,
                    )
                })
                .unwrap();
            reddish(&target)
        };
        let first = draw_once();
        assert!(first > 0);
        assert_eq!(engine.cached_layouts(), 1);
        assert_eq!(draw_once(), first, "the kept layout looks the same");
        assert_eq!(engine.cached_layouts(), 1);
    }

    #[test]
    fn failed_and_long_layouts_are_not_kept_and_the_oldest_make_room() {
        let (_com, engine) = engine();
        let _ = engine.layout("keep me", &style(-1.0), &[], &[], 100.0);
        assert_eq!(
            engine.cached_layouts(),
            0,
            "a failed request is asked again"
        );
        let long = "a".repeat(LAYOUT_CACHE_MAX_TEXT + 1);
        let _ = lay(&engine, &long, f32::INFINITY);
        assert_eq!(
            engine.cached_layouts(),
            0,
            "a long text is the caller's to keep"
        );
        let _ = lay(&engine, &"a".repeat(LAYOUT_CACHE_MAX_TEXT), f32::INFINITY);
        assert_eq!(engine.cached_layouts(), 1);

        // Fill the cache; `used` is the oldest request, `recent` was asked for again last.
        let width = f32::INFINITY;
        for index in 0..LAYOUT_CACHE_LIMIT - 1 {
            let _ = lay(&engine, &format!("t{index}"), width);
        }
        assert_eq!(engine.cached_layouts(), LAYOUT_CACHE_LIMIT);
        let _ = lay(&engine, "t0", width);
        let _ = lay(&engine, "overflow", width);
        // A quarter of the entries went, the one used again stayed.
        let kept = engine.cached_layouts();
        assert!(
            kept < LAYOUT_CACHE_LIMIT && kept > LAYOUT_CACHE_LIMIT / 2,
            "{kept}"
        );
        let _ = lay(&engine, "t0", width);
        assert_eq!(engine.cached_layouts(), kept, "t0 survived the eviction");
        let _ = lay(&engine, "t1", width);
        assert_eq!(engine.cached_layouts(), kept + 1, "t1 was among the oldest");
    }

    #[test]
    fn an_entry_answers_only_the_request_it_was_made_for() {
        let (_com, engine) = engine();
        let layout = engine
            .create_layout("Hello", &style(16.0), &[], &[], 90.0)
            .unwrap();
        let mut cache = LayoutCache::default();
        let key = request_key("Hello", &style(16.0), &[], &[], 90.0);
        cache.insert(key, &style(16.0), &[], &[], 90.0, &layout);
        // The same hash with another request (a collision) is a miss.
        assert!(
            cache
                .get(key, "Other", &style(16.0), &[], &[], 90.0)
                .is_none()
        );
        assert!(
            cache
                .get(key, "Hello", &style(16.0), &[], &[], 90.0)
                .is_some()
        );
        let other = key.wrapping_add(1);
        assert!(
            cache
                .get(other, "Hello", &style(16.0), &[], &[], 90.0)
                .is_none()
        );
    }

    #[test]
    fn text_formats_resolve_families() {
        let (_com, engine) = engine();
        let format = engine
            .text_format("Cascadia Mono", 13.0, 400, false)
            .unwrap();
        // SAFETY: plain getter on a live format.
        let size = unsafe { format.GetFontSize() };
        assert_eq!(size, 13.0);
        assert!(engine.text_format("Segoe UI", 0.0, 400, true).is_err());
        assert_eq!(FontBytes::Static(b"ab").as_slice(), b"ab");
    }

    const RED: Color = Color::from_rgb_u32(0xFF0000);
    const BLUE: Color = Color::from_rgb_u32(0x0000FF);

    #[test]
    fn engines_and_layouts_describe_their_state_when_debugged() {
        let (_com, engine) = engine();
        let text = format!("{engine:?}");
        assert!(text.contains("bundled_fonts: 1"), "{text}");
        let layout = engine
            .create_layout("a\nb", &style(16.0), &[], &[], f32::INFINITY)
            .unwrap();
        let text = format!("{layout:?}");
        assert!(text.contains(r#"text: "a\nb""#), "{text}");
        assert!(text.contains("lines: 2"), "{text}");
    }

    #[test]
    fn the_collection_holds_the_bundled_family_next_to_the_system_ones() {
        let (_com, engine) = engine();
        let collection = engine.collection();
        let mut index = 0u32;
        let mut exists = BOOL(0);
        // SAFETY: valid out pointers on a live collection.
        unsafe {
            collection.FindFamilyName(&HSTRING::from("Cascadia Mono"), &mut index, &mut exists)
        }
        .unwrap();
        assert!(exists.as_bool());
        // SAFETY: plain getter on a live collection.
        let families = unsafe { collection.GetFontFamilyCount() };
        assert!(families > 1, "{families} families");
        assert!(index < families);
    }

    #[test]
    fn inline_slots_have_no_overhang_and_accept_missing_break_outputs() {
        let slot: IDWriteInlineObject = InlineSlot {
            width: 12.0,
            height: 8.0,
            baseline: 6.0,
        }
        .into();
        // SAFETY: plain getters on a live inline object.
        let metrics = unsafe { slot.GetMetrics() }.unwrap();
        // SAFETY: as above.
        let overhang = unsafe { slot.GetOverhangMetrics() }.unwrap();
        assert_eq!(
            (metrics.width, metrics.height, metrics.baseline),
            (12.0, 8.0, 6.0)
        );
        assert_eq!(overhang, DWRITE_OVERHANG_METRICS::default());
        // Either output pointer, or both, may be null: only the given ones are written.
        let mut before = DWRITE_BREAK_CONDITION_MUST_BREAK;
        let mut after = DWRITE_BREAK_CONDITION_MUST_BREAK;
        // SAFETY: valid or null out pointers.
        unsafe { slot.GetBreakConditions(&mut before, std::ptr::null_mut()) }.unwrap();
        assert_eq!(
            (before, after),
            (
                DWRITE_BREAK_CONDITION_NEUTRAL,
                DWRITE_BREAK_CONDITION_MUST_BREAK
            )
        );
        // SAFETY: valid or null out pointers.
        unsafe { slot.GetBreakConditions(std::ptr::null_mut(), &mut after) }.unwrap();
        assert_eq!(after, DWRITE_BREAK_CONDITION_NEUTRAL);
        // SAFETY: valid or null out pointers.
        unsafe { slot.GetBreakConditions(std::ptr::null_mut(), std::ptr::null_mut()) }.unwrap();
    }

    #[test]
    fn line_metrics_gain_the_lines_directwrite_does_not_report() {
        let line = |length: u32, newline: u32| DWRITE_LINE_METRICS {
            length,
            newlineLength: newline,
            height: 20.0,
            baseline: 15.0,
            ..Default::default()
        };
        // Two lines that both end in a break: the empty line after the last one is added.
        let lines = convert_lines(
            &[line(3, 1), line(3, 1)],
            &Utf16Map::new("ab\ncd\n"),
            6,
            20.0,
        );
        assert_eq!(
            lines,
            [
                LineMetrics {
                    top: 0.0,
                    height: 20.0,
                    baseline: 15.0,
                    start: 0,
                    end: 3,
                    newline_len: 1,
                },
                LineMetrics {
                    top: 20.0,
                    height: 20.0,
                    baseline: 35.0,
                    start: 3,
                    end: 6,
                    newline_len: 1,
                },
                LineMetrics {
                    top: 40.0,
                    height: 20.0,
                    baseline: 55.0,
                    start: 6,
                    end: 6,
                    newline_len: 0,
                },
            ]
        );
        // A last line without a break is the end.
        let lines = convert_lines(&[line(3, 1), line(2, 0)], &Utf16Map::new("ab\ncd"), 5, 20.0);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[1].start, lines[1].end), (3, 5));
        // No lines at all: one empty line as tall as the layout.
        assert_eq!(
            convert_lines(&[], &Utf16Map::new(""), 0, 18.0),
            [LineMetrics {
                top: 0.0,
                height: 18.0,
                baseline: 18.0,
                start: 0,
                end: 0,
                newline_len: 0,
            }]
        );
    }

    #[test]
    fn positions_belong_to_the_line_that_starts_above_them() {
        let (_com, engine) = engine();
        let layout = engine
            .create_layout("a\nb\nc", &style(16.0), &[], &[], f32::INFINITY)
            .unwrap();
        let second = layout.lines[1];
        assert_eq!(layout.line_at(-40.0), 0, "above the text");
        assert_eq!(layout.line_at(second.top + 1.0), 1);
        assert_eq!(
            layout.line_at(second.top - 0.25),
            1,
            "within the tolerance of the next line's top"
        );
        assert_eq!(layout.line_at(1.0e6), 2, "below the text");
    }

    #[test]
    fn ranges_inside_a_scalar_are_empty_and_pieces_of_a_line_merge() {
        let (_com, engine) = engine();
        // Bytes 1..3 of the emoji (bytes 1..5) are inside one UTF-16 pair: no characters.
        let emoji = lay(&engine, "a😀", f32::INFINITY);
        assert!(emoji.range_rects(1..3).is_empty());
        // Left-to-right text around a right-to-left run is more than one box for DirectWrite,
        // formatting runs of one line are too: the range covers the line in a single rectangle.
        let text = "ab \u{5D0}\u{5D1} cd";
        let mixed = lay(&engine, text, f32::INFINITY);
        let rects = mixed.range_rects(0..text.len());
        assert_eq!(rects.len(), 1);
        assert!(rects[0].x.abs() < 1.5, "{rects:?}");
        assert!(
            (rects[0].right() - mixed.size().width).abs() < 1.5,
            "{rects:?}"
        );
        let big = StyledRange {
            range: 2..4,
            style: TextStyle::new("Segoe UI", 40.0).with_weight(FontWeight::BOLD),
        };
        let styled = engine.layout("abcdef", &style(16.0), &[big], &[], f32::INFINITY);
        let rects = styled.range_rects(0..6);
        assert_eq!(rects.len(), 1);
        assert!(rects[0].x.abs() < 1.5, "{rects:?}");
        assert!(
            (rects[0].right() - styled.size().width).abs() < 1.5,
            "{rects:?}"
        );
    }

    /// A layout with a colored run, a colored raised run and a raised run without a color.
    fn runs_layout(engine: &DWriteEngine) -> DWriteLayout {
        let ranges = [
            StyledRange {
                range: 0..1,
                style: style(16.0).with_color(RED),
            },
            StyledRange {
                range: 2..3,
                style: style(16.0).with_color(BLUE).with_baseline_shift(6.0),
            },
            StyledRange {
                range: 3..4,
                style: style(16.0).with_baseline_shift(6.0),
            },
        ];
        engine
            .create_layout("abcdef", &style(16.0), &ranges, &[], f32::INFINITY)
            .unwrap()
    }

    fn solid_brush(
        target: &ID2D1RenderTarget,
        color: Color,
    ) -> Result<ID2D1SolidColorBrush, WinError> {
        let d2d = D2D1_COLOR_F {
            r: color.r,
            g: color.g,
            b: color.b,
            a: color.a,
        };
        // SAFETY: valid target and color pointer.
        unsafe { target.CreateSolidColorBrush(&d2d, None) }.ctx("test brush")
    }

    /// Number of clearly red pixels of the target.
    fn reddish(target: &WicTarget) -> usize {
        let image = target.pixels().unwrap();
        (0..image.height)
            .flat_map(|y| (0..image.width).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let [b, g, r, _] = image.pixel(x, y).unwrap();
                r > g.saturating_add(60) && r > b.saturating_add(60)
            })
            .count()
    }

    #[test]
    fn a_refused_brush_aborts_the_draw_and_the_layout_draws_again() {
        let (_com, engine) = engine();
        let wic = Wic::new().unwrap();
        let factory = d2d_factory(false).unwrap();
        let target = WicTarget::new(&factory, &wic, 80, 40, 96.0).unwrap();
        let layout = runs_layout(&engine);
        let mut refused_call = 1;
        loop {
            let mut calls = 0;
            let outcome = target.paint(Some(Color::WHITE), |render_target| {
                layout.draw(
                    render_target,
                    Point::new(2.0, 2.0),
                    Color::BLACK,
                    &mut |color| {
                        calls += 1;
                        if calls == refused_call {
                            Err(WinError::Invalid("brush refused"))
                        } else {
                            solid_brush(render_target, color)
                        }
                    },
                )
            });
            if calls < refused_call {
                outcome.unwrap();
                break;
            }
            assert_eq!(outcome.unwrap_err().to_string(), "brush refused");
            refused_call += 1;
        }
        assert_eq!(
            refused_call, 5,
            "brushes: default, transparent, one per colored run and pass"
        );
        assert!(reddish(&target) > 0, "the red run is drawn");
    }

    /// Fault injection: every checked system call of an operation fails in turn.
    #[cfg(feature = "fault-injection")]
    mod faults {
        use super::*;
        use crate::fault;

        /// Runs `operation` with the checked call number `n` failing; whether it was reached.
        fn armed<R>(n: u32, operation: impl FnOnce() -> R) -> (R, bool) {
            fault::arm(n);
            let outcome = operation();
            let reached = fault::tripped();
            fault::disarm();
            (outcome, reached)
        }

        /// Fails the checked calls of `run` one after the other (`run(n)` makes call `n` fail
        /// and tells whether it got there), hands every failed outcome to `on_failure` and
        /// returns the outcome of the first run that was not affected.
        fn sweep<R>(mut run: impl FnMut(u32) -> (R, bool), mut on_failure: impl FnMut(R)) -> R {
            let mut n = 0;
            loop {
                let (outcome, reached) = run(n);
                if !reached {
                    return outcome;
                }
                on_failure(outcome);
                n += 1;
            }
        }

        /// The failed operation an OS error names ("{context} failed: {source}").
        fn context(error: &WinError) -> String {
            let text = error.to_string();
            text.split(" failed: ").next().unwrap().to_owned()
        }

        #[test]
        fn engine_creation_reports_every_failing_call() {
            let _com = ComGuard::apartment_threaded().unwrap();
            let mut contexts = Vec::new();
            let engine = sweep(
                |n| {
                    armed(n, || {
                        DWriteEngine::with_fonts(vec![FontBytes::Static(CASCADIA_MONO)])
                    })
                },
                |outcome| contexts.push(context(&outcome.unwrap_err())),
            );
            assert_eq!(
                contexts,
                [
                    "create DirectWrite factory",
                    "create in-memory font loader",
                    "register in-memory font loader",
                    "create font set builder",
                    "system font set",
                    "add system fonts",
                    "create font set",
                    "create font collection",
                ]
            );
            assert_eq!(engine.unwrap().bundled_font_count(), 1);
        }

        #[test]
        fn a_failing_text_format_is_reported() {
            let (_com, engine) = engine();
            let mut contexts = Vec::new();
            let format = sweep(
                |n| armed(n, || engine.text_format("Segoe UI", 12.0, 400, false)),
                |outcome| contexts.push(context(&outcome.unwrap_err())),
            );
            assert_eq!(contexts, ["create text format"]);
            assert!(format.is_ok());
        }

        #[test]
        fn a_failing_call_while_building_a_layout_is_reported_or_costs_only_its_geometry() {
            let (_com, engine) = engine();
            let base = style(16.0)
                .with_underline(true)
                .with_strikethrough(true)
                .with_line_height(LineHeight::Multiple(1.5));
            let ranges = [StyledRange {
                range: 1..3,
                style: TextStyle::new("Segoe UI", 20.0)
                    .with_weight(FontWeight::BOLD)
                    .with_italic(true)
                    .with_underline(true)
                    .with_strikethrough(true)
                    .with_background(Color::from_rgb_u32(0xFFFF00)),
            }];
            let objects = [InlineObject {
                at: 2,
                size: Size::new(10.0, 12.0),
                baseline: 9.0,
                id: 7,
            }];
            let mut contexts = Vec::new();
            let mut degraded = 0;
            let layout = sweep(
                |n| {
                    armed(n, || {
                        engine.create_layout("abcd", &base, &ranges, &objects, f32::INFINITY)
                    })
                },
                |outcome| match outcome {
                    Err(error) => contexts.push(context(&error)),
                    Ok(layout) => {
                        // A failed hit test only loses the rectangle derived from it.
                        assert!(
                            layout.backgrounds().is_empty()
                                != layout.inline_object_rect(7).is_none()
                        );
                        degraded += 1;
                    }
                },
            )
            .unwrap();
            assert_eq!(
                contexts,
                [
                    "create text format",
                    "disable word wrapping",
                    "create text layout",
                    "base underline",
                    "base strikethrough",
                    "range family",
                    "range size",
                    "range weight",
                    "range style",
                    "range underline",
                    "range strikethrough",
                    "count clusters",
                    "cluster metrics",
                    "count lines",
                    "line metrics",
                    "set line spacing",
                    "set inline object",
                    "text metrics",
                    "count lines",
                    "line metrics",
                ]
            );
            // The two calls of the background's range rectangles and the object's position.
            assert_eq!(degraded, 3);
            assert_eq!(layout.backgrounds().len(), 1);
            assert!(layout.inline_object_rect(7).is_some());
        }

        #[test]
        fn failing_hit_tests_fall_back_to_the_line_boxes() {
            let (_com, engine) = engine();
            let layout = engine
                .create_layout("ab\ncd", &style(16.0), &[], &[], f32::INFINITY)
                .unwrap();
            let (first, last) = (
                layout.line_metrics(0).unwrap(),
                layout.line_metrics(1).unwrap(),
            );
            // A caret inside the text falls back to the start of the first line, a caret after
            // the text to the start of the last line.
            let (caret, reached) = armed(0, || layout.caret_rect(1, false));
            assert!(reached);
            assert_eq!(caret, Rect::new(0.0, first.top, 0.0, first.height));
            let (caret, reached) = armed(0, || layout.caret_rect(5, false));
            assert!(reached);
            assert_eq!(caret, Rect::new(0.0, last.top, 0.0, last.height));
            let (hit, reached) = armed(0, || layout.hit_test_point(Point::new(3.0, 3.0)));
            assert!(reached);
            assert_eq!(
                hit,
                HitTest {
                    offset: 0,
                    trailing: false,
                    inside: false,
                    length: 0,
                }
            );
            // The count probe and the fill of the rectangles may each fail.
            let mut failures = 0;
            let rects = sweep(
                |n| armed(n, || layout.range_rects(0..2)),
                |rects| {
                    assert!(rects.is_empty());
                    failures += 1;
                },
            );
            assert_eq!(failures, 2);
            assert_eq!(rects.len(), 1);
        }

        #[test]
        fn failing_effects_abort_the_draw_and_failing_hit_tests_only_shrink_the_clip() {
            let (_com, engine) = engine();
            let wic = Wic::new().unwrap();
            let factory = d2d_factory(false).unwrap();
            let target = WicTarget::new(&factory, &wic, 80, 40, 96.0).unwrap();
            let layout = runs_layout(&engine);
            let (mut aborted, mut shrunk) = (0, 0);
            sweep(
                |n| {
                    let mut reached = false;
                    let outcome = target.paint(Some(Color::WHITE), |render_target| {
                        let (outcome, injected) = armed(n, || {
                            layout.draw(
                                render_target,
                                Point::new(2.0, 2.0),
                                Color::BLACK,
                                &mut |color| solid_brush(render_target, color),
                            )
                        });
                        reached = injected;
                        outcome
                    });
                    (outcome, reached)
                },
                |outcome| match outcome {
                    Err(error) => {
                        assert_eq!(context(&error), "set drawing effect");
                        aborted += 1;
                    }
                    Ok(()) => shrunk += 1,
                },
            )
            .unwrap();
            // Effects: one reset and one per run in the unshifted pass (4), a reset and the
            // runs that are hidden or colored in the raised pass (3); the raised runs' two
            // rectangle lookups of two calls each only shrink the clip.
            assert_eq!((aborted, shrunk), (7, 4));
            assert!(reddish(&target) > 0, "the layout still draws");
        }
    }
}
