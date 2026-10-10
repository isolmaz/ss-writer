//! The taskbar Jump List: recent files and folders, and the "New window" task.
//!
//! [`JumpList`] builds a custom destination list (`ICustomDestinationList`) for one
//! AppUserModelID. Every destination is a shell link that starts the application's executable
//! with the destination as its command line argument, so it works without a registered file
//! association (portable mode); the title is the file name and the tooltip the full path.
//! The task starts the executable with [`NEW_WINDOW_ARG`], which the running instance receives
//! as a [`crate::instance::Launch`].
//!
//! The list is addressed by its AppUserModelID only ([`JumpList::new`]), so tests build and
//! delete a list under an ID of their own and never touch the application's real one.
//! [`set_process_app_id`] gives the process the ID the list belongs to; it must run before the
//! first window is created.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVAR_CHANGE_FLAGS, PROPVARIANT, PropVariantChangeType,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, GetCurrentProcessExplicitAppUserModelID,
    ICustomDestinationList, IShellItem, IShellLinkW, SIGDN_FILESYSPATH,
    SetCurrentProcessExplicitAppUserModelID, ShellLink,
};
use windows::core::{HSTRING, IUnknown, Interface};

use crate::com::{OsResultExt, WinError, from_wide};
use crate::fault::check;
use crate::instance::NEW_WINDOW_ARG;

/// Room for the text of one shell link property (arguments, path, tooltip).
const TEXT_CAPACITY: usize = 4096;

/// Gives this process the AppUserModelID `id`: the identity the taskbar groups its windows and
/// the Jump List under. Call it once at startup, before any window exists.
///
/// # Errors
/// The shell rejected the ID (empty, longer than 128 characters or containing spaces).
pub fn set_process_app_id(id: &str) -> Result<(), WinError> {
    // SAFETY: a valid NUL-terminated string.
    unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(id)) }
        .ctx("SetCurrentProcessExplicitAppUserModelID")
}

/// The AppUserModelID this process was given with [`set_process_app_id`].
///
/// # Errors
/// The process has no explicit ID.
pub fn process_app_id() -> Result<String, WinError> {
    // SAFETY: no preconditions; the result is a CoTaskMem string.
    let id = unsafe { GetCurrentProcessExplicitAppUserModelID() }
        .ctx("GetCurrentProcessExplicitAppUserModelID")?;
    // SAFETY: `id` is a valid NUL-terminated string that is copied and freed exactly once.
    let text = unsafe {
        let text = from_wide(id.as_wide());
        CoTaskMemFree(Some(id.0.cast_const().cast()));
        text
    };
    Ok(text)
}

/// `argument` as one command line argument that `CommandLineToArgvW` (and so the Rust
/// runtime) reads back unchanged: in quotes, quotes escaped, backslashes before a quote
/// doubled (a folder such as `C:\` ends in one).
#[must_use]
pub fn quote_argument(argument: &OsStr) -> OsString {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let mut quoted = vec![QUOTE];
    let mut backslashes = 0_usize;
    for unit in argument.encode_wide() {
        match unit {
            BACKSLASH => {
                backslashes += 1;
                quoted.push(unit);
            }
            QUOTE => {
                // 2n + 1 backslashes then a quote read back as n backslashes and a quote.
                quoted.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                quoted.push(unit);
                backslashes = 0;
            }
            _ => {
                backslashes = 0;
                quoted.push(unit);
            }
        }
    }
    // The closing quote must not escape: double the backslashes in front of it.
    quoted.extend(std::iter::repeat_n(BACKSLASH, backslashes));
    quoted.push(QUOTE);
    OsString::from_wide(&quoted)
}

