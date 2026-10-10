//! Printing through `ID2D1PrintControl` into an XPS file target: the produced package is read
//! back as the OPC zip it is and its pages are counted and measured. The print dialog is a fake
//! host; nothing here shows a window, uses a printer or touches the user's print queue.

use std::ffi::c_void;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;

use ssw_render::scene::{
    Color as SceneColor, Fill, FillRule, Paint, Rect as SceneRect, Scene, SceneItem,
    Size as SceneSize,
};
use ssw_ui::canvas::SceneId;
use ssw_ui::geometry::Rect;
use ssw_win::canvas::{CanvasAssets, ImageStore, SceneRenderer};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{Driver, GfxDevices};
use ssw_win::print::{
    PRINT_SCENE_ID_BASE, PrintDialogHost, PrintDialogRequest, PrintSelection, PrintSurface, choose,
    dialog_ranges, page_plan, page_size_dip, parse_devnames, print_pages, print_selection_to,
    print_to_xps_file, print_with_dialog, printer_target, xps_file_target,
};
use ssw_win::wic::Wic;
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, HWND};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::Direct2D::ID2D1RenderTarget;
use windows::Win32::Storage::Xps::Printing::{
    ID_DOCUMENTPACKAGETARGET_MSXPS, ID_DOCUMENTPACKAGETARGET_OPENXPS,
};
use windows::Win32::Storage::Xps::{IXpsDocumentPackageTarget, XPS_DOCUMENT_TYPE_XPS};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Controls::Dialogs::PRINTPAGERANGE;
use windows::core::{GUID, Interface};

/// Fills the solid rectangles of a scene, scaled from the scene's size into the destination,
/// with plain D2D calls on the target it is handed (the print command list's context).
struct RectRenderer;

impl SceneRenderer for RectRenderer {
    fn draw(&self, target: &ID2D1RenderTarget, scene: &Scene, dest: Rect) -> Result<(), WinError> {
        let (sx, sy) = (
            dest.width / scene.size.width,
            dest.height / scene.size.height,
        );
        for item in &scene.items {
            let SceneItem::Fill(fill) = item else {
                continue;
            };
            let Paint::Solid(color) = &fill.paint else {
                continue;
            };
            let Some(bounds) = fill.path.bounds() else {
                continue;
            };
            let d2d = D2D1_COLOR_F {
                r: f32::from(color.r) / 255.0,
                g: f32::from(color.g) / 255.0,
                b: f32::from(color.b) / 255.0,
                a: f32::from(color.a) / 255.0,
            };
            // SAFETY: valid render target inside a frame and valid color / rectangle pointers.
            unsafe {
                let brush = target
                    .CreateSolidColorBrush(&d2d, None)
                    .map_err(|error| WinError::from_os("scene brush", error))?;
                let rect = D2D_RECT_F {
                    left: dest.x + bounds.x * sx,
                    top: dest.y + bounds.y * sy,
                    right: dest.x + (bounds.x + bounds.width) * sx,
                    bottom: dest.y + (bounds.y + bounds.height) * sy,
                };
                target.FillRectangle(&rect, &brush);
            }
        }
        Ok(())
    }
}

/// A page of `width` x `height` points with one red rectangle.
fn page(width: f32, height: f32) -> Arc<Scene> {
    let mut scene = Scene::new(SceneSize::new(width, height));
    scene.push(SceneItem::Fill(Fill {
        path: SceneRect::new(50.0, 50.0, 200.0, 100.0).to_path(),
        rule: FillRule::NonZero,
        paint: Paint::Solid(SceneColor::rgb(255, 0, 0)),
    }));
    Arc::new(scene)
}

struct Rig {
    devices: GfxDevices,
    wic: Wic,
    assets: CanvasAssets,
    /// Declared last so it drops last: COM objects must be released before COM is torn down.
    _com: ComGuard,
}

fn rig(with_renderer: bool) -> Rig {
    let com = ComGuard::apartment_threaded().unwrap();
    let devices = GfxDevices::create_on(Driver::Warp, 1).unwrap();
    let wic = Wic::new().unwrap();
    let engine = DWriteEngine::new().unwrap();
    let mut assets = CanvasAssets::new(ImageStore::new(wic.clone()), engine);
    if with_renderer {
        assets.set_scene_renderer(Box::new(RectRenderer));
    }
    Rig {
        _com: com,
        devices,
        wic,
        assets,
    }
}

impl Rig {
    fn surface(&mut self) -> PrintSurface<'_> {
        PrintSurface {
            devices: &self.devices,
            wic: &self.wic,
            assets: &mut self.assets,
        }
    }
}

