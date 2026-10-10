//! DirectManipulation: touch and precision-touchpad panning with inertia.
//!
//! [`DirectManipulation`] wraps the manager, the viewport and the content of one window in
//! **manual-update mode** (the spike found the automatic mode stalls for windows that receive no
//! composition frames): the host calls [`DirectManipulation::update`] once per frame while a
//! contact is down or the viewport is moving, and the viewport's event handler hands the content
//! transform to a [`DmListener`] as scroll-offset and zoom callbacks. The COM handler only
//! queues events; the listener is called from `update`, on the caller's stack, never from inside
//! DirectManipulation.
//!
//! [`ScrollDriver`] is the integration the window uses: it combines DirectManipulation (finger
//! and touchpad contacts, inertia, rubber banding) with the platform-neutral
//! [`ssw_ui::scroll_physics::Scroller`] (animated wheel, programmatic scrolling) behind one
//! authoritative offset and performs the hand-off between the two.
//!
//! # Window wiring
//!
//! - `WM_POINTERDOWN` and `DM_POINTERHITTEST` -> [`ScrollDriver::on_pointer_message`].
//! - `WM_MOUSEWHEEL` -> [`ScrollDriver::on_wheel`].
//! - Every animation frame while [`ScrollFrame::more`] (or after any of the calls above reported
//!   a change) -> [`ScrollDriver::frame`].
//! - `WM_SETTINGCHANGE`, `WM_DPICHANGED`, resize and content changes ->
//!   [`ScrollDriver::refresh_system_settings`] and [`ScrollDriver::set_layout`].
//! - `WM_ACTIVATE` -> [`ScrollDriver::activate`] / [`ScrollDriver::deactivate`].
//!
//! The calling thread must be in a single-threaded apartment (the UI thread).

use std::ffi::c_void;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use ssw_ui::geometry::Size;
use ssw_ui::input::WheelEvent;
use ssw_ui::scroll_physics::{Extent, Scroller, WheelLines};
use windows::Win32::Foundation::{HWND, RECT, WPARAM};
use windows::Win32::Graphics::DirectManipulation::{
    DIRECTMANIPULATION_BUILDING, DIRECTMANIPULATION_CONFIGURATION,
    DIRECTMANIPULATION_CONFIGURATION_INTERACTION, DIRECTMANIPULATION_CONFIGURATION_RAILS_X,
    DIRECTMANIPULATION_CONFIGURATION_RAILS_Y, DIRECTMANIPULATION_CONFIGURATION_SCALING,
    DIRECTMANIPULATION_CONFIGURATION_SCALING_INERTIA,
    DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA,
    DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y, DIRECTMANIPULATION_DISABLED,
    DIRECTMANIPULATION_ENABLED, DIRECTMANIPULATION_INERTIA, DIRECTMANIPULATION_READY,
    DIRECTMANIPULATION_RUNNING, DIRECTMANIPULATION_STATUS, DIRECTMANIPULATION_SUSPENDED,
    DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE, DirectManipulationManager,
    IDirectManipulationContent, IDirectManipulationFrameInfoProvider, IDirectManipulationManager,
    IDirectManipulationPrimaryContent, IDirectManipulationUpdateManager,
    IDirectManipulationViewport, IDirectManipulationViewportEventHandler,
    IDirectManipulationViewportEventHandler_Impl,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Input::Pointer::{
    GetPointerInfo, GetPointerType, POINTER_FLAG_INCONTACT, POINTER_INFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DM_POINTERHITTEST, POINTER_INPUT_TYPE, PT_PEN, PT_TOUCH, PT_TOUCHPAD,
    SPI_GETCLIENTAREAANIMATION, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    SystemParametersInfoW, WM_POINTERDOWN,
};
use windows::core::{Ref, implement};

use crate::com::{OsResultExt, WinError};
use crate::dpi::dip_to_px;

/// Zoom factors closer than this are the same zoom.
const ZOOM_EPSILON: f64 = 1e-6;
/// Frames in which a viewport that was just told to animate is updated before it reports motion.
const ANIMATION_GRACE_FRAMES: u8 = 3;

/// The user's "Animation effects" setting (`SPI_GETCLIENTAREAANIMATION`); `true` when the query
/// fails (animations are the Windows default).
#[must_use]
pub fn client_area_animation() -> bool {
    let mut enabled: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL (4 bytes) through `pvparam`, which points
    // at `enabled` for the duration of the call. A failed query leaves the default in place.
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&raw mut enabled).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    enabled != 0
}

