//! Single instance: two in-process "instances" with a unique mutex name and message-only
//! windows. Launches travel through real `WM_COPYDATA` messages, also across threads.

use std::path::PathBuf;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ssw_win::com::WinError;
use ssw_win::instance::{
    Acquired, COPYDATA_TAG, InstanceConfig, Launch, PrimaryInstance, SecondaryInstance, Wake,
};
use ssw_win::window::MessageWindow;
use windows::Win32::Foundation::{CloseHandle, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DispatchMessageW, GWLP_USERDATA, MSG, PM_REMOVE, PeekMessageW,
    RegisterClassExW, SendMessageW, SetWindowLongPtrW, TranslateMessage, UnregisterClassW, WM_APP,
    WM_COPYDATA, WNDCLASSEXW,
};
use windows::core::{HSTRING, PCWSTR};

/// A configuration no other test or process shares.
fn unique_config() -> InstanceConfig {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    InstanceConfig::new(format!(
        "SSWTest{}x{nanos}x{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn primary(config: &InstanceConfig, wake: Option<Wake>) -> PrimaryInstance {
    match config.acquire(wake).unwrap() {
        Acquired::Primary(first) => first,
        Acquired::Secondary(_) => panic!("expected to be the first instance"),
    }
}

fn secondary(config: &InstanceConfig) -> SecondaryInstance {
    match config.acquire(None).unwrap() {
        Acquired::Secondary(later) => later,
        Acquired::Primary(_) => panic!("expected another instance to be running"),
    }
}

/// Delivers the messages waiting for this thread's windows (also the ones other threads sent).
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: a standard message pump over this thread's queue; `msg` is a valid out value.
    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
        // SAFETY: `msg` was just filled by `PeekMessageW`.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Sends a hand-made `WM_COPYDATA` to `hwnd` and returns the window's answer.
fn send_raw(hwnd: HWND, tag: usize, payload: &[u8], null_data: bool) -> isize {
    let data = COPYDATASTRUCT {
        dwData: tag,
        cbData: payload.len() as u32,
        lpData: if null_data {
            null_mut()
        } else {
            payload.as_ptr().cast_mut().cast()
        },
    };
    // SAFETY: `data` and `payload` outlive the synchronous call; the receiver only reads them.
    unsafe {
        SendMessageW(
            hwnd,
            WM_COPYDATA,
            Some(WPARAM(0)),
            Some(LPARAM(&raw const data as isize)),
        )
    }
    .0
}

fn sample(new_window: bool) -> Launch {
    Launch {
        paths: vec![
            PathBuf::from(r"C:\Users\dilek\Notlar\günlük ☃.md"),
            PathBuf::from(r"D:\Projeler\Çalışma"),
        ],
        new_window,
    }
}

#[test]
fn a_later_launch_reaches_the_first_instance_and_wakes_the_application() {
    let config = unique_config();
    let target = MessageWindow::new().unwrap();
    let wake = Wake {
        hwnd: target.hwnd(),
        message: WM_APP + 7,
    };
    let first = primary(&config, Some(wake));
    let later = secondary(&config);
    let with_files = sample(false);
    let window_only = Launch {
        paths: Vec::new(),
        new_window: true,
    };
    later.forward(&with_files).unwrap();
    later.forward(&window_only).unwrap();
    assert_eq!(first.take_launches(), vec![with_files, window_only]);
    assert!(first.take_launches().is_empty());
    // One wake-up message per launch reached the window the instance was told about.
    for _ in 0..2 {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid out value; the filter selects the wake-up message only.
        let woken = unsafe {
            PeekMessageW(
                &mut msg,
                Some(target.hwnd()),
                WM_APP + 7,
                WM_APP + 7,
                PM_REMOVE,
            )
        };
        assert!(woken.as_bool());
    }
}

#[test]
fn a_launch_from_another_thread_is_delivered_while_the_first_instance_pumps() {
    let config = unique_config();
    let first = primary(&config, None);
    let later = secondary(&config);
    let launch = sample(true);
    std::thread::scope(|scope| {
        let sender = scope.spawn(|| later.forward(&launch).map_err(|error| error.to_string()));
        while !sender.is_finished() {
            pump();
            std::thread::sleep(Duration::from_millis(1));
        }
        sender.join().unwrap().unwrap();
    });
    assert_eq!(first.take_launches(), vec![launch]);
}

#[test]
fn messages_that_are_not_launches_are_rejected_and_queue_nothing() {
    let config = unique_config();
    let first = primary(&config, None);
    let hwnd = first.hwnd();
    let good = sample(false).encode();
    assert_eq!(send_raw(hwnd, COPYDATA_TAG, &good, false), 1);
    assert_eq!(first.take_launches(), vec![sample(false)]);

    assert_eq!(
        send_raw(hwnd, COPYDATA_TAG + 1, &good, false),
        0,
        "foreign tag"
    );
    assert_eq!(
        send_raw(hwnd, COPYDATA_TAG, &good[1..], false),
        0,
        "odd length"
    );
    assert_eq!(
        send_raw(hwnd, COPYDATA_TAG, &[2, 0], false),
        0,
        "unknown flag"
    );
    assert_eq!(send_raw(hwnd, COPYDATA_TAG, &[], true), 0, "no data");
    // SAFETY: a null `lparam` is a valid (if empty) message; the receiver checks for it.
    let bare = unsafe { SendMessageW(hwnd, WM_COPYDATA, None, None) };
    assert_eq!(bare.0, 0, "no COPYDATASTRUCT");
    assert!(first.take_launches().is_empty());
}

#[test]
fn a_window_without_state_rejects_launches() {
    let config = unique_config();
    let first = primary(&config, None);
    let later = secondary(&config);
    // The state of a window being torn down is already detached.
    // SAFETY: clears the pointer the window procedure reads; it only ever handles null.
    unsafe { SetWindowLongPtrW(first.hwnd(), GWLP_USERDATA, 0) };
    let error = later.forward(&sample(false)).unwrap_err();
    assert!(matches!(error, WinError::Invalid(_)), "{error}");
    assert!(first.take_launches().is_empty());
}

#[test]
fn dropping_the_first_instance_lets_the_next_launch_take_over() {
    let config = unique_config();
    let first = primary(&config, None);
    assert!(matches!(
        config.acquire(None).unwrap(),
        Acquired::Secondary(_)
    ));
    drop(first);
    let again = primary(&config, None);
    // Another application name is another instance.
    let other = primary(&unique_config(), None);
    assert_ne!(again.hwnd(), other.hwnd());
    secondary(&config).forward(&sample(false)).unwrap();
    assert_eq!(again.take_launches(), vec![sample(false)]);
    assert!(other.take_launches().is_empty());
}

#[test]
fn a_launch_finds_no_window_when_the_mutex_owner_never_creates_one() {
    let config = unique_config();
    // SAFETY: creates a named mutex with default security; closed below.
    let held = unsafe { CreateMutexW(None, false, &HSTRING::from(config.mutex_name())) }.unwrap();
    let later = secondary(&config);
    let started = Instant::now();
    let error = later
        .forward_within(&sample(false), Duration::from_millis(150))
        .unwrap_err();
    assert!(
        matches!(
            error,
            WinError::Os {
                context: "FindWindowExW",
                ..
            }
        ),
        "{error}"
    );
    assert!(started.elapsed() >= Duration::from_millis(150));
    // SAFETY: closes the handle created above.
    unsafe { CloseHandle(held) }.unwrap();
}

#[test]
fn a_first_instance_that_does_not_answer_times_out() {
    let config = unique_config();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let thread_config = config.clone();
    let owner = std::thread::spawn(move || {
        let first = primary(&thread_config, None);
        ready_tx.send(()).unwrap();
        // No message is pumped until released.
        release_rx.recv().unwrap();
        // Destroying the window discards the launch that timed out, so its payload (freed by
        // the sender) is never read.
        drop(first);
    });
    ready_rx.recv().unwrap();
    let later = secondary(&config);
    let error = later
        .forward_within(&sample(false), Duration::from_millis(200))
        .unwrap_err();
    assert!(
        matches!(
            error,
            WinError::Os {
                context: "SendMessageTimeoutW",
                ..
            }
        ),
        "{error}"
    );
    release_tx.send(()).unwrap();
    owner.join().unwrap();
}

/// A window procedure for a class a test registers itself.
unsafe extern "system" fn plain_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: default processing with the original arguments.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

#[test]
fn acquire_fails_when_the_window_class_is_taken_and_keeps_the_mutex_free() {
    let config = unique_config();
    let class = HSTRING::from(config.window_class());
    // SAFETY: querying the handle of the current module.
    let module = HINSTANCE(unsafe { GetModuleHandleW(None) }.unwrap().0);
    let description = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(plain_proc),
        hInstance: module,
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    // SAFETY: the class structure is fully initialized; the name outlives the call.
    assert_ne!(unsafe { RegisterClassExW(&description) }, 0);

    let error = config.acquire(None).unwrap_err();
    assert!(
        matches!(
            error,
            WinError::Os {
                context: "RegisterClassExW",
                ..
            }
        ),
        "{error}"
    );

    // The failed attempt released the mutex: once the class is gone this process is first.
    // SAFETY: removes the class registered above; no window uses it.
    unsafe { UnregisterClassW(&class, Some(module)) }.unwrap();
    let _first = primary(&config, None);
}
