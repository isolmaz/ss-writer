//! Native popup menus built from a platform-neutral model.
//!
//! [`MenuModel`] describes items, separators and submenus with check marks, radio marks and
//! accelerator text. [`NativeMenu::build`] turns it into an `HMENU` that destroys itself when
//! dropped. [`PopupMenus::show`] builds the menu, hands it to a [`PopupHost`] (the modal
//! `TrackPopupMenuEx` call, which is pointer driven) and returns the chosen command id;
//! tests substitute a host that inspects the menu instead of showing it.
//!
//! Labels are used as given: a `&` marks the mnemonic letter (`&&` is a literal ampersand), so
//! the localized strings decide the mnemonics. Accelerator text is the shell's key naming
//! (`GetKeyNameTextW`), localized to the UI language: `Ctrl+Shift+P` is shown the way the
//! user's keyboard calls its keys.
//!
//! [`apply_menu_theme`] switches the process's native menus to dark or light (uxtheme
//! ordinals 135 and 136; no public API exists) when the system has them.

use ssw_core::keymap::KeyChord;
use windows::Win32::Foundation::{FreeLibrary, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, HMENU, InsertMenuItemW, MENU_ITEM_MASK, MENU_ITEM_STATE,
    MENU_ITEM_TYPE, MENUITEMINFOW, MFS_CHECKED, MFS_DISABLED, MFT_RADIOCHECK, MFT_SEPARATOR,
    MFT_STRING, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, TPM_LEFTALIGN,
    TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TPM_TOPALIGN,
};
use windows::core::{HSTRING, PCSTR, PCWSTR, PWSTR, w};

use crate::com::{OsResultExt, WinError};
use crate::fault::check;
use crate::keyboard::key_name;

/// The mark in front of a menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckState {
    /// No mark.
    #[default]
    None,
    /// A check mark: the option is on.
    Check,
    /// A bullet: this is the selected one of a group of exclusive items.
    Radio,
}

/// One command of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuItem {
    /// The command id [`PopupMenus::show`] returns when the item is chosen: 1 to `i32::MAX`.
    pub id: u32,
    /// The text, with `&` before the mnemonic letter.
    pub label: String,
    /// The shortcut shown right-aligned after the label.
    pub accelerator: Option<KeyChord>,
    /// False greys the item out.
    pub enabled: bool,
    /// The mark in front of the label.
    pub check: CheckState,
}

impl MenuItem {
    /// An enabled, unmarked item without accelerator.
    #[must_use]
    pub fn new(id: u32, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            accelerator: None,
            enabled: true,
            check: CheckState::None,
        }
    }

    /// Shows `chord` as the item's shortcut.
    #[must_use]
    pub fn accelerator(mut self, chord: KeyChord) -> Self {
        self.accelerator = Some(chord);
        self
    }

    /// Greys the item out.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// Marks the item with a check mark.
    #[must_use]
    pub fn checked(mut self) -> Self {
        self.check = CheckState::Check;
        self
    }

    /// Marks the item with a radio bullet.
    #[must_use]
    pub fn radio(mut self) -> Self {
        self.check = CheckState::Radio;
        self
    }
}

/// A menu that opens from an item of another menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submenu {
    /// The text, with `&` before the mnemonic letter.
    pub label: String,
    /// False greys the entry out.
    pub enabled: bool,
    /// The entries of the opened menu.
    pub menu: MenuModel,
}

impl Submenu {
    /// An enabled submenu.
    #[must_use]
    pub fn new(label: impl Into<String>, entries: Vec<MenuEntry>) -> Self {
        Self {
            label: label.into(),
            enabled: true,
            menu: MenuModel::new(entries),
        }
    }

    /// Greys the entry out.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// One row of a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    /// A command.
    Item(MenuItem),
    /// A submenu.
    Submenu(Submenu),
    /// A separator line.
    Separator,
}

/// The rows of a menu, top to bottom.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MenuModel {
    /// The rows.
    pub entries: Vec<MenuEntry>,
}

impl MenuModel {
    /// A menu with `entries`.
    #[must_use]
    pub fn new(entries: Vec<MenuEntry>) -> Self {
        Self { entries }
    }

