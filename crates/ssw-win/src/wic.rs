//! WIC image decoding and PNG encoding.

use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, GUID_WICPixelFormat32bppBGRA,
    GUID_WICPixelFormat32bppPBGRA, IWICBitmap, IWICBitmapFrameDecode, IWICBitmapSource,
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapEncoderNoCache,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
use windows::Win32::System::Com::StructuredStorage::{
    CreateStreamOnHGlobal, GetHGlobalFromStream, IPropertyBag2,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, IStream, STATFLAG_NONAME, STATSTG,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::{GUID, Interface};

use crate::com::{OsResultExt, WinError, delivered};
use crate::fault::check;

/// Pixels of a decoded image: 32-bit BGRA with premultiplied alpha, rows top to bottom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl DecodedImage {
    /// Bytes per row.
    #[must_use]
    pub fn stride(&self) -> u32 {
        self.width * 4
    }

    /// The pixel at `(x, y)` as `[b, g, r, a]` (premultiplied), or `None` out of range.
    #[must_use]
    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = (y * self.stride() + x * 4) as usize;
        let bytes = self.bgra.get(offset..offset + 4)?;
        Some([bytes[0], bytes[1], bytes[2], bytes[3]])
    }
}

/// The WIC imaging factory.
#[derive(Debug, Clone)]
pub struct Wic {
    factory: IWICImagingFactory,
}

impl Wic {
    /// Creates the factory; COM must be initialized on the calling thread.
    ///
    /// # Errors
    /// Fails when WIC is unavailable.
    pub fn new() -> Result<Self, WinError> {
        // SAFETY: standard in-process COM activation of the WIC factory.
        let factory = check(unsafe {
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
        })
        .ctx("create WIC factory")?;
        Ok(Self { factory })
    }

    /// The underlying factory.
    #[must_use]
    pub fn factory(&self) -> &IWICImagingFactory {
        &self.factory
    }

    /// Decodes any WIC-supported format (PNG, JPEG, GIF, BMP, ICO, WebP when the codec is
    /// installed, ...) into premultiplied BGRA.
    ///
    /// # Errors
    /// Fails when the bytes are not a supported image.
    pub fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, WinError> {
        // SAFETY: the stream only borrows `bytes`, which outlives every use below; all COM
        // calls receive valid interface pointers and correctly sized buffers.
        unsafe {
            let stream = check(self.factory.CreateStream()).ctx("WIC CreateStream")?;
            check(stream.InitializeFromMemory(bytes)).ctx("WIC stream from memory")?;
            let decoder = check(self.factory.CreateDecoderFromStream(
                &stream,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            ))
            .ctx("WIC decoder")?;
            let frame = check(decoder.GetFrame(0)).ctx("WIC frame")?;
            self.convert_frame(&frame)
        }
    }

    /// # Safety
    /// `frame` must be a valid decoded frame.
    unsafe fn convert_frame(
        &self,
        frame: &IWICBitmapFrameDecode,
    ) -> Result<DecodedImage, WinError> {
        // SAFETY: see the function contract; buffers are sized from the reported dimensions.
        unsafe {
            let converter =
                check(self.factory.CreateFormatConverter()).ctx("WIC format converter")?;
            check(converter.Initialize(
                frame,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            ))
            .ctx("WIC convert to PBGRA")?;
            let (mut width, mut height) = (0u32, 0u32);
            check(converter.GetSize(&mut width, &mut height)).ctx("WIC GetSize")?;
            let mut bgra = vec![0u8; (width as usize) * (height as usize) * 4];
            check(converter.CopyPixels(std::ptr::null(), width * 4, &mut bgra))
                .ctx("WIC CopyPixels")?;
            Ok(DecodedImage {
                width,
                height,
                bgra,
            })
        }
    }