/// The title of a destination: the last path component, or the whole path for a drive root.
fn entry_title(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Reads one text property of a shell link through `get`.
fn read_text(
    get: impl FnOnce(&mut [u16]) -> windows::core::Result<()>,
    context: &'static str,
) -> Result<String, WinError> {
    let mut buffer = vec![0_u16; TEXT_CAPACITY];
    check(get(buffer.as_mut_slice())).ctx(context)?;
    Ok(from_wide(&buffer))
}

/// A shell link that starts `exe` with `arguments`, titled `title`, with `tooltip` as its
/// description.
fn shell_link(
    exe: &Path,
    arguments: &OsStr,
    title: &str,
    tooltip: &str,
) -> Result<IShellLinkW, WinError> {
    // SAFETY: `ShellLink` is the registered class of `IShellLinkW`.
    let link: IShellLinkW =
        check(unsafe { CoCreateInstance(&ShellLink, None::<&IUnknown>, CLSCTX_INPROC_SERVER) })
            .ctx("create IShellLinkW")?;
    // SAFETY: `link` is a valid object; every string is NUL-terminated and outlives its call.
    unsafe {
        check(link.SetPath(&HSTRING::from(exe.as_os_str()))).ctx("IShellLink::SetPath")?;
        check(link.SetArguments(&HSTRING::from(arguments))).ctx("IShellLink::SetArguments")?;
        check(link.SetIconLocation(&HSTRING::from(exe.as_os_str()), 0))
            .ctx("IShellLink::SetIconLocation")?;
        check(link.SetDescription(&HSTRING::from(tooltip))).ctx("IShellLink::SetDescription")?;
    }
    let store = check(link.cast::<IPropertyStore>()).ctx("IShellLink as IPropertyStore")?;
    // The text constructor makes a BSTR; `System.Title` is a string property.
    let text = PROPVARIANT::from(title);
    let mut value = PROPVARIANT::default();
    // SAFETY: both variants are valid and live for the call.
    check(unsafe {
        PropVariantChangeType(
            &raw mut value,
            &raw const text,
            PROPVAR_CHANGE_FLAGS(0),
            VT_LPWSTR,
        )
    })
    .ctx("PropVariantChangeType")?;
    // SAFETY: `store` is valid; the key and the value outlive the calls.
    unsafe {
        check(store.SetValue(&PKEY_Title, &value)).ctx("IPropertyStore::SetValue(System.Title)")?;
        check(store.Commit()).ctx("IPropertyStore::Commit")?;
    }
    Ok(link)
}

/// An `IObjectArray` holding `links`.
fn object_array(links: &[IShellLinkW]) -> Result<IObjectArray, WinError> {
    // SAFETY: `EnumerableObjectCollection` is the registered class of `IObjectCollection`.
    let collection: IObjectCollection = check(unsafe {
        CoCreateInstance(
            &EnumerableObjectCollection,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER,
        )
    })
    .ctx("create IObjectCollection")?;
    for link in links {
        // SAFETY: `collection` and `link` are valid objects.
        check(unsafe { collection.AddObject(link) }).ctx("IObjectCollection::AddObject")?;
    }
    check(collection.cast::<IObjectArray>()).ctx("IObjectCollection as IObjectArray")
}

/// Every object of `array`.
fn objects(array: &IObjectArray) -> Result<Vec<IUnknown>, WinError> {
    // SAFETY: `array` is a valid object.
    let count = check(unsafe { array.GetCount() }).ctx("IObjectArray::GetCount")?;
    let mut found = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is below the count just read.
        let object = check(unsafe { array.GetAt::<IUnknown>(index) }).ctx("IObjectArray::GetAt")?;
        found.push(object);
    }
    Ok(found)
}

/// What identifies a destination the shell hands back (the arguments of a link, the path of a
/// file item), lower-cased for comparison; `None` for objects of any other kind.
fn destination_key(object: &IUnknown) -> Option<String> {
    if let Ok(link) = object.cast::<IShellLinkW>() {
        // SAFETY: `link` is a valid object; the buffer holds `TEXT_CAPACITY` characters.
        let arguments = read_text(
            |buffer| unsafe { link.GetArguments(buffer) },
            "GetArguments",
        );
        return arguments.ok().map(|text| text.trim().to_lowercase());
    }
    let item = object.cast::<IShellItem>().ok()?;
    // SAFETY: `item` is a valid object; the name is a CoTaskMem string copied and freed here.
    let path = unsafe {
        let name = check(item.GetDisplayName(SIGDN_FILESYSPATH)).ok()?;
        let text = from_wide(name.as_wide());
        CoTaskMemFree(Some(name.0.cast_const().cast()));
        text
    };
    Some(path.to_lowercase())
}

/// The Jump List of one application.
#[derive(Debug, Clone)]
pub struct JumpList {
    app_id: String,
    exe: PathBuf,
    recent_title: String,
    new_window_title: String,
}

