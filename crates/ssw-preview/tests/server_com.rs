//! The DLL as a COM server: class factory, object and lock counts behind `DllCanUnloadNow`,
//! interface coverage, aggregation, the site and the visuals calls without a window.

mod common;

use std::ffi::c_void;

use common::{Session, class_factory, new_handler, serial};
use ssw_preview::factory::{CLSID_PREVIEW_HANDLER, DllCanUnloadNow, DllGetClassObject};
use ssw_preview::server::{can_unload, live_objects, server_locks};
use ssw_win::com::ComGuard;
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_FAIL, E_NOINTERFACE, E_NOTIMPL, E_POINTER,
    S_FALSE, S_OK,
};
use windows::Win32::Graphics::Gdi::LOGFONTW;
use windows::Win32::System::Com::IClassFactory;
use windows::Win32::System::Ole::{IObjectWithSite, IOleWindow};
use windows::Win32::UI::Shell::PropertiesSystem::IInitializeWithStream;
use windows::Win32::UI::Shell::{IPreviewHandler, IPreviewHandlerVisuals};
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::core::{GUID, IUnknown, Interface};

#[test]
fn the_clsid_is_the_one_the_installer_registers() {
    let registered = GUID::from_u128(0xB7D4E2A1_6C3F_4E8B_A5D9_2F1C0E7B94A3);
    assert_eq!(CLSID_PREVIEW_HANDLER, registered);
    let wix = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../installer/Registration.wxs"
    ))
    .unwrap();
    assert!(wix.contains("{B7D4E2A1-6C3F-4E8B-A5D9-2F1C0E7B94A3}"));
    assert!(wix.contains("ThreadingModel\" Type=\"string\" Value=\"Apartment\""));
}

#[test]
fn the_dll_unloads_only_when_no_object_is_alive() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    assert_eq!(DllCanUnloadNow(), S_OK);
    assert!(can_unload());
    assert_eq!(live_objects(), 0);
    let factory = class_factory();
    assert_eq!(live_objects(), 1);
    assert_eq!(DllCanUnloadNow(), S_FALSE);
    // SAFETY: plain call on a live factory.
    let handler =
        unsafe { factory.CreateInstance::<_, IPreviewHandler>(None::<&IUnknown>) }.unwrap();
    assert_eq!(live_objects(), 2);
    drop(factory);
    assert_eq!(live_objects(), 1, "the handler outlives its factory");
    assert_eq!(DllCanUnloadNow(), S_FALSE);
    // Other interface pointers keep the same object alive.
    let site: IObjectWithSite = handler.cast().unwrap();
    drop(handler);
    assert_eq!(live_objects(), 1);
    drop(site);
    assert_eq!(live_objects(), 0);
    assert_eq!(DllCanUnloadNow(), S_OK);
}

#[test]
fn server_locks_keep_the_dll_loaded_and_unbalanced_releases_are_ignored() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = class_factory();
    // SAFETY: plain calls on a live factory.
    unsafe {
        factory.LockServer(true).unwrap();
        factory.LockServer(true).unwrap();
    }
    assert_eq!(server_locks(), 2);
    drop(factory);
    assert_eq!(DllCanUnloadNow(), S_FALSE, "locked even without objects");
    let factory = class_factory();
    // SAFETY: plain calls on a live factory.
    unsafe {
        factory.LockServer(false).unwrap();
        factory.LockServer(false).unwrap();
        factory.LockServer(false).unwrap();
    }
    assert_eq!(server_locks(), 0);
    drop(factory);
    assert_eq!(DllCanUnloadNow(), S_OK);
}