/// What one wheel notch scrolls (`SPI_GETWHEELSCROLLLINES`); three lines when the query fails.
#[must_use]
pub fn wheel_scroll_lines() -> WheelLines {
    let mut lines: u32 = 3;
    // SAFETY: SPI_GETWHEELSCROLLLINES writes one UINT (4 bytes) through `pvparam`, which points
    // at `lines` for the duration of the call. A failed query leaves the default in place.
    let _ = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            Some((&raw mut lines).cast::<c_void>()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    WheelLines::from_system(lines)
}

/// The state of a DirectManipulation viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmStatus {
    /// The viewport is being set up.
    Building,
    /// Enabled and waiting for input.
    Enabled,
    /// Disabled; it ignores input.
    Disabled,
    /// A contact is manipulating the content.
    Running,
    /// The contact was released and the content coasts on inertia.
    Inertia,
    /// The manipulation (and its inertia) is complete.
    Ready,
    /// Suspended by the system.
    Suspended,
    /// A status this build does not know.
    Unknown(i32),
}

impl DmStatus {
    /// The status for a raw `DIRECTMANIPULATION_STATUS`.
    #[must_use]
    pub fn from_raw(raw: DIRECTMANIPULATION_STATUS) -> Self {
        match raw {
            DIRECTMANIPULATION_BUILDING => DmStatus::Building,
            DIRECTMANIPULATION_ENABLED => DmStatus::Enabled,
            DIRECTMANIPULATION_DISABLED => DmStatus::Disabled,
            DIRECTMANIPULATION_RUNNING => DmStatus::Running,
            DIRECTMANIPULATION_INERTIA => DmStatus::Inertia,
            DIRECTMANIPULATION_READY => DmStatus::Ready,
            DIRECTMANIPULATION_SUSPENDED => DmStatus::Suspended,
            other => DmStatus::Unknown(other.0),
        }
    }

    /// True while the content moves on its own or under a contact (`Running`, `Inertia`): the
    /// host must keep calling [`DirectManipulation::update`].
    #[must_use]
    pub fn is_moving(self) -> bool {
        matches!(self, DmStatus::Running | DmStatus::Inertia)
    }
}

/// The content transform in DIPs: the content is scaled by `zoom` and shifted so that
/// `(offset_x, offset_y)` is the visible top-left corner (in zoomed DIPs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DmTransform {
    /// Cumulative zoom factor (1.0 = unscaled).
    pub zoom: f64,
    /// Horizontal scroll offset.
    pub offset_x: f64,
    /// Vertical scroll offset.
    pub offset_y: f64,
}

impl DmTransform {
    /// Converts DirectManipulation's `[sx, 0, 0, sy, tx, ty]` matrix (pixels; the content moves
    /// by `t`, so the offset is `-t`) into DIPs at `scale` pixels per DIP.
    #[must_use]
    pub fn from_matrix(matrix: &[f32; 6], scale: f32) -> Self {
        let scale = f64::from(scale);
        Self {
            zoom: f64::from(matrix[0]),
            offset_x: -f64::from(matrix[4]) / scale,
            offset_y: -f64::from(matrix[5]) / scale,
        }
    }
}

/// Receives what the viewport reports; called from [`DirectManipulation::update`] in the order
/// DirectManipulation produced it.
pub trait DmListener {
    /// The content moved: the scroll offset in DIPs (outside the content while rubber banding).
    fn scrolled(&mut self, offset_x: f64, offset_y: f64);

    /// The cumulative zoom factor changed (only with [`DmConfig::zoom`]). Called before the
    /// `scrolled` of the same update, whose offsets are in zoomed DIPs.
    fn zoomed(&mut self, zoom: f64);

    /// The viewport changed state.
    fn status_changed(&mut self, current: DmStatus, previous: DmStatus);
}

/// The zoom limits of a viewport that pinch-zooms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoomRange {
    /// Smallest zoom factor (> 0).
    pub min: f32,
    /// Largest zoom factor (>= `min`).
    pub max: f32,
}

/// How a viewport is configured.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DmConfig {
    /// Pinch zoom with these limits; `None` pans vertically only.
    pub zoom: Option<ZoomRange>,
}

/// Something DirectManipulation reported from inside a callback.
#[derive(Debug, Clone, Copy)]
enum Pending {
    Status {
        current: DmStatus,
        previous: DmStatus,
    },
    Content([f32; 6]),
}

/// The viewport event handler: queues status changes and content transforms for the UI-side
/// `update`. A closed queue (the viewport is being dropped) discards them.
#[implement(IDirectManipulationViewportEventHandler)]
struct Handler {
    events: Sender<Pending>,
}

impl IDirectManipulationViewportEventHandler_Impl for Handler_Impl {
    fn OnViewportStatusChanged(
        &self,
        _viewport: Ref<'_, IDirectManipulationViewport>,
        current: DIRECTMANIPULATION_STATUS,
        previous: DIRECTMANIPULATION_STATUS,
    ) -> windows::core::Result<()> {
        let _ = self.events.send(Pending::Status {
            current: DmStatus::from_raw(current),
            previous: DmStatus::from_raw(previous),
        });
        Ok(())
    }

