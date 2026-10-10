//! File associations and link safety: query helpers only.
//!
//! The installer registers the per-user associations; this module answers questions about
//! them ([`default_app_for`], [`is_default_app`]), opens the system page where the user picks
//! the default app ([`open_default_apps_settings`], through the [`ShellHost`] seam) and decides
//! whether a link target needs a confirmation before it is opened ([`is_dangerous_link`]).

use std::path::{Path, PathBuf};

use ssw_ui::services::ServiceError;
use windows::Win32::Foundation::{HWND, S_FALSE};
use windows::Win32::UI::Shell::{
    ASSOCF_NOTRUNCATE, ASSOCSTR, ASSOCSTR_EXECUTABLE, ASSOCSTR_FRIENDLYAPPNAME, AssocQueryStringW,
    SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HRESULT, HSTRING, PCWSTR, PWSTR, w};

use crate::com::from_wide;
use crate::services::failed;
use crate::services::shell::{ShellHost, allowed_scheme, is_dangerous_extension};

const SERVICE: &str = "assoc";

/// The Settings page where the default app per file type is chosen.
pub const DEFAULT_APPS_URI: &str = "ms-settings:defaultapps";

/// The extension (with the dot) of the file a link target names, as Windows would resolve it;
/// `None` for links the shell opens as web or mail addresses and for targets without one.
///
/// The target is taken as it will be opened (percent-decoding is the caller's job). A
/// `?query` or `#fragment` is not part of the file name, and Windows ignores trailing dots and
/// spaces (`setup.exe.` runs `setup.exe`).
#[must_use]
pub fn link_extension(target: &str) -> Option<String> {
    if allowed_scheme(target).is_some() {
        return None;
    }
    let path = target.split(['#', '?']).next().unwrap_or_default();
    let path = path.trim_end_matches(['.', ' ']);
    let name = path.rsplit(['/', '\\']).next().unwrap_or_default();
    let (_, extension) = name.rsplit_once('.')?;
    (!extension.is_empty()).then(|| format!(".{extension}"))
}

/// True when opening the file `target` names needs the user's confirmation: the shell
/// considers its type dangerous (`AssocIsDangerous`: executables, scripts, installers).
#[must_use]
pub fn is_dangerous_link(target: &str) -> bool {
    link_extension(target).is_some_and(|extension| is_dangerous_extension(&extension))
}

/// Opens the Settings page for default apps through `host`.
///
/// # Errors
/// The host failed to run the request.
pub fn open_default_apps_settings(
    host: &mut impl ShellHost,
    owner: Option<HWND>,
) -> Result<(), ServiceError> {
    let target = HSTRING::from(DEFAULT_APPS_URI);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        hwnd: owner.unwrap_or_default(),
        lpVerb: w!("open"),
        lpFile: PCWSTR(target.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    host.execute(&mut info)
        .map_err(|error| failed(SERVICE, error))
}

/// The app Windows opens files of one type with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultApp {
    /// The name the user knows the app by ("Notepad").
    pub name: String,
    /// The program file; `None` for apps the shell cannot map to one (some packaged apps).
    pub executable: Option<PathBuf>,
}

/// The program the shell names as the handler of a file type nobody handles: the "Choose an
/// app" prompt.
const OPEN_WITH_PROMPT: &str = "OpenWith.exe";

/// The default app for `extension` (with the dot, ".md"); `None` when no app is associated.
///
/// For a type without an association the shell does not fail the query: it answers with the
/// "Choose an app" prompt (`OpenWith.exe`), which is the absence of a default app and is
/// reported as such.
#[must_use]
pub fn default_app_for(extension: &str) -> Option<DefaultApp> {
    default_app_with(|kind| query(kind, extension))
}

/// The default app from the association strings `query` answers.
fn default_app_with(query: impl Fn(ASSOCSTR) -> Option<String>) -> Option<DefaultApp> {
    let name = query(ASSOCSTR_FRIENDLYAPPNAME)?;
    let executable = query(ASSOCSTR_EXECUTABLE).map(PathBuf::from);
    if executable.as_deref().is_some_and(is_open_with_prompt) {
        return None;
    }
    Some(DefaultApp { name, executable })
}

/// True when `path` is the "Choose an app" prompt program.
fn is_open_with_prompt(path: &Path) -> bool {
    has_file_name(path, OPEN_WITH_PROMPT)
}

/// True when the default app for `extension` is the program file called `executable_name`
/// (compared without regard to case), e.g. `is_default_app(".md", "SSWriter.exe")`.
#[must_use]
pub fn is_default_app(extension: &str, executable_name: &str) -> bool {
    default_app_for(extension)
        .and_then(|app| app.executable)
        .is_some_and(|path| has_file_name(&path, executable_name))
}

/// True when the last component of `path` equals `name` ignoring ASCII case.
fn has_file_name(path: &Path, name: &str) -> bool {
    path.file_name()
        .is_some_and(|file| file.to_string_lossy().eq_ignore_ascii_case(name))
}

