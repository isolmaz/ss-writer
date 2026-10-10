//! The calls that put system UI on the screen and wait for the user: the popup menu
//! (`TrackPopupMenuEx`), the file dialogs (`IModalWindow::Show`), the task dialog
//! (`TaskDialogIndirect`) and the print dialog (`PrintDlgEx`).
//!
//! Everything around these calls (building the menu, the dialog configuration and the print
//! request, reading the answers) lives in [`crate::menus`], [`crate::services::dialogs`] and
//! [`crate::print`], where tests drive it through hosts that answer as a user would. The
//! calls themselves are kept here and nowhere else, because a test never puts system UI on the
//! user's screen. This file is the one documented exception of the coverage gate
//! (`cargo xtask coverage`, CONTRIBUTING.md); it holds nothing that could be tested without
//! showing such UI.

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Controls::Dialogs::{PRINTPAGERANGE, PrintDlgExW};
use windows::Win32::UI::Controls::{TASKDIALOGCONFIG, TaskDialogIndirect};
use windows::Win32::UI::Shell::{IFileDialog, IShellItem};
use windows::Win32::UI::WindowsAndMessaging::{
    HMENU, PostMessageW, SetForegroundWindow, TrackPopupMenuEx, WM_NULL,
};

use crate::com::WinError;
use crate::menus::{PopupHost, PopupRequest, SystemPopupHost};
use crate::print::{
    MAX_RANGES, PrintDialogHost, PrintDialogRequest, PrintSelection, SystemPrintDialogHost,
    dialog_for, finish_dialog,
};
use crate::services::dialogs::{DialogHost, SystemDialogHost, items_after_show};

impl PopupHost for SystemPopupHost {
    fn track(&mut self, menu: HMENU, request: &PopupRequest) -> Option<u32> {
        // SAFETY: `menu` is a valid menu and `request.owner` a window of this thread. A refused
        // foreground change only affects how a click outside dismisses the menu.
        let chosen = unsafe {
            let _ = SetForegroundWindow(request.owner);
            let chosen = TrackPopupMenuEx(
                menu,
                request.flags,
                request.x,
                request.y,
                request.owner,
                None,
            );
            let _ = PostMessageW(Some(request.owner), WM_NULL, WPARAM(0), LPARAM(0));
            chosen
        };
        u32::try_from(chosen.0).ok().filter(|&id| id != 0)
    }
}

impl DialogHost for SystemDialogHost {
    fn pick(
        &mut self,
        dialog: &IFileDialog,
        owner: Option<HWND>,
    ) -> windows::core::Result<Vec<IShellItem>> {
        // SAFETY: modal dialog shown on the calling thread; `dialog` is a live COM object.
        let shown = unsafe { dialog.Show(owner) };
        items_after_show(dialog, shown)
    }

    fn task_dialog(&mut self, config: &TASKDIALOGCONFIG) -> windows::core::Result<i32> {
        let mut pressed = 0i32;
        // SAFETY: the caller keeps the configuration and every string/array it points to alive
        // for the (modal) call.
        let shown = unsafe { TaskDialogIndirect(config, Some(&mut pressed), None, None) };
        // The command id only means something when the dialog was shown.
        shown.and(Ok(pressed))
    }
}

impl PrintDialogHost for SystemPrintDialogHost {
    fn show(
        &mut self,
        owner: HWND,
        request: &PrintDialogRequest,
    ) -> Result<Option<PrintSelection>, WinError> {
        let mut ranges = [PRINTPAGERANGE::default(); MAX_RANGES];
        let mut dialog = dialog_for(owner, request, &mut ranges);
        // SAFETY: `dialog` is initialized and points at `ranges`; both outlive the modal call.
        let shown = unsafe { PrintDlgExW(&mut dialog) };
        finish_dialog(shown, &dialog, &ranges, request.page_count)
    }
}
