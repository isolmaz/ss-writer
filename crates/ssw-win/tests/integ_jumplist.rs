//! The Jump List: the shell links it is built from are read back through the shell's own
//! interfaces, and a real list is committed and deleted under a throw-away AppUserModelID (the
//! application's own list is never touched). The committed list is checked in the bytes the
//! shell writes to disk.

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ssw_win::com::{ComGuard, WinError};
use ssw_win::instance::{Launch, NEW_WINDOW_ARG};
use ssw_win::jumplist::{JumpList, process_app_id, quote_argument, set_process_app_id};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IBindCtx,
};
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    CommandLineToArgvW, EnumerableObjectCollection, IShellItem, IShellLinkW,
    SHCreateItemFromParsingName,
};
use windows::core::{HSTRING, IUnknown, Interface};

fn unique_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{}x{nanos}", std::process::id())
}

/// What a shell link says about itself.
#[derive(Debug, PartialEq, Eq)]
struct LinkInfo {
    path: PathBuf,
    arguments: String,
    tooltip: String,
    title: String,
}

fn link_infos(array: &IObjectArray) -> Vec<LinkInfo> {
    // SAFETY: `array` is a valid object.
    let count = unsafe { array.GetCount() }.unwrap();
    (0..count)
        .map(|index| {
            // SAFETY: `index` is below the count.
            let unknown: IUnknown = unsafe { array.GetAt(index) }.unwrap();
            let link: IShellLinkW = unknown.cast().unwrap();
            let mut buffer = [0_u16; 4096];
            // SAFETY: valid link; the buffers hold the number of characters passed.
            let (path, arguments, tooltip) = unsafe {
                link.GetPath(&mut buffer, std::ptr::null_mut(), 0).unwrap();
                let path = text(&buffer);
                link.GetArguments(&mut buffer).unwrap();
                let arguments = text(&buffer);
                link.GetDescription(&mut buffer).unwrap();
                (path, arguments, text(&buffer))
            };
            let store: IPropertyStore = link.cast().unwrap();
            // SAFETY: valid store and key; the title is a CoTaskMem string copied and freed here.
            let title = unsafe {
                let value = store.GetValue(&PKEY_Title).unwrap();
                let name = PropVariantToStringAlloc(&value).unwrap();
                let title = String::from_utf16_lossy(name.as_wide());
                CoTaskMemFree(Some(name.0.cast_const().cast()));
                title
            };
            LinkInfo {
                path: PathBuf::from(path),
                arguments,
                tooltip,
                title,
            }
        })
        .collect()
}

fn text(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&unit| unit == 0).unwrap();
    String::from_utf16_lossy(&buffer[..end])
}

fn list_for(app_id: &str) -> JumpList {
    JumpList::new(
        app_id,
        std::env::current_exe().unwrap(),
        "Recent",
        "New window",
    )
}