    /// Checks that every command id can be returned by the system (1 to `i32::MAX`).
    fn validate(&self) -> Result<(), WinError> {
        for entry in &self.entries {
            match entry {
                MenuEntry::Item(item) if item.id == 0 || i32::try_from(item.id).is_err() => {
                    return Err(WinError::Invalid(
                        "a menu command id must be between 1 and i32::MAX",
                    ));
                }
                MenuEntry::Submenu(submenu) => submenu.menu.validate()?,
                MenuEntry::Item(_) | MenuEntry::Separator => {}
            }
        }
        Ok(())
    }
}

/// The text of `chord` as the shell names its keys ("Ctrl+Shift+P", localized). Keys the
/// system has no name for fall back to the portable form of [`KeyChord`]'s `Display`.
#[must_use]
pub fn accelerator_text(chord: KeyChord) -> String {
    let name = |vk: u16, fallback: &str| key_name(vk, 0).unwrap_or_else(|_| fallback.to_owned());
    let mut parts = Vec::new();
    if chord.ctrl {
        parts.push(name(VK_CONTROL.0, "Ctrl"));
    }
    if chord.shift {
        parts.push(name(VK_SHIFT.0, "Shift"));
    }
    if chord.alt {
        parts.push(name(VK_MENU.0, "Alt"));
    }
    if chord.win {
        parts.push(name(VK_LWIN.0, "Win"));
    }
    parts.push(name(chord.vk, &KeyChord::new(chord.vk).to_string()));
    parts.join("+")
}

/// The text of an item row: the label, a tab and the accelerator text when there is one.
fn item_text(item: &MenuItem) -> HSTRING {
    match item.accelerator {
        Some(chord) => HSTRING::from(format!("{}\t{}", item.label, accelerator_text(chord))),
        None => HSTRING::from(item.label.as_str()),
    }
}

/// A native menu that is destroyed (with its submenus) when dropped.
#[derive(Debug)]
pub struct NativeMenu {
    hmenu: HMENU,
}

impl NativeMenu {
    /// Builds the menu `model` describes.
    ///
    /// # Errors
    /// A command id is outside 1 to `i32::MAX`, or the system could not create or fill a menu.
    pub fn build(model: &MenuModel) -> Result<Self, WinError> {
        model.validate()?;
        Self::build_level(model)
    }

    fn build_level(model: &MenuModel) -> Result<Self, WinError> {
        // SAFETY: creates an empty popup menu, owned by the returned value.
        let created = unsafe { CreatePopupMenu() };
        // From here on `menu` owns the handle, so an early return (also a failure injected
        // right after the call) destroys it.
        let menu = check(created.map(|hmenu| Self { hmenu })).ctx("CreatePopupMenu")?;
        for (index, entry) in model.entries.iter().enumerate() {
            menu.insert(index as u32, entry)?;
        }
        Ok(menu)
    }

    /// The menu handle, valid while `self` lives.
    #[must_use]
    pub fn hmenu(&self) -> HMENU {
        self.hmenu
    }

    fn insert(&self, index: u32, entry: &MenuEntry) -> Result<(), WinError> {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            ..Default::default()
        };
        // The row's text and submenu must outlive the `InsertMenuItemW` call below; separators
        // have no text.
        let (text, child) = match entry {
            MenuEntry::Separator => {
                info.fMask = MIIM_FTYPE;
                info.fType = MFT_SEPARATOR;
                (HSTRING::new(), None)
            }
            MenuEntry::Item(item) => {
                info.fMask =
                    MENU_ITEM_MASK(MIIM_ID.0 | MIIM_STATE.0 | MIIM_FTYPE.0 | MIIM_STRING.0);
                info.fType = if item.check == CheckState::Radio {
                    MENU_ITEM_TYPE(MFT_STRING.0 | MFT_RADIOCHECK.0)
                } else {
                    MFT_STRING
                };
                info.fState = state(item.enabled, item.check != CheckState::None);
                info.wID = item.id;
                (item_text(item), None)
            }
            MenuEntry::Submenu(submenu) => {
                let level = Self::build_level(&submenu.menu)?;
                info.fMask =
                    MENU_ITEM_MASK(MIIM_STATE.0 | MIIM_FTYPE.0 | MIIM_STRING.0 | MIIM_SUBMENU.0);
                info.fType = MFT_STRING;
                info.fState = state(submenu.enabled, false);
                info.hSubMenu = level.hmenu;
                (HSTRING::from(submenu.label.as_str()), Some(level))
            }
        };
        // Ignored for separators (`MIIM_STRING` is not in their mask).
        info.dwTypeData = PWSTR(text.as_ptr().cast_mut());
        // SAFETY: `self.hmenu` is a valid menu; `info` is fully initialized and its string
        // (`text`) and submenu (`child`) outlive the call.
        check(unsafe { InsertMenuItemW(self.hmenu, index, true, &raw const info) })
            .ctx("InsertMenuItemW")?;
        if let Some(level) = child {
            // The parent menu owns the submenu now and destroys it with itself.
            std::mem::forget(level);
        }
        Ok(())
    }
}

