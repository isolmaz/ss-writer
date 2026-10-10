//! Printing: pages from the Typst layout as vectors through `ID2D1PrintControl`.
//!
//! The pages are [`ssw_render::scene::Scene`]s (points, white paper included) from
//! `Exporter::layout_pages`. [`print_pages`] draws each one into a Direct2D command list with the
//! window canvas' own `draw_scene` path (the scene renderer installed in the
//! [`CanvasAssets`]) and hands it to the print control, so text and graphics stay vector all the
//! way to the printer; nothing is rasterized here.
//!
//! # Targets
//!
//! A print job needs an `IPrintDocumentPackageTarget`:
//! - [`printer_target`] asks the print system for a printer's target (the real print path);
//! - [`xps_file_target`] is a target that writes an XPS package to a file without any printer, so
//!   "Save as XPS" works and the tests need neither a printer nor a dialog.
//!
//! # The print dialog
//!
//! The system dialog (`PrintDlgEx`) sits behind [`PrintDialogHost`], like the file dialogs behind
//! `DialogHost`: [`SystemPrintDialogHost`] shows it, tests substitute a host that answers as the
//! user would. [`choose`] validates the answer, [`page_plan`] expands page ranges and copies and
//! [`print_selection`] prints the result.
//!
//! The device mode (orientation, duplex, paper tray) the dialog returns is not forwarded to the
//! job: the job uses the printer's default print ticket and the page size of the layout.

use std::ffi::c_void;
use std::ops::RangeInclusive;
use std::path::Path;
use std::sync::Arc;