/// A file in the temp directory that is removed when the value drops.
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("ssw-print-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        TempFile(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Every part of an XPS package: `(name, bytes)`.
fn package_parts(path: &Path) -> Vec<(String, Vec<u8>)> {
    let file = File::open(path).expect("the XPS file exists");
    let mut archive = zip::ZipArchive::new(file).expect("the XPS file is an OPC zip package");
    (0..archive.len())
        .map(|index| {
            let mut entry = archive.by_index(index).unwrap();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            (entry.name().expect("a valid part name").into_owned(), bytes)
        })
        .collect()
}

/// The XML text of a package part (UTF-8 or UTF-16 with a byte order mark).
fn markup(bytes: &[u8]) -> String {
    if let Some(utf16) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = utf16
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The numeric value of attribute `name` on the first `<FixedPage` element.
fn page_attribute(page_markup: &str, name: &str) -> f64 {
    let element = &page_markup[page_markup
        .find("<FixedPage")
        .unwrap_or_else(|| panic!("no FixedPage element in {page_markup}"))..];
    let marker = format!(" {name}=\"");
    let start = element
        .find(&marker)
        .unwrap_or_else(|| panic!("no {name} attribute in {element}"))
        + marker.len();
    let end = element[start..].find('"').unwrap() + start;
    element[start..end].parse().unwrap()
}

fn fixed_pages(path: &Path) -> Vec<String> {
    let mut parts: Vec<(String, Vec<u8>)> = package_parts(path)
        .into_iter()
        .filter(|(name, _)| name.ends_with(".fpage"))
        .collect();
    parts.sort_by(|a, b| a.0.cmp(&b.0));
    parts.iter().map(|(_, bytes)| markup(bytes)).collect()
}

fn approx(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} is not within {tolerance} of {expected}"
    );
}

#[test]
fn pages_become_xps_pages_of_the_right_size() {
    let mut rig = rig(true);
    let file = TempFile::new("pages.xps");
    // A4 portrait, A4 landscape and US Letter, in points.
    let pages = [page(595.0, 842.0), page(842.0, 595.0), page(612.0, 792.0)];
    let printed = print_to_xps_file(&mut rig.surface(), file.path(), &pages).unwrap();
    assert_eq!(printed, 3);

    let parts = package_parts(file.path());
    assert!(
        parts.iter().any(|(name, _)| name == "[Content_Types].xml"),
        "{:?}",
        parts.iter().map(|(name, _)| name).collect::<Vec<_>>()
    );
    let pages_markup = fixed_pages(file.path());
    assert_eq!(pages_markup.len(), 3);
    // XPS measures in 1/96 inch: points * 96 / 72.
    for (markup, (width, height)) in
        pages_markup
            .iter()
            .zip([(595.0, 842.0), (842.0, 595.0), (612.0, 792.0)])
    {
        approx(page_attribute(markup, "Width"), width * 96.0 / 72.0, 1.0);
        approx(page_attribute(markup, "Height"), height * 96.0 / 72.0, 1.0);
        // The page content stayed vector: the rectangle is a path element, not an image.
        assert!(markup.contains("<Path"), "{markup}");
        assert!(!markup.contains("ImageBrush"), "{markup}");
    }
    // The job's scene ids are free again.
    for index in 0..3 {
        assert!(
            rig.assets
                .scene(SceneId(PRINT_SCENE_ID_BASE + index))
                .is_none()
        );
    }
}

#[test]
fn a_failed_job_leaves_no_file_and_frees_its_scene_ids() {
    // Without a scene renderer the first page cannot be drawn.
    let mut rig = rig(false);
    let file = TempFile::new("failed.xps");
    let error =
        print_to_xps_file(&mut rig.surface(), file.path(), &[page(595.0, 842.0)]).unwrap_err();
    assert!(
        matches!(error, WinError::Invalid("no scene renderer is installed")),
        "{error:?}"
    );
    assert!(!file.path().exists());
    assert!(rig.assets.scene(SceneId(PRINT_SCENE_ID_BASE)).is_none());

    // A page without an area is refused before anything is drawn.
    let mut rig = self::rig(true);
    let empty = Arc::new(Scene::new(SceneSize::new(0.0, 842.0)));
    let error = print_to_xps_file(&mut rig.surface(), file.path(), &[empty]).unwrap_err();
    assert!(matches!(error, WinError::Invalid(_)), "{error:?}");
    assert!(!file.path().exists());
}

