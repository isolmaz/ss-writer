//! Graphics devices and render targets.
//!
//! - [`GfxDevices`]: the D3D11 (BGRA) device, DXGI device, D2D factory and D2D device. Created
//!   off the UI thread by [`DeviceStartup::spawn`] (device creation takes 200+ ms) and handed
//!   over through a channel. A generation counter identifies a device incarnation; after a
//!   device loss [`GfxDevices::recreate`] produces the next generation and every dependent
//!   resource is rebuilt.
//! - [`SharedDevices`]: what the UI thread holds while the worker creates them: windows open
//!   first and take the devices when [`SharedDevices::poll`] has them.
//! - [`WindowSurface`]: a per-window DirectComposition visual showing a premultiplied flip
//!   swap chain, drawn through a D2D device context.
//! - [`WicTarget`]: a WIC bitmap render target for tests and export (same `Canvas`).

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;

use ssw_ui::geometry::Color;
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_SIZE_U, D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_MULTI_THREADED,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_SOFTWARE, D2D1_RENDER_TARGET_USAGE_NONE,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE, D2D1CreateFactory, ID2D1Device, ID2D1DeviceContext,
    ID2D1Factory1, ID2D1Image, ID2D1RenderTarget,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter, IDXGIDevice,
    IDXGIDevice1, IDXGIFactory2, IDXGIOutput, IDXGISurface, IDXGISwapChain1,
};
use windows::Win32::Graphics::Imaging::{
    GUID_WICPixelFormat32bppPBGRA, IWICBitmap, WICBitmapCacheOnDemand,
};
use windows::core::{HRESULT, Interface};

use crate::com::{ComGuard, OsResultExt, WinError, delivered};
use crate::fault::check;
use crate::wic::{DecodedImage, Wic};

/// Which D3D11 driver to create the device on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Hardware,
    /// The software rasterizer (fallback and tests).
    Warp,
}

impl Driver {
    fn raw(self) -> D3D_DRIVER_TYPE {
        match self {
            Driver::Hardware => D3D_DRIVER_TYPE_HARDWARE,
            Driver::Warp => D3D_DRIVER_TYPE_WARP,
        }
    }
}

/// Creates a Direct2D factory.
///
/// # Errors
/// Fails when Direct2D is unavailable.
pub fn d2d_factory(multi_threaded: bool) -> Result<ID2D1Factory1, WinError> {
    let kind = if multi_threaded {
        D2D1_FACTORY_TYPE_MULTI_THREADED
    } else {
        D2D1_FACTORY_TYPE_SINGLE_THREADED
    };
    // SAFETY: plain factory creation without options.
    check(unsafe { D2D1CreateFactory::<ID2D1Factory1>(kind, None) }).ctx("create D2D factory")
}

/// The shared graphics devices of one generation.
#[derive(Debug, Clone)]
pub struct GfxDevices {
    generation: u64,
    driver: Driver,
    d3d: ID3D11Device,
    dxgi: IDXGIDevice,
    factory: ID2D1Factory1,
    device: ID2D1Device,
}

impl GfxDevices {
    /// Creates generation `generation` on the hardware driver, falling back to WARP.
    ///
    /// # Errors
    /// Fails when neither driver can create a BGRA-capable device.
    pub fn create(generation: u64) -> Result<Self, WinError> {
        Self::create_on(Driver::Hardware, generation)
            .or_else(|_| Self::create_on(Driver::Warp, generation))
    }

    /// Creates generation `generation` on a specific driver.
    ///
    /// # Errors
    /// Fails when the driver cannot create the device.
    pub fn create_on(driver: Driver, generation: u64) -> Result<Self, WinError> {
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        // SAFETY: valid out pointer and feature level list; BGRA support is required by D2D.
        let created = check(unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                driver.raw(),
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        });
        let d3d = delivered(created, device).ctx("D3D11CreateDevice")?;
        let dxgi: IDXGIDevice = check(d3d.cast()).ctx("query IDXGIDevice")?;
        let factory = d2d_factory(true)?;
        // SAFETY: valid DXGI device; the latency hint is best effort.
        unsafe {
            if let Ok(device1) = check(dxgi.cast::<IDXGIDevice1>()) {
                let _ = device1.SetMaximumFrameLatency(1);
            }
        }
        // SAFETY: valid factory and DXGI device.
        let device = check(unsafe { factory.CreateDevice(&dxgi) }).ctx("create D2D device")?;
        Ok(Self {
            generation,
            driver,
            d3d,
            dxgi,
            factory,
            device,
        })
    }

    /// The next generation on the same driver kind, used after a device loss.
    ///
    /// # Errors
    /// Fails when the device cannot be recreated.
    pub fn recreate(&self) -> Result<Self, WinError> {
        Self::create_on(self.driver, self.generation + 1)
            .or_else(|_| Self::create_on(Driver::Warp, self.generation + 1))
    }

    /// Identifies this incarnation; resources created from another generation are stale.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The driver the device runs on.
    #[must_use]
    pub fn driver(&self) -> Driver {
        self.driver
    }

    #[must_use]
    pub fn d2d_factory(&self) -> &ID2D1Factory1 {
        &self.factory
    }

    #[must_use]
    pub fn d2d_device(&self) -> &ID2D1Device {
        &self.device
    }

    #[must_use]
    pub fn d3d_device(&self) -> &ID3D11Device {
        &self.d3d
    }

    /// Why the device was removed, or `S_OK` while it is healthy.
    pub fn removed_reason(&self) -> HRESULT {
        // SAFETY: plain getter on a live device.
        removal_code(unsafe { self.d3d.GetDeviceRemovedReason() })
    }
}

