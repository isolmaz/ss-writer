//! Native menus: the `HMENU` built from a model is read back with `GetMenuItemInfoW`; the
//! popup is "shown" by a scripted host that inspects the menu, so nothing ever appears on
//! screen. The process's menu theme is switched through the real uxtheme exports.

use std::cell::RefCell;
use std::rc::Rc;

use ssw_core::keymap::{KeyChord, vk};
use ssw_win::com::WinError;
use ssw_win::keyboard::key_name;
use ssw_win::menus::{
    CheckState, MenuEntry, MenuItem, MenuModel, MenuTheme, NativeMenu, PopupHost, PopupMenus,
    PopupRequest, Submenu, accelerator_text, apply_menu_theme,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetMenuItemInfoW, HMENU, IsMenu, MENU_ITEM_MASK, MENUITEMINFOW, MFS_CHECKED, MFS_DISABLED,
    MFT_RADIOCHECK, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU,
    TPM_NONOTIFY, TPM_RETURNCMD,
};
use windows::core::PWSTR;

/// One row of a native menu as the system reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Separator,
    Item {
        id: u32,
        text: String,
        disabled: bool,
        checked: bool,
        radio: bool,
    },
    Submenu {
        text: String,
        disabled: bool,
        rows: Vec<Row>,
    },
}

fn read_menu(hmenu: HMENU) -> Vec<Row> {
    // SAFETY: `hmenu` is a valid menu handle of the caller.
    let count = unsafe { windows::Win32::UI::WindowsAndMessaging::GetMenuItemCount(Some(hmenu)) };
    (0..u32::try_from(count).unwrap())
        .map(|index| read_row(hmenu, index))
        .collect()
}

fn read_row(hmenu: HMENU, index: u32) -> Row {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MENU_ITEM_MASK(
            MIIM_ID.0 | MIIM_STATE.0 | MIIM_FTYPE.0 | MIIM_STRING.0 | MIIM_SUBMENU.0,
        ),
        dwTypeData: PWSTR::null(),
        ..Default::default()
    };
    // SAFETY: the first call only reports the text length (no buffer); `info` is initialized.
    unsafe { GetMenuItemInfoW(hmenu, index, true, &raw mut info) }.unwrap();
    if info.fType.0 & MFT_SEPARATOR.0 != 0 {
        return Row::Separator;
    }
    let mut buffer = vec![0_u16; info.cch as usize + 1];
    info.dwTypeData = PWSTR(buffer.as_mut_ptr());
    info.cch = buffer.len() as u32;
    // SAFETY: `buffer` holds `cch` characters, as announced in `info`.
    unsafe { GetMenuItemInfoW(hmenu, index, true, &raw mut info) }.unwrap();
    let text = String::from_utf16_lossy(&buffer[..info.cch as usize]);
    let disabled = info.fState.0 & MFS_DISABLED.0 == MFS_DISABLED.0;
    if info.hSubMenu.0.is_null() {
        Row::Item {
            id: info.wID,
            text,
            disabled,
            checked: info.fState.0 & MFS_CHECKED.0 != 0,
            radio: info.fType.0 & MFT_RADIOCHECK.0 != 0,
        }
    } else {
        Row::Submenu {
            text,
            disabled,
            rows: read_menu(info.hSubMenu),
        }
    }
}

fn plain(id: u32, text: &str) -> Row {
    Row::Item {
        id,
        text: text.to_owned(),
        disabled: false,
        checked: false,
        radio: false,
    }
}

fn sample_model() -> MenuModel {
    MenuModel::new(vec![
        MenuEntry::Item(
            MenuItem::new(1, "&Open\u{2026}").accelerator(KeyChord::new(vk::O).with_ctrl()),
        ),
        MenuEntry::Item(MenuItem::new(2, "&Save").disabled()),
        MenuEntry::Separator,
        MenuEntry::Item(MenuItem::new(3, "&Word wrap").checked()),
        MenuEntry::Submenu(Submenu::new(
            "&Theme",
            vec![
                MenuEntry::Item(MenuItem::new(10, "&Light")),
                MenuEntry::Item(MenuItem::new(11, "&Dark").radio()),
                MenuEntry::Separator,
                MenuEntry::Submenu(
                    Submenu::new("&More", vec![MenuEntry::Item(MenuItem::new(12, "Deep"))])
                        .disabled(),
                ),
            ],
        )),
        MenuEntry::Item(MenuItem::new(13, "Fo&rmat \u{00DC}\u{011F}")),
    ])
}

