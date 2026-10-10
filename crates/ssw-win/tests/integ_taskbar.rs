//! Taskbar progress: the request sequence the taskbar receives (recorded), and the real
//! `ITaskbarList3` driven for a hidden window that never gets a taskbar button.

use std::cell::RefCell;
use std::rc::Rc;

use ssw_win::com::{ComGuard, WinError};
use ssw_win::taskbar::{
    Progress, SystemTaskbar, Taskbar, TaskbarApi, taskbar_button_created_message,
};
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::UI::Shell::{
    TBPF_ERROR, TBPF_INDETERMINATE, TBPF_NOPROGRESS, TBPF_NORMAL, TBPFLAG,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Call {
    State(i32),
    Value(u64, u64),
}

/// Records every call; fails the ones selected.
struct Recording {
    calls: Rc<RefCell<Vec<Call>>>,
    fail_state: bool,
    fail_value: bool,
}

impl TaskbarApi for Recording {
    fn set_state(&mut self, _hwnd: HWND, state: TBPFLAG) -> windows::core::Result<()> {
        self.calls.borrow_mut().push(Call::State(state.0));
        if self.fail_state {
            return Err(E_FAIL.into());
        }
        Ok(())
    }

    fn set_value(&mut self, _hwnd: HWND, completed: u64, total: u64) -> windows::core::Result<()> {
        self.calls.borrow_mut().push(Call::Value(completed, total));
        if self.fail_value {
            return Err(E_FAIL.into());
        }
        Ok(())
    }
}

fn recording(fail_state: bool, fail_value: bool) -> (Taskbar<Recording>, Rc<RefCell<Vec<Call>>>) {
    let calls = Rc::new(RefCell::new(Vec::new()));
    let api = Recording {
        calls: Rc::clone(&calls),
        fail_state,
        fail_value,
    };
    (Taskbar::with_api(HWND::default(), api), calls)
}

fn normal(completed: u64, total: u64) -> Progress {
    Progress::Normal { completed, total }
}

#[test]
fn progress_changes_become_state_then_value_calls() {
    let (mut taskbar, calls) = recording(false, false);
    taskbar.set(Progress::Indeterminate).unwrap();
    taskbar.set(normal(1, 4)).unwrap();
    taskbar.set(normal(2, 4)).unwrap();
    taskbar
        .set(Progress::Error {
            completed: 2,
            total: 4,
        })
        .unwrap();
    taskbar.set(Progress::None).unwrap();
    assert_eq!(
        *calls.borrow(),
        [
            Call::State(TBPF_INDETERMINATE.0),
            Call::State(TBPF_NORMAL.0),
            Call::Value(1, 4),
            Call::State(TBPF_NORMAL.0),
            Call::Value(2, 4),
            Call::State(TBPF_ERROR.0),
            Call::Value(2, 4),
            Call::State(TBPF_NOPROGRESS.0),
        ]
    );
    assert_eq!(taskbar.progress(), Progress::None);
}

#[test]
fn an_unchanged_request_makes_no_call() {
    let (mut taskbar, calls) = recording(false, false);
    taskbar.set(Progress::None).unwrap();
    assert!(calls.borrow().is_empty(), "nothing is shown yet");
    taskbar.set(normal(1, 2)).unwrap();
    let after_first = calls.borrow().len();
    taskbar.set(normal(1, 2)).unwrap();
    assert_eq!(calls.borrow().len(), after_first);
}

#[test]
fn a_refused_call_is_reported_and_keeps_the_shown_progress() {
    let (mut state_fails, calls) = recording(true, false);
    let error = state_fails.set(normal(1, 2)).unwrap_err();
    assert!(
        matches!(error, WinError::Os { context, .. } if context.contains("SetProgressState")),
        "{error}"
    );
    assert_eq!(*calls.borrow(), [Call::State(TBPF_NORMAL.0)]);
    assert_eq!(state_fails.progress(), Progress::None);

    let (mut value_fails, _) = recording(false, true);
    let error = value_fails.set(normal(1, 2)).unwrap_err();
    assert!(
        matches!(error, WinError::Os { context, .. } if context.contains("SetProgressValue")),
        "{error}"
    );
    assert_eq!(value_fails.progress(), Progress::None);
}

#[test]
fn a_recreated_button_gets_the_current_progress_again() {
    let (mut taskbar, calls) = recording(false, false);
    taskbar.set(normal(3, 9)).unwrap();
    calls.borrow_mut().clear();
    taskbar.button_created().unwrap();
    assert_eq!(
        *calls.borrow(),
        [Call::State(TBPF_NORMAL.0), Call::Value(3, 9)]
    );
}

#[test]
fn dropping_the_taskbar_clears_a_visible_progress_only() {
    let (taskbar, calls) = recording(false, false);
    drop(taskbar);
    assert!(calls.borrow().is_empty());

    let (mut taskbar, calls) = recording(false, false);
    taskbar.set(Progress::Indeterminate).unwrap();
    calls.borrow_mut().clear();
    drop(taskbar);
    assert_eq!(*calls.borrow(), [Call::State(TBPF_NOPROGRESS.0)]);
}

#[test]
fn the_button_created_message_is_a_registered_message() {
    let message = taskbar_button_created_message();
    assert!(
        (0xC000..=0xFFFF).contains(&message),
        "registered messages live in 0xC000..=0xFFFF, got {message:#x}"
    );
    assert_eq!(message, taskbar_button_created_message());
}

/// A top-level window that is never shown: no taskbar button, no activation.
fn hidden_window() -> HWND {
    // SAFETY: creates a window of the system `STATIC` class without `WS_VISIBLE`.
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("STATIC"),
            w!("ssw-win taskbar test"),
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
fn the_real_taskbar_accepts_every_progress_state_for_a_hidden_window() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let hwnd = hidden_window();
    {
        let mut taskbar = Taskbar::new(hwnd).unwrap();
        taskbar.set(Progress::Indeterminate).unwrap();
        taskbar.set(normal(1, 4)).unwrap();
        taskbar
            .set(Progress::Error {
                completed: 1,
                total: 4,
            })
            .unwrap();
        taskbar.button_created().unwrap();
        taskbar.set(Progress::None).unwrap();
    }
    {
        let mut system = SystemTaskbar::new().unwrap();
        system.set_state(hwnd, TBPF_NORMAL).unwrap();
        system.set_value(hwnd, 5, 10).unwrap();
        system.set_state(hwnd, TBPF_NOPROGRESS).unwrap();
    }
    // SAFETY: destroys the window created above on its owning thread.
    unsafe { DestroyWindow(hwnd) }.unwrap();
}