/// The `HRESULT` behind the outcome of `GetDeviceRemovedReason`: the reason when the device was
/// removed, `S_OK` while it is healthy.
fn removal_code(outcome: windows::core::Result<()>) -> HRESULT {
    match outcome {
        Ok(()) => HRESULT(0),
        Err(error) => error.code(),
    }
}

/// Hand-over wrapper for the worker-created devices.
struct Handoff(Result<GfxDevices, WinError>);

// SAFETY: the D3D11 device, DXGI device and a multi-threaded D2D factory/device are free
// threaded; the worker drops its references before the receiver can observe the value and
// never touches the objects again, so ownership moves cleanly to the receiving thread.
unsafe impl Send for Handoff {}

/// What runs when the device thread is done (see [`DeviceStartup::notify_on_ready`]).
type ReadyHook = Box<dyn FnOnce() + Send>;

/// The "result is waiting" signal of a [`DeviceStartup`], shared with its worker thread.
///
/// A hook registered before the worker finishes runs on the worker right after it handed the
/// result over; one registered after runs at once on the registering thread. Either way it
/// runs exactly once, and never while the lock is held.
#[derive(Default)]
struct Ready {
    state: Mutex<ReadyState>,
}

#[derive(Default)]
struct ReadyState {
    done: bool,
    hook: Option<ReadyHook>,
}

impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ready").finish_non_exhaustive()
    }
}

impl Ready {
    fn lock(&self) -> MutexGuard<'_, ReadyState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Registers `hook` (a hook that did not run yet is replaced); runs it now when the worker
    /// is done.
    fn on_ready(&self, hook: ReadyHook) {
        let mut state = self.lock();
        if state.done {
            drop(state);
            hook();
        } else {
            state.hook = Some(hook);
        }
    }

