//! Dialogs and shell requests are prepared by the real code against real COM objects; only the
//! calls that would put a window on screen or start a program are answered by a scripted host,
//! which also inspects what it was handed. The clock is measured against real time.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use ssw_ui::services::{
    Clock, Dialogs, FileFilter, MessageBoxButtons, MessageBoxIcon, MessageBoxOptions,
    MessageBoxResult, OpenFileOptions, PickFolderOptions, SaveFileOptions, ServiceError, Shell,
};
use ssw_win::com::{ComGuard, WinError};
use ssw_win::services::clock::QpcClock;
use ssw_win::services::dialogs::{DialogHost, DialogStrings, SystemDialogHost, WindowsDialogs};
use ssw_win::services::service_error;
use ssw_win::services::shell::{ItemIdList, ShellHost, WindowsShell};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, E_UNEXPECTED, ERROR_CANCELLED, HWND, S_OK};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Controls::{TASKDIALOG_BUTTON, TASKDIALOGCONFIG};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOLDERID_RecycleBinFolder,
    FileOperation, IEnumShellItems, IFileOperation, IFileOperationProgressSink, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::Shell::{
    FDAP, FILEOPENDIALOGOPTIONS, FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    IFileDialog, IFileDialog_Impl, IFileDialogEvents, IFileOpenDialog, IFileOpenDialog_Impl,
    IModalWindow_Impl, IShellItem, IShellItemArray, IShellItemFilter, SEE_MASK_NOASYNC,
    SHCreateItemFromParsingName, SHCreateShellItemArrayFromShellItem, SHELLEXECUTEINFOW,
    SIGDN_FILESYSPATH,
};
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

/// A unique token for the names of items a test recycles; purges exactly the Recycle Bin items
/// carrying the token (permanently, without UI) on drop, also when the test fails halfway.
struct RecycleBinGuard {
    token: String,
}

impl RecycleBinGuard {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self {
            token: format!("ssw-win-test-{}-{nanos}", std::process::id()),
        }
    }

    fn name(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.token)
    }

    /// Deletes the Recycle Bin items whose name contains the token; returns how many there were.
    fn purge(&self) -> Result<usize, String> {
        // COM state is per thread: use a short-lived apartment of our own. COM errors are not
        // `Send`, so they cross the thread boundary as text.
        let token = self.token.clone();
        std::thread::spawn(move || purge_on_sta(&token).map_err(|error| error.to_string()))
            .join()
            .unwrap_or_else(|_| Err("the purge thread panicked".to_owned()))
    }
}

impl Drop for RecycleBinGuard {
    fn drop(&mut self) {
        // Best effort: a failing cleanup must not mask the test's own result.
        let _ = self.purge();
    }
}

fn purge_on_sta(token: &str) -> windows::core::Result<usize> {
    // SAFETY: initializes COM for this dedicated thread; balanced by `CoUninitialize` below.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
    let result = purge_items(token);
    // SAFETY: balances the successful `CoInitializeEx` above.
    unsafe { CoUninitialize() };
    result
}

fn purge_items(token: &str) -> windows::core::Result<usize> {
    // SAFETY: `FOLDERID_RecycleBinFolder` is a valid GUID; no token means the current user.
    let bin: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_RecycleBinFolder, KF_FLAG_DEFAULT, None) }?;
    // SAFETY: `bin` is a valid shell item on this thread; `BHID_EnumItems` yields an enumerator.
    let items: IEnumShellItems = unsafe { bin.BindToHandler(None::<&IBindCtx>, &BHID_EnumItems) }?;
    let mut ours = Vec::new();
    loop {
        let mut slot: [Option<IShellItem>; 1] = [None];
        let mut fetched = 0u32;
        // SAFETY: `slot` has room for the one requested item and `fetched` is a valid out value.
        unsafe { items.Next(&mut slot, Some(&raw mut fetched)) }?;
        let Some(item) = slot[0].take() else {
            break;
        };
        // SAFETY: `item` is a valid shell item; the display name is a CoTaskMem string that is
        // copied and then released exactly once.
        let text = unsafe {
            let name = item.GetDisplayName(SIGDN_NORMALDISPLAY)?;
            let text = String::from_utf16_lossy(name.as_wide());
            CoTaskMemFree(Some(name.0.cast_const().cast()));
            text
        };
        if text.contains(token) {
            ours.push(item);
        }
    }
    if ours.is_empty() {
        return Ok(0);
    }
    // SAFETY: `FileOperation` is the registered CLSID of `IFileOperation`; no `FOF_ALLOWUNDO`
    // means deleting from the bin is permanent; no UI of any kind.
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None::<&IUnknown>, CLSCTX_ALL)?;
        operation.SetOperationFlags(FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT)?;
        for item in &ours {
            operation.DeleteItem(item, None::<&IFileOperationProgressSink>)?;
        }
        operation.PerformOperations()?;
    }
    Ok(ours.len())
}