    /// A WIC bitmap holding premultiplied BGRA pixels (`width * height * 4` bytes).
    ///
    /// # Errors
    /// Fails when the pixel buffer length does not match or WIC refuses the bitmap.
    pub fn bitmap_from_bgra(
        &self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Result<IWICBitmap, WinError> {
        if bgra.len() != (width as usize) * (height as usize) * 4 {
            return Err(WinError::Invalid(
                "pixel buffer does not match the image size",
            ));
        }
        // SAFETY: `bgra` is valid for the stated stride and height; WIC copies it.
        unsafe {
            self.factory
                .CreateBitmapFromMemory(
                    width,
                    height,
                    &GUID_WICPixelFormat32bppPBGRA,
                    width * 4,
                    bgra,
                )
                .ctx("WIC bitmap from memory")
        }
    }

    /// Encodes premultiplied BGRA pixels as PNG.
    ///
    /// # Errors
    /// Fails when the image is empty, inconsistent or the encoder fails.
    pub fn encode_png(&self, image: &DecodedImage) -> Result<Vec<u8>, WinError> {
        if image.width == 0
            || image.height == 0
            || image.bgra.len() != (image.width as usize) * (image.height as usize) * 4
        {
            return Err(WinError::Invalid(
                "cannot encode an empty or inconsistent image",
            ));
        }
        let straight = unpremultiply(&image.bgra);
        // SAFETY: the encoder writes into a COM-owned memory stream; pixel buffers are sized
        // from the image dimensions; the HGLOBAL is locked only while copied out.
        unsafe {
            let stream: IStream =
                check(CreateStreamOnHGlobal(Default::default(), true)).ctx("PNG memory stream")?;
            let encoder = check(
                self.factory
                    .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null()),
            )
            .ctx("PNG encoder")?;
            check(encoder.Initialize(&stream, WICBitmapEncoderNoCache)).ctx("PNG encoder init")?;
            let mut frame = None;
            let mut options: Option<IPropertyBag2> = None;
            let created = check(encoder.CreateNewFrame(&mut frame, &mut options));
            let frame = delivered(created, frame).ctx("PNG new frame")?;
            check(frame.Initialize(options.as_ref())).ctx("PNG frame init")?;
            check(frame.SetSize(image.width, image.height)).ctx("PNG frame size")?;
            let mut format: GUID = GUID_WICPixelFormat32bppBGRA;
            check(frame.SetPixelFormat(&mut format)).ctx("PNG pixel format")?;
            check(frame.WritePixels(image.height, image.width * 4, &straight))
                .ctx("PNG write pixels")?;
            check(frame.Commit()).ctx("PNG frame commit")?;
            check(encoder.Commit()).ctx("PNG commit")?;
            read_stream(&stream)
        }
    }
}

/// Reads the whole memory stream.
///
/// # Safety
/// `stream` must be an `IStream` created by `CreateStreamOnHGlobal`.
unsafe fn read_stream(stream: &IStream) -> Result<Vec<u8>, WinError> {
    // SAFETY: see the function contract; the HGLOBAL is locked only for the copy, and the copy
    // length is the stream's own size, which never exceeds the allocation.
    unsafe {
        let mut stat = STATSTG::default();
        check(stream.Stat(&mut stat, STATFLAG_NONAME)).ctx("stream stat")?;
        let hglobal = check(GetHGlobalFromStream(stream)).ctx("stream HGLOBAL")?;
        if (GlobalSize(hglobal) as u64) < stat.cbSize {
            return Err(WinError::Invalid(
                "memory stream is shorter than its length",
            ));
        }
        // The block holds that many bytes, so the length fits `usize`.
        let length = stat.cbSize as usize;
        let pointer = GlobalLock(hglobal) as *const u8;
        if pointer.is_null() {
            return Err(WinError::Invalid("memory stream could not be locked"));
        }
        let bytes = std::slice::from_raw_parts(pointer, length).to_vec();
        let _ = GlobalUnlock(hglobal);
        Ok(bytes)
    }
}

/// Converts premultiplied BGRA to straight-alpha BGRA.
#[must_use]
pub fn unpremultiply(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for pixel in out.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha != 0 && alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    out
}

/// Converts straight-alpha BGRA to premultiplied BGRA.
#[must_use]
pub fn premultiply(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for pixel in out.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha != 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
            }
        }
    }
    out
}

