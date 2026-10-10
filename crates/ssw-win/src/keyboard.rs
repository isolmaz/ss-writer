//! Keyboard layout services: the AltGr character probe and key display names.
//!
//! [`ToUnicodeCharProbe`] answers "does Ctrl+Alt+key type a character in this layout" with
//! `ToUnicodeEx` and the "do not change the keyboard state" flag (`0x4`), so asking never eats a
//! dead key. [`key_name`] gives the localized name of a key (`GetKeyNameTextW`) for shortcut
//! display. [`session_layouts`] lists the layouts the user has, so a layout other than the
//! active one can be probed ([`ToUnicodeCharProbe::for_layout`]). No layout is ever loaded,
//! activated or unloaded here: that would change the user's session.

use thiserror::Error;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyNameTextW, GetKeyboardLayout, GetKeyboardLayoutList, HKL, MAPVK_VK_TO_VSC,
    MapVirtualKeyW, ToUnicodeEx,
};

use ssw_ui::shortcuts::CharProbe;

/// `ToUnicodeEx` flag: do not change the keyboard state (dead keys stay pending).
const NO_STATE_CHANGE: u32 = 0x4;
/// Bit set in a key-state byte of a pressed key.
const DOWN: u8 = 0x80;
/// Low bit of the Caps Lock key-state byte: the toggle is on.
const TOGGLED: u8 = 0x01;

const VK_SHIFT: usize = 0x10;
const VK_CONTROL: usize = 0x11;
const VK_MENU: usize = 0x12;
const VK_CAPITAL: usize = 0x14;
const VK_LCONTROL: usize = 0xA2;
const VK_RMENU: usize = 0xA5;

/// Failures of the keyboard services.
#[derive(Debug, Error)]
pub enum KeyboardError {
    /// The system has no name for the key.
    #[error("virtual key {vk:#04x} has no name")]
    NoName { vk: u16 },
}

/// Probes a keyboard layout with `ToUnicodeEx`.
#[derive(Debug, Clone, Copy)]
pub struct ToUnicodeCharProbe {
    hkl: Option<HKL>,
}

impl ToUnicodeCharProbe {
    /// Probes the layout of the current thread, looked up on every call so a layout switch is
    /// picked up.
    #[must_use]
    pub fn current() -> Self {
        Self { hkl: None }
    }

    /// Probes a specific layout (for example one of [`session_layouts`]).
    #[must_use]
    pub fn for_layout(hkl: HKL) -> Self {
        Self { hkl: Some(hkl) }
    }

    /// The text `vk`/`scan` types with the given modifier state, or `None` when it types no
    /// printable character. `altgr` presses Ctrl+Alt (what the OS reports for AltGr). Control
    /// characters (Ctrl+A → U+0001) are not text. A dead key counts as text: it types its
    /// diacritic.
    #[must_use]
    pub fn char_for(
        &self,
        vk: u16,
        scan: u16,
        shift: bool,
        caps: bool,
        altgr: bool,
    ) -> Option<String> {
        let mut state = [0_u8; 256];
        if shift {
            state[VK_SHIFT] = DOWN;
        }
        if caps {
            state[VK_CAPITAL] = TOGGLED;
        }
        if altgr {
            state[VK_CONTROL] = DOWN;
            state[VK_LCONTROL] = DOWN;
            state[VK_MENU] = DOWN;
            state[VK_RMENU] = DOWN;
        }
        let mut buffer = [0_u16; 8];
        let hkl = self.hkl.unwrap_or_else(current_layout);
        // SAFETY: `state` is a valid 256-byte key-state array and `buffer` a valid writable
        // slice for the duration of the call; `hkl` is a layout handle obtained from the OS.
        // The flag keeps the thread's keyboard state unchanged.
        let written = unsafe {
            ToUnicodeEx(
                u32::from(vk),
                u32::from(scan),
                &state,
                &mut buffer,
                NO_STATE_CHANGE,
                Some(hkl),
            )
        };
        // A negative count is a dead key: one character (the accent) is in the buffer.
        let count = usize::try_from(written.unsigned_abs()).map_or(0, |n| n.min(buffer.len()));
        let text = String::from_utf16_lossy(&buffer[..count]);
        (!text.is_empty() && !text.chars().any(char::is_control)).then_some(text)
    }
}