    /// The worker is done: runs the registered hook.
    fn complete(&self) {
        let hook = {
            let mut state = self.lock();
            state.done = true;
            state.hook.take()
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

/// Device creation running on a worker thread.
#[derive(Debug)]
pub struct DeviceStartup {
    receiver: Receiver<Handoff>,
    worker: JoinHandle<()>,
    ready: Arc<Ready>,
}

/// What [`DeviceStartup::poll`] found.
#[derive(Debug)]
pub enum DevicePoll {
    /// The worker is still creating the devices: the startup, to poll again.
    Pending(DeviceStartup),
    /// The worker is done: the devices, or why there are none.
    Done(Result<GfxDevices, WinError>),
}

impl DeviceStartup {
    /// Starts creating generation 1 on a new thread.
    #[must_use]
    pub fn spawn() -> Self {
        Self::spawn_with(Driver::Hardware)
    }

    /// Starts creating generation 1 on `driver` (no hardware fallback).
    #[must_use]
    pub fn spawn_with(driver: Driver) -> Self {
        Self::spawn_with_factory(move || {
            if driver == Driver::Hardware {
                GfxDevices::create(1)
            } else {
                GfxDevices::create_on(driver, 1)
            }
        })
    }

    /// Runs `create` on a new thread (COM initialized there for it) and hands its result over.
    /// [`DeviceStartup::spawn_with`] passes the real creation; tests pass one that holds the
    /// devices back or fails, to see what the application does while they are not there.
    #[must_use]
    pub fn spawn_with_factory(
        create: impl FnOnce() -> Result<GfxDevices, WinError> + Send + 'static,
    ) -> Self {
        let (sender, receiver) = channel();
        let ready = Arc::new(Ready::default());
        let signal = Arc::clone(&ready);
        let worker = std::thread::spawn(move || {
            let result = ComGuard::multi_threaded().and_then(|_com| create());
            crate::perf::mark(crate::perf::MARK_DEVICE_READY);
            // The receiver may be gone when the process shuts down early.
            let _ = sender.send(Handoff(result));
            signal.complete();
        });
        Self {
            receiver,
            worker,
            ready,
        }
    }

    /// Runs `hook` when the result can be taken without waiting: on the worker thread right
    /// after the devices were created, or at once on this thread when they already were. The
    /// hook must not block; it is meant to wake the UI thread (`PostMessageW`). A later call
    /// replaces a hook that did not run yet.
    pub fn notify_on_ready(&self, hook: impl FnOnce() + Send + 'static) {
        self.ready.on_ready(Box::new(hook));
    }

    /// Blocks until the devices are ready.
    ///
    /// # Errors
    /// Returns the creation error, or [`WinError::Invalid`] when the worker died.
    pub fn wait(self) -> Result<GfxDevices, WinError> {
        let received = self.receiver.recv().ok();
        self.finish(received)
    }

    /// Takes the devices when the worker is done, without blocking.
    #[must_use]
    pub fn poll(self) -> DevicePoll {
        match self.receiver.try_recv() {
            Ok(received) => DevicePoll::Done(self.finish(Some(received))),
            Err(TryRecvError::Empty) => DevicePoll::Pending(self),
            Err(TryRecvError::Disconnected) => DevicePoll::Done(self.finish(None)),
        }
    }

    /// Joins the finished worker and turns what it handed over into the outcome.
    fn finish(self, received: Option<Handoff>) -> Result<GfxDevices, WinError> {
        let _ = self.worker.join();
        match received {
            Some(Handoff(result)) => result,
            None => Err(WinError::Invalid("device thread ended without a result")),
        }
    }
}

/// What the devices were reported as when nothing is left to report.
const DEVICES_GONE: &str = "the graphics devices are not available";

/// Where the devices stand for the UI thread.
#[derive(Debug)]
enum DevicesState {
    /// The worker is creating them.
    Creating(DeviceStartup),
    /// They are here.
    Ready(GfxDevices),
    /// Their creation failed; the call that saw the failure got its error.
    Gone,
}

/// The graphics devices as the UI thread uses them: created by a worker while the first window
/// opens, taken without blocking ([`SharedDevices::poll`]) whenever a window can use them, or
/// with blocking ([`SharedDevices::get`]) where nothing can go on without them. Clones share
/// one state. Not `Send`: the UI thread owns it (the devices themselves are free threaded).
#[derive(Debug, Clone)]
pub struct SharedDevices {
    state: Rc<RefCell<DevicesState>>,
}

impl From<GfxDevices> for SharedDevices {
    /// Devices that exist already.
    fn from(devices: GfxDevices) -> Self {
        Self {
            state: Rc::new(RefCell::new(DevicesState::Ready(devices))),
        }
    }
}

impl SharedDevices {
    /// The devices `startup` creates.
    #[must_use]
    pub fn new(startup: DeviceStartup) -> Self {
        Self {
            state: Rc::new(RefCell::new(DevicesState::Creating(startup))),
        }
    }

    /// The devices when they are ready; `Ok(None)` while the worker still creates them.
    ///
    /// # Errors
    /// The creation error (once), then [`WinError::Invalid`]: there are no devices.
    pub fn poll(&self) -> Result<Option<GfxDevices>, WinError> {
        self.resolve(false)
    }

    /// The devices; waits for the worker when it is not done.
    ///
    /// # Errors
    /// The creation error (once), then [`WinError::Invalid`]: there are no devices.
    pub fn get(&self) -> Result<GfxDevices, WinError> {
        self.resolve(true)?.ok_or(WinError::Invalid(DEVICES_GONE))
    }

    fn resolve(&self, wait: bool) -> Result<Option<GfxDevices>, WinError> {
        let mut state = self.state.borrow_mut();
        let outcome = match std::mem::replace(&mut *state, DevicesState::Gone) {
            DevicesState::Creating(startup) if wait => startup.wait(),
            DevicesState::Creating(startup) => match startup.poll() {
                DevicePoll::Pending(startup) => {
                    *state = DevicesState::Creating(startup);
                    return Ok(None);
                }
                DevicePoll::Done(outcome) => outcome,
            },
            DevicesState::Ready(devices) => Ok(devices),
            DevicesState::Gone => Err(WinError::Invalid(DEVICES_GONE)),
        };
        let devices = outcome?;
        *state = DevicesState::Ready(devices.clone());
        Ok(Some(devices))
    }
}

fn clear_color(color: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    }
}

/// A DirectComposition visual bound to one window, showing a premultiplied swap chain.
#[derive(Debug)]
pub struct WindowSurface {
    generation: u64,
    swap: IDXGISwapChain1,
    context: ID2D1DeviceContext,
    /// Must outlive nothing else but must be dropped before a new target for the same HWND is
    /// created (field order drops it after the visual and the swap chain are released).
    _visual: IDCompositionVisual,
    _target: IDCompositionTarget,
    _dcomp: IDCompositionDevice,
    width: u32,
    height: u32,
    dpi: f32,
}

impl WindowSurface {
    /// Binds a swap chain of `width` x `height` pixels to `hwnd`.
    ///
    /// # Errors
    /// Fails when swap chain, composition or D2D context creation fails.
    pub fn new(
        devices: &GfxDevices,
        hwnd: HWND,
        width: u32,
        height: u32,
        dpi: f32,
    ) -> Result<Self, WinError> {
        let (width, height) = (width.max(1), height.max(1));
        // SAFETY: all objects are created from the live devices; descriptors are fully
        // initialized; `hwnd` is only used as a composition target.
        unsafe {
            let adapter = check(devices.dxgi.GetAdapter()).ctx("DXGI adapter")?;
            let factory: IDXGIFactory2 = check(adapter.GetParent()).ctx("DXGI factory")?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                // `DXGI_SCALING_NONE` is rejected for composition swap chains.
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            };
            let swap = check(factory.CreateSwapChainForComposition(
                &devices.d3d,
                &desc,
                None::<&IDXGIOutput>,
            ))
            .ctx("create composition swap chain")?;
            let dcomp: IDCompositionDevice = check(DCompositionCreateDevice(&devices.dxgi))
                .ctx("create DirectComposition device")?;
            let target =
                check(dcomp.CreateTargetForHwnd(hwnd, true)).ctx("create composition target")?;
            let visual = check(dcomp.CreateVisual()).ctx("create composition visual")?;
            check(visual.SetContent(&swap)).ctx("set visual content")?;
            check(target.SetRoot(&visual)).ctx("set composition root")?;
            check(dcomp.Commit()).ctx("commit composition")?;
            let context = check(
                devices
                    .device
                    .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE),
            )
            .ctx("create D2D device context")?;
            context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Ok(Self {
                generation: devices.generation,
                swap,
                context,
                _visual: visual,
                _target: target,
                _dcomp: dcomp,
                width,
                height,
                dpi,
            })
        }
    }