#[test]
fn an_unwritable_path_is_an_error() {
    let mut rig = rig(true);
    let missing = std::env::temp_dir()
        .join(format!("ssw-print-missing-{}", std::process::id()))
        .join("nested")
        .join("out.xps");
    let error = print_to_xps_file(&mut rig.surface(), &missing, &[page(595.0, 842.0)]).unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
}

#[test]
fn page_sizes_are_converted_from_points_to_dips() {
    let (width, height) = page_size_dip(&Scene::new(SceneSize::new(72.0, 36.0))).unwrap();
    approx(f64::from(width), 96.0, 1e-4);
    approx(f64::from(height), 48.0, 1e-4);
    for size in [
        SceneSize::new(0.0, 10.0),
        SceneSize::new(10.0, -1.0),
        SceneSize::new(f32::NAN, 10.0),
        SceneSize::new(10.0, f32::INFINITY),
    ] {
        assert!(
            matches!(page_size_dip(&Scene::new(size)), Err(WinError::Invalid(_))),
            "{size:?}"
        );
    }
}

#[test]
fn the_file_target_speaks_the_package_target_protocol() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let file = TempFile::new("protocol.xps");
    let target = xps_file_target(file.path()).unwrap();

    // It offers the Microsoft XPS package type, in memory the caller frees.
    let mut count = 0u32;
    let mut types: *mut GUID = null_mut();
    // SAFETY: valid out pointers; the array is CoTaskMem memory freed below.
    unsafe { target.GetPackageTargetTypes(&mut count, &mut types) }.unwrap();
    assert_eq!(count, 1);
    // SAFETY: the target returned one GUID at `types`.
    assert_eq!(unsafe { *types }, ID_DOCUMENTPACKAGETARGET_MSXPS);
    // SAFETY: allocated with CoTaskMemAlloc by the target and not used afterwards.
    unsafe { CoTaskMemFree(Some(types.cast_const().cast::<c_void>())) };
    // SAFETY: null out pointers are what is being checked.
    let refused = unsafe { target.GetPackageTargetTypes(null_mut(), null_mut()) };
    assert_eq!(refused.unwrap_err().code(), E_POINTER);

    // The XPS package target of that type reports XPS and hands out the object factory.
    // SAFETY: valid GUID pointer.
    let package: IXpsDocumentPackageTarget =
        unsafe { target.GetPackageTarget(&ID_DOCUMENTPACKAGETARGET_MSXPS) }.unwrap();
    // SAFETY: plain calls on a live object.
    unsafe {
        assert_eq!(package.GetXpsType().unwrap(), XPS_DOCUMENT_TYPE_XPS);
        package.GetXpsOMFactory().unwrap();
    }
    // Other package types are not supported.
    // SAFETY: valid GUID pointer.
    let unsupported = unsafe {
        target.GetPackageTarget::<IXpsDocumentPackageTarget>(&ID_DOCUMENTPACKAGETARGET_OPENXPS)
    };
    assert_eq!(unsupported.unwrap_err().code(), E_INVALIDARG);
    // Null pointers are refused.
    // SAFETY: the vtable call passes null pointers on purpose; the target checks them.
    let status = unsafe {
        (Interface::vtable(&target).GetPackageTarget)(target.as_raw(), null(), null(), null_mut())
    };
    assert_eq!(status, E_POINTER);
    // Cancelling is accepted.
    // SAFETY: plain call on a live object.
    unsafe { target.Cancel() }.unwrap();
}

#[test]
fn printing_to_an_unknown_printer_fails_before_anything_is_drawn() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let error = printer_target("SS Writer test printer that does not exist", "job").unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
}

/// A print dialog that answers with a canned selection.
struct FakeHost {
    answer: Option<PrintSelection>,
    asked: Vec<PrintDialogRequest>,
}

impl FakeHost {
    fn new(answer: Option<PrintSelection>) -> Self {
        FakeHost {
            answer,
            asked: Vec::new(),
        }
    }
}

impl PrintDialogHost for FakeHost {
    fn show(
        &mut self,
        _owner: HWND,
        request: &PrintDialogRequest,
    ) -> Result<Option<PrintSelection>, WinError> {
        self.asked.push(*request);
        Ok(self.answer.clone())
    }
}

fn selection(
    ranges: Vec<std::ops::RangeInclusive<u32>>,
    copies: u16,
    collate: bool,
) -> PrintSelection {
    PrintSelection {
        printer: "SS Writer test printer that does not exist".to_owned(),
        copies,
        collate,
        ranges,
    }
}