#[test]
fn recent_destinations_open_their_path_in_the_application() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let list = list_for("SSWriter.Test.Links");
    let recent = vec![
        PathBuf::from(r"C:\Users\dilek\Notlar\günlük notları.md"),
        PathBuf::from(r"D:\Projeler\Çalışma\"),
        PathBuf::from(r"E:\"),
    ];
    let links = link_infos(&list.recent_links(&recent).unwrap());
    let exe = std::env::current_exe().unwrap();
    assert_eq!(links.len(), 3);
    for (link, path) in links.iter().zip(&recent) {
        assert_eq!(link.path, exe);
        assert_eq!(
            link.arguments,
            quote_argument(path.as_os_str()).to_string_lossy()
        );
        assert_eq!(link.tooltip, path.display().to_string());
    }
    assert_eq!(links[0].title, "günlük notları.md");
    assert_eq!(links[1].title, "Çalışma");
    assert_eq!(links[2].title, r"E:\");
    assert_eq!(links[1].arguments, r#""D:\Projeler\Çalışma\\""#);
}

#[test]
fn the_new_window_task_carries_the_new_window_argument() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let list = JumpList::new(
        "SSWriter.Test.Task",
        std::env::current_exe().unwrap(),
        "Son kullanılanlar",
        "Yeni pencere",
    );
    let tasks = link_infos(&list.task_links().unwrap());
    assert_eq!(
        tasks,
        [LinkInfo {
            path: std::env::current_exe().unwrap(),
            arguments: NEW_WINDOW_ARG.to_owned(),
            tooltip: "Yeni pencere".to_owned(),
            title: "Yeni pencere".to_owned(),
        }]
    );
    // What the task starts the application with is what the running instance understands.
    let launch = Launch::from_args([OsString::from(tasks[0].arguments.clone())]);
    assert_eq!(
        launch,
        Launch {
            paths: Vec::new(),
            new_window: true
        }
    );
}

/// Parses `command_line` the way the runtime does and returns the arguments after the
/// program name.
fn parse(command_line: &OsStr) -> Vec<OsString> {
    let mut count = 0_i32;
    // SAFETY: a valid NUL-terminated string; the returned block is freed below.
    let argv = unsafe { CommandLineToArgvW(&HSTRING::from(command_line), &raw mut count) };
    assert!(!argv.is_null());
    // SAFETY: `argv` points to `count` valid NUL-terminated strings until freed.
    let arguments = (1..usize::try_from(count).unwrap())
        .map(|index| unsafe { OsString::from_wide((*argv.add(index)).as_wide()) })
        .collect();
    // SAFETY: the block came from `CommandLineToArgvW`, which documents `LocalFree`.
    unsafe { LocalFree(Some(HLOCAL(argv.cast()))) };
    arguments
}

#[test]
fn quoted_arguments_survive_command_line_parsing() {
    for original in [
        r"C:\Users\dilek\Notlar\günlük notları.md",
        r"D:\Projeler\Çalışma\",
        r"E:\",
        r"\\server\share\a b\",
        r#"has "quotes" inside"#,
        r#"ends with backslash-quote\""#,
        "",
    ] {
        let mut command_line = OsString::from("program.exe ");
        command_line.push(quote_argument(OsStr::new(original)));
        command_line.push(" ");
        command_line.push(quote_argument(OsStr::new("second")));
        assert_eq!(
            parse(&command_line),
            [OsString::from(original), OsString::from("second")],
            "{original:?}"
        );
    }
}

#[test]
fn destinations_the_user_removed_are_left_out_of_the_category() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let list = list_for("SSWriter.Test.Removed");
    let (kept, gone, also_gone) = (
        PathBuf::from(r"C:\Notes\kept.md"),
        PathBuf::from(r"C:\Notes\Gone.MD"),
        PathBuf::from(r"C:\Notes\folder\"),
    );
    // The shell hands removed destinations back as the objects they were added as.
    let removed = list
        .recent_links(&[
            PathBuf::from(r"c:\notes\gone.md"),
            PathBuf::from(r"C:\Notes\folder\"),
        ])
        .unwrap();
    let visible = list
        .without_removed(&[kept.clone(), gone, also_gone], &removed)
        .unwrap();
    assert_eq!(visible, [kept]);

    let nothing_removed = list.recent_links(&[]).unwrap();
    let all = [PathBuf::from(r"C:\a.md"), PathBuf::from(r"C:\b.md")];
    assert_eq!(list.without_removed(&all, &nothing_removed).unwrap(), all);
}

/// An `IObjectArray` holding `objects`, as the shell builds its lists.
fn collection(objects: &[&IUnknown]) -> IObjectArray {
    // SAFETY: `EnumerableObjectCollection` is the registered class of `IObjectCollection`.
    let collection: IObjectCollection = unsafe {
        CoCreateInstance(
            &EnumerableObjectCollection,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    }
    .unwrap();
    for object in objects {
        // SAFETY: both objects are valid.
        unsafe { collection.AddObject(*object) }.unwrap();
    }
    collection.cast().unwrap()
}

#[test]
fn removed_file_items_are_matched_by_path_and_other_objects_are_ignored() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let list = list_for("SSWriter.Test.RemovedItems");
    let folder = std::env::temp_dir().join(format!("ssw-jumplist-{}", unique_token()));
    std::fs::create_dir_all(&folder).unwrap();
    let gone = folder.join("Gone.md");
    std::fs::write(&gone, "removed from the list").unwrap();
    let kept = folder.join("kept.md");
    std::fs::write(&kept, "still listed").unwrap();

    // The shell hands back a destination that is a file as a shell item.
    // SAFETY: a NUL-terminated path of an existing file; no bind context.
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(&HSTRING::from(gone.as_os_str()), None::<&IBindCtx>) }
            .unwrap();
    let item: IUnknown = item.cast().unwrap();
    // An object that is neither a shell link nor a shell item names no destination.
    let stranger: IUnknown = collection(&[]).cast().unwrap();
    let removed = collection(&[&item, &stranger]);
    let visible = list
        .without_removed(&[kept.clone(), gone.clone()], &removed)
        .unwrap();
    assert_eq!(visible, std::slice::from_ref(&kept));

    // Only the stranger: nothing is removed.
    let removed = collection(&[&stranger]);
    let visible = list
        .without_removed(&[kept.clone(), gone.clone()], &removed)
        .unwrap();
    assert_eq!(visible, [kept, gone]);
    std::fs::remove_dir_all(&folder).unwrap();
}

#[test]
fn a_list_whose_links_cannot_be_made_is_not_written() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let token = format!("sswjump{}", unique_token());
    // A program path no shell link can hold (longer than MAX_PATH).
    let exe = PathBuf::from(format!(r"C:\{token}\{}.exe", "x".repeat(300)));
    let list = JumpList::new(
        format!("SSWriter.Test.{token}"),
        exe,
        "Recent",
        "New window",
    );
    let error = list.task_links().unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
    let error = list
        .recent_links(&[PathBuf::from(r"C:\notes\a.md")])
        .unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");

    // The failure aborts the list in progress: nothing is committed, nothing is written.
    let error = list.update(&[PathBuf::from(r"C:\notes\a.md")]).unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
    let error = list.update(&[]).unwrap_err();
    assert!(matches!(error, WinError::Os { .. }), "{error:?}");
    assert!(files_containing(&token).is_empty());
}

/// The files the shell keeps custom Jump Lists in.
fn custom_destination_files() -> Vec<PathBuf> {
    let appdata = std::env::var_os("APPDATA").expect("APPDATA is set for a user session");
    let folder = Path::new(&appdata).join(r"Microsoft\Windows\Recent\CustomDestinations");
    std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .filter_map(|entry| Some(entry.ok()?.path()))
                .collect()
        })
        .unwrap_or_default()
}