    /// The generation of the devices this surface was built on.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Swap chain size in pixels.
    #[must_use]
    pub fn size_px(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Updates the DPI used to map DIPs to pixels.
    pub fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
    }

    /// Resizes the swap chain; a no-op when the size is unchanged.
    ///
    /// # Errors
    /// Fails (with [`WinError::DeviceLost`] when the device is gone) if `ResizeBuffers` fails.
    pub fn resize(&mut self, width: u32, height: u32) -> Result<(), WinError> {
        let (width, height) = (width.max(1), height.max(1));
        if (width, height) == (self.width, self.height) {
            return Ok(());
        }
        // SAFETY: every D2D reference to the back buffer is dropped (`SetTarget(None)`)
        // before resizing, as DXGI requires.
        unsafe {
            self.context.SetTarget(None::<&ID2D1Image>);
            check(self.swap.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            ))
            .ctx("resize swap chain")?;
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    /// The device context, for creating device-dependent resources.
    #[must_use]
    pub fn context(&self) -> &ID2D1DeviceContext {
        &self.context
    }

    /// Draws one frame: clears to transparent (so the backdrop shows through), runs `draw` in
    /// DIP space and presents.
    ///
    /// # Errors
    /// Returns the first drawing error; [`WinError::DeviceLost`] means the caller must
    /// recreate the devices and this surface.
    pub fn paint<R>(
        &mut self,
        draw: impl FnOnce(&ID2D1RenderTarget) -> Result<R, WinError>,
    ) -> Result<R, WinError> {
        let mut timer = crate::perf::FrameTimer::begin();
        // SAFETY: the back buffer is wrapped for the duration of the frame only: `BackBuffer`
        // resets the target before `Present` (or when the drawing code unwinds), and
        // `DrawScope` always pairs `BeginDraw` with `EndDraw`.
        unsafe {
            // Fallible lookups first, so no error can leave the context half set up.
            let target: ID2D1RenderTarget =
                check(self.context.cast()).ctx("query render target")?;
            let surface: IDXGISurface = check(self.swap.GetBuffer(0)).ctx("get back buffer")?;
            let properties = D2D1_BITMAP_PROPERTIES1 {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: self.dpi,
                dpiY: self.dpi,
                bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
                colorContext: ManuallyDrop::new(None),
            };
            let bitmap = check(
                self.context
                    .CreateBitmapFromDxgiSurface(&surface, Some(&properties)),
            )
            .ctx("wrap back buffer")?;
            self.context.SetTarget(&bitmap);
            let back_buffer = BackBuffer(&self.context);
            self.context.SetDpi(self.dpi, self.dpi);
            let scope = DrawScope::begin(&target);
            target.Clear(Some(&clear_color(Color::TRANSPARENT)));
            timer.ready();
            let drawn = draw(&target);
            timer.recorded();
            let ended = scope.end();
            drop(back_buffer);
            timer.drawn();
            let value = drawn?;
            ended.ctx("EndDraw")?;
            let status = self.swap.Present(1, DXGI_PRESENT(0));
            classify_present(status)?;
            timer.presented();
            Ok(value)
        }
    }
}

/// One `BeginDraw` .. `EndDraw` pair of a render target. A scope dropped without
/// [`DrawScope::end`] (the drawing code panicked) still ends the draw, so the target never
/// stays inside `BeginDraw`.
struct DrawScope<'a> {
    target: &'a ID2D1RenderTarget,
    ended: bool,
}

impl<'a> DrawScope<'a> {
    fn begin(target: &'a ID2D1RenderTarget) -> Self {
        // SAFETY: a live render target; paired with `EndDraw` in `end` or `Drop`.
        unsafe { target.BeginDraw() };
        Self {
            target,
            ended: false,
        }
    }

    fn end(mut self) -> windows::core::Result<()> {
        self.ended = true;
        // SAFETY: pairs the `BeginDraw` of `begin`.
        check(unsafe { self.target.EndDraw(None, None) })
    }
}

impl Drop for DrawScope<'_> {
    fn drop(&mut self) {
        if !self.ended {
            // SAFETY: pairs the `BeginDraw` of `begin`; the outcome is irrelevant while unwinding.
            let _ = unsafe { self.target.EndDraw(None, None) };
        }
    }
}

