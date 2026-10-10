//! Link safety against the real `AssocIsDangerous`, association queries against the real
//! shell (read-only), and the default-apps request inspected by a scripted shell host (nothing
//! is ever launched).

use ssw_ui::services::ServiceError;
use ssw_win::assoc::{
    DEFAULT_APPS_URI, default_app_for, is_dangerous_link, is_default_app,
    open_default_apps_settings,
};
use ssw_win::services::shell::{ItemIdList, ShellHost};
use windows::Win32::Foundation::{E_FAIL, HWND};
use windows::Win32::UI::Shell::{SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};

#[test]
fn executable_content_links_need_confirmation_and_documents_do_not() {
    for dangerous in [
        "tools/setup.exe",
        r"..\run.bat",
        "file:///C:/tools/start.cmd",
        "setup.exe.",
        "installer.EXE#download",
    ] {
        assert!(is_dangerous_link(dangerous), "{dangerous}");
    }
    for harmless in [
        "notes.md",
        "photo.png",
        "../docs/readme.txt",
        "README",
        "https://example.com/setup.exe",
        "mailto:me@example.com",
        "page.md#setup.exe",
        "",
    ] {
        assert!(!is_dangerous_link(harmless), "{harmless}");
    }
}

#[derive(Debug, Default)]
struct Seen {
    verb: String,
    file: String,
    owner: isize,
    mask: u32,
    show: i32,
}

struct Scripted {
    seen: Seen,
    fail: bool,
}

impl ShellHost for Scripted {
    fn execute(&mut self, info: &mut SHELLEXECUTEINFOW) -> windows::core::Result<()> {
        // SAFETY: the strings are NUL-terminated and alive for the call.
        let (verb, file) = unsafe {
            (
                info.lpVerb.to_string().unwrap(),
                info.lpFile.to_string().unwrap(),
            )
        };
        assert_eq!(info.cbSize as usize, size_of::<SHELLEXECUTEINFOW>());
        self.seen = Seen {
            verb,
            file,
            owner: info.hwnd.0 as isize,
            mask: info.fMask,
            show: info.nShow,
        };
        if self.fail {
            Err(E_FAIL.into())
        } else {
            Ok(())
        }
    }

    fn select_in_explorer(&mut self, _item: &ItemIdList) -> windows::core::Result<()> {
        panic!("the default-apps page is opened, nothing is revealed");
    }
}

#[test]
fn the_default_apps_page_is_requested_with_the_open_verb() {
    let mut host = Scripted {
        seen: Seen::default(),
        fail: false,
    };
    open_default_apps_settings(&mut host, Some(HWND(0x1234 as *mut _))).unwrap();
    assert_eq!(host.seen.verb, "open");
    assert_eq!(host.seen.file, "ms-settings:defaultapps");
    assert_eq!(host.seen.file, DEFAULT_APPS_URI);
    assert_eq!(host.seen.owner, 0x1234);
    assert_ne!(host.seen.mask & SEE_MASK_NOASYNC, 0);
    assert_eq!(host.seen.show, 1, "SW_SHOWNORMAL");

    open_default_apps_settings(&mut host, None).unwrap();
    assert_eq!(host.seen.owner, 0);
}

#[test]
fn a_failing_host_is_reported_as_an_assoc_failure() {
    let mut host = Scripted {
        seen: Seen::default(),
        fail: true,
    };
    let error = open_default_apps_settings(&mut host, None).unwrap_err();
    assert!(
        matches!(
            error,
            ServiceError::Failed {
                service: "assoc",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn an_extension_nobody_handles_has_no_default_app() {
    assert_eq!(default_app_for(".ssw-no-such-extension-7f3a"), None);
    assert!(!is_default_app(
        ".ssw-no-such-extension-7f3a",
        "anything.exe"
    ));
}

#[test]
fn the_default_app_of_a_text_file_is_described_and_matched_by_file_name() {
    let Some(app) = default_app_for(".txt") else {
        // A machine without any app for .txt has nothing to describe.
        return;
    };
    assert!(!app.name.is_empty());
    let Some(executable) = app.executable else {
        return;
    };
    let file_name = executable
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(is_default_app(".txt", &file_name));
    assert!(is_default_app(".txt", &file_name.to_uppercase()));
    assert!(!is_default_app(".txt", "ssw-not-the-default-app.exe"));
}