use ssw_render::scene::Scene;
use ssw_ui::canvas::{Canvas, SceneId};
use ssw_ui::geometry::Rect;
use windows::Win32::Foundation::{
    E_INVALIDARG, E_OUTOFMEMORY, E_POINTER, GlobalFree, HGLOBAL, HWND,
};
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_F;
use windows::Win32::Graphics::Direct2D::{
    D2D1_COLOR_SPACE_SRGB, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_PRINT_CONTROL_PROPERTIES,
    D2D1_PRINT_FONT_SUBSET_MODE_DEFAULT, ID2D1DeviceContext, ID2D1Image, ID2D1PrintControl,
    ID2D1RenderTarget,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::Storage::Packaging::Opc::IOpcPartUri;
use windows::Win32::Storage::Xps::Printing::{
    ID_DOCUMENTPACKAGETARGET_MSXPS, IPrintDocumentPackageTarget, IPrintDocumentPackageTarget_Impl,
    IPrintDocumentPackageTargetFactory, PrintDocumentPackageTargetFactory,
};
use windows::Win32::Storage::Xps::{
    IXpsDocumentPackageTarget, IXpsDocumentPackageTarget_Impl, IXpsOMCoreProperties,
    IXpsOMImageResource, IXpsOMObjectFactory, IXpsOMPackageWriter, IXpsOMPrintTicketResource,
    XPS_DOCUMENT_TYPE, XPS_DOCUMENT_TYPE_XPS, XPS_INTERLEAVING_OFF, XpsOMObjectFactory,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc, IStream, STGM_CREATE, STGM_READWRITE,
    STGM_SHARE_DENY_WRITE,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::UI::Controls::Dialogs::{
    PD_ALLPAGES, PD_COLLATE, PD_NOCURRENTPAGE, PD_NOSELECTION, PD_PAGENUMS, PD_RESULT_PRINT,
    PRINTDLGEXW, PRINTPAGERANGE, PrintDlgExW, START_PAGE_GENERAL,
};
use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
use windows::core::{GUID, HSTRING, Interface, Ref, implement};

use crate::canvas::{CanvasAssets, D2dCanvas, TargetResources};
use crate::com::{OsResultExt, WinError};
use crate::gfx::GfxDevices;
use crate::wic::Wic;

/// Scene ids from here up are reserved for the pages of a print job while it runs.
pub const PRINT_SCENE_ID_BASE: u64 = 0x5353_5750_0000_0000;

/// DIPs (1/96 inch) per point (1/72 inch).
const DIPS_PER_POINT: f32 = 96.0 / 72.0;
/// Resolution for the parts of a page the printer cannot take as vectors.
const RASTER_DPI: f32 = 150.0;
/// Most page ranges the system dialog can return.
const MAX_RANGES: usize = 16;

/// What a print job draws with: the graphics devices, the image decoder and the canvas assets
/// (scene renderer, images, text engine).
#[derive(Debug)]
pub struct PrintSurface<'a> {
    /// The Direct2D device the print control is created on.
    pub devices: &'a GfxDevices,
    /// The imaging factory the print control rasterizes unsupported content with.
    pub wic: &'a Wic,
    /// The assets of the canvas that draws the pages; its scene renderer must be installed.
    pub assets: &'a mut CanvasAssets,
}

/// The size of a page scene in DIPs.
///
/// # Errors
/// Fails when the scene has no area.
pub fn page_size_dip(scene: &Scene) -> Result<(f32, f32), WinError> {
    let (width, height) = (scene.size.width, scene.size.height);
    if width > 0.0 && height > 0.0 && width.is_finite() && height.is_finite() {
        Ok((width * DIPS_PER_POINT, height * DIPS_PER_POINT))
    } else {
        Err(WinError::Invalid("a page has no size"))
    }
}

/// Prints `pages` into `target`: one page per scene, in order. Returns the number of pages
/// added. On failure the job is cancelled.
///
/// # Errors
/// Fails when a page has no size, the print control cannot be created, drawing fails (for
/// example without a scene renderer) or the print system rejects a page.
pub fn print_pages(
    surface: &mut PrintSurface<'_>,
    target: &IPrintDocumentPackageTarget,
    pages: &[Arc<Scene>],
) -> Result<usize, WinError> {
    let result = add_pages(surface, target, pages);
    if result.is_err() {
        // SAFETY: plain call on a live target; the job is already failing, so a failed cancel
        // adds nothing.
        let _ = unsafe { target.Cancel() };
    }
    result
}

fn add_pages(
    surface: &mut PrintSurface<'_>,
    target: &IPrintDocumentPackageTarget,
    pages: &[Arc<Scene>],
) -> Result<usize, WinError> {
    let sizes: Vec<(f32, f32)> = pages
        .iter()
        .map(|page| page_size_dip(page))
        .collect::<Result<_, _>>()?;
    let properties = D2D1_PRINT_CONTROL_PROPERTIES {
        fontSubset: D2D1_PRINT_FONT_SUBSET_MODE_DEFAULT,
        rasterDPI: RASTER_DPI,
        colorSpace: D2D1_COLOR_SPACE_SRGB,
    };
    let device = surface.devices.d2d_device();
    // SAFETY: live device, imaging factory and target; `properties` outlives the call.
    let control = unsafe {
        device.CreatePrintControl(surface.wic.factory(), target, Some(&raw const properties))
    }
    .ctx("create the print control")?;
    // SAFETY: live device.
    let context = unsafe { device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE) }
        .ctx("create the print device context")?;
    let mut resources = TargetResources::new(
        surface.devices.d2d_factory().clone(),
        surface.devices.generation(),
    );
    for (index, page) in pages.iter().enumerate() {
        surface
            .assets
            .register_scene(page_scene_id(index), Arc::clone(page));
    }
    let drawn = draw_pages(&context, &control, surface.assets, &mut resources, &sizes);
    for index in 0..pages.len() {
        surface.assets.remove_scene(page_scene_id(index));
    }
    drawn?;
    // SAFETY: plain call on the live print control; every page was added.
    unsafe { control.Close() }.ctx("close the print control")?;
    Ok(pages.len())
}

fn page_scene_id(index: usize) -> SceneId {
    SceneId(PRINT_SCENE_ID_BASE + index as u64)
}