/// Keeps the swap chain's back buffer set as the target of a device context and resets the
/// target when dropped: `ResizeBuffers` fails while D2D still references the buffer.
struct BackBuffer<'a>(&'a ID2D1DeviceContext);

impl Drop for BackBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: plain call on the live device context.
        unsafe { self.0.SetTarget(None::<&ID2D1Image>) };
    }
}

/// Maps a `Present` result: device-lost codes become [`WinError::DeviceLost`], other failures
/// [`WinError::Os`], success codes (including "occluded") pass.
///
/// # Errors
/// See above.
pub fn classify_present(status: HRESULT) -> Result<(), WinError> {
    check(status.ok()).ctx("Present")
}

/// A WIC bitmap with a software D2D render target; the same `Canvas` implementation draws
/// into it. Used for pixel tests and image export.
#[derive(Debug)]
pub struct WicTarget {
    bitmap: IWICBitmap,
    target: ID2D1RenderTarget,
    width: u32,
    height: u32,
}

impl WicTarget {
    /// A `width` x `height` pixel target at `dpi`.
    ///
    /// # Errors
    /// Fails when the size is zero or WIC / D2D refuse.
    pub fn new(
        factory: &ID2D1Factory1,
        wic: &Wic,
        width: u32,
        height: u32,
        dpi: f32,
    ) -> Result<Self, WinError> {
        if width == 0 || height == 0 {
            return Err(WinError::Invalid("render target size must not be zero"));
        }
        // SAFETY: the render target keeps its own reference to the bitmap.
        unsafe {
            let bitmap = check(wic.factory().CreateBitmap(
                width,
                height,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnDemand,
            ))
            .ctx("create WIC bitmap")?;
            let properties = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let target = check(factory.CreateWicBitmapRenderTarget(&bitmap, &properties))
                .ctx("create WIC render target")?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Ok(Self {
                bitmap,
                target,
                width,
                height,
            })
        }
    }

    #[must_use]
    pub fn render_target(&self) -> &ID2D1RenderTarget {
        &self.target
    }

    #[must_use]
    pub fn size_px(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Clears to `background` (transparent for `None`), runs `draw` and ends the frame.
    ///
    /// # Errors
    /// Returns the drawing error, or the `EndDraw` error.
    pub fn paint<R>(
        &self,
        background: Option<Color>,
        draw: impl FnOnce(&ID2D1RenderTarget) -> Result<R, WinError>,
    ) -> Result<R, WinError> {
        let scope = DrawScope::begin(&self.target);
        // SAFETY: valid color pointer on the live target, inside the draw.
        unsafe {
            self.target
                .Clear(Some(&clear_color(background.unwrap_or(Color::TRANSPARENT))));
        }
        let drawn = draw(&self.target);
        let ended = scope.end();
        let value = drawn?;
        ended.ctx("EndDraw")?;
        Ok(value)
    }

    /// Reads the pixels (premultiplied BGRA).
    ///
    /// # Errors
    /// Fails when WIC cannot copy the pixels.
    pub fn pixels(&self) -> Result<DecodedImage, WinError> {
        let mut bgra = vec![0u8; (self.width as usize) * (self.height as usize) * 4];
        // SAFETY: the buffer holds `stride * height` bytes.
        unsafe {
            check(
                self.bitmap
                    .CopyPixels(std::ptr::null(), self.width * 4, &mut bgra),
            )
            .ctx("copy pixels")?;
        }
        Ok(DecodedImage {
            width: self.width,
            height: self.height,
            bgra,
        })
    }

    /// The target as `D2D_SIZE_U`.
    #[must_use]
    pub fn size_u(&self) -> D2D_SIZE_U {
        D2D_SIZE_U {
            width: self.width,
            height: self.height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "fault-injection")]
    use crate::fault::{all_failed, failed_step};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::Sender;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
    };
    use windows::core::w;

    #[test]
    fn present_status_is_classified() {
        assert!(classify_present(HRESULT(0)).is_ok());
        // DXGI_STATUS_OCCLUDED is a success code.
        assert!(classify_present(HRESULT(0x087A_0001)).is_ok());
        assert!(
            classify_present(DXGI_ERROR_DEVICE_REMOVED)
                .unwrap_err()
                .is_device_lost()
        );
        let other = classify_present(E_FAIL).unwrap_err();
        assert!(!other.is_device_lost());
        assert!(other.to_string().starts_with("Present failed"));
    }

    #[test]
    fn devices_are_created_off_thread_and_recreated_with_a_new_generation() {
        let devices = DeviceStartup::spawn().wait().unwrap();
        assert_eq!(devices.generation(), 1);
        assert_eq!(devices.removed_reason(), HRESULT(0));
        let next = devices.recreate().unwrap();
        assert_eq!(next.generation(), 2);
        assert_eq!(next.driver(), devices.driver());
        let _ = (next.d2d_factory(), next.d2d_device(), next.d3d_device());
    }

    #[test]
    fn the_reason_of_a_removed_device_is_its_error_code() {
        assert_eq!(removal_code(Ok(())), HRESULT(0));
        assert_eq!(
            removal_code(Err(DXGI_ERROR_DEVICE_REMOVED.into())),
            DXGI_ERROR_DEVICE_REMOVED
        );
    }