#[test]
fn the_factory_refuses_other_classes_bad_pointers_and_unknown_interfaces() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let mut raw: *mut c_void = std::ptr::dangling_mut();
    let other = GUID::from_u128(1);
    // SAFETY: valid GUID pointers and a valid out pointer.
    let status = unsafe { DllGetClassObject(&other, &IClassFactory::IID, &mut raw) };
    assert_eq!(status, CLASS_E_CLASSNOTAVAILABLE);
    assert!(raw.is_null(), "failures leave the out pointer null");
    // SAFETY: a null class id is rejected; the out pointer is valid.
    let status = unsafe { DllGetClassObject(std::ptr::null(), &IClassFactory::IID, &mut raw) };
    assert_eq!(status, E_POINTER);
    // SAFETY: a null out pointer is rejected before anything is written.
    let status = unsafe {
        DllGetClassObject(
            &CLSID_PREVIEW_HANDLER,
            &IClassFactory::IID,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(status, E_POINTER);
    // The factory implements IClassFactory, not the handler's interface.
    // SAFETY: valid GUID pointers and a valid out pointer.
    let status =
        unsafe { DllGetClassObject(&CLSID_PREVIEW_HANDLER, &IPreviewHandler::IID, &mut raw) };
    assert_eq!(status, E_NOINTERFACE);
    assert!(raw.is_null());
    assert_eq!(live_objects(), 0, "the refused factory is gone again");
}

#[test]
fn creating_an_instance_without_an_out_pointer_is_refused_and_makes_nothing() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = class_factory();
    let create = Interface::vtable(&factory).CreateInstance;
    // SAFETY: a live factory and a valid interface id; the null out pointer is the point and is
    // rejected before anything is created or written.
    let status = unsafe {
        create(
            factory.as_raw(),
            std::ptr::null_mut(),
            &IPreviewHandler::IID,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(status, E_POINTER);
    assert_eq!(
        live_objects(),
        1,
        "no handler was made, only the factory lives"
    );
}

#[test]
fn the_handler_answers_every_interface_with_one_identity() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let handler = new_handler();
    let identity = |unknown: IUnknown| unknown.as_raw();
    let base = identity(handler.cast().unwrap());
    assert_eq!(
        identity(
            handler
                .cast::<IInitializeWithStream>()
                .unwrap()
                .cast()
                .unwrap()
        ),
        base
    );
    assert_eq!(
        identity(handler.cast::<IObjectWithSite>().unwrap().cast().unwrap()),
        base
    );
    assert_eq!(
        identity(handler.cast::<IOleWindow>().unwrap().cast().unwrap()),
        base
    );
    assert_eq!(
        identity(
            handler
                .cast::<IPreviewHandlerVisuals>()
                .unwrap()
                .cast()
                .unwrap()
        ),
        base
    );
    let error = handler.cast::<IClassFactory>().unwrap_err();
    assert_eq!(error.code(), E_NOINTERFACE);
}

#[test]
fn the_handler_refuses_aggregation() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let factory = class_factory();
    let outer: IUnknown = new_handler().cast().unwrap();
    // SAFETY: plain call on a live factory.
    let error = unsafe { factory.CreateInstance::<_, IPreviewHandler>(&outer) }.unwrap_err();
    assert_eq!(error.code(), CLASS_E_NOAGGREGATION);
}

#[test]
fn the_site_is_kept_returned_and_released() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let handler = new_handler();
    let with_site: IObjectWithSite = handler.cast().unwrap();
    // SAFETY: plain calls with live interface pointers.
    unsafe {
        assert_eq!(
            with_site.GetSite::<IUnknown>().unwrap_err().code(),
            E_FAIL,
            "no site yet"
        );
        let site: IUnknown = new_handler().cast().unwrap();
        with_site.SetSite(&site).unwrap();
        assert_eq!(
            with_site.GetSite::<IUnknown>().unwrap().as_raw(),
            site.as_raw()
        );
        with_site.SetSite(None::<&IUnknown>).unwrap();
        assert!(with_site.GetSite::<IUnknown>().is_err());
    }
}

#[test]
fn get_site_refuses_null_pointers_and_keeps_the_site() {
    let _serial = serial();
    let _com = ComGuard::apartment_threaded().unwrap();
    let with_site: IObjectWithSite = new_handler().cast().unwrap();
    let site: IUnknown = new_handler().cast().unwrap();
    // SAFETY: plain call with live interface pointers.
    unsafe { with_site.SetSite(&site) }.unwrap();
    let get_site = Interface::vtable(&with_site).GetSite;
    let mut out: *mut c_void = std::ptr::null_mut();
    // SAFETY: a live object; the null interface id is the point and is rejected up front.
    let status = unsafe { get_site(with_site.as_raw(), std::ptr::null(), &mut out) };
    assert_eq!(status, E_POINTER);
    // SAFETY: a live object and a valid interface id; the null out pointer is the point.
    let status = unsafe { get_site(with_site.as_raw(), &IUnknown::IID, std::ptr::null_mut()) };
    assert_eq!(status, E_POINTER);
    // SAFETY: plain call with a live interface pointer.
    let kept = unsafe { with_site.GetSite::<IUnknown>() }.unwrap();
    assert_eq!(
        kept.as_raw(),
        site.as_raw(),
        "the refused calls left the site alone"
    );
}

#[test]
fn calls_without_a_window_fail_cleanly_or_do_nothing() {
    let session = Session::new();
    let handler = &session.handler;
    let window: IOleWindow = handler.cast().unwrap();
    // SAFETY: plain calls; null pointers are the point of some of them.
    unsafe {
        assert_eq!(window.GetWindow().unwrap_err().code(), E_FAIL);
        assert_eq!(
            window.ContextSensitiveHelp(false).unwrap_err().code(),
            E_NOTIMPL
        );
        handler.SetFocus().unwrap();
        handler.Unload().unwrap();
        assert_eq!(
            handler.SetRect(std::ptr::null()).unwrap_err().code(),
            E_POINTER
        );
        // No site, no frame: the key is not handled (`S_FALSE`, a success code).
        handler.TranslateAccelerator(&MSG::default()).unwrap();
        let visuals: IPreviewHandlerVisuals = handler.cast().unwrap();
        assert_eq!(
            visuals.SetFont(std::ptr::null()).unwrap_err().code(),
            E_POINTER
        );
        visuals.SetFont(&LOGFONTW::default()).unwrap();
    }
}
