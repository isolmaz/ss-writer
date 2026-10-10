//! Test support (the `test-support` feature, and always in this crate's own unit tests): a
//! desktop of its own for the tests that really show, maximize and raise windows.
//!
//! A window that is shown or activated on the user's desktop interrupts the user. A
//! [`PrivateDesktop`] is a desktop object of the interactive window station that is never
//! switched to, so it is not the input desktop: windows made on it are shown, activated and
//! focused only within it, and nothing of it ever appears on a screen or takes the focus away
//! from what the user is doing. System dialogs (`TaskDialog`, `MessageBox`, the print and file
//! dialogs, the share sheet) never belong on it: they are not run by tests at all.

use std::ffi::c_void;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CONTROL_FLAGS, GetThreadDesktop,
    GetUserObjectInformationW, HDESK, SetThreadDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{HSTRING, PCWSTR};

use crate::com::{OsResultExt, WinError};

/// `GENERIC_ALL`: the access the creator of a desktop asks for. Holding the right to switch to
/// the desktop does not switch to it; nothing here ever calls `SwitchDesktop`.
const ACCESS_ALL: u32 = 0x1000_0000;

/// Room for the name of a desktop (names are at most a few dozen characters here).
const NAME_UNITS: usize = 256;

/// A desktop that exists as long as the value lives.
#[derive(Debug)]
pub struct PrivateDesktop {
    handle: HDESK,
    name: String,
}

impl PrivateDesktop {
    /// Creates a desktop with a name no other test process shares.
    ///
    /// # Errors
    /// The desktop cannot be created (a session without an interactive window station).
    pub fn create() -> Result<Self, WinError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let name = format!("ssw-test-{}-{nanos}", std::process::id());
        // SAFETY: a valid NUL-terminated name; no device, mode or security attributes.
        let handle = unsafe {
            CreateDesktopW(
                &HSTRING::from(name.as_str()),
                PCWSTR::null(),
                None,
                DESKTOP_CONTROL_FLAGS(0),
                ACCESS_ALL,
                None,
            )
        }
        .ctx("CreateDesktopW")?;
        Ok(Self { handle, name })
    }

    /// The name of the desktop ([`current_desktop_name`] of a thread that runs on it).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Runs `work` on a new thread that is attached to this desktop before it does anything
    /// else, so every window and COM object `work` creates lives on the desktop, and returns
    /// its result. A panic of `work` is raised again in the caller. The thread is gone when this
    /// returns: no window of the desktop outlives the call unless `work` leaked it.
    ///
    /// # Panics
    /// The thread cannot be attached to the desktop.
    pub fn run<R: Send>(&self, work: impl FnOnce() -> R + Send) -> R {
        let desktop = self.handle.0 as usize;
        std::thread::scope(|scope| {
            let thread = scope.spawn(move || {
                // SAFETY: the handle is the desktop of `self`, which outlives the scope; the
                // thread owns no window, hook or COM object yet.
                unsafe { SetThreadDesktop(HDESK(desktop as *mut c_void)) }
                    .expect("the new thread attaches to the private desktop");
                work()
            });
            match thread.join() {
                Ok(result) => result,
                Err(payload) => std::panic::resume_unwind(payload),
            }
        })
    }
}