/// The source interface of a WIC bitmap, for `CreateBitmapFromWicBitmap`.
#[must_use]
pub fn as_source(bitmap: &IWICBitmap) -> Option<IWICBitmapSource> {
    bitmap.cast().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;

    fn sample() -> DecodedImage {
        // 2x2: opaque red, half-transparent green (premultiplied), transparent, opaque blue.
        DecodedImage {
            width: 2,
            height: 2,
            bgra: vec![
                0, 0, 255, 255, //
                0, 64, 0, 128, //
                0, 0, 0, 0, //
                255, 0, 0, 255,
            ],
        }
    }

    #[test]
    fn png_round_trips_premultiplied_pixels() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let image = sample();
        let png = wic.encode_png(&image).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(
            png.ends_with(&[0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82]),
            "ends with IEND"
        );
        let decoded = wic.decode(&png).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.pixel(0, 0), Some([0, 0, 255, 255]));
        assert_eq!(decoded.pixel(1, 1), Some([255, 0, 0, 255]));
        assert_eq!(decoded.pixel(0, 1), Some([0, 0, 0, 0]));
        let green = decoded.pixel(1, 0).unwrap();
        assert_eq!(green[3], 128);
        assert!((i32::from(green[1]) - 64).abs() <= 1, "{green:?}");
        assert_eq!(decoded.pixel(2, 0), None);
        assert_eq!(decoded.pixel(0, 2), None);
    }

    #[test]
    fn invalid_inputs_are_rejected() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        assert!(wic.decode(b"definitely not an image").is_err());
        let empty = DecodedImage {
            width: 0,
            height: 0,
            bgra: Vec::new(),
        };
        let refused = "cannot encode an empty or inconsistent image";
        assert_eq!(wic.encode_png(&empty).unwrap_err().to_string(), refused);
        let short = DecodedImage {
            width: 2,
            height: 2,
            bgra: vec![0; 4],
        };
        assert_eq!(wic.encode_png(&short).unwrap_err().to_string(), refused);
        assert_eq!(
            wic.bitmap_from_bgra(2, 2, &[0; 3]).unwrap_err().to_string(),
            "pixel buffer does not match the image size"
        );
    }

    #[test]
    fn a_pixel_the_buffer_does_not_hold_is_none() {
        // The size claims 2x2 pixels; the buffer holds only the first one.
        let truncated = DecodedImage {
            width: 2,
            height: 2,
            bgra: vec![1, 2, 3, 4],
        };
        assert_eq!(truncated.pixel(0, 0), Some([1, 2, 3, 4]));
        assert_eq!(truncated.pixel(1, 1), None);
    }

    #[test]
    fn a_memory_stream_whose_block_cannot_be_read_is_refused() {
        use windows::Win32::Foundation::{GlobalFree, HGLOBAL};
        use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalReAlloc};

        let _com = ComGuard::apartment_threaded().unwrap();
        // An empty stream owns a block of no bytes, which cannot be locked.
        // SAFETY: a null handle makes the stream allocate its own block, freed with the stream.
        let empty = unsafe { CreateStreamOnHGlobal(HGLOBAL::default(), true) }.unwrap();
        // SAFETY: `empty` was created by `CreateStreamOnHGlobal`.
        let error = unsafe { read_stream(&empty) }.unwrap_err();
        assert_eq!(error.to_string(), "memory stream could not be locked");

        // SAFETY: the block is the caller's (`false`): the stream never frees it; it is freed
        // below after the stream is gone. It shrinks behind the stream's back, which still
        // reports the old length.
        let (stream, block) = unsafe {
            let block = GlobalAlloc(GMEM_MOVEABLE, 256).unwrap();
            let stream = CreateStreamOnHGlobal(block, false).unwrap();
            (stream, GlobalReAlloc(block, 1, GMEM_MOVEABLE.0).unwrap())
        };
        // SAFETY: `stream` was created by `CreateStreamOnHGlobal`.
        let error = unsafe { read_stream(&stream) }.unwrap_err();
        assert_eq!(
            error.to_string(),
            "memory stream is shorter than its length"
        );
        drop(stream);
        // SAFETY: `block` is the live allocation made above, freed once.
        // (`GlobalFree` returns NULL on success, which its `Result` wrapper reports as an error.)
        let _ = unsafe { GlobalFree(Some(block)) };
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn every_failing_step_of_the_wic_operations_is_reported_under_its_own_name() {
        use crate::fault::{all_failed, failed_step, sweep};

        let _com = ComGuard::apartment_threaded().unwrap();
        let steps = sweep(|| failed_step(Wic::new()));
        assert_eq!(steps, all_failed(&["create WIC factory"]));

        let wic = Wic::new().unwrap();
        let image = sample();
        let png = wic.encode_png(&image).unwrap();
        let steps = sweep(|| failed_step(wic.decode(&png)));
        assert_eq!(
            steps,
            all_failed(&[
                "WIC CreateStream",
                "WIC stream from memory",
                "WIC decoder",
                "WIC frame",
                "WIC format converter",
                "WIC convert to PBGRA",
                "WIC GetSize",
                "WIC CopyPixels",
            ])
        );
        let steps = sweep(|| failed_step(wic.encode_png(&image)));
        assert_eq!(
            steps,
            all_failed(&[
                "PNG memory stream",
                "PNG encoder",
                "PNG encoder init",
                "PNG new frame",
                "PNG frame init",
                "PNG frame size",
                "PNG pixel format",
                "PNG write pixels",
                "PNG frame commit",
                "PNG commit",
                "stream stat",
                "stream HGLOBAL",
            ])
        );
        // The operations are unharmed by the failures they survived.
        assert_eq!(
            wic.decode(&png).unwrap().pixel(0, 0),
            Some([0, 0, 255, 255])
        );
    }

    #[test]
    fn bitmaps_wrap_pixels() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let wic = Wic::new().unwrap();
        let image = sample();
        let bitmap = wic.bitmap_from_bgra(2, 2, &image.bgra).unwrap();
        assert!(as_source(&bitmap).is_some());
        assert!(wic.factory().cast::<IWICImagingFactory>().is_ok());
    }

    #[test]
    fn alpha_conversions_are_inverse_within_rounding() {
        let straight = [10u8, 200, 100, 128, 5, 5, 5, 255, 9, 9, 9, 0];
        let premultiplied = premultiply(&straight);
        assert_eq!(&premultiplied[4..8], &[5, 5, 5, 255]);
        assert_eq!(&premultiplied[8..12], &[0, 0, 0, 0]);
        assert_eq!(premultiplied[3], 128);
        assert_eq!(premultiplied[0], 5);
        assert_eq!(premultiplied[1], 100);
        let back = unpremultiply(&premultiplied);
        assert!((i32::from(back[1]) - 200).abs() <= 2);
        assert_eq!(&back[4..8], &[5, 5, 5, 255]);
        assert_eq!(&back[8..12], &[0, 0, 0, 0]);
    }
}