fn draw_pages(
    context: &ID2D1DeviceContext,
    control: &ID2D1PrintControl,
    assets: &CanvasAssets,
    resources: &mut TargetResources,
    sizes: &[(f32, f32)],
) -> Result<(), WinError> {
    let render_target: ID2D1RenderTarget = context.cast().ctx("query the render target")?;
    for (index, &(width, height)) in sizes.iter().enumerate() {
        // SAFETY: live device context; the command list is closed before it is added and the
        // context's target is reset (and its draw ended) on every path, so no D2D reference to
        // the list outlives the page.
        unsafe {
            let list = context
                .CreateCommandList()
                .ctx("create the page command list")?;
            context.SetTarget(&list);
            context.BeginDraw();
            let mut canvas = D2dCanvas::new(&render_target, assets, resources);
            canvas.draw_scene(page_scene_id(index), Rect::new(0.0, 0.0, width, height));
            let drawn = canvas.finish();
            let ended = context.EndDraw(None, None);
            context.SetTarget(None::<&ID2D1Image>);
            drawn?;
            ended.ctx("end the page drawing")?;
            list.Close().ctx("close the page command list")?;
            control
                .AddPage(
                    &list,
                    D2D_SIZE_F { width, height },
                    None::<&IStream>,
                    None,
                    None,
                )
                .ctx("add the page")?;
        }
    }
    Ok(())
}

/// The print target of `printer` (a printer name as the system lists it) for a job called
/// `job_name`.
///
/// # Errors
/// Fails when the print system is unavailable or knows no such printer.
pub fn printer_target(
    printer: &str,
    job_name: &str,
) -> Result<IPrintDocumentPackageTarget, WinError> {
    // SAFETY: standard in-process COM activation.
    let factory: IPrintDocumentPackageTargetFactory = unsafe {
        CoCreateInstance(
            &PrintDocumentPackageTargetFactory,
            None,
            CLSCTX_INPROC_SERVER,
        )
    }
    .ctx("create the print target factory")?;
    // SAFETY: live factory; the strings outlive the call; no output stream or ticket means the
    // job goes to the printer with its default ticket.
    unsafe {
        factory.CreateDocumentPackageTargetForPrintJob(
            &HSTRING::from(printer),
            &HSTRING::from(job_name),
            None::<&IStream>,
            None::<&IStream>,
        )
    }
    .ctx("create the print job")
}

/// The XPS package a file target writes: the object Direct2D's print control talks to.
#[implement(IXpsDocumentPackageTarget)]
struct XpsPackage {
    output: IStream,
    factory: IXpsOMObjectFactory,
}

impl IXpsDocumentPackageTarget_Impl for XpsPackage_Impl {
    fn GetXpsOMPackageWriter(
        &self,
        documentsequencepartname: Ref<'_, IOpcPartUri>,
        discardcontrolpartname: Ref<'_, IOpcPartUri>,
    ) -> windows::core::Result<IXpsOMPackageWriter> {
        // SAFETY: live factory and stream; the part names are the objects the caller passed for
        // the duration of this call.
        unsafe {
            self.factory.CreatePackageWriterOnStream(
                &self.output,
                false,
                XPS_INTERLEAVING_OFF,
                documentsequencepartname.as_ref(),
                None::<&IXpsOMCoreProperties>,
                None::<&IXpsOMImageResource>,
                None::<&IXpsOMPrintTicketResource>,
                discardcontrolpartname.as_ref(),
            )
        }
    }

    fn GetXpsOMFactory(&self) -> windows::core::Result<IXpsOMObjectFactory> {
        Ok(self.factory.clone())
    }

    fn GetXpsType(&self) -> windows::core::Result<XPS_DOCUMENT_TYPE> {
        Ok(XPS_DOCUMENT_TYPE_XPS)
    }
}

/// A print target that writes an XPS package to a file stream.
#[implement(IPrintDocumentPackageTarget)]
struct XpsFileTarget {
    output: IStream,
    factory: IXpsOMObjectFactory,
}

impl IPrintDocumentPackageTarget_Impl for XpsFileTarget_Impl {
    fn GetPackageTargetTypes(
        &self,
        targetcount: *mut u32,
        targettypes: *mut *mut GUID,
    ) -> windows::core::Result<()> {
        if targetcount.is_null() || targettypes.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: both out pointers are non-null and valid for writes per the interface
        // contract; the array is CoTaskMem memory the caller frees.
        unsafe {
            let types = CoTaskMemAlloc(size_of::<GUID>()).cast::<GUID>();
            if types.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            types.write(ID_DOCUMENTPACKAGETARGET_MSXPS);
            targetcount.write(1);
            targettypes.write(types);
        }
        Ok(())
    }