#[test]
fn the_native_menu_mirrors_items_marks_separators_and_submenus() {
    let menu = NativeMenu::build(&sample_model()).unwrap();
    let ctrl_o = accelerator_text(KeyChord::new(vk::O).with_ctrl());
    assert_eq!(
        read_menu(menu.hmenu()),
        [
            plain(1, &format!("&Open\u{2026}\t{ctrl_o}")),
            Row::Item {
                id: 2,
                text: "&Save".to_owned(),
                disabled: true,
                checked: false,
                radio: false,
            },
            Row::Separator,
            Row::Item {
                id: 3,
                text: "&Word wrap".to_owned(),
                disabled: false,
                checked: true,
                radio: false,
            },
            Row::Submenu {
                text: "&Theme".to_owned(),
                disabled: false,
                rows: vec![
                    plain(10, "&Light"),
                    Row::Item {
                        id: 11,
                        text: "&Dark".to_owned(),
                        disabled: false,
                        checked: true,
                        radio: true,
                    },
                    Row::Separator,
                    Row::Submenu {
                        text: "&More".to_owned(),
                        disabled: true,
                        rows: vec![plain(12, "Deep")],
                    },
                ],
            },
            plain(13, "Fo&rmat \u{00DC}\u{011F}"),
        ]
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn every_failing_system_call_of_building_a_menu_is_reported_and_nothing_leaks() {
    let model = sample_model();
    let steps = ssw_win::fault::sweep(|| {
        NativeMenu::build(&model).err().map(|error| {
            error
                .to_string()
                .split(" failed: ")
                .next()
                .unwrap_or_default()
                .to_owned()
        })
    });
    // The root menu, four of its rows, the Theme menu with three rows, the More menu with its
    // row, then the rows that hold the finished submenus, and the last row of the root.
    let menu = "CreatePopupMenu";
    let row = "InsertMenuItemW";
    let order = [
        menu, row, row, row, row, menu, row, row, row, menu, row, row, row, row,
    ];
    let expected: Vec<Option<String>> = order.iter().map(|step| Some((*step).to_owned())).collect();
    assert_eq!(steps, expected);
    // Without a failure the same model builds.
    assert!(NativeMenu::build(&model).is_ok());
}

#[test]
fn an_empty_model_builds_an_empty_menu_that_is_destroyed_on_drop() {
    let menu = NativeMenu::build(&MenuModel::default()).unwrap();
    let hmenu = menu.hmenu();
    assert!(read_menu(hmenu).is_empty());
    // SAFETY: only asks whether the handle is a menu.
    assert!(unsafe { IsMenu(hmenu) }.as_bool());
    drop(menu);
    // SAFETY: only asks whether the handle is a menu.
    assert!(!unsafe { IsMenu(hmenu) }.as_bool());
}

#[test]
fn command_ids_must_be_ids_the_system_can_return() {
    for bad in [0, 0x8000_0000, u32::MAX] {
        let model = MenuModel::new(vec![MenuEntry::Item(MenuItem::new(bad, "x"))]);
        assert!(
            matches!(NativeMenu::build(&model), Err(WinError::Invalid(_))),
            "{bad}"
        );
        let nested = MenuModel::new(vec![MenuEntry::Submenu(Submenu::new(
            "s",
            vec![MenuEntry::Item(MenuItem::new(bad, "x"))],
        ))]);
        assert!(NativeMenu::build(&nested).is_err(), "{bad} in a submenu");
    }
    let largest = MenuModel::new(vec![MenuEntry::Item(MenuItem::new(i32::MAX as u32, "x"))]);
    assert!(NativeMenu::build(&largest).is_ok());
}

#[test]
fn accelerator_text_joins_the_shell_key_names_in_modifier_order() {
    let name = |vk: u16| key_name(vk, 0).unwrap();
    let (ctrl, shift, alt, win) = (name(0x11), name(0x10), name(0x12), name(0x5B));
    assert_eq!(
        accelerator_text(KeyChord::new(vk::S).with_ctrl()),
        format!("{ctrl}+{}", name(vk::S))
    );
    assert_eq!(
        accelerator_text(KeyChord::new(vk::P).with_ctrl().with_shift()),
        format!("{ctrl}+{shift}+{}", name(vk::P))
    );
    assert_eq!(
        accelerator_text(KeyChord::new(vk::F5)),
        name(vk::F5),
        "no modifiers, no separator"
    );
    assert_eq!(
        accelerator_text(
            KeyChord::new(vk::A)
                .with_ctrl()
                .with_shift()
                .with_alt()
                .with_win()
        ),
        format!("{ctrl}+{shift}+{alt}+{win}+{}", name(vk::A))
    );
    // A key without a name falls back to the portable spelling.
    assert_eq!(
        accelerator_text(KeyChord::new(0xFF).with_alt()),
        format!("{alt}+VK_0xFF")
    );
}

/// Inspects the menu it is asked to show and answers with a scripted choice.
struct Inspecting {
    choose: Option<u32>,
    seen: Seen,
}

type Seen = Rc<RefCell<Vec<(HMENU, PopupRequest, Vec<Row>)>>>;

impl PopupHost for Inspecting {
    fn track(&mut self, menu: HMENU, request: &PopupRequest) -> Option<u32> {
        self.seen
            .borrow_mut()
            .push((menu, *request, read_menu(menu)));
        self.choose
    }
}

fn inspecting(choose: Option<u32>) -> (PopupMenus<Inspecting>, Seen) {
    let seen = Seen::default();
    let host = Inspecting {
        choose,
        seen: Rc::clone(&seen),
    };
    (PopupMenus::with_host(host), seen)
}

#[test]
fn a_popup_is_built_handed_to_the_host_and_destroyed_afterwards() {
    let owner = HWND(0x4242 as *mut _);
    let (mut popups, seen) = inspecting(Some(11));
    let chosen = popups.show(&sample_model(), owner, 120, -30).unwrap();
    assert_eq!(chosen, Some(11));
    let seen = seen.borrow();
    let (hmenu, request, rows) = &seen[0];
    assert_eq!(request.owner, owner);
    assert_eq!((request.x, request.y), (120, -30));
    assert_ne!(request.flags & TPM_RETURNCMD.0, 0, "the id is returned");
    assert_ne!(request.flags & TPM_NONOTIFY.0, 0, "no WM_COMMAND is posted");
    assert_eq!(rows.len(), 6);
    // SAFETY: only asks whether the handle is still a menu.
    let still_menu = unsafe { IsMenu(*hmenu) }.as_bool();
    assert!(!still_menu, "destroyed after tracking");
}

#[test]
fn a_dismissed_popup_chooses_nothing_and_an_invalid_model_never_reaches_the_host() {
    let owner = HWND(0x4242 as *mut _);
    let (mut popups, seen) = inspecting(None);
    assert_eq!(popups.show(&sample_model(), owner, 0, 0).unwrap(), None);
    let bad = MenuModel::new(vec![MenuEntry::Item(MenuItem::new(0, "x"))]);
    assert!(popups.show(&bad, owner, 0, 0).is_err());
    assert_eq!(seen.borrow().len(), 1);
}

#[test]
fn popup_menus_made_without_a_host_use_the_system_one() {
    // Only built, never shown: tests never put a system menu on screen.
    for popups in [PopupMenus::new(), PopupMenus::default()] {
        assert!(format!("{popups:?}").contains("SystemPopupHost"));
    }
}

#[test]
fn item_builders_set_their_fields() {
    let item = MenuItem::new(7, "x");
    assert!(item.enabled && item.check == CheckState::None && item.accelerator.is_none());
    assert_eq!(item.clone().checked().check, CheckState::Check);
    assert_eq!(item.clone().radio().check, CheckState::Radio);
    assert!(!item.disabled().enabled);
}

#[test]
fn menu_themes_apply_through_the_uxtheme_exports() {
    for theme in [MenuTheme::Dark, MenuTheme::Light, MenuTheme::System] {
        assert!(apply_menu_theme(theme), "{theme:?}");
    }
}