fn item_for(path: &Path) -> IShellItem {
    // SAFETY: valid NUL-terminated path of an existing file.
    unsafe {
        SHCreateItemFromParsingName::<_, _, IShellItem>(
            &HSTRING::from(path.as_os_str()),
            None::<&IBindCtx>,
        )
    }
    .unwrap()
}

fn path_of(item: &IShellItem) -> PathBuf {
    // SAFETY: the display name is a CoTaskMem string copied and freed here.
    unsafe {
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).unwrap();
        let path = PathBuf::from(name.to_string().unwrap());
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        path
    }
}

fn temp_file(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("ssw-win-services-{}-{name}", std::process::id()));
    std::fs::write(&path, b"x").unwrap();
    path
}

/// What the scripted dialog host saw of a file dialog.
#[derive(Debug, Clone, PartialEq)]
struct FileDialogCall {
    force_file_system: bool,
    multi_select: bool,
    pick_folders: bool,
    /// The folder the dialog reports before it is shown, when it reports one.
    folder: Option<PathBuf>,
    /// The edit-box text the dialog reports before it is shown, when it reports one.
    file_name: Option<String>,
    owner: Option<isize>,
}

/// What the scripted dialog host saw of a task dialog.
#[derive(Debug, Clone, PartialEq)]
struct TaskDialogCall {
    title: String,
    content: String,
    common_buttons: i32,
    custom_buttons: Vec<(i32, String)>,
    has_icon: bool,
}

#[derive(Default)]
struct Seen {
    files: Vec<FileDialogCall>,
    tasks: Vec<TaskDialogCall>,
}

struct ScriptedDialogs {
    seen: Rc<RefCell<Seen>>,
    picked: Vec<PathBuf>,
    pressed: i32,
    fail: bool,
}

impl DialogHost for ScriptedDialogs {
    fn pick(
        &mut self,
        dialog: &IFileDialog,
        owner: Option<HWND>,
    ) -> windows::core::Result<Vec<IShellItem>> {
        if self.fail {
            return Err(E_FAIL.into());
        }
        // SAFETY: plain getters on the live dialog; the strings are freed after copying.
        let call = unsafe {
            let options = dialog.GetOptions()?;
            // The edit-box text and the current folder are read back from a dialog that was
            // never shown; the OS answers `E_FAIL` for what does not exist yet, which is not an
            // error of the configuration under test.
            let file_name = dialog.GetFileName().ok().map(|name| {
                let text = name.to_string().unwrap();
                CoTaskMemFree(Some(name.0.cast_const().cast()));
                text
            });
            FileDialogCall {
                force_file_system: options.0 & FOS_FORCEFILESYSTEM.0 != 0,
                multi_select: options.0 & FOS_ALLOWMULTISELECT.0 != 0,
                pick_folders: options.0 & FOS_PICKFOLDERS.0 != 0,
                folder: dialog.GetFolder().ok().map(|folder| path_of(&folder)),
                file_name,
                owner: owner.map(|hwnd| hwnd.0 as isize),
            }
        };
        self.seen.borrow_mut().files.push(call);
        Ok(self.picked.iter().map(|path| item_for(path)).collect())
    }

