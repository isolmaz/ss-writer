//! Single instance per user session.
//!
//! A named mutex in the session's `Local\` namespace decides which process is first
//! ([`InstanceConfig::acquire`]). The first process becomes the [`PrimaryInstance`]: it owns a
//! hidden message-only window that receives the command lines of later launches. Every later
//! process is a [`SecondaryInstance`]: it sends its [`Launch`] (files and folders to open, the
//! "new window" request) to that window with `WM_COPYDATA` and exits.
//!
//! The primary only queues what it receives ([`PrimaryInstance::take_launches`]) and, when
//! asked, posts a wake-up message to a window of the application so its message loop notices.
//! What to do with a launch (open tabs in the last active window, open a new window for a
//! folder) is application policy.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowExW,
    GWLP_USERDATA, GetWindowLongPtrW, GetWindowThreadProcessId, HWND_MESSAGE, PostMessageW,
    RegisterClassExW, SMTO_ABORTIFHUNG, SendMessageTimeoutW, SetWindowLongPtrW, UnregisterClassW,
    WINDOW_EX_STYLE, WM_COPYDATA, WNDCLASSEXW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::com::{OsResultExt, WinError};
use crate::fault::check;

/// The command-line option that asks for a new window.
pub const NEW_WINDOW_ARG: &str = "--new-window";

/// How long [`SecondaryInstance::forward`] waits for the running instance to appear and answer.
pub const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a secondary instance looks for the primary's window while it is not there yet
/// (the primary owns the mutex a moment before its window exists).
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// `dwData` of the `WM_COPYDATA` message that carries a [`Launch`]: "SSW" and the payload
/// version.
pub const COPYDATA_TAG: usize = 0x5353_5701;

/// Bit 0 of the first payload unit: open a new window.
const FLAG_NEW_WINDOW: u16 = 1;

/// What a later launch asks the running instance to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Launch {
    /// Files and folders to open, as absolute paths.
    pub paths: Vec<PathBuf>,
    /// Open a new window even when files are given.
    pub new_window: bool,
}

impl Launch {
    /// Builds a launch from the process arguments (without the program name).
    ///
    /// [`NEW_WINDOW_ARG`] sets [`Launch::new_window`]; every other argument starting with `--`
    /// is an option of the application's own command line and is skipped; empty arguments are
    /// skipped; everything else is a path, made absolute against the current directory of
    /// this process (the running instance has a different one).
    #[must_use]
    pub fn from_args<I>(args: I) -> Self
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut launch = Self::default();
        for argument in args {
            if argument.is_empty() {
                continue;
            }
            if argument == NEW_WINDOW_ARG {
                launch.new_window = true;
            } else if !argument.to_string_lossy().starts_with("--") {
                let path = PathBuf::from(argument);
                launch
                    .paths
                    .push(std::path::absolute(&path).unwrap_or(path));
            }
        }
        launch
    }

    /// The `WM_COPYDATA` payload: little-endian UTF-16 units, a flag unit, then every path
    /// followed by a NUL (Windows paths cannot contain NUL). Lossless for paths that are not
    /// valid Unicode.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let flags = if self.new_window { FLAG_NEW_WINDOW } else { 0 };
        let mut units = vec![flags];
        for path in &self.paths {
            units.extend(path.as_os_str().encode_wide());
            units.push(0);
        }
        units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
    }

    /// Parses a payload produced by [`Launch::encode`]; `None` for anything else (odd length,
    /// unknown flags, a path without its terminator).
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if !bytes.len().is_multiple_of(2) {
            return None;
        }
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .copied()
            .map(u16::from_le_bytes)
            .collect();
        let (&flags, rest) = units.split_first()?;
        if flags & !FLAG_NEW_WINDOW != 0 || rest.last().is_some_and(|&unit| unit != 0) {
            return None;
        }
        let mut parts: Vec<&[u16]> = rest.split(|&unit| unit == 0).collect();
        // The text after the last terminator (empty by the check above).
        parts.pop();
        Some(Self {
            paths: parts
                .into_iter()
                .map(|part| PathBuf::from(OsString::from_wide(part)))
                .collect(),
            new_window: flags & FLAG_NEW_WINDOW != 0,
        })
    }
}

/// A window to wake when a launch arrives: `message` is posted to `hwnd` (no parameters).
#[derive(Debug, Clone, Copy)]
pub struct Wake {
    /// The window that gets the message.
    pub hwnd: HWND,
    /// The message number, usually `WM_APP + n`.
    pub message: u32,
}

/// Names the mutex and the window class of one application's single instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceConfig {
    name: String,
}

impl InstanceConfig {
    /// A configuration for the application called `name`; two configurations with the same
    /// name refer to the same instance.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// The mutex name (`Local\` namespace: one per logon session).
    #[must_use]
    pub fn mutex_name(&self) -> String {
        format!("Local\\{}.Instance", self.name)
    }