impl CharProbe for ToUnicodeCharProbe {
    fn produces_char(&self, vk: u16, scan: u16, shift: bool, caps: bool) -> bool {
        self.char_for(vk, scan, shift, caps, true).is_some()
    }
}

/// Whether the Caps Lock toggle is on (the layout needs it to decide what a key types).
#[must_use]
pub fn caps_lock_on() -> bool {
    // SAFETY: a pure query of the calling thread's key state.
    let state =
        unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(VK_CAPITAL as i32) };
    state & 1 != 0
}

fn current_layout() -> HKL {
    // SAFETY: GetKeyboardLayout has no preconditions; thread id 0 means the calling thread.
    unsafe { GetKeyboardLayout(0) }
}

/// The keyboard layouts of the user's session, as `GetKeyboardLayoutList` reports them (the
/// low word of a handle is its language id, `0x041F` for Turkish). Probe one with
/// [`ToUnicodeCharProbe::for_layout`]; nothing is loaded or activated.
#[must_use]
pub fn session_layouts() -> Vec<HKL> {
    // SAFETY: a `None` list asks only for the count.
    let count = unsafe { GetKeyboardLayoutList(None) };
    let mut list = vec![HKL::default(); usize::try_from(count).unwrap_or(0)];
    // SAFETY: `list` is a writable slice of exactly the size the first call reported.
    let filled = unsafe { GetKeyboardLayoutList(Some(&mut list)) };
    list.truncate(usize::try_from(filled).unwrap_or(0));
    list
}

/// Keys whose scan code needs the "extended key" bit for `GetKeyNameTextW`.
fn is_extended(vk: u16) -> bool {
    matches!(
        vk,
        0x21..=0x28 // PgUp PgDn End Home Left Up Right Down
            | 0x2D // Insert
            | 0x2E // Delete
            | 0x5B // Left Windows
            | 0x5C // Right Windows
            | 0x5D // Apps
            | 0x6F // Numpad divide
            | 0x90 // NumLock
            | 0xA3 // Right Ctrl
            | 0xA5 // Right Alt
    )
}