/// The item state flags for an enabled / marked combination.
fn state(enabled: bool, checked: bool) -> MENU_ITEM_STATE {
    let mut flags = 0;
    if !enabled {
        flags |= MFS_DISABLED.0;
    }
    if checked {
        flags |= MFS_CHECKED.0;
    }
    MENU_ITEM_STATE(flags)
}

impl Drop for NativeMenu {
    fn drop(&mut self) {
        // SAFETY: destroys the menu created in `build_level`, with the submenus it owns. A
        // failure leaves nothing to report to.
        let _ = unsafe { DestroyMenu(self.hmenu) };
    }
}

/// Where and how a popup menu opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupRequest {
    /// The window that owns the menu and receives its messages.
    pub owner: HWND,
    /// Screen x of the menu's top-left corner.
    pub x: i32,
    /// Screen y of the menu's top-left corner.
    pub y: i32,
    /// The `TrackPopupMenuEx` flags: the command id is returned instead of posted.
    pub flags: u32,
}

/// The modal call that shows a popup menu and waits for the user.
pub trait PopupHost {
    /// Shows `menu` as described by `request`; the id of the chosen command, or `None` when the
    /// user dismissed the menu.
    fn track(&mut self, menu: HMENU, request: &PopupRequest) -> Option<u32>;
}

/// The real [`PopupHost`]: `TrackPopupMenuEx`. It brings `owner` to the foreground first (a
/// popup menu is only dismissed by a click elsewhere when its owner is foreground) and posts
/// `WM_NULL` afterwards, as the documentation prescribes.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPopupHost;

/// The flags every popup menu opens with: left/top aligned at the point, either mouse button
/// selects, the chosen command is returned and no message is posted for it.
const POPUP_FLAGS: u32 =
    TPM_LEFTALIGN.0 | TPM_TOPALIGN.0 | TPM_RETURNCMD.0 | TPM_NONOTIFY.0 | TPM_RIGHTBUTTON.0;

/// Shows popup menus through a [`PopupHost`].
#[derive(Debug)]
pub struct PopupMenus<H: PopupHost = SystemPopupHost> {
    host: H,
}

impl PopupMenus {
    /// Popup menus shown by the system.
    #[must_use]
    pub fn new() -> Self {
        Self::with_host(SystemPopupHost)
    }
}

impl Default for PopupMenus {
    fn default() -> Self {
        Self::new()
    }
}

impl<H: PopupHost> PopupMenus<H> {
    /// Popup menus shown by `host`.
    #[must_use]
    pub fn with_host(host: H) -> Self {
        Self { host }
    }

    /// Builds `model`, shows it with its corner at the screen point (`x`, `y`) and returns the
    /// id of the chosen command (`None` when dismissed). The menu is destroyed afterwards.
    ///
    /// # Errors
    /// The menu could not be built.
    pub fn show(
        &mut self,
        model: &MenuModel,
        owner: HWND,
        x: i32,
        y: i32,
    ) -> Result<Option<u32>, WinError> {
        let menu = NativeMenu::build(model)?;
        let request = PopupRequest {
            owner,
            x,
            y,
            flags: POPUP_FLAGS,
        };
        Ok(self.host.track(menu.hmenu(), &request))
    }
}

/// How native menus are themed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuTheme {
    /// Follow the system's light or dark setting.
    System,
    /// Always light.
    Light,
    /// Always dark.
    Dark,
}