    /// The class name of the primary's message-only window.
    #[must_use]
    pub fn window_class(&self) -> String {
        format!("{}.InstanceWindow", self.name)
    }

    /// Becomes the first instance or finds the running one.
    ///
    /// `wake` is only used by a primary instance.
    ///
    /// # Errors
    /// The mutex, the window class or the window cannot be created.
    pub fn acquire(&self, wake: Option<Wake>) -> Result<Acquired, WinError> {
        let class = HSTRING::from(self.window_class());
        let (mutex, existed) = open_mutex(&HSTRING::from(self.mutex_name()))?;
        if existed {
            return Ok(Acquired::Secondary(SecondaryInstance { class }));
        }
        let state = Box::new(State {
            queue: RefCell::new(VecDeque::new()),
            wake,
        });
        let hwnd = create_window(&class, &state, HWND_MESSAGE)?;
        Ok(Acquired::Primary(PrimaryInstance {
            hwnd,
            class,
            state,
            _mutex: mutex,
        }))
    }
}

/// The outcome of [`InstanceConfig::acquire`].
#[derive(Debug)]
pub enum Acquired {
    /// No other instance runs: this process owns the mutex and the receiving window.
    Primary(PrimaryInstance),
    /// Another instance runs: forward the launch to it.
    Secondary(SecondaryInstance),
}

/// The mutex handle of the first instance; closing it lets the next launch become first.
#[derive(Debug)]
struct MutexHandle(HANDLE);

impl Drop for MutexHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from `CreateMutexW` and is closed exactly once.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Creates or opens the named mutex; the flag tells whether it existed before.
fn open_mutex(name: &HSTRING) -> Result<(MutexHandle, bool), WinError> {
    // SAFETY: `name` is a valid NUL-terminated string; default security attributes.
    let created = unsafe { CreateMutexW(None, false, name) };
    // SAFETY: reads the calling thread's last error right after the call; `CreateMutexW`
    // reports "opened an existing mutex" through it even on success.
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    // The handle belongs to its guard from here on: a failure injected after the call closes it
    // again, so the name is free for the next attempt.
    let handle = check(created.map(MutexHandle)).ctx("CreateMutexW")?;
    Ok((handle, existed))
}

fn module_instance() -> HINSTANCE {
    // SAFETY: querying the handle of the current module.
    unsafe { GetModuleHandleW(None) }
        .map(|module| HINSTANCE(module.0))
        .unwrap_or_default()
}

/// What the window procedure needs; lives in a box owned by the [`PrimaryInstance`].
#[derive(Debug)]
struct State {
    queue: RefCell<VecDeque<Launch>>,
    wake: Option<Wake>,
}

/// Registers `class` and creates the window with `state` attached under `parent` (the
/// message-only root: [`HWND_MESSAGE`]); the class is unregistered again when that fails.
fn create_window(class: &HSTRING, state: &State, parent: HWND) -> Result<HWND, WinError> {
    let description = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(instance_proc),
        hInstance: module_instance(),
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    // SAFETY: the class structure is fully initialized; the name outlives the call.
    if unsafe { RegisterClassExW(&description) } == 0 {
        return Err(WinError::from_os(
            "RegisterClassExW",
            windows::core::Error::from_thread(),
        ));
    }
    // SAFETY: creates a window of the class registered above; a parent that is not a window
    // makes the call fail, nothing else.
    let created = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            PCWSTR::null(),
            Default::default(),
            0,
            0,
            0,
            0,
            Some(parent),
            None,
            Some(module_instance()),
            None,
        )
    };
    match created {
        Ok(hwnd) => {
            // SAFETY: the window belongs to this thread; `state` outlives it because the
            // `PrimaryInstance` clears the pointer and destroys the window before dropping it.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, std::ptr::from_ref(state) as isize) };
            Ok(hwnd)
        }
        Err(error) => {
            // SAFETY: unregisters the class registered above; no window uses it.
            let _ = unsafe { UnregisterClassW(class, Some(module_instance())) };
            Err(WinError::from_os("CreateWindowExW", error))
        }
    }
}

/// The window procedure of the receiving window.
unsafe extern "system" fn instance_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_COPYDATA {
        // SAFETY: for WM_COPYDATA `lparam` is null or points to the sender's COPYDATASTRUCT,
        // valid for the duration of this call.
        let accepted = unsafe { receive(hwnd, lparam) };
        return LRESULT(isize::from(accepted));
    }
    // SAFETY: default processing with the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Decodes and queues one `WM_COPYDATA` payload; false when it is not a launch of ours.