impl JumpList {
    /// The list of the application `app_id` whose program is `exe`. `recent_title` names the
    /// category of recent files and folders, `new_window_title` the task (both localized by the
    /// caller).
    #[must_use]
    pub fn new(
        app_id: impl Into<String>,
        exe: impl Into<PathBuf>,
        recent_title: impl Into<String>,
        new_window_title: impl Into<String>,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            exe: exe.into(),
            recent_title: recent_title.into(),
            new_window_title: new_window_title.into(),
        }
    }

    /// One link per entry of `recent`: opens the entry in the application.
    ///
    /// # Errors
    /// A shell object could not be created or filled.
    pub fn recent_links(&self, recent: &[PathBuf]) -> Result<IObjectArray, WinError> {
        let links = recent
            .iter()
            .map(|path| {
                shell_link(
                    &self.exe,
                    &quote_argument(path.as_os_str()),
                    &entry_title(path),
                    &path.display().to_string(),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        object_array(&links)
    }

    /// The user tasks: one link that opens a new window.
    ///
    /// # Errors
    /// A shell object could not be created or filled.
    pub fn task_links(&self) -> Result<IObjectArray, WinError> {
        let link = shell_link(
            &self.exe,
            OsStr::new(NEW_WINDOW_ARG),
            &self.new_window_title,
            &self.new_window_title,
        )?;
        object_array(&[link])
    }

    /// The entries of `recent` the user has not removed from the list: the shell refuses a
    /// category that contains a removed destination.
    ///
    /// # Errors
    /// `removed` could not be read.
    pub fn without_removed(
        &self,
        recent: &[PathBuf],
        removed: &IObjectArray,
    ) -> Result<Vec<PathBuf>, WinError> {
        let removed: HashSet<String> = objects(removed)?
            .iter()
            .filter_map(destination_key)
            .collect();
        let kept = |path: &&PathBuf| {
            let as_argument = quote_argument(path.as_os_str())
                .to_string_lossy()
                .to_lowercase();
            let as_path = path.to_string_lossy().to_lowercase();
            !removed.contains(&as_argument) && !removed.contains(&as_path)
        };
        Ok(recent.iter().filter(kept).cloned().collect())
    }

    /// Replaces the Jump List with the category of `recent` entries (as many as the shell
    /// shows, minus the ones the user removed) and the "New window" task. Returns how many
    /// recent entries the list shows.
    ///
    /// # Errors
    /// The shell refused the list; the previous list stays as it was.
    pub fn update(&self, recent: &[PathBuf]) -> Result<usize, WinError> {
        // SAFETY: `DestinationList` is the registered class of `ICustomDestinationList`.
        let list: ICustomDestinationList = check(unsafe {
            CoCreateInstance(&DestinationList, None::<&IUnknown>, CLSCTX_INPROC_SERVER)
        })
        .ctx("create ICustomDestinationList")?;
        // SAFETY: `list` is a valid object; the ID is NUL-terminated.
        check(unsafe { list.SetAppID(&HSTRING::from(self.app_id.as_str())) })
            .ctx("ICustomDestinationList::SetAppID")?;
        let mut slots = 0_u32;
        // SAFETY: `slots` is a valid out value.
        let removed = check(unsafe { list.BeginList::<IObjectArray>(&raw mut slots) })
            .ctx("ICustomDestinationList::BeginList")?;
        let shown = match self.fill(&list, recent, &removed, slots) {
            Ok(shown) => shown,
            Err(error) => {
                // SAFETY: ends the list started above; the original failure is the one to
                // report.
                let _ = unsafe { list.AbortList() };
                return Err(error);
            }
        };
        // SAFETY: `list` is a valid object with a list in progress.
        check(unsafe { list.CommitList() }).ctx("ICustomDestinationList::CommitList")?;
        Ok(shown)
    }

    fn fill(
        &self,
        list: &ICustomDestinationList,
        recent: &[PathBuf],
        removed: &IObjectArray,
        slots: u32,
    ) -> Result<usize, WinError> {
        let mut visible = self.without_removed(recent, removed)?;
        visible.truncate(slots as usize);
        if !visible.is_empty() {
            let links = self.recent_links(&visible)?;
            // SAFETY: `list` has a list in progress; the title is NUL-terminated.
            check(unsafe {
                list.AppendCategory(&HSTRING::from(self.recent_title.as_str()), &links)
            })
            .ctx("ICustomDestinationList::AppendCategory")?;
        }
        let tasks = self.task_links()?;
        // SAFETY: `list` has a list in progress.
        check(unsafe { list.AddUserTasks(&tasks) }).ctx("ICustomDestinationList::AddUserTasks")?;
        Ok(visible.len())
    }

    /// Deletes the Jump List of this application's ID.
    ///
    /// # Errors
    /// The shell refused to delete it.
    pub fn delete(&self) -> Result<(), WinError> {
        // SAFETY: `DestinationList` is the registered class of `ICustomDestinationList`.
        let list: ICustomDestinationList = check(unsafe {
            CoCreateInstance(&DestinationList, None::<&IUnknown>, CLSCTX_INPROC_SERVER)
        })
        .ctx("create ICustomDestinationList")?;
        // SAFETY: `list` is a valid object; the ID is NUL-terminated.
        check(unsafe { list.DeleteList(&HSTRING::from(self.app_id.as_str())) })
            .ctx("ICustomDestinationList::DeleteList")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quoted(text: &str) -> String {
        quote_argument(OsStr::new(text))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn arguments_are_quoted_for_command_line_parsing() {
        assert_eq!(quoted(r"C:\Users\a b\note.md"), r#""C:\Users\a b\note.md""#);
        // A trailing backslash would escape the closing quote: it is doubled.
        assert_eq!(quoted(r"C:\"), r#""C:\\""#);
        assert_eq!(quoted(r"D:\dir\"), r#""D:\dir\\""#);
        // A quote is escaped, and backslashes in front of it are doubled first.
        assert_eq!(quoted(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quoted(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quoted(""), r#""""#);
    }

    #[test]
    fn titles_are_file_names_or_the_whole_root() {
        assert_eq!(entry_title(Path::new(r"C:\Notes\todo.md")), "todo.md");
        assert_eq!(entry_title(Path::new(r"C:\Notes\Project\")), "Project");
        assert_eq!(entry_title(Path::new(r"C:\")), r"C:\");
    }
}