    fn task_dialog(&mut self, config: &TASKDIALOGCONFIG) -> windows::core::Result<i32> {
        if self.fail {
            return Err(E_FAIL.into());
        }
        let (title, content) = (config.pszWindowTitle, config.pszContent);
        let (count, buttons) = (config.cButtons, config.pButtons);
        let common = config.dwCommonButtons.0;
        // SAFETY: the union holds the main icon as a pointer-sized value, and `pszMainIcon`
        // is the member the code under test writes.
        let icon = unsafe { config.Anonymous1.pszMainIcon };
        // SAFETY: the strings and the button array outlive this call, as the contract states.
        let call = unsafe {
            let custom: &[TASKDIALOG_BUTTON] = if count == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(buttons, count as usize)
            };
            TaskDialogCall {
                title: title.to_string().unwrap(),
                content: content.to_string().unwrap(),
                common_buttons: common,
                custom_buttons: custom
                    .iter()
                    .map(|button| {
                        let (id, text) = (button.nButtonID, button.pszButtonText);
                        (id, text.to_string().unwrap())
                    })
                    .collect(),
                has_icon: !icon.is_null(),
            }
        };
        self.seen.borrow_mut().tasks.push(call);
        Ok(self.pressed)
    }
}

fn scripted(
    picked: Vec<PathBuf>,
    pressed: i32,
    fail: bool,
) -> (WindowsDialogs<ScriptedDialogs>, Rc<RefCell<Seen>>) {
    let seen = Rc::new(RefCell::new(Seen::default()));
    let host = ScriptedDialogs {
        seen: Rc::clone(&seen),
        picked,
        pressed,
        fail,
    };
    let strings = DialogStrings {
        save: "Kaydet".to_owned(),
        dont_save: "Kaydetme".to_owned(),
    };
    (WindowsDialogs::with_host(None, strings, host), seen)
}

fn failed(error: &ServiceError, service: &str) -> bool {
    matches!(error, ServiceError::Failed { service: name, .. } if *name == service)
}

#[test]
fn open_dialogs_are_configured_and_their_items_become_paths() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let (first, second) = (temp_file("a.md"), temp_file("b.txt"));
    let dir = std::env::temp_dir();
    let filters = vec![FileFilter {
        name: "Markdown".to_owned(),
        extensions: vec!["md".to_owned()],
    }];
    let (mut dialogs, seen) = scripted(vec![first.clone(), second.clone()], 0, false);
    let multiple = OpenFileOptions {
        title: "Open".to_owned(),
        filters: filters.clone(),
        allow_multiple: true,
        initial_dir: Some(dir.clone()),
    };
    let paths = dialogs.open_file(&multiple).unwrap().unwrap();
    assert_eq!(paths, [first.clone(), second.clone()]);
    let single = OpenFileOptions {
        allow_multiple: false,
        ..multiple.clone()
    };
    dialogs.open_file(&single).unwrap();
    // A missing initial folder is ignored rather than failing the dialog.
    let missing = OpenFileOptions {
        initial_dir: Some(dir.join("ssw-win-no-such-folder-1f4a")),
        ..single
    };
    dialogs.open_file(&missing).unwrap();
    let calls = seen.borrow().files.clone();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].force_file_system && calls[0].multi_select && !calls[0].pick_folders);
    assert!(calls[1].force_file_system && !calls[1].multi_select);
    // When the dialog reports its folder before being shown, it is the one that was asked for.
    if let Some(folder) = &calls[0].folder {
        assert_eq!(folder.canonicalize().unwrap(), dir.canonicalize().unwrap());
    }
    assert_ne!(
        calls[2].folder.as_deref(),
        Some(dir.join("ssw-win-no-such-folder-1f4a").as_path())
    );
    for call in &calls {
        assert_eq!(call.owner, None);
    }
    std::fs::remove_file(first).unwrap();
    std::fs::remove_file(second).unwrap();
}

#[test]
fn cancelled_dialogs_return_none_and_host_failures_are_reported() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let (mut cancelled, _) = scripted(Vec::new(), 0, false);
    assert_eq!(
        cancelled.open_file(&OpenFileOptions::default()).unwrap(),
        None
    );
    assert_eq!(
        cancelled.save_file(&SaveFileOptions::default()).unwrap(),
        None
    );
    assert_eq!(
        cancelled
            .pick_folder(&PickFolderOptions::default())
            .unwrap(),
        None
    );
    let (mut broken, _) = scripted(Vec::new(), 0, true);
    assert!(failed(
        &broken.open_file(&OpenFileOptions::default()).unwrap_err(),
        "dialogs"
    ));
    assert!(failed(
        &broken.save_file(&SaveFileOptions::default()).unwrap_err(),
        "dialogs"
    ));
    assert!(failed(
        &broken
            .pick_folder(&PickFolderOptions::default())
            .unwrap_err(),
        "dialogs"
    ));
    assert!(failed(
        &broken
            .message_box(&MessageBoxOptions::default())
            .unwrap_err(),
        "dialogs"
    ));
}

