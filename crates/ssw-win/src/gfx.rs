//! Graphics devices and render targets.
//!
//! - [`GfxDevices`]: the D3D11 (BGRA) device, DXGI device, D2D factory and D2D device. Created
//!   off the UI thread by [`DeviceStartup::spawn`] (device creation takes 200+ ms) and handed
//!   over through a channel. A generation counter identifies a device incarnation; after a
//!   device loss [`GfxDevices::recreate`] produces the next generation and every dependent
//!   resource is rebuilt.
//! - [`WindowSurface`]: a per-window DirectComposition visual showing a premultiplied flip
//!   swap chain, drawn through a D2D device context.
//! - [`WicTarget`]: a WIC bitmap render target for tests and export (same `Canvas`).

use std::mem::ManuallyDrop;
use std::sync::mpsc::{Receiver, channel};
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

use crate::com::{ComGuard, OsResultExt, WinError, is_device_lost};
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
    unsafe { D2D1CreateFactory::<ID2D1Factory1>(kind, None) }.ctx("create D2D factory")
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
        unsafe {
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
        }
        .ctx("D3D11CreateDevice")?;
        let d3d = device.ok_or(WinError::Invalid("D3D11CreateDevice returned no device"))?;
        let dxgi: IDXGIDevice = d3d.cast().ctx("query IDXGIDevice")?;
        let factory = d2d_factory(true)?;
        // SAFETY: valid DXGI device; the latency hint is best effort.
        unsafe {
            if let Ok(device1) = dxgi.cast::<IDXGIDevice1>() {
                let _ = device1.SetMaximumFrameLatency(1);
            }
        }
        // SAFETY: valid factory and DXGI device.
        let device = unsafe { factory.CreateDevice(&dxgi) }.ctx("create D2D device")?;
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
        match unsafe { self.d3d.GetDeviceRemovedReason() } {
            Ok(()) => HRESULT(0),
            Err(error) => error.code(),
        }
    }
}

/// Hand-over wrapper for the worker-created devices.
struct Handoff(Result<GfxDevices, WinError>);

// SAFETY: the D3D11 device, DXGI device and a multi-threaded D2D factory/device are free
// threaded; the worker drops its references before the receiver can observe the value and
// never touches the objects again, so ownership moves cleanly to the receiving thread.
unsafe impl Send for Handoff {}

/// Device creation running on a worker thread.
#[derive(Debug)]
pub struct DeviceStartup {
    receiver: Receiver<Handoff>,
    worker: JoinHandle<()>,
}

impl std::fmt::Debug for Handoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handoff")
    }
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
        let (sender, receiver) = channel();
        let worker = std::thread::spawn(move || {
            let result = ComGuard::multi_threaded().and_then(|_com| {
                if driver == Driver::Hardware {
                    GfxDevices::create(1)
                } else {
                    GfxDevices::create_on(driver, 1)
                }
            });
            // The receiver may be gone when the process shuts down early.
            let _ = sender.send(Handoff(result));
        });
        Self { receiver, worker }
    }

    /// Blocks until the devices are ready.
    ///
    /// # Errors
    /// Returns the creation error, or [`WinError::Invalid`] when the worker died.
    pub fn wait(self) -> Result<GfxDevices, WinError> {
        let received = self.receiver.recv();
        let _ = self.worker.join();
        match received {
            Ok(Handoff(result)) => result,
            Err(_) => Err(WinError::Invalid("device thread ended without a result")),
        }
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
            let adapter = devices.dxgi.GetAdapter().ctx("DXGI adapter")?;
            let factory: IDXGIFactory2 = adapter.GetParent().ctx("DXGI factory")?;
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
            let swap = factory
                .CreateSwapChainForComposition(&devices.d3d, &desc, None::<&IDXGIOutput>)
                .ctx("create composition swap chain")?;
            let dcomp: IDCompositionDevice =
                DCompositionCreateDevice(&devices.dxgi).ctx("create DirectComposition device")?;
            let target = dcomp
                .CreateTargetForHwnd(hwnd, true)
                .ctx("create composition target")?;
            let visual = dcomp.CreateVisual().ctx("create composition visual")?;
            visual.SetContent(&swap).ctx("set visual content")?;
            target.SetRoot(&visual).ctx("set composition root")?;
            dcomp.Commit().ctx("commit composition")?;
            let context = devices
                .device
                .CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)
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
            self.swap
                .ResizeBuffers(
                    0,
                    width,
                    height,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )
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
        // SAFETY: the back buffer is wrapped for the duration of the frame only: `BackBuffer`
        // resets the target before `Present` (or when the drawing code unwinds), and
        // `DrawScope` always pairs `BeginDraw` with `EndDraw`.
        unsafe {
            // Fallible lookups first, so no error can leave the context half set up.
            let target: ID2D1RenderTarget = self.context.cast().ctx("query render target")?;
            let surface: IDXGISurface = self.swap.GetBuffer(0).ctx("get back buffer")?;
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
            let bitmap = self
                .context
                .CreateBitmapFromDxgiSurface(&surface, Some(&properties))
                .ctx("wrap back buffer")?;
            self.context.SetTarget(&bitmap);
            let back_buffer = BackBuffer(&self.context);
            self.context.SetDpi(self.dpi, self.dpi);
            let scope = DrawScope::begin(&target);
            target.Clear(Some(&clear_color(Color::TRANSPARENT)));
            let drawn = draw(&target);
            let ended = scope.end();
            drop(back_buffer);
            let value = drawn?;
            ended.ctx("EndDraw")?;
            let status = self.swap.Present(1, DXGI_PRESENT(0));
            classify_present(status)?;
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
        unsafe { self.target.EndDraw(None, None) }
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
    if status.is_ok() {
        Ok(())
    } else if is_device_lost(status) {
        Err(WinError::DeviceLost { context: "Present" })
    } else {
        Err(WinError::from_os("Present", status.into()))
    }
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
            let bitmap = wic
                .factory()
                .CreateBitmap(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapCacheOnDemand,
                )
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
            let target = factory
                .CreateWicBitmapRenderTarget(&bitmap, &properties)
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
            self.bitmap
                .CopyPixels(std::ptr::null(), self.width * 4, &mut bgra)
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
    use windows::Win32::Foundation::E_FAIL;
    use windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;

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
}