    fn OnViewportUpdated(
        &self,
        _viewport: Ref<'_, IDirectManipulationViewport>,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnContentUpdated(
        &self,
        _viewport: Ref<'_, IDirectManipulationViewport>,
        content: Ref<'_, IDirectManipulationContent>,
    ) -> windows::core::Result<()> {
        let content = content.ok()?;
        let mut matrix = [0f32; 6];
        // SAFETY: `content` is the live content DirectManipulation passed to this callback and
        // `matrix` holds the six floats of the transform.
        unsafe { content.GetContentTransform(&mut matrix) }?;
        let _ = self.events.send(Pending::Content(matrix));
        Ok(())
    }
}

/// A DirectManipulation viewport over the client area of one window, in manual-update mode.
pub struct DirectManipulation {
    manager: IDirectManipulationManager,
    update_manager: IDirectManipulationUpdateManager,
    viewport: IDirectManipulationViewport,
    content: IDirectManipulationContent,
    events: Receiver<Pending>,
    cookie: u32,
    hwnd: HWND,
    /// Pixels per DIP.
    scale: f32,
    /// The last zoom handed to a listener.
    zoom: f64,
}

impl std::fmt::Debug for DirectManipulation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectManipulation")
            .field("hwnd", &self.hwnd)
            .field("scale", &self.scale)
            .field("zoom", &self.zoom)
            .finish_non_exhaustive()
    }
}

/// The pixel rectangle `0,0 .. width x height` DIPs at `scale` (at least 1 x 1).
fn rect_px(size: Size, scale: f32) -> RECT {
    RECT {
        left: 0,
        top: 0,
        right: dip_to_px(size.width, scale).max(1),
        bottom: dip_to_px(size.height, scale).max(1),
    }
}

impl DirectManipulation {
    /// Creates the viewport for `hwnd`: vertical panning with inertia (and pinch zoom when
    /// `config` asks for it), manual updates, `viewport` visible out of `content`, both in DIPs
    /// at `scale` pixels per DIP. The viewport is activated and enabled.
    ///
    /// # Errors
    /// Fails when DirectManipulation is unavailable, the thread is not in a single-threaded
    /// apartment, the zoom limits are invalid or any setup call fails.
    pub fn new(
        hwnd: HWND,
        config: DmConfig,
        viewport: Size,
        content: Size,
        scale: f32,
    ) -> Result<Self, WinError> {
        if let Some(zoom) = config.zoom
            && !(zoom.min > 0.0 && zoom.max >= zoom.min)
        {
            return Err(WinError::Invalid("invalid zoom range"));
        }
        // SAFETY: standard in-process COM activation; the calling thread's apartment is the
        // caller's responsibility (documented on the module).
        let manager: IDirectManipulationManager =
            unsafe { CoCreateInstance(&DirectManipulationManager, None, CLSCTX_INPROC_SERVER) }
                .ctx("create the DirectManipulation manager")?;
        // SAFETY: live manager and a window handle owned by the caller; every pointer argument
        // below is a reference to a local that outlives its call.
        let (update_manager, viewport_object, content_object, events, cookie) = unsafe {
            let update_manager: IDirectManipulationUpdateManager = manager
                .GetUpdateManager()
                .ctx("get the DirectManipulation update manager")?;
            let viewport_object: IDirectManipulationViewport = manager
                .CreateViewport(None::<&IDirectManipulationFrameInfoProvider>, hwnd)
                .ctx("create the DirectManipulation viewport")?;
            let mut configuration = DIRECTMANIPULATION_CONFIGURATION_INTERACTION.0
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_Y.0
                | DIRECTMANIPULATION_CONFIGURATION_TRANSLATION_INERTIA.0
                | DIRECTMANIPULATION_CONFIGURATION_RAILS_X.0
                | DIRECTMANIPULATION_CONFIGURATION_RAILS_Y.0;
            if config.zoom.is_some() {
                configuration |= DIRECTMANIPULATION_CONFIGURATION_SCALING.0
                    | DIRECTMANIPULATION_CONFIGURATION_SCALING_INERTIA.0;
            }
            viewport_object
                .ActivateConfiguration(DIRECTMANIPULATION_CONFIGURATION(configuration))
                .ctx("configure the DirectManipulation viewport")?;
            viewport_object
                .SetViewportOptions(DIRECTMANIPULATION_VIEWPORT_OPTIONS_MANUALUPDATE)
                .ctx("select manual updates")?;

            let (sender, events) = channel();
            let handler: IDirectManipulationViewportEventHandler =
                Handler { events: sender }.into();
            let cookie = viewport_object
                .AddEventHandler(Some(hwnd), &handler)
                .ctx("add the viewport event handler")?;

            viewport_object
                .SetViewportRect(&rect_px(viewport, scale))
                .ctx("set the viewport rectangle")?;
            manager
                .Activate(hwnd)
                .ctx("activate DirectManipulation for the window")?;
            viewport_object
                .Enable()
                .ctx("enable the DirectManipulation viewport")?;

            let content_object: IDirectManipulationContent = viewport_object
                .GetPrimaryContent()
                .ctx("get the primary content")?;
            content_object
                .SetContentRect(&rect_px(content, scale))
                .ctx("set the content rectangle")?;
            if let Some(zoom) = config.zoom {
                let primary: IDirectManipulationPrimaryContent = viewport_object
                    .GetPrimaryContent()
                    .ctx("get the primary content for zoom limits")?;
                primary
                    .SetZoomBoundaries(zoom.min, zoom.max)
                    .ctx("set the zoom limits")?;
            }
            (
                update_manager,
                viewport_object,
                content_object,
                events,
                cookie,
            )
        };
        Ok(Self {
            manager,
            update_manager,
            viewport: viewport_object,
            content: content_object,
            events,
            cookie,
            hwnd,
            scale,
            zoom: 1.0,
        })
    }

