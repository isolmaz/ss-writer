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
use ssw_win::services::dialogs::{DialogHost, DialogStrings, WindowsDialogs};
use ssw_win::services::service_error;
use ssw_win::services::shell::{ItemIdList, ShellHost, WindowsShell};
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Com::{CoTaskMemFree, IBindCtx};
use windows::Win32::UI::Controls::{TASKDIALOG_BUTTON, TASKDIALOGCONFIG};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOLDERID_RecycleBinFolder,
    FileOperation, IEnumShellItems, IFileOperation, IFileOperationProgressSink, KF_FLAG_DEFAULT,
    SHGetKnownFolderItem, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, IFileDialog, IShellItem,
    SEE_MASK_NOASYNC, SHCreateItemFromParsingName, SHELLEXECUTEINFOW, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, IUnknown};

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
