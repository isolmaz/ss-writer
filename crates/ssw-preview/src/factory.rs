//! The class factory and the two exports COM calls: `DllGetClassObject` and `DllCanUnloadNow`.
//!
//! Registration is done by the installer (`installer/Registration.wxs`: per-user `HKCU`
//! `InprocServer32` with `ThreadingModel = Apartment`), so the DLL exports no
//! `DllRegisterServer`.

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::core::{BOOL, Error, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement};

use crate::handler::{guarded, new_handler};
use crate::server::{ObjectToken, can_unload, lock_server};

/// The CLSID of the preview handler; the installer registers the same value.
pub const CLSID_PREVIEW_HANDLER: GUID = GUID::from_u128(0xB7D4E2A1_6C3F_4E8B_A5D9_2F1C0E7B94A3);

/// Makes preview handlers.
#[implement(IClassFactory)]
struct ClassFactory {
    _token: ObjectToken,
}

#[allow(non_snake_case)]
impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> Result<()> {
        guarded(|| {
            if ppvobject.is_null() {
                return Err(Error::from(E_POINTER));
            }
            // SAFETY: `ppvobject` is non-null and valid for writing a pointer (the caller's
            // contract); every failure below leaves it null.
            unsafe { *ppvobject = std::ptr::null_mut() };
            if !punkouter.is_null() {
                return Err(Error::from(CLASS_E_NOAGGREGATION));
            }
            let handler = new_handler();
            // SAFETY: `ppvobject` is non-null; a null `riid` is answered with `E_POINTER`.
            unsafe { handler.query(riid, ppvobject) }.ok()
        })
    }

    fn LockServer(&self, flock: BOOL) -> Result<()> {
        lock_server(flock.as_bool());
        Ok(())
    }
}

/// COM asks the DLL for the class factory of `rclsid`.
///
/// Returns `S_OK` with a factory for [`CLSID_PREVIEW_HANDLER`] in `*ppv`,
/// `CLASS_E_CLASSNOTAVAILABLE` for any other class and `E_NOINTERFACE` when the factory does not
/// implement `riid`. Never unwinds.
///
/// # Safety
/// `rclsid` and `riid` must be null or point to valid GUIDs; `ppv` must be null or valid for
/// writing one pointer.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    catch_unwind(AssertUnwindSafe(|| {
        if ppv.is_null() {
            return E_POINTER;
        }
        // SAFETY: `ppv` is non-null and valid for writing a pointer (the caller's contract).
        unsafe { *ppv = std::ptr::null_mut() };
        // SAFETY: `rclsid` is null or a valid GUID pointer (the caller's contract).
        match unsafe { rclsid.as_ref() } {
            Some(clsid) if *clsid == CLSID_PREVIEW_HANDLER => {}
            Some(_) => return CLASS_E_CLASSNOTAVAILABLE,
            None => return E_POINTER,
        }
        let factory: IClassFactory = ClassFactory {
            _token: ObjectToken::new(),
        }
        .into();
        // SAFETY: `ppv` is non-null; a null `riid` is answered with `E_POINTER`.
        unsafe { factory.query(riid, ppv) }
    }))
    .unwrap_or(E_UNEXPECTED)
}

/// COM asks whether the DLL can be unloaded: `S_OK` when no class factory, preview handler or
/// server lock is alive, `S_FALSE` otherwise.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    if can_unload() { S_OK } else { S_FALSE }
}