    /// Resizes the viewport and the content (DIPs at `scale` pixels per DIP). Call it on resize,
    /// DPI change and whenever the document height changes.
    ///
    /// # Errors
    /// Fails when DirectManipulation rejects a rectangle.
    pub fn set_geometry(
        &mut self,
        viewport: Size,
        content: Size,
        scale: f32,
    ) -> Result<(), WinError> {
        self.scale = scale;
        // SAFETY: live viewport and content; the rectangles are locals that outlive the calls.
        unsafe {
            self.viewport
                .SetViewportRect(&rect_px(viewport, scale))
                .ctx("set the viewport rectangle")?;
            self.content
                .SetContentRect(&rect_px(content, scale))
                .ctx("set the content rectangle")
        }
    }

    /// The viewport rectangle DirectManipulation uses, in pixels.
    ///
    /// # Errors
    /// Fails when the query fails.
    pub fn viewport_rect(&self) -> Result<RECT, WinError> {
        // SAFETY: plain getter on a live viewport.
        unsafe { self.viewport.GetViewportRect() }.ctx("get the viewport rectangle")
    }

    /// The content rectangle DirectManipulation uses, in pixels.
    ///
    /// # Errors
    /// Fails when the query fails.
    pub fn content_rect(&self) -> Result<RECT, WinError> {
        // SAFETY: plain getter on live content.
        unsafe { self.content.GetContentRect() }.ctx("get the content rectangle")
    }

    /// Hands a pointer to the viewport (`SetContact`). Call it inside `WM_POINTERDOWN` for touch
    /// and pen and inside `DM_POINTERHITTEST` for a precision touchpad, before the contact moves.
    ///
    /// # Errors
    /// Fails when the pointer id is unknown (`ERROR_NOT_FOUND`) or already finished.
    pub fn set_contact(&self, pointer_id: u32) -> Result<(), WinError> {
        // SAFETY: plain call on a live viewport.
        unsafe { self.viewport.SetContact(pointer_id) }.ctx("set the manipulation contact")
    }

    /// One manipulation frame: advances the contact and the inertia, then delivers what the
    /// viewport reported to `listener` and returns the viewport's status.
    ///
    /// # Errors
    /// Fails when DirectManipulation fails to update or to report its status.
    pub fn update(&mut self, listener: &mut dyn DmListener) -> Result<DmStatus, WinError> {
        // SAFETY: plain call on the live update manager; `None` selects the default frame info.
        unsafe {
            self.update_manager
                .Update(None::<&IDirectManipulationFrameInfoProvider>)
        }
        .ctx("update DirectManipulation")?;
        while let Ok(pending) = self.events.try_recv() {
            match pending {
                Pending::Status { current, previous } => listener.status_changed(current, previous),
                Pending::Content(matrix) => {
                    let transform = DmTransform::from_matrix(&matrix, self.scale);
                    if (transform.zoom - self.zoom).abs() > ZOOM_EPSILON {
                        self.zoom = transform.zoom;
                        listener.zoomed(transform.zoom);
                    }
                    listener.scrolled(transform.offset_x, transform.offset_y);
                }
            }
        }
        self.status()
    }

    /// The viewport's current status.
    ///
    /// # Errors
    /// Fails when the query fails.
    pub fn status(&self) -> Result<DmStatus, WinError> {
        // SAFETY: plain getter on a live viewport.
        unsafe { self.viewport.GetStatus() }
            .map(DmStatus::from_raw)
            .ctx("get the viewport status")
    }