/// The display name of a key for the current layout and UI language ("Ctrl", "Enter", "S").
/// `scan` of 0 is derived from the virtual key.
///
/// # Errors
/// [`KeyboardError::NoName`] when the system has no name for the key.
pub fn key_name(vk: u16, scan: u16) -> Result<String, KeyboardError> {
    let scan = if scan == 0 {
        // SAFETY: MapVirtualKeyW has no preconditions.
        let mapped = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) };
        u16::try_from(mapped).unwrap_or(0)
    } else {
        scan
    };
    let extended = i32::from(is_extended(vk));
    let lparam = (i32::from(scan) << 16) | (extended << 24);
    let mut buffer = [0_u16; 64];
    // SAFETY: `buffer` is a valid writable slice; the API writes at most `buffer.len()` units.
    let length = unsafe { GetKeyNameTextW(lparam, &mut buffer) };
    let length = usize::try_from(length).unwrap_or(0);
    if length == 0 {
        return Err(KeyboardError::NoName { vk });
    }
    Ok(String::from_utf16_lossy(&buffer[..length]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handle of Turkish Q (language 0x041F, layout 0x041F): `HKL` `0x041F041F`.
    const TURKISH_Q_HKL: usize = 0x041F_041F;
    const VK_Q: u16 = 0x51;
    const VK_I: u16 = 0x49;
    const VK_7: u16 = 0x37;
    const VK_F5: u16 = 0x74;
    const SCAN_Q_POSITION: u16 = 0x10;
    const SCAN_I_POSITION: u16 = 0x17;
    const SCAN_7: u16 = 0x08;

    #[test]
    fn the_session_layouts_are_the_users_and_probing_one_changes_nothing() {
        let layouts = session_layouts();
        assert!(!layouts.is_empty());
        let active_before = current_layout();
        // Every layout of the user's session can be probed; none of them is activated by it.
        for hkl in layouts {
            let probe = ToUnicodeCharProbe::for_layout(hkl);
            // Function keys type nothing in any layout, even with AltGr.
            assert!(!probe.produces_char(VK_F5, 0x3F, false, false));
            assert!(probe.char_for(VK_F5, 0x3F, false, false, true).is_none());
        }
        assert_eq!(current_layout(), active_before);
    }

    #[test]
    fn uia_keyboard_turkish_q_altgr_layer_types_text() {
        let active_before = current_layout();
        // The user's own Turkish Q layout, when the session has it (it is only probed).
        for hkl in session_layouts()
            .into_iter()
            .filter(|hkl| hkl.0 as usize == TURKISH_Q_HKL)
        {
            let probe = ToUnicodeCharProbe::for_layout(hkl);
            // Function keys type nothing, whatever the layout, even with AltGr.
            assert!(!probe.produces_char(VK_F5, 0x3F, false, false));
            // AltGr+Q is '@' and AltGr+7 is '{' on the Turkish Q layout.
            assert_eq!(
                probe
                    .char_for(VK_Q, SCAN_Q_POSITION, false, false, true)
                    .as_deref(),
                Some("@")
            );
            assert_eq!(
                probe.char_for(VK_7, SCAN_7, false, false, true).as_deref(),
                Some("{")
            );
            assert!(probe.produces_char(VK_Q, SCAN_Q_POSITION, false, false));
            assert!(probe.produces_char(VK_7, SCAN_7, false, false));
            // Without AltGr the same keys type their plain letters; Turkish I is dotless.
            assert_eq!(
                probe
                    .char_for(VK_Q, SCAN_Q_POSITION, false, false, false)
                    .as_deref(),
                Some("q")
            );
            assert_eq!(
                probe
                    .char_for(VK_I, SCAN_I_POSITION, false, false, false)
                    .as_deref(),
                Some("\u{131}")
            );
            assert_eq!(
                probe
                    .char_for(VK_I, SCAN_I_POSITION, true, false, false)
                    .as_deref(),
                Some("I")
            );
            // Caps Lock upper-cases without Shift.
            assert_eq!(
                probe
                    .char_for(VK_Q, SCAN_Q_POSITION, false, true, false)
                    .as_deref(),
                Some("Q")
            );
        }
        // Probing never changes the user's active layout.
        assert_eq!(current_layout(), active_before);
    }

    #[test]
    fn caps_lock_follows_the_toggle_state_of_the_key() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL};

        // SAFETY: a pure query of the calling thread's key state.
        let toggled = unsafe { GetKeyState(i32::from(VK_CAPITAL.0)) } & 1 != 0;
        assert_eq!(caps_lock_on(), toggled);
    }

    #[test]
    fn uia_keyboard_current_layout_probe_rejects_control_characters_and_nonprinting_keys() {
        let probe = ToUnicodeCharProbe::current();
        assert!(!probe.produces_char(VK_F5, 0x3F, false, false));
        // Ctrl+Alt+Left never types text.
        assert!(!probe.produces_char(0x25, 0x4B, false, false));
        // Plain 'A' without modifiers types a letter on any Latin layout; a control key does not.
        assert!(probe.char_for(0x1B, 0x01, false, false, false).is_none());
    }

    #[test]
    fn uia_keyboard_key_names() {
        for (vk, scan) in [
            (0x0D_u16, 0x1C_u16),
            (0x11, 0x1D),
            (0x25, 0),
            (0x2E, 0),
            (0x70, 0),
            (0x53, 0x1F),
        ] {
            let name = key_name(vk, scan).unwrap();
            assert!(!name.is_empty(), "vk {vk:#x}");
        }
        // Deriving the scan code from the virtual key gives the same name as passing it.
        assert_eq!(key_name(0x0D, 0).unwrap(), key_name(0x0D, 0x1C).unwrap());
        // A virtual key with no scan code has no name.
        let err = key_name(0xFF, 0).unwrap_err();
        assert_eq!(err.to_string(), "virtual key 0xff has no name");
    }

    #[test]
    fn uia_keyboard_extended_keys() {
        for vk in [
            0x21_u16, 0x28, 0x2D, 0x2E, 0x5B, 0x5D, 0x6F, 0x90, 0xA3, 0xA5,
        ] {
            assert!(is_extended(vk), "{vk:#x}");
        }
        for vk in [0x0D_u16, 0x41, 0x70, 0x20, 0xA2] {
            assert!(!is_extended(vk), "{vk:#x}");
        }
    }
}