    #[test]
    fn waiting_for_a_worker_that_ended_without_a_result_is_an_error() {
        let (sender, receiver) = channel::<Handoff>();
        drop(sender);
        let startup = DeviceStartup {
            receiver,
            worker: std::thread::spawn(|| {}),
            ready: Arc::default(),
        };
        let error = startup.wait().unwrap_err();
        assert_eq!(error.to_string(), "device thread ended without a result");
    }

    /// What a poll came to: the startup when the worker is still busy, the outcome when it is
    /// done.
    fn split(poll: DevicePoll) -> (Option<DeviceStartup>, Option<Result<GfxDevices, WinError>>) {
        match poll {
            DevicePoll::Pending(startup) => (Some(startup), None),
            DevicePoll::Done(outcome) => (None, Some(outcome)),
        }
    }

    #[test]
    fn polling_a_worker_that_ended_without_a_result_is_an_error() {
        let (sender, receiver) = channel::<Handoff>();
        drop(sender);
        let startup = DeviceStartup {
            receiver,
            worker: std::thread::spawn(|| {}),
            ready: Arc::default(),
        };
        let (pending, done) = split(startup.poll());
        assert!(pending.is_none(), "the end of the device thread is final");
        let error = done.unwrap().unwrap_err();
        assert_eq!(error.to_string(), "device thread ended without a result");
    }

    #[test]
    fn the_ready_hook_runs_once_whichever_side_comes_first() {
        let runs = Arc::new(AtomicUsize::new(0));
        let hook = || -> ReadyHook {
            let runs = Arc::clone(&runs);
            Box::new(move || {
                runs.fetch_add(1, Ordering::SeqCst);
            })
        };
        let ready = Ready::default();
        assert_eq!(format!("{ready:?}"), "Ready { .. }");
        // Registered before the worker is done: runs when the worker completes, once.
        ready.on_ready(hook());
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        ready.complete();
        ready.complete();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // Registered after the worker was done: runs at once.
        ready.on_ready(hook());
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        // A hook that did not run yet is replaced by the next one.
        let fresh = Ready::default();
        fresh.on_ready(hook());
        fresh.on_ready(hook());
        fresh.complete();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        // Nothing registered: nothing to run.
        Ready::default().complete();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    /// A startup whose worker waits for the returned sender before it creates WARP devices.
    fn held_back() -> (DeviceStartup, Sender<()>) {
        let (release, released) = channel::<()>();
        let startup = DeviceStartup::spawn_with_factory(move || {
            let _ = released.recv();
            GfxDevices::create_on(Driver::Warp, 1)
        });
        (startup, release)
    }

    #[test]
    fn a_startup_tells_its_hook_and_is_polled_without_blocking() {
        let (startup, release) = held_back();
        let (told, heard) = channel::<()>();
        startup.notify_on_ready(move || {
            let _ = told.send(());
        });
        // The worker is held back: there is nothing to take yet.
        let (pending, done) = split(startup.poll());
        assert!(
            done.is_none(),
            "the devices cannot be there before the release"
        );
        let startup = pending.unwrap();
        release.send(()).unwrap();
        heard.recv_timeout(Duration::from_secs(60)).unwrap();
        // The hook runs after the hand-over: the devices are waiting.
        let (pending, done) = split(startup.poll());
        assert!(pending.is_none(), "the hook runs after the hand-over");
        assert_eq!(done.unwrap().unwrap().generation(), 1);
    }

    /// Polls `shared` until the worker is done with it.
    fn poll_until_done(shared: &SharedDevices) -> Result<GfxDevices, WinError> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(outcome) = shared.poll().transpose() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "the devices never arrived");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn shared_devices_are_polled_until_the_worker_has_them_and_stay_ready() {
        let (startup, release) = held_back();
        let shared = SharedDevices::new(startup);
        let other = shared.clone();
        assert!(shared.poll().unwrap().is_none(), "the worker is held back");
        release.send(()).unwrap();
        // Clones share one state: what one clone took, the other sees.
        assert_eq!(poll_until_done(&other).unwrap().generation(), 1);
        assert_eq!(shared.poll().unwrap().unwrap().generation(), 1);
        assert_eq!(shared.get().unwrap().generation(), 1);
        assert!(format!("{shared:?}").contains("Ready"));
    }

    #[test]
    fn getting_shared_devices_waits_and_existing_devices_are_ready_at_once() {
        let shared = SharedDevices::new(DeviceStartup::spawn_with(Driver::Warp));
        let devices = shared.get().unwrap();
        assert_eq!((devices.generation(), devices.driver()), (1, Driver::Warp));
        let existing = SharedDevices::from(devices);
        assert_eq!(existing.poll().unwrap().unwrap().generation(), 1);
    }