    /// The content transform as DirectManipulation holds it now.
    ///
    /// # Errors
    /// Fails when the query fails.
    pub fn content_transform(&self) -> Result<DmTransform, WinError> {
        let mut matrix = [0f32; 6];
        // SAFETY: live content; `matrix` holds the six floats of the transform.
        unsafe { self.content.GetContentTransform(&mut matrix) }
            .ctx("get the content transform")?;
        Ok(DmTransform::from_matrix(&matrix, self.scale))
    }

    /// Stops a running manipulation or inertia where it is.
    ///
    /// # Errors
    /// Fails when DirectManipulation cannot stop the viewport.
    pub fn stop(&self) -> Result<(), WinError> {
        // SAFETY: plain call on a live viewport.
        unsafe { self.viewport.Stop() }.ctx("stop the viewport")
    }

    /// Re-seats the content at once so that the visible top is `offset` DIPs (in zoomed DIPs) at
    /// `zoom`. The next `update` reports the new transform. Needed after every scroll that did
    /// not come from DirectManipulation, before a contact is handed over (with `zoom` 1).
    ///
    /// # Errors
    /// Fails when `zoom` is not positive or DirectManipulation rejects the rectangle.
    pub fn sync_to(&self, offset: f64, zoom: f64) -> Result<(), WinError> {
        self.zoom_to_rect(offset, zoom, false)
    }

    /// Moves the content to `offset` at `zoom` with DirectManipulation's own animation (about a
    /// second, not configurable); the viewport reports `Inertia` while it runs and every `update`
    /// delivers the transform of that frame.
    ///
    /// # Errors
    /// Fails when `zoom` is not positive or DirectManipulation rejects the rectangle.
    pub fn animate_to(&self, offset: f64, zoom: f64) -> Result<(), WinError> {
        self.zoom_to_rect(offset, zoom, true)
    }

    fn zoom_to_rect(&self, offset: f64, zoom: f64, animate: bool) -> Result<(), WinError> {
        if !(zoom.is_finite() && zoom > 0.0) {
            return Err(WinError::Invalid("the zoom must be a positive number"));
        }
        let viewport = self.viewport_rect()?;
        // The rectangle is in unzoomed content pixels: a zoomed viewport sees a smaller part.
        let top = offset * f64::from(self.scale) / zoom;
        let width = f64::from(viewport.right - viewport.left) / zoom;
        let height = f64::from(viewport.bottom - viewport.top) / zoom;
        // SAFETY: plain call on a live viewport.
        unsafe {
            self.viewport.ZoomToRect(
                0.0,
                top as f32,
                width as f32,
                (top + height) as f32,
                animate,
            )
        }
        .ctx("move the content")
    }

    /// Enables DirectManipulation for the window (it was activated by [`new`](Self::new)); call
    /// when the window is activated again.
    ///
    /// # Errors
    /// Fails when the manager rejects the window.
    pub fn activate(&self) -> Result<(), WinError> {
        // SAFETY: plain call on a live manager with the window the viewport was created for.
        unsafe { self.manager.Activate(self.hwnd) }.ctx("activate DirectManipulation")
    }

    /// Stops DirectManipulation from processing input for the window; call when it is
    /// deactivated.
    ///
    /// # Errors
    /// Fails when the manager rejects the window.
    pub fn deactivate(&self) -> Result<(), WinError> {
        // SAFETY: plain call on a live manager with the window the viewport was created for.
        unsafe { self.manager.Deactivate(self.hwnd) }.ctx("deactivate DirectManipulation")
    }
}

impl Drop for DirectManipulation {
    /// Tears the viewport down: stops it, removes the handler and abandons the viewport. Errors
    /// cannot be handled while dropping and leave nothing to clean up.
    fn drop(&mut self) {
        // SAFETY: live COM objects; each call only releases resources of this viewport.
        unsafe {
            let _ = self.viewport.Stop();
            let _ = self.viewport.RemoveEventHandler(self.cookie);
            let _ = self.viewport.Abandon();
            let _ = self.manager.Deactivate(self.hwnd);
        }
    }
}

/// True when a `msg` message about a pointer of type `kind` starts a manipulation: touch and pen
/// contacts arrive as `WM_POINTERDOWN`, precision-touchpad contacts as `DM_POINTERHITTEST`.
#[must_use]
pub fn wants_contact(msg: u32, kind: POINTER_INPUT_TYPE) -> bool {
    matches!(
        (msg, kind),
        (WM_POINTERDOWN, PT_TOUCH | PT_PEN) | (DM_POINTERHITTEST, PT_TOUCHPAD)
    )
}

/// The pointer id in the `wParam` of a pointer message.
#[must_use]
pub fn pointer_id(wparam: WPARAM) -> u32 {
    (wparam.0 & 0xFFFF) as u32
}

