//! File dialogs (`IFileOpenDialog` / `IFileSaveDialog`), the folder picker and
//! `TaskDialogIndirect` message boxes.
//!
//! Everything that prepares a dialog (filters, options, initial folder, button plans, task
//! dialog configuration) and everything that reads its answer (shell items to paths, command ids
//! to results) runs against real COM objects. Only the two calls that put a modal window on
//! screen sit behind [`DialogHost`]: [`SystemDialogHost`] makes them, tests substitute a host
//! that inspects the fully configured dialog and answers as the user would.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use ssw_ui::services::{
    Dialogs, FileFilter, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult,
    OpenFileOptions, PickFolderOptions, SaveFileOptions, ServiceError,
};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IBindCtx,
};
use windows::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOG_COMMON_BUTTON_FLAGS, TASKDIALOGCONFIG, TASKDIALOGCONFIG_0,
    TD_ERROR_ICON, TD_INFORMATION_ICON, TD_WARNING_ICON, TDCBF_CANCEL_BUTTON, TDCBF_NO_BUTTON,
    TDCBF_OK_BUTTON, TDCBF_YES_BUTTON, TaskDialogIndirect,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FILEOPENDIALOGOPTIONS, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog, IShellItem,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::{HRESULT, HSTRING, Interface, PCWSTR};

use super::failed;
use crate::fault::check;

const SERVICE: &str = "dialogs";

/// `HRESULT_FROM_WIN32(ERROR_CANCELLED)`: the user dismissed a common dialog.
const CANCELLED: HRESULT = HRESULT::from_win32(ERROR_CANCELLED.0);

/// Command ids of the custom buttons (the common ones keep `IDOK` ... `IDNO`).
const ID_OK: i32 = 1;
const ID_CANCEL: i32 = 2;
const ID_YES: i32 = 6;
const ID_NO: i32 = 7;
const ID_SAVE: i32 = 101;
const ID_DONT_SAVE: i32 = 102;

/// Localized labels of the buttons the system has no common button for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogStrings {
    pub save: String,
    pub dont_save: String,
}

/// The buttons of a message box: system common-button flags plus custom buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonPlan {
    /// `TDCBF_*` flags.
    pub common_flags: i32,
    /// `(command id, label)` of custom buttons, in display order.
    pub custom: Vec<(i32, String)>,
}

/// Which buttons to show for `buttons`.
#[must_use]
pub fn plan_buttons(buttons: MessageBoxButtons, strings: &DialogStrings) -> ButtonPlan {
    let (common_flags, custom) = match buttons {
        MessageBoxButtons::Ok => (TDCBF_OK_BUTTON.0, Vec::new()),
        MessageBoxButtons::OkCancel => (TDCBF_OK_BUTTON.0 | TDCBF_CANCEL_BUTTON.0, Vec::new()),
        MessageBoxButtons::YesNo => (TDCBF_YES_BUTTON.0 | TDCBF_NO_BUTTON.0, Vec::new()),
        MessageBoxButtons::YesNoCancel => (
            TDCBF_YES_BUTTON.0 | TDCBF_NO_BUTTON.0 | TDCBF_CANCEL_BUTTON.0,
            Vec::new(),
        ),
        MessageBoxButtons::SaveDontSaveCancel => (
            TDCBF_CANCEL_BUTTON.0,
            vec![
                (ID_SAVE, strings.save.clone()),
                (ID_DONT_SAVE, strings.dont_save.clone()),
            ],
        ),
    };
    ButtonPlan {
        common_flags,
        custom,
    }
}

/// The result for the command id a task dialog returned.
#[must_use]
pub fn map_result(id: i32) -> Option<MessageBoxResult> {
    match id {
        ID_OK => Some(MessageBoxResult::Ok),
        ID_CANCEL => Some(MessageBoxResult::Cancel),
        ID_YES => Some(MessageBoxResult::Yes),
        ID_NO => Some(MessageBoxResult::No),
        ID_SAVE => Some(MessageBoxResult::Save),
        ID_DONT_SAVE => Some(MessageBoxResult::DontSave),
        _ => None,
    }
}

/// `(display name, semicolon-separated patterns)` of every filter: the extensions are shown
/// next to the name, as in Explorer's own dialogs.
#[must_use]
pub fn filter_specs(filters: &[FileFilter]) -> Vec<(String, String)> {
    filters
        .iter()
        .filter(|filter| !filter.extensions.is_empty())
        .map(|filter| {
            let patterns: Vec<String> = filter
                .extensions
                .iter()
                .map(|extension| format!("*.{}", extension.trim_start_matches(['*', '.'])))
                .collect();
            (
                format!("{} ({})", filter.name, patterns.join(", ")),
                patterns.join(";"),
            )
        })
        .collect()
}