/// What `GetProcAddress` returns for an export whose signature it does not know.
type Export = unsafe extern "system" fn() -> isize;
/// `int SetPreferredAppMode(int mode)`; returns the previous mode.
type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
/// `void FlushMenuThemes(void)`.
type FlushMenuThemes = unsafe extern "system" fn();

/// The ordinal of `SetPreferredAppMode` in `uxtheme.dll`.
const SET_PREFERRED_APP_MODE: u16 = 135;
/// The ordinal of `FlushMenuThemes` in `uxtheme.dll`.
const FLUSH_MENU_THEMES: u16 = 136;

/// `PreferredAppMode::AllowDark`: follow the system.
const MODE_ALLOW_DARK: i32 = 1;
/// `PreferredAppMode::ForceDark`.
const MODE_FORCE_DARK: i32 = 2;
/// `PreferredAppMode::ForceLight`.
const MODE_FORCE_LIGHT: i32 = 3;

/// Applies `theme` to the native menus of this process (uxtheme ordinals 135 and 136). Returns
/// false when the system does not export them: menus then stay light.
pub fn apply_menu_theme(theme: MenuTheme) -> bool {
    apply_theme_exports(
        w!("uxtheme.dll"),
        SET_PREFERRED_APP_MODE,
        FLUSH_MENU_THEMES,
        theme,
    )
}

/// [`apply_menu_theme`] with the library and the ordinals of its two exports given: false when
/// the library does not load or lacks an export, in which case nothing is called.
fn apply_theme_exports(
    library: PCWSTR,
    set_mode_ordinal: u16,
    flush_ordinal: u16,
    theme: MenuTheme,
) -> bool {
    let mode = match theme {
        MenuTheme::System => MODE_ALLOW_DARK,
        MenuTheme::Light => MODE_FORCE_LIGHT,
        MenuTheme::Dark => MODE_FORCE_DARK,
    };
    // SAFETY: loads a library by name; released below.
    let Ok(uxtheme) = (unsafe { LoadLibraryW(library) }) else {
        return false;
    };
    // SAFETY: an ordinal is passed as a pointer-sized value below 0x10000, which is how
    // `GetProcAddress` takes ordinals; the module stays loaded until `FreeLibrary` below.
    let (set_mode, flush) = unsafe {
        (
            GetProcAddress(uxtheme, PCSTR(usize::from(set_mode_ordinal) as *const u8)),
            GetProcAddress(uxtheme, PCSTR(usize::from(flush_ordinal) as *const u8)),
        )
    };
    let applied = match (set_mode, flush) {
        (Some(set_mode), Some(flush)) => {
            // SAFETY: ordinal 135 is `int SetPreferredAppMode(int)` and ordinal 136 is
            // `void FlushMenuThemes(void)` on every Windows 11 build this application runs on;
            // both only change this process's menu theming.
            unsafe {
                let set_mode = std::mem::transmute::<Export, SetPreferredAppMode>(set_mode);
                let flush = std::mem::transmute::<Export, FlushMenuThemes>(flush);
                set_mode(mode);
                flush();
            }
            true
        }
        _ => false,
    };
    // SAFETY: balances the `LoadLibraryW` above; the functions are not used afterwards.
    let _ = unsafe { FreeLibrary(uxtheme) };
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_library_that_does_not_load_or_lacks_an_export_applies_nothing() {
        // No such library.
        assert!(!apply_theme_exports(
            w!("ssw-no-such-library.dll"),
            SET_PREFERRED_APP_MODE,
            FLUSH_MENU_THEMES,
            MenuTheme::Dark
        ));
        // Neither ordinal is exported.
        assert!(!apply_theme_exports(
            w!("uxtheme.dll"),
            0xFFF0,
            0xFFF1,
            MenuTheme::Dark
        ));
        // Only the one that sets the mode exists: nothing is called, the mode stays.
        assert!(!apply_theme_exports(
            w!("uxtheme.dll"),
            SET_PREFERRED_APP_MODE,
            0xFFF1,
            MenuTheme::Dark
        ));
        // Only the flush exists.
        assert!(!apply_theme_exports(
            w!("uxtheme.dll"),
            0xFFF0,
            FLUSH_MENU_THEMES,
            MenuTheme::Dark
        ));
    }
}