/// The position, in client pixels of `hwnd`, of the pointer whose id is in the `wParam` of a
/// pointer message; `None` when the system does not know the pointer (any more) or the window
/// cannot convert the position. The window uses it to start a manipulation only for contacts
/// that land on the scrollable area.
#[must_use]
pub fn pointer_client_position(hwnd: HWND, wparam: WPARAM) -> Option<(i32, i32)> {
    let mut info = POINTER_INFO::default();
    // SAFETY: valid out pointer; unknown ids make the call fail.
    unsafe { GetPointerInfo(pointer_id(wparam), &mut info) }.ok()?;
    let mut point = info.ptPixelLocation;
    // SAFETY: converts the screen position in the local `point` with the caller's window handle;
    // a window that is gone makes the call fail.
    unsafe { ScreenToClient(hwnd, &mut point) }
        .as_bool()
        .then_some((point.x, point.y))
}

/// Whether the pointer whose id is in the `wParam` of a pointer message is a pen. The pen acts
/// as a pointer (it selects text), so the window does not hand it to the viewport although
/// [`wants_contact`] accepts it. `false` for unknown pointers.
#[must_use]
pub fn pointer_is_pen(wparam: WPARAM) -> bool {
    pointer_type(pointer_id(wparam)) == Some(PT_PEN)
}

fn pointer_type(id: u32) -> Option<POINTER_INPUT_TYPE> {
    let mut kind = POINTER_INPUT_TYPE(0);
    // SAFETY: valid out pointer; unknown ids make the call fail.
    unsafe { GetPointerType(id, &mut kind) }.ok().map(|()| kind)
}

/// True while pointer `id` still touches the surface.
fn contact_held(id: u32) -> bool {
    let mut info = POINTER_INFO::default();
    // SAFETY: valid out pointer; unknown ids make the call fail.
    unsafe { GetPointerInfo(id, &mut info) }.is_ok()
        && info.pointerFlags.contains(POINTER_FLAG_INCONTACT)
}

/// Where a [`ScrollDriver`] scrolls: sizes in DIPs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollLayout {
    /// The visible area.
    pub viewport: Size,
    /// The scrollable range (including typewriter overscroll).
    pub extent: Extent,
    /// Pixels per DIP.
    pub scale: f32,
    /// Height of one text line, for wheel notches.
    pub line_height: f64,
}

impl ScrollLayout {
    /// The content size DirectManipulation needs for this layout: a viewport plus the scrollable
    /// range, so its bounds are exactly `0..=extent.max_offset()`.
    #[must_use]
    pub fn content_size(&self) -> Size {
        Size::new(
            self.viewport.width,
            (self.extent.max_offset() + f64::from(self.viewport.height)) as f32,
        )
    }
}

/// What one [`ScrollDriver::frame`] produced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollFrame {
    /// The offset to draw, in DIPs (outside the extent while the viewport rubber-bands).
    pub offset: f64,
    /// Whether the offset changed in this frame.
    pub changed: bool,
    /// The cumulative zoom factor, when it changed since the previous frame (also when the
    /// change was consumed by a re-seat or a take-over in between: it is reported once).
    pub zoom: Option<f64>,
    /// Whether another frame is needed (a contact is down, the viewport moves or a spring runs).
    pub more: bool,
}

/// What the driver learned from one `update`.
#[derive(Debug, Default)]
struct Absorbed {
    offset: Option<f64>,
    zoom: Option<f64>,
}

impl DmListener for Absorbed {
    fn scrolled(&mut self, _offset_x: f64, offset_y: f64) {
        self.offset = Some(offset_y);
    }

    fn zoomed(&mut self, zoom: f64) {
        self.zoom = Some(zoom);
    }

    fn status_changed(&mut self, _current: DmStatus, _previous: DmStatus) {}
}

/// Scrolling of one window: finger and touchpad contacts with inertia through
/// DirectManipulation, wheel and programmatic scrolling through a [`Scroller`], one authoritative
/// offset.
///
/// Whoever moved last owns the offset. Any move that did not come from DirectManipulation leaves
/// its content transform stale, so the transform is re-seated before the next contact. A wheel or
/// programmatic move while DirectManipulation is moving stops it first and adopts its final
/// position; a contact landing on a running spring freezes the spring where it is.
#[derive(Debug)]
pub struct ScrollDriver {
    dm: DirectManipulation,
    scroller: Scroller,
    /// The pointer of the contact being tracked, until it lifts.
    contact: Option<u32>,
    /// DirectManipulation's transform differs from the scroller's offset.
    stale: bool,
    /// Frames left in which DirectManipulation is updated although it does not report motion yet
    /// (after [`animate_to`](Self::animate_to)).
    grace: u8,
    /// The cumulative zoom the viewport is at; re-seating keeps it.
    zoom: f64,
    /// A zoom change not yet handed out in a [`ScrollFrame`].
    pending_zoom: Option<f64>,
    lines: WheelLines,
    line_height: f64,
    viewport_height: f64,
}