/// True when `error` is the user cancelling a common dialog.
#[must_use]
pub fn is_cancelled(code: HRESULT) -> bool {
    code == CANCELLED
}

/// The two OS calls that put a modal dialog on screen.
pub trait DialogHost {
    /// Shows the configured file dialog modally and returns the chosen items; empty when the
    /// user cancelled.
    ///
    /// # Errors
    /// The OS failed to show the dialog or to read its result.
    fn pick(
        &mut self,
        dialog: &IFileDialog,
        owner: Option<HWND>,
    ) -> windows::core::Result<Vec<IShellItem>>;

    /// Shows the described task dialog modally and returns the command id that closed it.
    ///
    /// # Errors
    /// The OS rejected the configuration or failed to show the dialog.
    fn task_dialog(&mut self, config: &TASKDIALOGCONFIG) -> windows::core::Result<i32>;
}

/// The real [`DialogHost`]: `IModalWindow::Show` and `TaskDialogIndirect`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDialogHost;

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

/// What a dialog returns once `Show` ended with `shown`: its items, nothing when the user
/// cancelled, the error otherwise.
fn items_after_show(
    dialog: &IFileDialog,
    shown: windows::core::Result<()>,
) -> windows::core::Result<Vec<IShellItem>> {
    match shown {
        Ok(()) => chosen_items(dialog),
        Err(error) if is_cancelled(error.code()) => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// The items a dismissed dialog returns: every selection of an open dialog, the single result
/// of a save dialog.
fn chosen_items(dialog: &IFileDialog) -> windows::core::Result<Vec<IShellItem>> {
    if let Ok(open) = dialog.cast::<IFileOpenDialog>() {
        // SAFETY: live COM objects; the array is enumerated by index within its count.
        unsafe {
            let results = check(open.GetResults())?;
            let count = check(results.GetCount())?;
            (0..count)
                .map(|index| check(results.GetItemAt(index)))
                .collect()
        }
    } else {
        // SAFETY: live COM object.
        Ok(vec![check(unsafe { dialog.GetResult() })?])
    }
}

/// The file-system path of a shell item.
fn item_path(item: &IShellItem) -> windows::core::Result<PathBuf> {
    // SAFETY: the display name is a CoTaskMem string that is copied and then freed.
    unsafe {
        let name = check(item.GetDisplayName(SIGDN_FILESYSPATH))?;
        let path = PathBuf::from(OsString::from_wide(name.as_wide()));
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        Ok(path)
    }
}

/// Applies `options`, the title, the file types and the initial folder to `dialog`.
fn configure(
    dialog: &IFileDialog,
    title: &str,
    filters: &[FileFilter],
    initial_dir: Option<&Path>,
    options: FILEOPENDIALOGOPTIONS,
) -> windows::core::Result<()> {
    let wide: Vec<(HSTRING, HSTRING)> = filter_specs(filters)
        .iter()
        .map(|(name, pattern)| {
            (
                HSTRING::from(name.as_str()),
                HSTRING::from(pattern.as_str()),
            )
        })
        .collect();
    let raw: Vec<COMDLG_FILTERSPEC> = wide
        .iter()
        .map(|(name, pattern)| COMDLG_FILTERSPEC {
            pszName: PCWSTR(name.as_ptr()),
            pszSpec: PCWSTR(pattern.as_ptr()),
        })
        .collect();
    // SAFETY: the filter strings (`wide`) outlive the calls; `dialog` is a live COM object.
    unsafe {
        let current = check(dialog.GetOptions())?;
        check(dialog.SetOptions(current | options))?;
        if !title.is_empty() {
            check(dialog.SetTitle(&HSTRING::from(title)))?;
        }
        if !raw.is_empty() {
            check(dialog.SetFileTypes(&raw))?;
        }
        if let Some(dir) = initial_dir
            && let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(
                &HSTRING::from(dir.as_os_str()),
                None::<&IBindCtx>,
            )
        {
            check(dialog.SetFolder(&folder))?;
        }
    }
    Ok(())
}

/// The Windows dialogs.
#[derive(Debug, Clone)]
pub struct WindowsDialogs<H = SystemDialogHost> {
    owner: Option<HWND>,
    strings: DialogStrings,
    host: H,
}

impl WindowsDialogs {
    /// Dialogs shown by the system, parented to `owner`.
    #[must_use]
    pub fn new(owner: Option<HWND>, strings: DialogStrings) -> Self {
        Self::with_host(owner, strings, SystemDialogHost)
    }
}

impl<H: DialogHost> WindowsDialogs<H> {
    /// Dialogs shown through `host`.
    #[must_use]
    pub fn with_host(owner: Option<HWND>, strings: DialogStrings, host: H) -> Self {
        Self {
            owner,
            strings,
            host,
        }
    }

    fn pick_paths(&mut self, dialog: &IFileDialog) -> Result<Vec<PathBuf>, ServiceError> {
        let items = self
            .host
            .pick(dialog, self.owner)
            .map_err(|error| failed(SERVICE, error))?;
        items
            .iter()
            .map(item_path)
            .collect::<windows::core::Result<Vec<PathBuf>>>()
            .map_err(|error| failed(SERVICE, error))
    }
}

/// Creates a file dialog of class `class` as an `IFileDialog`.
fn create_dialog<T: Interface>(
    class: &windows::core::GUID,
) -> Result<(T, IFileDialog), ServiceError> {
    // SAFETY: standard COM activation of an in-proc common dialog.
    let dialog: T = check(unsafe { CoCreateInstance(class, None, CLSCTX_INPROC_SERVER) })
        .map_err(|error| failed(SERVICE, error))?;
    let base = check(dialog.cast()).map_err(|error| failed(SERVICE, error))?;
    Ok((dialog, base))
}

impl<H: DialogHost> Dialogs for WindowsDialogs<H> {
    fn open_file(
        &mut self,
        options: &OpenFileOptions,
    ) -> Result<Option<Vec<PathBuf>>, ServiceError> {
        let (_dialog, base) = create_dialog::<IFileOpenDialog>(&FileOpenDialog)?;
        let mut flags = FOS_FORCEFILESYSTEM;
        if options.allow_multiple {
            flags |= FOS_ALLOWMULTISELECT;
        }
        configure(
            &base,
            &options.title,
            &options.filters,
            options.initial_dir.as_deref(),
            flags,
        )
        .map_err(|error| failed(SERVICE, error))?;
        let paths = self.pick_paths(&base)?;
        Ok((!paths.is_empty()).then_some(paths))
    }

    fn save_file(&mut self, options: &SaveFileOptions) -> Result<Option<PathBuf>, ServiceError> {
        let (_dialog, base) = create_dialog::<IFileSaveDialog>(&FileSaveDialog)?;
        configure(
            &base,
            &options.title,
            &options.filters,
            options.initial_dir.as_deref(),
            FOS_FORCEFILESYSTEM,
        )
        .map_err(|error| failed(SERVICE, error))?;
        // SAFETY: live COM object; the strings are copied by the call.
        unsafe {
            if let Some(name) = &options.suggested_name {
                check(base.SetFileName(&HSTRING::from(name.as_str())))
                    .map_err(|error| failed(SERVICE, error))?;
            }
            if let Some(extension) = &options.default_extension {
                check(base.SetDefaultExtension(&HSTRING::from(extension.as_str())))
                    .map_err(|error| failed(SERVICE, error))?;
            }
        }
        Ok(self.pick_paths(&base)?.into_iter().next())
    }

    fn pick_folder(
        &mut self,
        options: &PickFolderOptions,
    ) -> Result<Option<PathBuf>, ServiceError> {
        let (_dialog, base) = create_dialog::<IFileOpenDialog>(&FileOpenDialog)?;
        configure(
            &base,
            &options.title,
            &[],
            options.initial_dir.as_deref(),
            FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM,
        )
        .map_err(|error| failed(SERVICE, error))?;
        Ok(self.pick_paths(&base)?.into_iter().next())
    }

    fn message_box(
        &mut self,
        options: &MessageBoxOptions,
    ) -> Result<MessageBoxResult, ServiceError> {
        let plan = plan_buttons(options.buttons, &self.strings);
        let title = HSTRING::from(options.title.as_str());
        let message = HSTRING::from(options.message.as_str());
        let labels: Vec<HSTRING> = plan
            .custom
            .iter()
            .map(|(_, label)| HSTRING::from(label.as_str()))
            .collect();
        let buttons: Vec<TASKDIALOG_BUTTON> = plan
            .custom
            .iter()
            .zip(&labels)
            .map(|((id, _), label)| TASKDIALOG_BUTTON {
                nButtonID: *id,
                pszButtonText: PCWSTR(label.as_ptr()),
            })
            .collect();
        let icon = match options.icon {
            Some(MessageBoxIcon::Information) => TD_INFORMATION_ICON,
            Some(MessageBoxIcon::Warning) => TD_WARNING_ICON,
            Some(MessageBoxIcon::Error) => TD_ERROR_ICON,
            Some(MessageBoxIcon::Question) | None => PCWSTR::null(),
        };
        let config = TASKDIALOGCONFIG {
            cbSize: size_of::<TASKDIALOGCONFIG>() as u32,
            hwndParent: self.owner.unwrap_or_default(),
            dwCommonButtons: TASKDIALOG_COMMON_BUTTON_FLAGS(plan.common_flags),
            pszWindowTitle: PCWSTR(title.as_ptr()),
            Anonymous1: TASKDIALOGCONFIG_0 { pszMainIcon: icon },
            pszContent: PCWSTR(message.as_ptr()),
            cButtons: buttons.len() as u32,
            pButtons: if buttons.is_empty() {
                std::ptr::null()
            } else {
                buttons.as_ptr()
            },
            ..Default::default()
        };
        let pressed = self
            .host
            .task_dialog(&config)
            .map_err(|error| failed(SERVICE, error))?;
        map_result(pressed).ok_or_else(|| failed(SERVICE, format!("unexpected button {pressed}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::com::ComGuard;

    fn strings() -> DialogStrings {
        DialogStrings {
            save: "Kaydet".to_owned(),
            dont_save: "Kaydetme".to_owned(),
        }
    }

    #[test]
    fn button_plans_use_common_buttons_where_possible() {
        let cases = [
            (MessageBoxButtons::Ok, 1, 0),
            (MessageBoxButtons::OkCancel, 1 | 8, 0),
            (MessageBoxButtons::YesNo, 2 | 4, 0),
            (MessageBoxButtons::YesNoCancel, 2 | 4 | 8, 0),
            (MessageBoxButtons::SaveDontSaveCancel, 8, 2),
        ];
        for (buttons, flags, custom) in cases {
            let plan = plan_buttons(buttons, &strings());
            assert_eq!(plan.common_flags, flags, "{buttons:?}");
            assert_eq!(plan.custom.len(), custom, "{buttons:?}");
        }
        let plan = plan_buttons(MessageBoxButtons::SaveDontSaveCancel, &strings());
        assert_eq!(
            plan.custom,
            vec![(101, "Kaydet".to_owned()), (102, "Kaydetme".to_owned())]
        );
    }

    #[test]
    fn command_ids_map_to_results() {
        assert_eq!(map_result(1), Some(MessageBoxResult::Ok));
        assert_eq!(map_result(2), Some(MessageBoxResult::Cancel));
        assert_eq!(map_result(6), Some(MessageBoxResult::Yes));
        assert_eq!(map_result(7), Some(MessageBoxResult::No));
        assert_eq!(map_result(101), Some(MessageBoxResult::Save));
        assert_eq!(map_result(102), Some(MessageBoxResult::DontSave));
        assert_eq!(map_result(3), None);
    }

    #[test]
    fn filters_become_dialog_specs() {
        let filters = vec![
            FileFilter {
                name: "Markdown".to_owned(),
                extensions: vec!["md".to_owned(), ".markdown".to_owned()],
            },
            FileFilter {
                name: "Empty".to_owned(),
                extensions: vec![],
            },
            FileFilter {
                name: "Text".to_owned(),
                extensions: vec!["*.txt".to_owned()],
            },
        ];
        assert_eq!(
            filter_specs(&filters),
            vec![
                (
                    "Markdown (*.md, *.markdown)".to_owned(),
                    "*.md;*.markdown".to_owned()
                ),
                ("Text (*.txt)".to_owned(), "*.txt".to_owned()),
            ]
        );
        assert!(filter_specs(&[]).is_empty());
    }

    #[test]
    fn cancellation_is_recognized() {
        assert!(is_cancelled(HRESULT(0x8007_04C7_u32 as i32)));
        assert!(!is_cancelled(HRESULT(0x8000_4005_u32 as i32)));
    }

    #[test]
    fn a_dialog_that_was_never_shown_has_no_result() {
        let _com = ComGuard::apartment_threaded().unwrap();
        let (_open, open_base) = create_dialog::<IFileOpenDialog>(&FileOpenDialog).unwrap();
        assert!(chosen_items(&open_base).is_err());
        let (_save, save_base) = create_dialog::<IFileSaveDialog>(&FileSaveDialog).unwrap();
        assert!(chosen_items(&save_base).is_err());
    }

    #[test]
    fn the_outcome_of_showing_a_dialog_is_items_nothing_or_the_error() {
        use windows::Win32::Foundation::E_FAIL;

        let _com = ComGuard::apartment_threaded().unwrap();
        let (_open, dialog) = create_dialog::<IFileOpenDialog>(&FileOpenDialog).unwrap();
        // The user cancelled: not a failure, just no items.
        let cancelled = items_after_show(&dialog, Err(CANCELLED.into()));
        assert!(cancelled.unwrap().is_empty());
        // Any other failure is reported as it is.
        let failed = items_after_show(&dialog, Err(E_FAIL.into())).unwrap_err();
        assert_eq!(failed.code(), E_FAIL);
        // Accepted, but nothing was ever chosen: the dialog has no result to read.
        assert!(items_after_show(&dialog, Ok(())).is_err());
    }
}