    fn GetPackageTarget(
        &self,
        guidtargettype: *const GUID,
        riid: *const GUID,
        ppvtarget: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if guidtargettype.is_null() || riid.is_null() || ppvtarget.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: the pointers are non-null and valid per the interface contract.
        unsafe {
            ppvtarget.write(std::ptr::null_mut());
            if *guidtargettype != ID_DOCUMENTPACKAGETARGET_MSXPS {
                return Err(E_INVALIDARG.into());
            }
            let package: IXpsDocumentPackageTarget = XpsPackage {
                output: self.output.clone(),
                factory: self.factory.clone(),
            }
            .into();
            package.query(riid, ppvtarget).ok()
        }
    }

    fn Cancel(&self) -> windows::core::Result<()> {
        // Pages are written as they are added, so there is nothing to abort here; the caller
        // that owns the file removes it when the job failed.
        Ok(())
    }
}

/// A print target that writes an XPS package to `path` (created, replacing the file). Drop every
/// reference to the target and to the print control before reading the file: the stream is
/// released with them.
///
/// # Errors
/// Fails when the file cannot be created or the XPS object model is unavailable.
pub fn xps_file_target(path: &Path) -> Result<IPrintDocumentPackageTarget, WinError> {
    // SAFETY: `path` is converted to a NUL-terminated string that outlives the call.
    let output = unsafe {
        SHCreateStreamOnFileEx(
            &HSTRING::from(path.as_os_str()),
            (STGM_CREATE | STGM_READWRITE | STGM_SHARE_DENY_WRITE).0,
            FILE_ATTRIBUTE_NORMAL.0,
            true,
            None::<&IStream>,
        )
    }
    .ctx("create the XPS file")?;
    // SAFETY: standard in-process COM activation.
    let factory: IXpsOMObjectFactory =
        unsafe { CoCreateInstance(&XpsOMObjectFactory, None, CLSCTX_INPROC_SERVER) }
            .ctx("create the XPS object factory")?;
    Ok(XpsFileTarget { output, factory }.into())
}

/// Prints `pages` into an XPS file. A failed job leaves no file behind. Returns the number of
/// pages written.
///
/// # Errors
/// See [`xps_file_target`] and [`print_pages`].
pub fn print_to_xps_file(
    surface: &mut PrintSurface<'_>,
    path: &Path,
    pages: &[Arc<Scene>],
) -> Result<usize, WinError> {
    let written = {
        let target = xps_file_target(path)?;
        print_pages(surface, &target, pages)
    };
    if written.is_err() {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// What the print dialog is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintDialogRequest {
    /// The number of pages of the document (pages are numbered from 1).
    pub page_count: u32,
}

/// What the user chose in the print dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintSelection {
    /// The printer's name.
    pub printer: String,
    /// Copies (at least 1).
    pub copies: u16,
    /// Whether copies are printed document by document (1 2 3 1 2 3) rather than page by page
    /// (1 1 2 2 3 3).
    pub collate: bool,
    /// The pages to print, 1-based and inclusive, in order.
    pub ranges: Vec<RangeInclusive<u32>>,
}

/// The call that puts the print dialog on screen.
pub trait PrintDialogHost {
    /// Shows the dialog modally over `owner`; `None` when the user cancelled.
    ///
    /// # Errors
    /// The OS failed to show the dialog or to read its answer.
    fn show(
        &mut self,
        owner: HWND,
        request: &PrintDialogRequest,
    ) -> Result<Option<PrintSelection>, WinError>;
}

/// The real [`PrintDialogHost`]: `PrintDlgEx`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPrintDialogHost;