impl ScrollDriver {
    /// Creates the driver for `hwnd` and reads the system settings.
    ///
    /// # Errors
    /// See [`DirectManipulation::new`].
    pub fn new(hwnd: HWND, config: DmConfig, layout: &ScrollLayout) -> Result<Self, WinError> {
        let dm = DirectManipulation::new(
            hwnd,
            config,
            layout.viewport,
            layout.content_size(),
            layout.scale,
        )?;
        let mut scroller = Scroller::new(client_area_animation());
        scroller.set_extent(layout.extent);
        Ok(Self {
            dm,
            scroller,
            contact: None,
            stale: false,
            grace: 0,
            zoom: 1.0,
            pending_zoom: None,
            lines: wheel_scroll_lines(),
            line_height: layout.line_height,
            viewport_height: f64::from(layout.viewport.height),
        })
    }

    /// The offset to draw, in DIPs.
    #[must_use]
    pub fn offset(&self) -> f64 {
        self.scroller.offset()
    }

    /// The scroll model (offset, target, extent, animation state).
    #[must_use]
    pub fn scroller(&self) -> &Scroller {
        &self.scroller
    }

    /// The viewport.
    #[must_use]
    pub fn manipulation(&self) -> &DirectManipulation {
        &self.dm
    }

    /// Whether a contact is being tracked.
    #[must_use]
    pub fn has_contact(&self) -> bool {
        self.contact.is_some()
    }

    /// Whether DirectManipulation's transform lags the offset.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Re-reads "Animation effects" and the wheel setting (`WM_SETTINGCHANGE`). Returns whether
    /// the offset changed (turning animations off finishes a running animation).
    pub fn refresh_system_settings(&mut self) -> bool {
        self.lines = wheel_scroll_lines();
        let changed = self.scroller.set_animations(client_area_animation());
        self.stale |= changed;
        changed
    }

    /// Applies a new size, DPI, line height or content length. Returns whether the offset
    /// changed.
    ///
    /// # Errors
    /// Fails when DirectManipulation rejects the geometry or cannot be stopped.
    pub fn set_layout(&mut self, layout: &ScrollLayout) -> Result<bool, WinError> {
        self.take_over()?;
        self.line_height = layout.line_height;
        self.viewport_height = f64::from(layout.viewport.height);
        let changed = self.scroller.set_extent(layout.extent);
        self.dm
            .set_geometry(layout.viewport, layout.content_size(), layout.scale)?;
        self.stale = true;
        Ok(changed)
    }

    /// Routes a `WM_POINTERDOWN` or `DM_POINTERHITTEST` message: starts a manipulation for touch,
    /// pen and precision-touchpad pointers. Returns whether a contact started; mouse pointers and
    /// unknown ids are left alone.
    ///
    /// # Errors
    /// Fails when the contact cannot be handed to DirectManipulation.
    pub fn on_pointer_message(&mut self, msg: u32, wparam: WPARAM) -> Result<bool, WinError> {
        let id = pointer_id(wparam);
        self.on_pointer(msg, id, pointer_type(id))
    }