impl Drop for PrivateDesktop {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateDesktopW` and is closed exactly once; the threads
        // of `run` are gone. A desktop that still has windows is destroyed with the process.
        let _ = unsafe { CloseDesktop(self.handle) };
    }
}

/// The name of the desktop the calling thread runs on.
///
/// # Panics
/// The desktop of the thread cannot be queried.
#[must_use]
pub fn current_desktop_name() -> String {
    // SAFETY: a plain query of the calling thread.
    let desktop = unsafe { GetThreadDesktop(GetCurrentThreadId()) }.expect("the thread's desktop");
    let mut name = [0u16; NAME_UNITS];
    let mut needed = 0u32;
    // SAFETY: `name` is a writable buffer of the stated size; `needed` is a valid out value.
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            size_of_val(&name) as u32,
            Some(&mut needed),
        )
    }
    .expect("the name of the desktop");
    let length = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(NAME_UNITS);
    String::from_utf16_lossy(&name[..length])
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use ssw_ui::canvas::Canvas;
    use ssw_ui::input::Event;
    use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, IsWindowVisible, IsZoomed, SW_MINIMIZE, SW_SHOW, SW_SHOWMAXIMIZED,
        SW_SHOWNOACTIVATE, ShowWindow,
    };

    use super::{PrivateDesktop, current_desktop_name};
    use crate::window::{ShowMode, WindowHandler, WindowHost, WindowInfo, WindowOptions};

    /// A window that ignores everything it is told.
    struct Silent;

    impl WindowHandler for Silent {
        fn on_event(&mut self, _info: &WindowInfo, _event: Event) {}
        fn on_paint(&mut self, _info: &WindowInfo, _canvas: &mut dyn Canvas) {}
    }

    fn visible_window() -> WindowHost {
        let options = WindowOptions {
            title: "private desktop".to_owned(),
            rect: Some((40, 40, 640, 480)),
            dark: false,
            no_activate: false,
        };
        WindowHost::create(&options, Box::new(Silent), Theme::light(DEFAULT_ACCENT)).unwrap()
    }

    fn is_visible(host: &WindowHost) -> bool {
        // SAFETY: a plain query of a window of this thread.
        unsafe { IsWindowVisible(host.hwnd()) }.as_bool()
    }

    fn is_zoomed(host: &WindowHost) -> bool {
        // SAFETY: a plain query of a window of this thread.
        unsafe { IsZoomed(host.hwnd()) }.as_bool()
    }

    fn is_iconic(host: &WindowHost) -> bool {
        // SAFETY: a plain query of a window of this thread.
        unsafe { IsIconic(host.hwnd()) }.as_bool()
    }

    #[test]
    fn a_new_window_is_hidden_then_unseen_then_maximized_then_normal_in_that_order() {
        let modes: Vec<ShowMode> = [
            (true, true, true),
            (true, true, false),
            (true, false, true),
            (true, false, false),
            (false, true, true),
            (false, true, false),
            (false, false, true),
            (false, false, false),
        ]
        .into_iter()
        .map(|(hidden, offscreen, maximized)| {
            ShowMode::for_new_window(hidden, offscreen, maximized)
        })
        .collect();
        assert_eq!(
            modes,
            [
                ShowMode::Hidden,
                ShowMode::Hidden,
                ShowMode::Hidden,
                ShowMode::Hidden,
                ShowMode::NoActivate,
                ShowMode::NoActivate,
                ShowMode::Maximized,
                ShowMode::Normal,
            ]
        );
    }

    #[test]
    fn every_mode_has_its_own_show_command_and_a_hidden_window_has_none() {
        assert_eq!(ShowMode::Hidden.command(), None);
        assert_eq!(ShowMode::NoActivate.command(), Some(SW_SHOWNOACTIVATE));
        assert_eq!(ShowMode::Maximized.command(), Some(SW_SHOWMAXIMIZED));
        assert_eq!(ShowMode::Normal.command(), Some(SW_SHOW));
    }

    #[test]
    fn work_runs_on_a_desktop_of_its_own_and_its_panic_reaches_the_caller() {
        let own = current_desktop_name();
        let desktop = PrivateDesktop::create().unwrap();
        assert!(desktop.name().starts_with("ssw-test-"));
        assert_ne!(desktop.name(), own);

        let seen = desktop.run(current_desktop_name);
        assert_eq!(seen, desktop.name());
        // The calling thread stays where it was.
        assert_eq!(current_desktop_name(), own);

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            desktop.run(|| -> u32 { panic!("work failed") })
        }))
        .unwrap_err();
        assert_eq!(
            panicked.downcast_ref::<&str>().copied(),
            Some("work failed")
        );
        // The desktop is still usable after a panic.
        assert_eq!(desktop.run(|| 7), 7);
    }

    #[test]
    fn windows_are_shown_maximized_minimized_and_raised_on_the_private_desktop() {
        let desktop = PrivateDesktop::create().unwrap();
        desktop.run(|| {
            let host = visible_window();
            assert!(!is_visible(&host), "a new window is hidden");

            host.show_mode(ShowMode::Hidden);
            assert!(!is_visible(&host), "the hidden mode shows nothing");

            host.show_mode(ShowMode::NoActivate);
            assert!(is_visible(&host));
            assert!(!is_zoomed(&host));

            host.show_mode(ShowMode::Maximized);
            assert!(is_zoomed(&host));
            assert!(host.info().maximized);

            // SAFETY: minimizes the window of this thread.
            let _ = unsafe { ShowWindow(host.hwnd(), SW_MINIMIZE) };
            assert!(is_iconic(&host));
            host.bring_to_front();
            assert!(!is_iconic(&host), "raising restores a minimized window");

            // Raising a window that is not minimized only asks for the foreground.
            host.bring_to_front();
            assert!(is_visible(&host));
        });

        desktop.run(|| {
            let host = visible_window();
            host.show();
            assert!(is_visible(&host));
            assert!(!is_zoomed(&host));
            host.destroy();
        });
    }
}