    #[test]
    fn a_failed_creation_is_reported_once_and_then_there_are_no_devices() {
        let failing = || {
            SharedDevices::new(DeviceStartup::spawn_with_factory(|| {
                Err(WinError::Invalid("no adapter"))
            }))
        };
        let polled = failing();
        assert_eq!(
            poll_until_done(&polled).unwrap_err().to_string(),
            "no adapter"
        );
        for error in [polled.poll().unwrap_err(), polled.get().unwrap_err()] {
            assert_eq!(error.to_string(), DEVICES_GONE);
        }
        // A blocking get sees the failure as well.
        assert_eq!(failing().get().unwrap_err().to_string(), "no adapter");
    }

    /// A top-level window of the system `STATIC` class that is never shown or activated.
    struct HiddenWindow(HWND);

    impl HiddenWindow {
        fn new() -> Self {
            // SAFETY: creates a window of a system class without `WS_VISIBLE`.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                    w!("STATIC"),
                    w!("ssw-win surface test"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    None,
                    None,
                )
            }
            .unwrap();
            Self(hwnd)
        }
    }

    impl Drop for HiddenWindow {
        fn drop(&mut self) {
            // SAFETY: destroys the window created in `new` on its own thread.
            let _ = unsafe { DestroyWindow(self.0) };
        }
    }

    #[test]
    fn a_window_surface_tracks_its_size_and_dpi_and_paints_a_frame() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let devices = GfxDevices::create(5).unwrap();
        let window = HiddenWindow::new();
        let mut surface = WindowSurface::new(&devices, window.0, 64, 48, 96.0).unwrap();
        assert_eq!(surface.generation(), 5);
        assert_eq!(surface.size_px(), (64, 48));
        // The same size changes nothing.
        surface.resize(64, 48).unwrap();
        assert_eq!(surface.size_px(), (64, 48));
        // A swap chain is never empty: zero becomes one pixel.
        surface.resize(80, 0).unwrap();
        assert_eq!(surface.size_px(), (80, 1));

        surface.set_dpi(144.0);
        let dpi = surface
            .paint(|target| {
                let (mut x, mut y) = (0.0, 0.0);
                // SAFETY: valid out values on the live render target.
                unsafe { target.GetDpi(&mut x, &mut y) };
                Ok((x, y))
            })
            .unwrap();
        assert_eq!(dpi, (144.0, 144.0));
        assert!(surface.context().cast::<ID2D1RenderTarget>().is_ok());
    }

    #[test]
    fn warp_devices_work_without_hardware() {
        let warp = DeviceStartup::spawn_with(Driver::Warp).wait().unwrap();
        assert_eq!(warp.driver(), Driver::Warp);
        let next = warp.recreate().unwrap();
        assert_eq!((next.generation(), next.driver()), (2, Driver::Warp));
    }

    #[test]
    fn wic_target_clears_and_reads_back_pixels() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let factory = d2d_factory(false).unwrap();
        assert!(WicTarget::new(&factory, &wic, 0, 4, 96.0).is_err());
        let target = WicTarget::new(&factory, &wic, 4, 3, 96.0).unwrap();
        assert_eq!(target.size_px(), (4, 3));
        assert_eq!(target.size_u().width, 4);
        target
            .paint(Some(Color::from_rgb_u32(0x336699)), |_| Ok(()))
            .unwrap();
        let image = target.pixels().unwrap();
        assert_eq!(image.pixel(3, 2), Some([0x99, 0x66, 0x33, 0xFF]));
        target.paint(None, |_| Ok(())).unwrap();
        assert_eq!(target.pixels().unwrap().pixel(0, 0), Some([0, 0, 0, 0]));
        let error = target
            .paint(None, |_| -> Result<(), WinError> {
                Err(WinError::Invalid("draw failed"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
        assert!(target.render_target().cast::<ID2D1RenderTarget>().is_ok());
    }

    #[test]
    fn a_panic_in_the_drawing_code_still_ends_the_draw() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let factory = d2d_factory(false).unwrap();
        let target = WicTarget::new(&factory, &wic, 4, 4, 96.0).unwrap();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = target.paint(
                Some(Color::from_rgb_u32(0xFF0000)),
                |_| -> Result<(), WinError> { panic!("drawing code failed") },
            );
        }));
        assert!(outcome.is_err());
        // The target is not stuck inside `BeginDraw`: the next frame is drawn and ended normally.
        target
            .paint(Some(Color::from_rgb_u32(0x00FF00)), |_| Ok(()))
            .unwrap();
        assert_eq!(
            target.pixels().unwrap().pixel(0, 0),
            Some([0x00, 0xFF, 0x00, 0xFF])
        );
    }

    #[test]
    fn a_drawing_error_of_a_window_surface_frame_is_the_result_of_the_frame() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let devices = GfxDevices::create(1).unwrap();
        let window = HiddenWindow::new();
        let mut surface = WindowSurface::new(&devices, window.0, 32, 32, 96.0).unwrap();
        let error = surface
            .paint(|_| -> Result<(), WinError> { Err(WinError::Invalid("draw failed")) })
            .unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
        // The frame was ended all the same: the next one is drawn and presented.
        assert_eq!(surface.paint(|_| Ok(3)).unwrap(), 3);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_creating_devices_is_reported_under_its_own_name() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let steps = crate::fault::sweep(|| failed_step(d2d_factory(false)));
        assert_eq!(steps, all_failed(&["create D2D factory"]));
        let steps = crate::fault::sweep(|| failed_step(GfxDevices::create_on(Driver::Warp, 1)));
        // The frame latency hint is best effort: its failure does not fail the creation.
        assert_eq!(
            steps,
            [
                Some("D3D11CreateDevice"),
                Some("query IDXGIDevice"),
                Some("create D2D factory"),
                None,
                Some("create D2D device"),
            ]
            .map(|step| step.map(str::to_owned))
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_failing_hardware_device_falls_back_to_warp_and_both_failing_is_an_error() {
        use crate::fault;

        let _com = ComGuard::apartment_threaded().unwrap();
        fault::arm(0);
        let devices = GfxDevices::create(7).unwrap();
        assert!(fault::tripped());
        assert_eq!((devices.generation(), devices.driver()), (7, Driver::Warp));

        fault::arm(0);
        let next = devices.recreate().unwrap();
        assert!(fault::tripped());
        assert_eq!((next.generation(), next.driver()), (8, Driver::Warp));

        fault::arm_from(0);
        assert_eq!(
            failed_step(GfxDevices::create(9)).as_deref(),
            Some("D3D11CreateDevice")
        );
        assert_eq!(
            failed_step(devices.recreate()).as_deref(),
            Some("D3D11CreateDevice")
        );
        fault::disarm();
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_building_a_window_surface_is_reported() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let devices = GfxDevices::create(5).unwrap();
        let window = HiddenWindow::new();
        let steps = crate::fault::sweep(|| {
            failed_step(WindowSurface::new(&devices, window.0, 64, 48, 96.0))
        });
        assert_eq!(
            steps,
            all_failed(&[
                "DXGI adapter",
                "DXGI factory",
                "create composition swap chain",
                "create DirectComposition device",
                "create composition target",
                "create composition visual",
                "set visual content",
                "set composition root",
                "commit composition",
                "create D2D device context",
            ])
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn a_surface_that_fails_to_resize_keeps_its_size_and_reports_a_lost_device() {
        use crate::fault;

        let _com = ComGuard::apartment_threaded().unwrap();
        let devices = GfxDevices::create(1).unwrap();
        let window = HiddenWindow::new();
        let mut surface = WindowSurface::new(&devices, window.0, 64, 48, 96.0).unwrap();
        fault::arm(0);
        let error = surface.resize(80, 60).unwrap_err();
        assert!(error.to_string().starts_with("resize swap chain failed"));
        assert!(!error.is_device_lost());
        assert_eq!(surface.size_px(), (64, 48));
        fault::arm_with(0, DXGI_ERROR_DEVICE_REMOVED);
        assert!(surface.resize(80, 60).unwrap_err().is_device_lost());
        assert_eq!(surface.size_px(), (64, 48));
        surface.resize(80, 60).unwrap();
        assert_eq!(surface.size_px(), (80, 60));
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_a_frame_is_reported_and_the_surface_keeps_working() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let devices = GfxDevices::create(1).unwrap();
        let window = HiddenWindow::new();
        let mut surface = WindowSurface::new(&devices, window.0, 64, 48, 96.0).unwrap();
        // One drawing closure for every frame below: the frame code is exercised on all its
        // paths in one instantiation.
        let broken = std::cell::Cell::new(false);
        let frame = |_: &ID2D1RenderTarget| -> Result<(), WinError> {
            if broken.get() {
                Err(WinError::Invalid("draw failed"))
            } else {
                Ok(())
            }
        };
        let steps = crate::fault::sweep(|| failed_step(surface.paint(frame)));
        assert_eq!(
            steps,
            all_failed(&[
                "query render target",
                "get back buffer",
                "wrap back buffer",
                "EndDraw",
                "Present",
            ])
        );
        broken.set(true);
        let error = surface.paint(frame).unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
        broken.set(false);
        surface.paint(frame).unwrap();
        assert_eq!(surface.paint(|_| Ok(1)).unwrap(), 1);
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_a_wic_target_is_reported() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let factory = d2d_factory(false).unwrap();
        let steps = crate::fault::sweep(|| failed_step(WicTarget::new(&factory, &wic, 4, 3, 96.0)));
        assert_eq!(
            steps,
            all_failed(&["create WIC bitmap", "create WIC render target"])
        );
        let target = WicTarget::new(&factory, &wic, 4, 3, 96.0).unwrap();
        // One drawing closure for every frame below: the frame code is exercised on all its
        // paths in one instantiation.
        let broken = std::cell::Cell::new(false);
        let frame = |_: &ID2D1RenderTarget| -> Result<(), WinError> {
            if broken.get() {
                Err(WinError::Invalid("draw failed"))
            } else {
                Ok(())
            }
        };
        let steps = crate::fault::sweep(|| failed_step(target.paint(None, frame)));
        assert_eq!(steps, all_failed(&["EndDraw"]));
        broken.set(true);
        let error = target.paint(None, frame).unwrap_err();
        assert_eq!(error.to_string(), "draw failed");
        broken.set(false);
        target.paint(None, frame).unwrap();
        let steps = crate::fault::sweep(|| failed_step(target.pixels()));
        assert_eq!(steps, all_failed(&["copy pixels"]));
    }
}