    /// [`on_pointer_message`](Self::on_pointer_message) for a pointer whose type is already
    /// known (`None`: the system does not know the id).
    ///
    /// # Errors
    /// Fails when the contact cannot be handed to DirectManipulation.
    pub fn on_pointer(
        &mut self,
        msg: u32,
        id: u32,
        kind: Option<POINTER_INPUT_TYPE>,
    ) -> Result<bool, WinError> {
        match kind {
            Some(kind) if wants_contact(msg, kind) => {
                self.start_contact(id)?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Hands pointer `id` to DirectManipulation: freezes a running spring, re-seats a stale
    /// transform and sets the contact.
    ///
    /// # Errors
    /// Fails when the transform cannot be re-seated or the pointer is unknown.
    pub fn start_contact(&mut self, id: u32) -> Result<(), WinError> {
        self.scroller.stop();
        self.reseat()?;
        self.dm.set_contact(id)?;
        self.contact = Some(id);
        Ok(())
    }

    /// Moves to `offset` at `zoom` with DirectManipulation's own animation: about a second, a
    /// fixed front-loaded ease-out, not configurable (prefer [`scroll_to`](Self::scroll_to) for
    /// polished scrolling). The following frames deliver its offsets and zoom.
    ///
    /// # Errors
    /// Fails when `zoom` is not positive or DirectManipulation rejects the move.
    pub fn animate_to(&mut self, offset: f64, zoom: f64) -> Result<(), WinError> {
        self.take_over()?;
        self.scroller.stop();
        self.reseat()?;
        self.dm.animate_to(offset, zoom)?;
        self.grace = ANIMATION_GRACE_FRAMES;
        Ok(())
    }

    /// Makes DirectManipulation's transform match the scroller's offset when a scroll that did
    /// not come from DirectManipulation left it behind.
    fn reseat(&mut self) -> Result<(), WinError> {
        if self.stale {
            self.dm.sync_to(self.scroller.offset(), self.zoom)?;
            let mut absorbed = Absorbed::default();
            self.dm.update(&mut absorbed)?;
            self.note_zoom(absorbed.zoom);
            self.stale = false;
        }
        Ok(())
    }

    /// Remembers a zoom DirectManipulation reported, to hand it out in the next frame even when
    /// the update that carried it was consumed by a re-seat or a take-over.
    fn note_zoom(&mut self, zoom: Option<f64>) {
        if let Some(zoom) = zoom {
            self.zoom = zoom;
            self.pending_zoom = Some(zoom);
        }
    }

    /// Applies a wheel event (the vertical part) with the system's lines per notch. Returns
    /// whether the offset changed or an animation started; keep calling
    /// [`frame`](Self::frame) while [`Scroller::is_animating`].
    ///
    /// # Errors
    /// Fails when a running manipulation cannot be stopped.
    pub fn on_wheel(&mut self, event: &WheelEvent) -> Result<bool, WinError> {
        let notch = self
            .lines
            .notch_distance(self.line_height, self.viewport_height);
        self.drive(|scroller| scroller.wheel(event, notch))
    }

    /// Animates to `offset` (programmatic scroll).
    ///
    /// # Errors
    /// Fails when a running manipulation cannot be stopped.
    pub fn scroll_to(&mut self, offset: f64) -> Result<bool, WinError> {
        self.drive(|scroller| scroller.scroll_to(offset))
    }

    /// Jumps to `offset` without animation.
    ///
    /// # Errors
    /// Fails when a running manipulation cannot be stopped.
    pub fn jump_to(&mut self, offset: f64) -> Result<bool, WinError> {
        self.drive(|scroller| scroller.jump_to(offset))
    }

    /// One frame of the pump: updates DirectManipulation while a contact is down or the viewport
    /// moves (adopting its offset), otherwise advances the scroller's animation to `now`.
    ///
    /// # Errors
    /// Fails when DirectManipulation fails to update or to report its status.
    pub fn frame(&mut self, now: Duration) -> Result<ScrollFrame, WinError> {
        if let Some(id) = self.contact
            && !contact_held(id)
        {
            self.contact = None;
        }
        let mut status = self.dm.status()?;
        let busy = self.contact.is_some() || status.is_moving() || self.grace > 0;
        self.grace = self.grace.saturating_sub(1);
        let mut absorbed = Absorbed::default();
        if busy {
            status = self.dm.update(&mut absorbed)?;
        }
        let mut changed = false;
        if let Some(offset) = absorbed.offset {
            changed = self.scroller.adopt(offset);
            self.stale = false;
        }
        self.note_zoom(absorbed.zoom);
        if !busy {
            changed |= self.scroller.tick(now);
        }
        let more = self.contact.is_some()
            || status.is_moving()
            || self.grace > 0
            || self.scroller.is_animating();
        Ok(ScrollFrame {
            offset: self.scroller.offset(),
            changed,
            zoom: self.pending_zoom.take(),
            more,
        })
    }

    /// Enables DirectManipulation again (window activated).
    ///
    /// # Errors
    /// See [`DirectManipulation::activate`].
    pub fn activate(&self) -> Result<(), WinError> {
        self.dm.activate()
    }

    /// Stops DirectManipulation input (window deactivated) and drops the tracked contact.
    ///
    /// # Errors
    /// See [`DirectManipulation::deactivate`].
    pub fn deactivate(&mut self) -> Result<(), WinError> {
        self.contact = None;
        self.dm.deactivate()
    }

    /// Runs a scroller move: takes over from DirectManipulation first and leaves its transform
    /// stale afterwards.
    fn drive(&mut self, apply: impl FnOnce(&mut Scroller) -> bool) -> Result<bool, WinError> {
        self.take_over()?;
        let moved = apply(&mut self.scroller);
        self.stale |= moved;
        Ok(moved)
    }

    /// Stops a running manipulation (contact or inertia) and adopts its final position.
    fn take_over(&mut self) -> Result<(), WinError> {
        if self.contact.is_none() && !self.dm.status()?.is_moving() {
            return Ok(());
        }
        self.dm.stop()?;
        let transform = self.dm.content_transform()?;
        self.scroller.adopt(transform.offset_y);
        if (transform.zoom - self.zoom).abs() > ZOOM_EPSILON {
            self.note_zoom(Some(transform.zoom));
        }
        self.contact = None;
        Ok(())
    }
}