///
/// # Safety
/// `lparam` is null or points to a valid `COPYDATASTRUCT` whose `lpData` holds `cbData` bytes.
unsafe fn receive(hwnd: HWND, lparam: LPARAM) -> bool {
    // SAFETY: by the caller's contract.
    let Some(data) = (unsafe { (lparam.0 as *const COPYDATASTRUCT).as_ref() }) else {
        return false;
    };
    if data.dwData != COPYDATA_TAG || data.lpData.is_null() {
        return false;
    }
    // SAFETY: by the caller's contract `lpData` holds `cbData` readable bytes.
    let bytes =
        unsafe { std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize) };
    let Some(launch) = Launch::decode(bytes) else {
        return false;
    };
    // SAFETY: the pointer was stored by `create_window` and is cleared before the state is
    // dropped; the window procedure runs on the thread that owns the window.
    let state = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const State).as_ref() };
    let Some(state) = state else {
        return false;
    };
    // Nothing between this borrow and its release dispatches messages, so no other borrow of
    // the queue can be live on this thread.
    state.queue.borrow_mut().push_back(launch);
    if let Some(wake) = state.wake {
        // SAFETY: plain message post; a destroyed target window only makes it fail, and the
        // launch is queued either way.
        let _ = unsafe { PostMessageW(Some(wake.hwnd), wake.message, WPARAM(0), LPARAM(0)) };
    }
    true
}

/// The first instance: receives the launches of later processes.
///
/// Must be created, polled and dropped on the thread that runs the message loop; the type is
/// `!Send` because it owns a window.
#[derive(Debug)]
pub struct PrimaryInstance {
    hwnd: HWND,
    class: HSTRING,
    state: Box<State>,
    /// Declared last: released after the window is gone, so a launch that starts meanwhile
    /// cannot find a half-destroyed primary.
    _mutex: MutexHandle,
}

impl PrimaryInstance {
    /// The receiving message-only window.
    #[must_use]
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// The launches received since the last call, oldest first. Messages are delivered while
    /// the thread pumps its message loop.
    #[must_use]
    pub fn take_launches(&self) -> Vec<Launch> {
        self.state.queue.borrow_mut().drain(..).collect()
    }
}

impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        // SAFETY: detaches the state, destroys the window on its owning thread and removes the
        // class; the state box is dropped afterwards.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            let _ = DestroyWindow(self.hwnd);
            let _ = UnregisterClassW(&self.class, Some(module_instance()));
        }
    }
}

/// A later launch: hands its [`Launch`] to the running instance.
#[derive(Debug, Clone)]
pub struct SecondaryInstance {
    class: HSTRING,
}

impl SecondaryInstance {
    /// Sends `launch` to the running instance and lets it take the foreground ([`FORWARD_TIMEOUT`]).
    ///
    /// # Errors
    /// The running instance has no window yet, did not answer in time or rejected the launch.
    pub fn forward(&self, launch: &Launch) -> Result<(), WinError> {
        self.forward_within(launch, FORWARD_TIMEOUT)
    }

