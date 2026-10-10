//! The crate error type, COM apartment guards and small FFI string helpers.

use thiserror::Error;
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR,
};
use windows::Win32::System::Com::{
    COINIT, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize,
};
use windows::core::HRESULT;

/// `D2DERR_RECREATE_TARGET`: the render target must be recreated (windows-rs has no constant).
pub const D2DERR_RECREATE_TARGET: HRESULT = HRESULT(0x8899_000C_u32 as i32);

/// Failure of a Win32 platform call.
#[derive(Debug, Error)]
pub enum WinError {
    /// An OS call failed.
    #[error("{context} failed: {source}")]
    Os {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    /// The graphics device was removed or reset; every device resource must be recreated.
    #[error("the graphics device was lost during {context}")]
    DeviceLost { context: &'static str },
    /// The caller violated a precondition of the API.
    #[error("{0}")]
    Invalid(&'static str),
}

impl WinError {
    /// Wraps an OS error, classifying device-lost codes.
    #[must_use]
    pub fn from_os(context: &'static str, source: windows::core::Error) -> Self {
        if is_device_lost(source.code()) {
            WinError::DeviceLost { context }
        } else {
            WinError::Os { context, source }
        }
    }

    /// True for [`WinError::DeviceLost`].
    #[must_use]
    pub fn is_device_lost(&self) -> bool {
        matches!(self, WinError::DeviceLost { .. })
    }
}

/// Adds call-site context to `windows::core::Result`.
pub trait OsResultExt<T> {
    /// Converts the error into a [`WinError`] naming the failed operation.
    ///
    /// # Errors
    /// Returns [`WinError::DeviceLost`] for device-removed codes and [`WinError::Os`] otherwise.
    fn ctx(self, context: &'static str) -> Result<T, WinError>;
}

impl<T> OsResultExt<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T, WinError> {
        self.map_err(|source| WinError::from_os(context, source))
    }
}

/// True when `hr` means the D3D / DXGI / D2D device must be recreated.
#[must_use]
pub fn is_device_lost(hr: HRESULT) -> bool {
    hr == DXGI_ERROR_DEVICE_REMOVED
        || hr == DXGI_ERROR_DEVICE_RESET
        || hr == DXGI_ERROR_DEVICE_HUNG
        || hr == DXGI_ERROR_DRIVER_INTERNAL_ERROR
        || hr == D2DERR_RECREATE_TARGET
}

/// Keeps COM initialized on the creating thread; uninitializes on drop.
///
/// `CoUninitialize` unloads the in-process COM servers this thread activated (the WIC codecs,
/// shell objects, ...): releasing one of their objects afterwards jumps into unmapped code and
/// crashes with an access violation. A guard must therefore be dropped after every COM object
/// of its thread: bind it first in a scope (locals drop in reverse order) and declare it last
/// in a struct (fields drop in declaration order).
#[derive(Debug)]
pub struct ComGuard {
    initialized: bool,
}

impl ComGuard {
    /// Initializes a single-threaded apartment (the UI thread).
    ///
    /// # Errors
    /// Fails when COM cannot be initialized.
    pub fn apartment_threaded() -> Result<Self, WinError> {
        Self::init(COINIT_APARTMENTTHREADED)
    }

    /// Initializes the multi-threaded apartment (worker threads).
    ///
    /// # Errors
    /// Fails when COM cannot be initialized.
    pub fn multi_threaded() -> Result<Self, WinError> {
        Self::init(COINIT_MULTITHREADED)
    }

    fn init(mode: COINIT) -> Result<Self, WinError> {
        // SAFETY: plain COM initialization of the calling thread; paired with `CoUninitialize`
        // in `Drop` only when this call succeeded.
        let hr = unsafe { CoInitializeEx(None, mode) };
        if hr.is_ok() {
            Ok(Self { initialized: true })
        } else if hr == RPC_E_CHANGED_MODE {
            // The thread already runs in the other apartment type; COM is usable as is.
            Ok(Self { initialized: false })
        } else {
            Err(WinError::from_os("CoInitializeEx", hr.into()))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: balances the successful `CoInitializeEx` in `init` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// UTF-16 with a trailing NUL, for `PCWSTR` arguments.
#[must_use]
pub fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decodes UTF-16 up to the first NUL (or the end), replacing unpaired surrogates.
#[must_use]
pub fn from_wide(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG};

    #[test]
    fn device_lost_codes_are_classified() {
        for hr in [
            DXGI_ERROR_DEVICE_REMOVED,
            DXGI_ERROR_DEVICE_RESET,
            DXGI_ERROR_DEVICE_HUNG,
            DXGI_ERROR_DRIVER_INTERNAL_ERROR,
            D2DERR_RECREATE_TARGET,
        ] {
            assert!(is_device_lost(hr), "{hr:?}");
        }
        for hr in [E_FAIL, E_INVALIDARG, HRESULT(0)] {
            assert!(!is_device_lost(hr), "{hr:?}");
        }
    }

    #[test]
    fn os_errors_keep_context_and_classify_device_loss() {
        let lost: windows::core::Result<()> = Err(DXGI_ERROR_DEVICE_REMOVED.into());
        let error = lost.ctx("Present").unwrap_err();
        assert!(error.is_device_lost());
        assert_eq!(
            error.to_string(),
            "the graphics device was lost during Present"
        );

        let failed: windows::core::Result<()> = Err(E_INVALIDARG.into());
        let error = failed.ctx("CreateWidget").unwrap_err();
        assert!(!error.is_device_lost());
        assert!(error.to_string().starts_with("CreateWidget failed: "));

        let ok: windows::core::Result<u32> = Ok(7);
        assert_eq!(ok.ctx("noop").unwrap(), 7);
        assert_eq!(WinError::Invalid("bad").to_string(), "bad");
    }

    #[test]
    fn com_guards_nest_and_tolerate_the_other_apartment() {
        let outer = ComGuard::multi_threaded().unwrap();
        // Same mode again: S_FALSE, still initialized.
        let inner = ComGuard::multi_threaded().unwrap();
        assert!(inner.initialized);
        // The thread is already MTA, so STA is refused with RPC_E_CHANGED_MODE and tolerated.
        let other = ComGuard::apartment_threaded().unwrap();
        assert!(!other.initialized);
        drop(other);
        drop(inner);
        drop(outer);
    }

    #[test]
    fn wide_strings_round_trip() {
        let wide = to_wide("Çay İ😀");
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(from_wide(&wide), "Çay İ😀");
        assert_eq!(from_wide(&[72, 105, 0, 88]), "Hi");
        assert_eq!(from_wide(&[72, 105]), "Hi");
        assert_eq!(from_wide(&[0xD800]), "\u{FFFD}");
        assert_eq!(to_wide(""), vec![0]);
    }
}