/// The custom destination files whose bytes contain `token` as UTF-16 text.
fn files_containing(token: &str) -> Vec<PathBuf> {
    let needle: Vec<u8> = token.encode_utf16().flat_map(u16::to_le_bytes).collect();
    custom_destination_files()
        .into_iter()
        .filter(|path| {
            std::fs::read(path).is_ok_and(|bytes| {
                bytes
                    .windows(needle.len())
                    .any(|window| window == needle.as_slice())
            })
        })
        .collect()
}

/// Polls until `condition` holds (the shell writes the file as part of the call, but a
/// loaded machine may need a moment).
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    condition()
}

#[test]
fn a_committed_list_is_written_for_its_own_id_and_deleted_again() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let token = format!("sswjump{}", unique_token());
    let app_id = format!("SSWriter.Test.{token}");
    let list = list_for(&app_id);
    let recent = [
        PathBuf::from(format!(r"C:\{token}\first note.md")),
        PathBuf::from(format!(r"C:\{token}\Folder\")),
    ];
    assert!(files_containing(&token).is_empty());

    assert_eq!(list.update(&recent).unwrap(), 2);
    assert!(
        eventually(|| files_containing(&token).len() == 1),
        "the shell writes one custom destinations file for the ID"
    );
    // Replacing the list works on the existing one.
    assert_eq!(list.update(&recent[..1]).unwrap(), 1);

    list.delete().unwrap();
    assert!(
        eventually(|| files_containing(&token).is_empty()),
        "deleting the list removes its file"
    );
}

#[test]
fn the_process_app_id_can_be_set_and_read_back() {
    let id = format!("SSWriter.Test.Process.{}", unique_token());
    // Nothing has given this test process an explicit ID yet.
    assert!(process_app_id().is_err());
    set_process_app_id(&id).unwrap();
    assert_eq!(process_app_id().unwrap(), id);
    // IDs are limited to 128 characters.
    assert!(set_process_app_id(&"A".repeat(129)).is_err());
    assert_eq!(
        process_app_id().unwrap(),
        id,
        "a rejected ID changes nothing"
    );
}

/// The operation an error reports as failed ("IShellLink::SetPath" for "IShellLink::SetPath
/// failed: ...").
#[cfg(feature = "fault-injection")]
fn reported<T>(outcome: Result<T, WinError>) -> Result<T, String> {
    outcome.map_err(|error| {
        error
            .to_string()
            .split(" failed: ")
            .next()
            .unwrap_or_default()
            .to_owned()
    })
}

/// What a sweep reports when each of `steps` fails in turn.
#[cfg(feature = "fault-injection")]
fn failing(steps: &[&str]) -> Vec<Option<String>> {
    steps.iter().map(|step| Some((*step).to_owned())).collect()
}

/// The checked calls that make one shell link, in call order.
#[cfg(feature = "fault-injection")]
const LINK_STEPS: [&str; 9] = [
    "create IShellLinkW",
    "IShellLink::SetPath",
    "IShellLink::SetArguments",
    "IShellLink::SetIconLocation",
    "IShellLink::SetDescription",
    "IShellLink as IPropertyStore",
    "PropVariantChangeType",
    "IPropertyStore::SetValue(System.Title)",
    "IPropertyStore::Commit",
];

/// The checked calls that gather `links` shell links into an object array.
#[cfg(feature = "fault-injection")]
fn array_steps(links: usize) -> Vec<&'static str> {
    let mut steps = vec!["create IObjectCollection"];
    steps.extend(std::iter::repeat_n("IObjectCollection::AddObject", links));
    steps.push("IObjectCollection as IObjectArray");
    steps
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_failing_step_of_building_links_is_reported_by_the_step_that_failed() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let list = list_for("SSWriter.Test.FaultLinks");

    let mut task = LINK_STEPS.to_vec();
    task.extend(array_steps(1));
    assert_eq!(
        ssw_win::fault::sweep(|| reported(list.task_links()).err()),
        failing(&task)
    );

    let recent = [
        PathBuf::from(r"C:\Notes\first.md"),
        PathBuf::from(r"C:\Notes\Folder\"),
    ];
    let mut both = LINK_STEPS.to_vec();
    both.extend(LINK_STEPS);
    both.extend(array_steps(2));
    assert_eq!(
        ssw_win::fault::sweep(|| reported(list.recent_links(&recent)).err()),
        failing(&both)
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn a_destination_the_shell_cannot_describe_stays_listed_and_an_unreadable_array_is_an_error() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let list = list_for("SSWriter.Test.FaultRemoved");
    let folder = std::env::temp_dir().join(format!("ssw-jumplist-fault-{}", unique_token()));
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join("Gone.md");
    std::fs::write(&file, "removed from the list").unwrap();
    let linked = PathBuf::from(r"C:\Notes\Linked.md");
    let kept = PathBuf::from(r"C:\Notes\kept.md");

    // The user removed a shell link, a file item and something that is neither.
    // SAFETY: a NUL-terminated path of an existing file; no bind context.
    let item: IShellItem =
        unsafe { SHCreateItemFromParsingName(&HSTRING::from(file.as_os_str()), None::<&IBindCtx>) }
            .unwrap();
    let item: IUnknown = item.cast().unwrap();
    let links = list.recent_links(std::slice::from_ref(&linked)).unwrap();
    // SAFETY: the array holds the one link just made.
    let link: IUnknown = unsafe { links.GetAt(0) }.unwrap();
    let stranger: IUnknown = collection(&[]).cast().unwrap();
    let removed = collection(&[&link, &item, &stranger]);
    let recent = [linked.clone(), file.clone(), kept.clone()];

    assert_eq!(
        list.without_removed(&recent, &removed).unwrap(),
        std::slice::from_ref(&kept)
    );
    // The array is read first (its count, then its three objects); then the link's arguments
    // and the item's path are asked for. A destination the shell cannot describe is not
    // known to be removed: it stays in the category.
    let outcomes = ssw_win::fault::sweep(|| reported(list.without_removed(&recent, &removed)));
    assert_eq!(
        outcomes,
        [
            Err("IObjectArray::GetCount".to_owned()),
            Err("IObjectArray::GetAt".to_owned()),
            Err("IObjectArray::GetAt".to_owned()),
            Err("IObjectArray::GetAt".to_owned()),
            Ok(vec![linked, kept.clone()]),
            Ok(vec![file, kept]),
        ]
    );
    std::fs::remove_dir_all(&folder).unwrap();
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_failing_step_of_updating_or_deleting_a_list_is_reported() {
    let _com = ComGuard::apartment_threaded().unwrap();
    let token = format!("sswjump{}", unique_token());
    let list = list_for(&format!("SSWriter.Test.{token}"));
    let recent = [PathBuf::from(format!(r"C:\{token}\first note.md"))];
    let commit = "ICustomDestinationList::CommitList";

    let mut steps = vec![
        "create ICustomDestinationList",
        "ICustomDestinationList::SetAppID",
        "ICustomDestinationList::BeginList",
        "IObjectArray::GetCount",
    ];
    steps.extend(LINK_STEPS);
    steps.extend(array_steps(1));
    steps.push("ICustomDestinationList::AppendCategory");
    steps.extend(LINK_STEPS);
    steps.extend(array_steps(1));
    steps.push("ICustomDestinationList::AddUserTasks");
    steps.push(commit);
    let outcomes = ssw_win::fault::sweep(|| {
        let step = reported(list.update(&recent)).err();
        // The failed list is aborted: only a list that was committed reaches the disk.
        if step.as_deref().is_some_and(|step| step != commit) {
            assert!(files_containing(&token).is_empty(), "{step:?}");
        }
        step
    });
    assert_eq!(outcomes, failing(&steps));

    // The sweep ended with an update that went through (and the one that reported the failed
    // commit had been committed all the same): the list exists until it is deleted.
    assert!(
        eventually(|| files_containing(&token).len() == 1),
        "the shell writes the committed list"
    );
    let deleting = ssw_win::fault::sweep(|| reported(list.delete()).err());
    assert_eq!(
        deleting,
        failing(&[
            "create ICustomDestinationList",
            "ICustomDestinationList::DeleteList"
        ])
    );
    assert!(
        eventually(|| files_containing(&token).is_empty()),
        "deleting the list removes its file"
    );
}