#[test]
fn save_and_folder_dialogs_carry_their_options() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let target = temp_file("save.md");
    let (mut dialogs, seen) = scripted(vec![target.clone()], 0, false);
    let saved = dialogs
        .save_file(&SaveFileOptions {
            title: "Save as".to_owned(),
            filters: Vec::new(),
            suggested_name: Some("taslak.md".to_owned()),
            default_extension: Some("md".to_owned()),
            initial_dir: Some(std::env::temp_dir()),
        })
        .unwrap();
    assert_eq!(saved, Some(target.clone()));
    let folder = dialogs
        .pick_folder(&PickFolderOptions {
            title: "Folder".to_owned(),
            initial_dir: None,
        })
        .unwrap();
    assert_eq!(
        folder,
        Some(target.clone()),
        "the first picked item is the answer"
    );
    let calls = seen.borrow().files.clone();
    // `SetFileName` succeeded (the call above returned `Ok`); when the dialog reports its edit
    // box before being shown, it holds the suggested name or nothing, never another name.
    assert!(
        calls[0]
            .file_name
            .as_deref()
            .is_none_or(|name| name.is_empty() || name == "taslak.md"),
        "{:?}",
        calls[0].file_name
    );
    assert!(calls[0].force_file_system && !calls[0].pick_folders && !calls[0].multi_select);
    assert!(calls[1].pick_folders && calls[1].force_file_system);
    std::fs::remove_file(target).unwrap();
}

#[test]
fn message_boxes_hand_the_planned_buttons_to_the_task_dialog() {
    let (mut dialogs, seen) = scripted(Vec::new(), 101, false);
    let result = dialogs
        .message_box(&MessageBoxOptions {
            title: "Kaydet?".to_owned(),
            message: "Değişiklikler kaydedilsin mi?".to_owned(),
            buttons: MessageBoxButtons::SaveDontSaveCancel,
            icon: Some(MessageBoxIcon::Warning),
        })
        .unwrap();
    assert_eq!(result, MessageBoxResult::Save);
    let (mut yes_no, yes_no_seen) = scripted(Vec::new(), 7, false);
    let plain = yes_no
        .message_box(&MessageBoxOptions {
            title: "t".to_owned(),
            message: "m".to_owned(),
            buttons: MessageBoxButtons::YesNo,
            icon: None,
        })
        .unwrap();
    assert_eq!(plain, MessageBoxResult::No);
    assert_eq!(
        seen.borrow().tasks,
        [TaskDialogCall {
            title: "Kaydet?".to_owned(),
            content: "Değişiklikler kaydedilsin mi?".to_owned(),
            common_buttons: 8,
            custom_buttons: vec![(101, "Kaydet".to_owned()), (102, "Kaydetme".to_owned())],
            has_icon: true,
        }]
    );
    assert_eq!(
        yes_no_seen.borrow().tasks,
        [TaskDialogCall {
            title: "t".to_owned(),
            content: "m".to_owned(),
            common_buttons: 2 | 4,
            custom_buttons: Vec::new(),
            has_icon: false,
        }]
    );
    // A command id the plan never offered is an error, not a guess.
    let (mut odd, _) = scripted(Vec::new(), 99, false);
    let error = odd.message_box(&MessageBoxOptions::default()).unwrap_err();
    assert!(failed(&error, "dialogs"), "{error}");
}

#[test]
fn every_icon_is_handed_to_the_task_dialog_except_the_plain_question() {
    let icons = [
        (Some(MessageBoxIcon::Information), true),
        (Some(MessageBoxIcon::Warning), true),
        (Some(MessageBoxIcon::Error), true),
        (Some(MessageBoxIcon::Question), false),
        (None, false),
    ];
    for (icon, has_icon) in icons {
        let (mut dialogs, seen) = scripted(Vec::new(), 1, false);
        let result = dialogs
            .message_box(&MessageBoxOptions {
                icon,
                buttons: MessageBoxButtons::Ok,
                ..MessageBoxOptions::default()
            })
            .unwrap();
        assert_eq!(result, MessageBoxResult::Ok);
        let tasks = seen.borrow().tasks.clone();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].has_icon, has_icon, "{icon:?}");
    }
}