/// Asks the shell for one string of the association of `extension`.
fn query(kind: ASSOCSTR, extension: &str) -> Option<String> {
    let association = HSTRING::from(extension);
    read_string(|out, length| {
        // SAFETY: `association` is a valid NUL-terminated string; `out` is `None` (the call only
        // reports the required length, in characters with the terminator) or a buffer of
        // `*length` characters, the size passed in.
        unsafe {
            AssocQueryStringW(
                ASSOCF_NOTRUNCATE,
                kind,
                &association,
                PCWSTR::null(),
                out,
                std::ptr::from_mut(length),
            )
        }
    })
}

/// The string of a call that reports its length first (`S_FALSE` and the length with the
/// terminator for a `None` buffer) and fills a buffer of that length second.
fn read_string(mut call: impl FnMut(Option<PWSTR>, &mut u32) -> HRESULT) -> Option<String> {
    let mut length = 0_u32;
    if call(None, &mut length) != S_FALSE || length == 0 {
        return None;
    }
    let mut buffer = vec![0_u16; length as usize];
    call(Some(PWSTR(buffer.as_mut_ptr())), &mut length)
        .is_ok()
        .then(|| from_wide(&buffer))
}

#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{E_FAIL, S_OK};

    use super::*;

    #[test]
    fn link_extensions_follow_how_windows_resolves_the_file() {
        let cases = [
            ("notes.md", Some(".md")),
            ("../tools/Setup.EXE", Some(".EXE")),
            (r"C:\tools\run.bat", Some(".bat")),
            ("file:///C:/tools/run.ps1", Some(".ps1")),
            ("setup.exe.", Some(".exe")),
            ("setup.exe . ", Some(".exe")),
            ("page.md#section", Some(".md")),
            ("tool.exe?x=1", Some(".exe")),
            ("archive.tar.gz", Some(".gz")),
            (".gitignore", Some(".gitignore")),
            ("folder.d/readme", None),
            ("README", None),
            ("", None),
            ("https://example.com/setup.exe", None),
            ("mailto:me@example.com", None),
        ];
        for (target, expected) in cases {
            assert_eq!(link_extension(target).as_deref(), expected, "{target:?}");
        }
    }

    #[test]
    fn the_choose_an_app_prompt_is_recognized_by_its_file_name() {
        assert!(is_open_with_prompt(Path::new(
            r"C:\WINDOWS\system32\OpenWith.exe"
        )));
        assert!(is_open_with_prompt(Path::new(
            r"c:\windows\system32\openwith.EXE"
        )));
        assert!(!is_open_with_prompt(Path::new(r"C:\Windows\notepad.exe")));
    }

    /// Answers like the shell: the length with the terminator first, then `text`.
    fn shell_string(text: &str, second: HRESULT) -> impl FnMut(Option<PWSTR>, &mut u32) -> HRESULT {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        move |out, length| match out {
            None => {
                *length = wide.len() as u32;
                S_FALSE
            }
            Some(buffer) => {
                assert_eq!(
                    *length as usize,
                    wide.len(),
                    "the buffer has the reported size"
                );
                // SAFETY: the buffer holds `*length` characters, as asserted.
                unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), buffer.0, wide.len()) };
                second
            }
        }
    }

    #[test]
    fn a_string_is_read_in_two_calls_and_any_other_answer_is_none() {
        assert_eq!(
            read_string(shell_string("Notepad", S_OK)).as_deref(),
            Some("Notepad")
        );
        // The second call fails.
        assert_eq!(read_string(shell_string("Notepad", E_FAIL)), None);
        // The first call fails, succeeds without asking for a buffer, or reports no length.
        assert_eq!(read_string(|_, _| E_FAIL), None);
        assert_eq!(read_string(|_, _| S_OK), None);
        assert_eq!(read_string(|_, _| S_FALSE), None);
    }

    #[test]
    fn the_default_app_needs_a_name_and_the_choose_an_app_prompt_is_none() {
        let answers = |name: Option<&str>, exe: Option<&str>| {
            let (name, exe) = (name.map(str::to_owned), exe.map(str::to_owned));
            move |kind: ASSOCSTR| {
                if kind == ASSOCSTR_FRIENDLYAPPNAME {
                    name.clone()
                } else {
                    exe.clone()
                }
            }
        };
        assert_eq!(
            default_app_with(answers(Some("Notepad"), Some(r"C:\Windows\notepad.exe"))),
            Some(DefaultApp {
                name: "Notepad".to_owned(),
                executable: Some(PathBuf::from(r"C:\Windows\notepad.exe")),
            })
        );
        // A packaged app the shell cannot map to a program file.
        assert_eq!(
            default_app_with(answers(Some("Photos"), None)),
            Some(DefaultApp {
                name: "Photos".to_owned(),
                executable: None,
            })
        );
        assert_eq!(
            default_app_with(answers(
                Some("Open With"),
                Some(r"C:\Windows\System32\OpenWith.exe")
            )),
            None
        );
        assert_eq!(default_app_with(answers(None, Some(r"C:\x.exe"))), None);
    }

    #[test]
    fn file_names_compare_without_case() {
        assert!(has_file_name(
            Path::new(r"C:\Apps\SSWriter.exe"),
            "sswriter.EXE"
        ));
        assert!(!has_file_name(
            Path::new(r"C:\Apps\SSWriter.exe"),
            "notepad.exe"
        ));
        assert!(!has_file_name(Path::new(""), "notepad.exe"));
    }
}