    /// [`SecondaryInstance::forward`] with an explicit `timeout`, spent first on finding the
    /// window (the primary owns the mutex a moment before its window exists), then on the
    /// answer.
    ///
    /// # Errors
    /// The running instance has no window yet, did not answer in time or rejected the launch.
    pub fn forward_within(&self, launch: &Launch, timeout: Duration) -> Result<(), WinError> {
        let deadline = Instant::now() + timeout;
        let hwnd = loop {
            // SAFETY: the class name is a valid NUL-terminated string; no window title filter.
            let found =
                unsafe { FindWindowExW(Some(HWND_MESSAGE), None, &self.class, PCWSTR::null()) };
            match found {
                Ok(hwnd) => break hwnd,
                Err(error) if Instant::now() >= deadline => {
                    return Err(WinError::from_os("FindWindowExW", error));
                }
                Err(_) => std::thread::sleep(POLL_INTERVAL),
            }
        };
        let payload = launch.encode();
        let data = COPYDATASTRUCT {
            dwData: COPYDATA_TAG,
            // A command line is at most 32 767 characters.
            cbData: payload.len() as u32,
            lpData: payload.as_ptr().cast_mut().cast::<c_void>(),
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = u32::try_from(remaining.as_millis())
            .unwrap_or(u32::MAX)
            .max(1);
        let mut answer = 0_usize;
        // SAFETY: `data` and `payload` outlive the call, which does not return before the
        // receiver is done with them (or gives up); the receiver only reads.
        let sent = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM(std::ptr::from_ref(&data) as isize),
                SMTO_ABORTIFHUNG,
                millis,
                Some(&raw mut answer),
            )
        };
        if sent.0 == 0 {
            return Err(WinError::from_os(
                "SendMessageTimeoutW",
                windows::core::Error::from_thread(),
            ));
        }
        if answer == 0 {
            return Err(WinError::Invalid(
                "the running instance rejected the launch",
            ));
        }
        let mut process = 0_u32;
        // SAFETY: `process` is a valid out value.
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut process)) };
        // Best effort: only lets the running instance bring itself to the front. A refusal
        // changes nothing about the launch, which was delivered.
        // SAFETY: plain call with a process id.
        let _ = unsafe { AllowSetForegroundWindow(process) };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use windows::Win32::UI::WindowsAndMessaging::IsWindow;

    fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    #[test]
    fn launch_round_trips_paths_and_flag() {
        let launch = Launch {
            paths: vec![
                PathBuf::from(r"C:\Users\dilek\Notlar\günlük ☃ 😀.md"),
                PathBuf::from(r"\\server\share\a b\c.txt"),
                PathBuf::new(),
            ],
            new_window: true,
        };
        assert_eq!(Launch::decode(&launch.encode()), Some(launch));
        let none = Launch::default();
        assert_eq!(none.encode(), vec![0, 0]);
        assert_eq!(Launch::decode(&none.encode()), Some(none));
    }

    #[test]
    fn launch_round_trips_paths_that_are_not_unicode() {
        // A lone surrogate is legal in a Windows file name but not in UTF-16 text.
        let lone = OsString::from_wide(&[0x43, 0x3A, 0x5C, 0xD800, 0x61]);
        let launch = Launch {
            paths: vec![PathBuf::from(lone.clone())],
            new_window: false,
        };
        let decoded = Launch::decode(&launch.encode()).unwrap();
        assert_eq!(decoded.paths[0].as_os_str(), lone.as_os_str());
    }

    #[test]
    fn decode_rejects_malformed_payloads() {
        assert_eq!(Launch::decode(&[]), None, "empty");
        assert_eq!(Launch::decode(&[0]), None, "half a unit");
        assert_eq!(Launch::decode(&[2, 0]), None, "unknown flag bit");
        assert_eq!(
            Launch::decode(&[0, 0, b'a', 0]),
            None,
            "path without terminator"
        );
        assert_eq!(
            Launch::decode(&[1, 0]),
            Some(Launch {
                paths: Vec::new(),
                new_window: true
            })
        );
    }

    #[test]
    fn from_args_parses_the_flag_and_makes_paths_absolute() {
        let launch = Launch::from_args([
            os("notes.md"),
            os(NEW_WINDOW_ARG),
            os(""),
            os("--other-option"),
            os(r"C:\data\folder"),
        ]);
        assert!(launch.new_window);
        assert_eq!(
            launch.paths,
            vec![
                std::path::absolute("notes.md").unwrap(),
                PathBuf::from(r"C:\data\folder")
            ]
        );
        assert!(
            launch
                .paths
                .iter()
                .all(|path| Path::new(path).is_absolute())
        );
        assert_eq!(Launch::from_args(Vec::<OsString>::new()), Launch::default());
    }

    #[test]
    fn names_are_derived_from_the_application_name() {
        let config = InstanceConfig::new("SSWriter");
        assert_eq!(config.mutex_name(), r"Local\SSWriter.Instance");
        assert_eq!(config.window_class(), "SSWriter.InstanceWindow");
        assert_eq!(config, InstanceConfig::new(String::from("SSWriter")));
    }

    #[test]
    fn a_window_that_cannot_be_created_leaves_no_class_behind() {
        let class = HSTRING::from(format!("SSWriterTest{}.InstanceWindow", std::process::id()));
        let state = State {
            queue: RefCell::new(VecDeque::new()),
            wake: None,
        };
        // A parent that is no window: the class registers, the window cannot be created.
        let error =
            create_window(&class, &state, HWND(std::ptr::dangling_mut::<c_void>())).unwrap_err();
        assert!(
            matches!(
                error,
                WinError::Os {
                    context: "CreateWindowExW",
                    ..
                }
            ),
            "{error:?}"
        );
        // The class is gone again: the same name registers and creates a window now.
        let hwnd = create_window(&class, &state, HWND_MESSAGE).unwrap();
        // The class is registered now: a second window of the same name is refused by the
        // registration, which is reported as such and leaves the first window alone.
        let again = create_window(&class, &state, HWND_MESSAGE).unwrap_err();
        assert!(
            again.to_string().starts_with("RegisterClassExW failed"),
            "{again}"
        );
        assert!(!again.is_device_lost());
        // SAFETY: a plain query of the first window, still alive.
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
        // SAFETY: destroys the window created above on its own thread and removes its class.
        unsafe {
            DestroyWindow(hwnd).unwrap();
            UnregisterClassW(&class, Some(module_instance())).unwrap();
        }
    }
}