/// What a file dialog that is never shown answers to `Show`, `GetResult` and `GetResults`.
struct Script {
    answer: HRESULT,
    item: Option<IShellItem>,
    results: Option<IShellItemArray>,
}

/// The methods of a file dialog the code under test never calls on a dialog it did not create.
macro_rules! unsupported {
    ($($name:ident($($arg:ident: $kind:ty),*) -> $result:ty;)*) => {
        $(
            fn $name(&self $(, $arg: $kind)*) -> windows::core::Result<$result> {
                Err(E_NOTIMPL.into())
            }
        )*
    };
}

/// The `IModalWindow` and `IFileDialog` implementation of a scripted file dialog.
macro_rules! scripted_file_dialog {
    ($fake:ident) => {
        impl IModalWindow_Impl for $fake {
            fn Show(&self, _owner: HWND) -> windows::core::Result<()> {
                self.script.answer.ok()
            }
        }

        impl IFileDialog_Impl for $fake {
            unsupported! {
                SetFileTypes(_count: u32, _specs: *const COMDLG_FILTERSPEC) -> ();
                SetFileTypeIndex(_index: u32) -> ();
                GetFileTypeIndex() -> u32;
                Advise(_events: Ref<IFileDialogEvents>) -> u32;
                Unadvise(_cookie: u32) -> ();
                SetOptions(_options: FILEOPENDIALOGOPTIONS) -> ();
                GetOptions() -> FILEOPENDIALOGOPTIONS;
                SetDefaultFolder(_folder: Ref<IShellItem>) -> ();
                SetFolder(_folder: Ref<IShellItem>) -> ();
                GetFolder() -> IShellItem;
                GetCurrentSelection() -> IShellItem;
                SetFileName(_name: &PCWSTR) -> ();
                GetFileName() -> PWSTR;
                SetTitle(_title: &PCWSTR) -> ();
                SetOkButtonLabel(_text: &PCWSTR) -> ();
                SetFileNameLabel(_label: &PCWSTR) -> ();
                AddPlace(_place: Ref<IShellItem>, _where: FDAP) -> ();
                SetDefaultExtension(_extension: &PCWSTR) -> ();
                Close(_result: HRESULT) -> ();
                SetClientGuid(_guid: *const GUID) -> ();
                ClearClientData() -> ();
                SetFilter(_filter: Ref<IShellItemFilter>) -> ();
            }

            fn GetResult(&self) -> windows::core::Result<IShellItem> {
                self.script.item.clone().ok_or_else(|| E_UNEXPECTED.into())
            }
        }
    };
}

/// An open dialog (`IFileOpenDialog`: a list of results).
#[implement(IFileOpenDialog)]
struct ScriptedOpenDialog {
    script: Script,
}

scripted_file_dialog!(ScriptedOpenDialog_Impl);

impl IFileOpenDialog_Impl for ScriptedOpenDialog_Impl {
    fn GetResults(&self) -> windows::core::Result<IShellItemArray> {
        self.script
            .results
            .clone()
            .ok_or_else(|| E_UNEXPECTED.into())
    }

    fn GetSelectedItems(&self) -> windows::core::Result<IShellItemArray> {
        Err(E_NOTIMPL.into())
    }
}

/// A save dialog (only `IFileDialog`: a single result).
#[implement(IFileDialog)]
struct ScriptedSaveDialog {
    script: Script,
}

scripted_file_dialog!(ScriptedSaveDialog_Impl);

fn open_dialog_answering(answer: HRESULT, file: Option<&Path>) -> IFileDialog {
    let results = file.map(|file| {
        // SAFETY: a live shell item of an existing file.
        unsafe { SHCreateShellItemArrayFromShellItem::<_, IShellItemArray>(&item_for(file)) }
            .unwrap()
    });
    let dialog: IFileOpenDialog = ScriptedOpenDialog {
        script: Script {
            answer,
            item: None,
            results,
        },
    }
    .into();
    dialog.cast().unwrap()
}

