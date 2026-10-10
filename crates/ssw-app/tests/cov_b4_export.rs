//! The failures of the system adapters behind the print and share seams that only an operating
//! system malfunction produces (the scene renderer of a print job cannot start, the share
//! interop is missing), made with the fault injection of `ssw-win`. Every job here is refused
//! before any dialog or sheet could open: the print job has no pages and the shared file does
//! not exist.

#![cfg(feature = "fault-injection")]

use ssw_app::ui::export::{PrintJob, Printing, Sharing, SystemPrinting, SystemSharing};
use ssw_ui::services::ServiceError;
use ssw_win::com::ComGuard;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{Driver, GfxDevices};
use ssw_win::wic::Wic;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

/// A top-level window that is never shown: the sharer needs a window handle.
fn hidden_window() -> HWND {
    // SAFETY: creates a window of the system `STATIC` class without `WS_VISIBLE`.
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!("ssw-app share fault test"),
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
    .unwrap()
}

#[test]
fn a_print_whose_scene_renderer_cannot_start_fails_before_any_dialog() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let devices = GfxDevices::create_on(Driver::Warp, 1).unwrap();
    let wic = Wic::new().unwrap();
    let text = DWriteEngine::new().unwrap();
    let mut printing = SystemPrinting::new(devices, wic, text);

    // The first system call of the job is the creation of the DirectWrite factory of the
    // scene renderer.
    ssw_win::fault::arm(0);
    let result = printing.print(PrintJob {
        owner: None,
        name: "never".to_owned(),
        pages: Vec::new(),
    });
    let reached = ssw_win::fault::tripped();
    ssw_win::fault::disarm();
    assert!(reached, "the job made a checked system call");
    match result {
        Err(ServiceError::Failed { service, message }) => {
            assert_eq!(service, "print");
            assert!(message.contains("create DirectWrite factory"), "{message}");
        }
        other => panic!("expected a failed print, got {other:?}"),
    }
}

#[test]
fn a_share_without_the_share_interop_fails_before_any_sheet() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let mut sharing = SystemSharing::default();
    let missing = std::env::temp_dir().join("ssw-app-share-fault-never-existed.md");
    let window = hidden_window();

    // The first system call of a sharer is the load of the interop factory.
    ssw_win::fault::arm(0);
    let result = sharing.share(Some(window), &missing, "Never");
    let reached = ssw_win::fault::tripped();
    ssw_win::fault::disarm();
    assert!(reached, "the sharer made a checked system call");
    match result {
        Err(ServiceError::Failed { service, message }) => {
            assert_eq!(service, "share");
            assert!(message.contains("IDataTransferManagerInterop"), "{message}");
        }
        other => panic!("expected a failed share, got {other:?}"),
    }
    drop(sharing);
    // SAFETY: destroys the window created above on its own thread.
    let _ = unsafe { DestroyWindow(window) };
}