impl PrintDialogHost for SystemPrintDialogHost {
    fn show(
        &mut self,
        owner: HWND,
        request: &PrintDialogRequest,
    ) -> Result<Option<PrintSelection>, WinError> {
        let mut ranges = [PRINTPAGERANGE::default(); MAX_RANGES];
        ranges[0] = PRINTPAGERANGE {
            nFromPage: 1,
            nToPage: request.page_count,
        };
        let mut dialog = PRINTDLGEXW {
            lStructSize: size_of::<PRINTDLGEXW>() as u32,
            hwndOwner: owner,
            Flags: PD_ALLPAGES | PD_NOSELECTION | PD_NOCURRENTPAGE,
            nPageRanges: 1,
            nMaxPageRanges: MAX_RANGES as u32,
            lpPageRanges: ranges.as_mut_ptr(),
            nMinPage: 1,
            nMaxPage: request.page_count,
            nCopies: 1,
            nStartPage: START_PAGE_GENERAL,
            ..Default::default()
        };
        // SAFETY: `dialog` is initialized and points at `ranges`; both outlive the modal call.
        let shown = unsafe { PrintDlgExW(&mut dialog) };
        // The dialog allocates the device mode and names even when it fails half way.
        let names = take_global_words(dialog.hDevNames);
        free_global(dialog.hDevMode);
        shown.ctx("show the print dialog")?;
        if dialog.dwResultAction != PD_RESULT_PRINT {
            return Ok(None);
        }
        let printer = names
            .as_deref()
            .and_then(parse_devnames)
            .ok_or(WinError::Invalid("the print dialog returned no printer"))?;
        let used = (dialog.nPageRanges as usize).min(MAX_RANGES);
        Ok(Some(PrintSelection {
            printer,
            copies: u16::try_from(dialog.nCopies.max(1)).unwrap_or(u16::MAX),
            collate: dialog.Flags.0 & PD_COLLATE.0 != 0,
            ranges: dialog_ranges(
                dialog.Flags.0 & PD_PAGENUMS.0 != 0,
                &ranges[..used],
                request.page_count,
            ),
        }))
    }
}

/// Copies a global memory block as UTF-16 words and frees it.
fn take_global_words(handle: HGLOBAL) -> Option<Vec<u16>> {
    if handle.0.is_null() {
        return None;
    }
    // SAFETY: `handle` is a live global block the dialog allocated; it is locked only while the
    // words are copied (within `GlobalSize`) and freed afterwards.
    let words = unsafe {
        let pointer = GlobalLock(handle).cast::<u16>();
        let words = if pointer.is_null() {
            None
        } else {
            Some(
                std::slice::from_raw_parts(pointer, GlobalSize(handle) / size_of::<u16>()).to_vec(),
            )
        };
        let _ = GlobalUnlock(handle);
        words
    };
    free_global(handle);
    words
}

fn free_global(handle: HGLOBAL) {
    if !handle.0.is_null() {
        // SAFETY: the dialog handed its allocation over to the caller, who frees it once.
        let _ = unsafe { GlobalFree(Some(handle)) };
    }
}

/// The printer (device) name in a `DEVNAMES` block read as UTF-16 words: four header words
/// (driver, device, port and default-flag offsets, in words) and the NUL-terminated strings.
#[must_use]
pub fn parse_devnames(words: &[u16]) -> Option<String> {
    let offset = usize::from(*words.get(1)?);
    let tail = words.get(offset..)?;
    let end = tail.iter().position(|&word| word == 0)?;
    (end > 0).then(|| String::from_utf16_lossy(&tail[..end]))
}

/// The page ranges of a dialog answer: `ranges` when the user typed page numbers, otherwise all
/// pages. Reversed ranges are put in order.
#[must_use]
pub fn dialog_ranges(
    page_numbers: bool,
    ranges: &[PRINTPAGERANGE],
    page_count: u32,
) -> Vec<RangeInclusive<u32>> {
    if page_numbers {
        ranges
            .iter()
            .map(|range| range.nFromPage.min(range.nToPage)..=range.nFromPage.max(range.nToPage))
            .collect()
    } else {
        vec![1..=page_count]
    }
}