fn save_dialog_answering(answer: HRESULT, file: Option<&Path>) -> IFileDialog {
    ScriptedSaveDialog {
        script: Script {
            answer,
            item: file.map(item_for),
            results: None,
        },
    }
    .into()
}

#[test]
fn the_system_host_shows_a_dialog_and_reads_the_items_the_user_chose() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let file = temp_file("chosen.md");
    let chosen = Some(file.as_path());
    let cancelled = HRESULT::from_win32(ERROR_CANCELLED.0);

    // An open dialog hands out its result list, a save dialog its single result.
    let accepted = SystemDialogHost
        .pick(&open_dialog_answering(S_OK, chosen), None)
        .unwrap();
    assert_eq!(
        accepted.iter().map(path_of).collect::<Vec<_>>(),
        std::slice::from_ref(&file)
    );
    let saved = SystemDialogHost
        .pick(&save_dialog_answering(S_OK, chosen), None)
        .unwrap();
    assert_eq!(
        saved.iter().map(path_of).collect::<Vec<_>>(),
        std::slice::from_ref(&file)
    );

    // Dismissing the dialog is no failure; any other outcome is reported as it is.
    for dialog in [
        open_dialog_answering(cancelled, chosen),
        save_dialog_answering(cancelled, chosen),
    ] {
        assert!(SystemDialogHost.pick(&dialog, None).unwrap().is_empty());
    }
    for dialog in [
        open_dialog_answering(E_FAIL, chosen),
        save_dialog_answering(E_FAIL, chosen),
    ] {
        let error = SystemDialogHost.pick(&dialog, None).unwrap_err();
        assert_eq!(error.code(), E_FAIL);
    }
    // Accepted without any result to read: the failure of the read, not an empty answer.
    for dialog in [
        open_dialog_answering(S_OK, None),
        save_dialog_answering(S_OK, None),
    ] {
        let error = SystemDialogHost.pick(&dialog, None).unwrap_err();
        assert_eq!(error.code(), E_UNEXPECTED);
    }
    std::fs::remove_file(file).unwrap();
}