#[test]
fn the_dialog_answer_is_clipped_to_the_document() {
    let owner = HWND::default();
    // No pages: the dialog is not even shown.
    let mut host = FakeHost::new(Some(selection(vec![1..=1], 1, false)));
    assert!(matches!(
        choose(&mut host, owner, 0),
        Err(WinError::Invalid(_))
    ));
    assert!(host.asked.is_empty());

    // Cancel.
    let mut host = FakeHost::new(None);
    assert_eq!(choose(&mut host, owner, 10).unwrap(), None);
    assert_eq!(host.asked, [PrintDialogRequest { page_count: 10 }]);

    // Ranges are clipped, empty ones dropped, zero copies become one.
    let mut host = FakeHost::new(Some(selection(vec![0..=2, 5..=99, 50..=60], 0, true)));
    let chosen = choose(&mut host, owner, 10).unwrap().unwrap();
    assert_eq!(chosen.ranges, [1..=2, 5..=10]);
    assert_eq!(chosen.copies, 1);
    assert!(chosen.collate);

    // Nothing of the answer exists in the document.
    let mut host = FakeHost::new(Some(selection(vec![11..=20], 1, false)));
    assert!(matches!(
        choose(&mut host, owner, 10),
        Err(WinError::Invalid(_))
    ));
}

#[test]
fn copies_and_ranges_expand_into_the_pages_to_print() {
    let ranges = vec![1..=2, 4..=4];
    assert_eq!(
        page_plan(&selection(ranges.clone(), 2, true), 4),
        [0, 1, 3, 0, 1, 3]
    );
    assert_eq!(
        page_plan(&selection(ranges.clone(), 2, false), 4),
        [0, 0, 1, 1, 3, 3]
    );
    assert_eq!(page_plan(&selection(ranges, 0, false), 4), [0, 1, 3]);
    // Pages the document does not have are dropped.
    assert_eq!(page_plan(&selection(vec![3..=9], 1, false), 4), [2, 3]);
    assert!(page_plan(&selection(vec![9..=12], 1, false), 4).is_empty());
}

#[test]
fn dialog_page_ranges_follow_the_page_number_choice() {
    let ranges = [
        PRINTPAGERANGE {
            nFromPage: 5,
            nToPage: 2,
        },
        PRINTPAGERANGE {
            nFromPage: 7,
            nToPage: 7,
        },
    ];
    assert_eq!(dialog_ranges(true, &ranges, 9), [2..=5, 7..=7]);
    // "All pages" ignores the ranges.
    assert_eq!(dialog_ranges(false, &ranges, 9), [1..=9]);
    assert!(dialog_ranges(true, &[], 9).is_empty());
}

#[test]
fn the_printer_name_is_read_from_a_devnames_block() {
    /// A DEVNAMES block: header (driver, device, port, flag offsets in words), then the strings.
    fn block(driver: &str, device: &str, port: &str) -> Vec<u16> {
        let mut words = vec![0u16; 4];
        let place = |text: &str, words: &mut Vec<u16>| {
            let offset = words.len() as u16;
            words.extend(text.encode_utf16());
            words.push(0);
            offset
        };
        let driver_at = place(driver, &mut words);
        let device_at = place(device, &mut words);
        let port_at = place(port, &mut words);
        words[0] = driver_at;
        words[1] = device_at;
        words[2] = port_at;
        words
    }
    assert_eq!(
        parse_devnames(&block("winspool", "Çay Yazıcısı 😀", "PORTPROMPT:")).as_deref(),
        Some("Çay Yazıcısı 😀")
    );
    // Malformed blocks are refused instead of read out of bounds.
    assert_eq!(parse_devnames(&[]), None);
    assert_eq!(parse_devnames(&[4]), None);
    assert_eq!(parse_devnames(&[4, 100, 0, 0]), None);
    // No terminator.
    assert_eq!(parse_devnames(&[4, 4, 0, 0, 0x41, 0x42]), None);
    // An empty name.
    assert_eq!(parse_devnames(&[4, 4, 0, 0, 0]), None);
}

#[test]
fn ctrl_p_without_a_choice_prints_nothing_and_an_unknown_printer_fails() {
    let mut rig = rig(true);
    let owner = HWND::default();
    let pages = || vec![(*page(595.0, 842.0)).clone(), (*page(595.0, 842.0)).clone()];

    // Cancelled: no job.
    let mut host = FakeHost::new(None);
    assert_eq!(
        print_with_dialog(&mut host, owner, &mut rig.surface(), "doc", pages()).unwrap(),
        None
    );
    assert_eq!(host.asked, [PrintDialogRequest { page_count: 2 }]);

    // Chosen, but the printer does not exist: the job cannot be created.
    let mut host = FakeHost::new(Some(selection(vec![1..=2], 1, false)));
    let error =
        print_with_dialog(&mut host, owner, &mut rig.surface(), "doc", pages()).unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
    // Nothing was left registered.
    assert!(rig.assets.scene(SceneId(PRINT_SCENE_ID_BASE)).is_none());
}