/// Asks `host` which printer, pages and copies to use for a document of `page_count` pages.
/// Ranges are clipped to the document and copies are at least 1. `None` when the user cancelled.
///
/// # Errors
/// Fails when there are no pages, the dialog fails or none of the chosen pages exist.
pub fn choose<H: PrintDialogHost>(
    host: &mut H,
    owner: HWND,
    page_count: u32,
) -> Result<Option<PrintSelection>, WinError> {
    if page_count == 0 {
        return Err(WinError::Invalid("there are no pages to print"));
    }
    let Some(mut selection) = host.show(owner, &PrintDialogRequest { page_count })? else {
        return Ok(None);
    };
    selection.copies = selection.copies.max(1);
    selection.ranges = selection
        .ranges
        .iter()
        .filter_map(|range| {
            let (start, end) = ((*range.start()).max(1), (*range.end()).min(page_count));
            (start <= end).then_some(start..=end)
        })
        .collect();
    if selection.ranges.is_empty() {
        return Err(WinError::Invalid("the selected pages do not exist"));
    }
    Ok(Some(selection))
}

/// The pages (0-based indices into the document) to print, in print order, with copies
/// expanded: collated copies repeat the whole selection, uncollated copies repeat each page.
/// Pages beyond `page_count` are dropped.
#[must_use]
pub fn page_plan(selection: &PrintSelection, page_count: usize) -> Vec<usize> {
    let pages: Vec<usize> = selection
        .ranges
        .iter()
        .flat_map(|range| range.clone())
        .filter_map(|page| usize::try_from(page).ok())
        .filter(|&page| (1..=page_count).contains(&page))
        .map(|page| page - 1)
        .collect();
    let copies = usize::from(selection.copies.max(1));
    if selection.collate {
        (0..copies).flat_map(|_| pages.iter().copied()).collect()
    } else {
        pages
            .iter()
            .flat_map(|&page| std::iter::repeat_n(page, copies))
            .collect()
    }
}

/// Prints the chosen pages of `pages` to the chosen printer. Returns the number of pages sent
/// (copies included).
///
/// # Errors
/// Fails when the printer is unknown or printing fails; see [`printer_target`] and
/// [`print_pages`].
pub fn print_selection(
    surface: &mut PrintSurface<'_>,
    selection: &PrintSelection,
    job_name: &str,
    pages: Vec<Scene>,
) -> Result<usize, WinError> {
    let pages: Vec<Arc<Scene>> = pages.into_iter().map(Arc::new).collect();
    let plan: Vec<Arc<Scene>> = page_plan(selection, pages.len())
        .into_iter()
        .map(|index| Arc::clone(&pages[index]))
        .collect();
    let target = printer_target(&selection.printer, job_name)?;
    print_pages(surface, &target, &plan)
}

/// The whole of `Ctrl+P`: shows the dialog through `host` and prints the chosen pages. `None`
/// when the user cancelled.
///
/// # Errors
/// See [`choose`] and [`print_selection`].
pub fn print_with_dialog<H: PrintDialogHost>(
    host: &mut H,
    owner: HWND,
    surface: &mut PrintSurface<'_>,
    job_name: &str,
    pages: Vec<Scene>,
) -> Result<Option<usize>, WinError> {
    let page_count = u32::try_from(pages.len()).unwrap_or(u32::MAX);
    match choose(host, owner, page_count)? {
        Some(selection) => print_selection(surface, &selection, job_name, pages).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc};

    /// A moveable global block holding `words`.
    fn global_words(words: &[u16]) -> HGLOBAL {
        let bytes = std::mem::size_of_val(words);
        // SAFETY: a fresh block of `bytes` bytes is locked, filled with exactly that many bytes
        // and unlocked.
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes).expect("allocation");
            let pointer = GlobalLock(handle).cast::<u16>();
            assert!(!pointer.is_null());
            std::ptr::copy_nonoverlapping(words.as_ptr(), pointer, words.len());
            let _ = GlobalUnlock(handle);
            handle
        }
    }

    #[test]
    fn global_blocks_are_copied_and_freed() {
        let words = [4u16, 4, 9, 0, 0x50, 0x44, 0x46, 0];
        let words_read = take_global_words(global_words(&words)).expect("a block with words");
        // The block may be rounded up to the allocator's granularity: the words come first.
        assert_eq!(&words_read[..words.len()], &words);
        // No block, no words (and nothing to free).
        assert_eq!(take_global_words(HGLOBAL::default()), None);
        free_global(HGLOBAL::default());
        // A block that is only freed.
        free_global(global_words(&[1, 2, 3]));
    }
}