/// Runs `run` once per system call it makes, each time failing that call, until a run makes no
/// more calls than the armed one (it then succeeds). Every failure must be reported as a
/// failure of the `dialogs` service. Returns how many calls could fail.
#[cfg(feature = "fault-injection")]
fn fail_each_call<T: std::fmt::Debug>(mut run: impl FnMut() -> Result<T, ServiceError>) -> u32 {
    use ssw_win::fault;

    for call in 0.. {
        fault::arm(call);
        let result = run();
        let tripped = fault::tripped();
        fault::disarm();
        if !tripped {
            result.unwrap();
            return call;
        }
        let error = result.unwrap_err();
        assert!(failed(&error, "dialogs"), "call {call}: {error:?}");
    }
    unreachable!("the loop ends with the first run that trips nothing")
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_failing_system_call_fails_the_dialog_instead_of_showing_it() {
    use ssw_win::fault;

    let _com = ComGuard::apartment_threaded().unwrap();
    let file = temp_file("sweep.md");
    let (mut dialogs, seen) = scripted(vec![file.clone()], 0, false);
    let filters = vec![FileFilter {
        name: "Markdown".to_owned(),
        extensions: vec!["md".to_owned()],
    }];
    let open = OpenFileOptions {
        title: "Open".to_owned(),
        filters: filters.clone(),
        allow_multiple: true,
        initial_dir: Some(std::env::temp_dir()),
    };
    // Create, cast, five calls that configure the dialog, then reading the path of the item.
    assert_eq!(fail_each_call(|| dialogs.open_file(&open)), 8);
    assert_eq!(
        seen.borrow().files.len(),
        2,
        "shown by the run that failed to read the path of the item and by the one that did not fail"
    );
    let save = SaveFileOptions {
        title: "Save".to_owned(),
        filters,
        suggested_name: Some("draft.md".to_owned()),
        default_extension: Some("md".to_owned()),
        initial_dir: Some(std::env::temp_dir()),
    };
    assert_eq!(fail_each_call(|| dialogs.save_file(&save)), 10);
    let folder = PickFolderOptions {
        title: "Folder".to_owned(),
        initial_dir: Some(std::env::temp_dir()),
    };
    assert_eq!(fail_each_call(|| dialogs.pick_folder(&folder)), 7);

    // The system host reads the result of an open dialog with three calls and that of a save
    // dialog with one; every one of them can fail.
    let chosen = Some(file.as_path());
    for (dialog, calls) in [
        (open_dialog_answering(S_OK, chosen), 3),
        (save_dialog_answering(S_OK, chosen), 1),
    ] {
        for call in 0..calls {
            fault::arm(call);
            let error = SystemDialogHost.pick(&dialog, None).unwrap_err();
            assert!(fault::tripped(), "call {call}");
            fault::disarm();
            assert_eq!(error.code(), E_FAIL);
        }
        fault::arm(calls);
        assert_eq!(SystemDialogHost.pick(&dialog, None).unwrap().len(), 1);
        assert!(!fault::tripped());
        fault::disarm();
    }

    // Dialogs of the system fail the same way before anything could be shown.
    let strings = DialogStrings {
        save: "Kaydet".to_owned(),
        dont_save: "Kaydetme".to_owned(),
    };
    let mut system = WindowsDialogs::new(None, strings);
    fault::arm(0);
    let error = system.open_file(&OpenFileOptions::default()).unwrap_err();
    assert!(fault::tripped());
    fault::disarm();
    assert!(failed(&error, "dialogs"), "{error:?}");
    std::fs::remove_file(file).unwrap();
}

/// Records what it was asked to run and answers with a scripted result.
struct ScriptedShell {
    executed: Executed,
    revealed: Revealed,
    fail: bool,
}

impl ShellHost for ScriptedShell {
    fn execute(&mut self, info: &mut SHELLEXECUTEINFOW) -> windows::core::Result<()> {
        // SAFETY: the strings are NUL-terminated and alive for the call.
        let (verb, file) = unsafe {
            (
                info.lpVerb.to_string().unwrap(),
                info.lpFile.to_string().unwrap(),
            )
        };
        assert_eq!(info.cbSize as usize, size_of::<SHELLEXECUTEINFOW>());
        self.executed
            .borrow_mut()
            .push((verb, file, info.fMask, info.nShow));
        if self.fail {
            Err(E_FAIL.into())
        } else {
            Ok(())
        }
    }

    fn select_in_explorer(&mut self, item: &ItemIdList) -> windows::core::Result<()> {
        self.revealed.borrow_mut().push(item.to_path());
        if self.fail {
            Err(E_FAIL.into())
        } else {
            Ok(())
        }
    }
}

type Executed = Rc<RefCell<Vec<(String, String, u32, i32)>>>;
type Revealed = Rc<RefCell<Vec<Option<PathBuf>>>>;

fn shell(fail: bool) -> (WindowsShell<ScriptedShell>, Executed, Revealed) {
    let executed = Rc::new(RefCell::new(Vec::new()));
    let revealed = Rc::new(RefCell::new(Vec::new()));
    let host = ScriptedShell {
        executed: Rc::clone(&executed),
        revealed: Rc::clone(&revealed),
        fail,
    };
    (WindowsShell::with_host(None, host), executed, revealed)
}

#[test]
fn shell_requests_are_validated_before_the_os_call() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let (mut shell, executed, _) = shell(false);
    // Web and mail links reach the shell exactly as given, with the `open` verb.
    shell.open_url("https://example.com/a?b=c").unwrap();
    shell.open_url("mailto:me@example.com").unwrap();
    let calls = executed.borrow().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].0, "open");
    assert_eq!(calls[0].1, "https://example.com/a?b=c");
    assert_ne!(calls[0].2 & SEE_MASK_NOASYNC, 0);
    assert_eq!(calls[0].3, 1, "SW_SHOWNORMAL");
    assert_eq!(calls[1].1, "mailto:me@example.com");
    // Everything else stays inside the app.
    for url in [
        "file:///C:/Windows/notepad.exe",
        "javascript:alert(1)",
        "ssw://x",
        "relative.md",
    ] {
        assert!(failed(&shell.open_url(url).unwrap_err(), "shell"), "{url}");
    }
    // Existing documents and folders are opened; executables and missing paths are refused.
    let document = temp_file("open.txt");
    shell.open_path(&document).unwrap();
    shell.open_path(&std::env::temp_dir()).unwrap();
    let exe = std::env::current_exe().unwrap();
    let refused = shell.open_path(&exe).unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("refusing to open executable content"),
        "{refused}"
    );
    let missing = std::env::temp_dir().join("ssw-win-no-such-file-77d1.txt");
    let absent = shell.open_path(&missing).unwrap_err();
    assert!(absent.to_string().contains("does not exist"), "{absent}");
    let calls = executed.borrow().clone();
    assert_eq!(calls.len(), 4, "refused requests never reached the host");
    assert_eq!(PathBuf::from(&calls[2].1), document);
    std::fs::remove_file(document).unwrap();
}