#[test]
fn pages_can_be_printed_into_any_target() {
    let mut rig = rig(true);
    let file = TempFile::new("target.xps");
    let pages = [page(595.0, 842.0), page(595.0, 842.0)];
    {
        let target = xps_file_target(file.path()).unwrap();
        assert_eq!(print_pages(&mut rig.surface(), &target, &pages).unwrap(), 2);
    }
    assert_eq!(fixed_pages(file.path()).len(), 2);
}

#[test]
fn the_chosen_pages_are_printed_into_the_given_target_with_their_copies() {
    let mut rig = rig(true);
    let file = TempFile::new("selection.xps");
    let pages = vec![
        (*page(595.0, 842.0)).clone(),
        (*page(842.0, 595.0)).clone(),
        (*page(612.0, 792.0)).clone(),
    ];
    // Pages 2 and 3, two collated copies: 2 3 2 3.
    let chosen = selection(vec![2..=3], 2, true);
    let printed = print_selection_to(&mut rig.surface(), &chosen, pages, &mut || {
        xps_file_target(file.path())
    })
    .unwrap();
    assert_eq!(printed, 4);
    let printed_pages = fixed_pages(file.path());
    assert_eq!(printed_pages.len(), 4);
    for (markup, (width, height)) in printed_pages.iter().zip([
        (842.0, 595.0),
        (612.0, 792.0),
        (842.0, 595.0),
        (612.0, 792.0),
    ]) {
        approx(page_attribute(markup, "Width"), width * 96.0 / 72.0, 1.0);
        approx(page_attribute(markup, "Height"), height * 96.0 / 72.0, 1.0);
    }

    // A target that cannot be opened fails the job before any page is drawn.
    let error = print_selection_to(&mut rig.surface(), &chosen, Vec::new(), &mut || {
        Err(WinError::Invalid("no target"))
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "no target");
}

/// A print dialog that cannot be shown.
struct BrokenHost;

impl PrintDialogHost for BrokenHost {
    fn show(
        &mut self,
        _owner: HWND,
        _request: &PrintDialogRequest,
    ) -> Result<Option<PrintSelection>, WinError> {
        Err(WinError::Invalid("the dialog could not be shown"))
    }
}

#[test]
fn a_dialog_that_cannot_be_shown_ends_ctrl_p_with_its_error() {
    let mut rig = rig(true);
    let owner = HWND::default();
    let error = choose(&mut BrokenHost, owner, 3).unwrap_err();
    assert_eq!(error.to_string(), "the dialog could not be shown");
    let pages = vec![(*page(595.0, 842.0)).clone()];
    let error =
        print_with_dialog(&mut BrokenHost, owner, &mut rig.surface(), "doc", pages).unwrap_err();
    assert_eq!(error.to_string(), "the dialog could not be shown");
}

/// Every system call of a print job that can fail does, in turn: the job fails with an OS
/// error, leaves no file behind and frees its scene ids.
#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_system_call_fails_the_job_and_cleans_up() {
    use ssw_win::fault;

    let mut rig = rig(true);
    let file = TempFile::new("sweep.xps");
    let pages = [page(595.0, 842.0), page(842.0, 595.0)];
    let mut injected = 0;
    for call in 0.. {
        fault::arm(call);
        let result = print_to_xps_file(&mut rig.surface(), file.path(), &pages);
        let tripped = fault::tripped();
        fault::disarm();
        if !tripped {
            assert_eq!(result.unwrap(), 2);
            break;
        }
        injected += 1;
        let error = result.unwrap_err();
        assert!(
            matches!(error, WinError::Os { .. }),
            "call {call}: {error:?}"
        );
        assert!(!file.path().exists(), "call {call}");
        for index in 0..2 {
            assert!(
                rig.assets
                    .scene(SceneId(PRINT_SCENE_ID_BASE + index))
                    .is_none(),
                "call {call}"
            );
        }
    }
    // The XPS object factory, the print control, its device context and render target, four
    // calls per page and closing the control.
    assert!(injected >= 13, "only {injected} calls could fail");
    assert_eq!(fixed_pages(file.path()).len(), 2);

    // The print system's factory is the first call of a job for a printer.
    fault::arm(0);
    let error = printer_target("SS Writer test printer that does not exist", "job").unwrap_err();
    assert!(fault::tripped());
    fault::disarm();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
}