#[test]
fn reveal_hands_the_resolved_item_to_the_host() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let (mut shell, _, revealed) = shell(false);
    let file = temp_file("reveal.md");
    shell.reveal_in_explorer(&file).unwrap();
    let seen = revealed.borrow().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].as_ref().map(|path| path.canonicalize().unwrap()),
        Some(file.canonicalize().unwrap())
    );
    let missing = std::env::temp_dir().join("ssw-win-no-such-file-77d1.md");
    assert!(shell.reveal_in_explorer(&missing).is_err());
    assert!(shell.recycle(&missing).is_err());
    assert_eq!(revealed.borrow().len(), 1);
    std::fs::remove_file(file).unwrap();
}

#[test]
fn host_failures_become_shell_errors() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let (mut shell, _, _) = shell(true);
    assert!(failed(
        &shell.open_url("https://example.com").unwrap_err(),
        "shell"
    ));
    let file = temp_file("fail.md");
    assert!(failed(&shell.open_path(&file).unwrap_err(), "shell"));
    assert!(failed(
        &shell.reveal_in_explorer(&file).unwrap_err(),
        "shell"
    ));
    std::fs::remove_file(file).unwrap();
}

#[test]
fn recycling_moves_a_file_to_the_bin_and_the_test_purges_it_again() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let bin = RecycleBinGuard::new();
    let file = std::env::temp_dir().join(bin.name("-note.md"));
    std::fs::write(&file, b"recycle me").unwrap();
    let (mut shell, executed, revealed) = shell(false);
    shell.recycle(&file).unwrap();
    assert!(!file.exists(), "the file left its folder");
    assert!(executed.borrow().is_empty() && revealed.borrow().is_empty());
    assert_eq!(
        bin.purge().unwrap(),
        1,
        "exactly this test's item was in the Recycle Bin"
    );
    assert_eq!(bin.purge().unwrap(), 0, "nothing of the test stays behind");
}

/// A shell that cannot create its file operation fails before anything is moved.
#[cfg(feature = "fault-injection")]
#[test]
fn a_file_operation_that_cannot_be_created_recycles_nothing() {
    use ssw_win::fault;

    let _com = ComGuard::apartment_threaded().unwrap();
    let file = temp_file("keep.md");
    let (mut shell, _, _) = shell(false);
    fault::arm(0);
    let error = shell.recycle(&file).unwrap_err();
    assert!(fault::tripped());
    fault::disarm();
    assert!(failed(&error, "shell"), "{error:?}");
    assert!(file.exists(), "the file stayed where it was");
    std::fs::remove_file(file).unwrap();
}

#[test]
fn platform_errors_name_their_service() {
    let error = service_error("clipboard", &WinError::Invalid("boom"));
    assert!(failed(&error, "clipboard"));
    assert!(error.to_string().contains("boom"), "{error}");
}

#[test]
fn the_clock_tracks_real_time_and_never_goes_backwards() {
    let clock = QpcClock::new();
    let wall = Instant::now();
    let first = clock.now();
    std::thread::sleep(Duration::from_millis(40));
    let second = clock.now();
    assert!(second - first >= Duration::from_millis(35));
    assert!(second - first <= wall.elapsed() + Duration::from_millis(20));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                let mut previous = clock.now();
                for _ in 0..2000 {
                    let now = clock.now();
                    assert!(now >= previous);
                    previous = now;
                }
                previous
            })
        })
        .collect();
    for worker in workers {
        assert!(worker.join().unwrap() >= second);
    }
}
