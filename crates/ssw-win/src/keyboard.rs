//! Keyboard layout services: the AltGr character probe and key display names.
//!
//! [`ToUnicodeCharProbe`] answers "does Ctrl+Alt+key type a character in this layout" with
//! `ToUnicodeEx` and the "do not change the keyboard state" flag (`0x4`), so asking never eats a
//! dead key. [`key_name`] gives the localized name of a key (`GetKeyNameTextW`) for shortcut
//! display. [`LoadedLayout`] loads a layout by KLID *without activating it* so tests can probe
//! other layouts while the user's active layout stays untouched.

use thiserror::Error;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ACTIVATE_KEYBOARD_LAYOUT_FLAGS, GetKeyNameTextW, GetKeyboardLayout, GetKeyboardLayoutList, HKL,
    LoadKeyboardLayoutW, MAPVK_VK_TO_VSC, MapVirtualKeyW, ToUnicodeEx, UnloadKeyboardLayout,
};
use windows::core::PCWSTR;

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
    /// A keyboard layout could not be loaded.
    #[error("loading keyboard layout {klid} failed: {source}")]
    Load {
        klid: String,
        #[source]
        source: windows::core::Error,
    },
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

    /// Probes a specific layout (for example one loaded with [`LoadedLayout`]).
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

/// The keyboard layouts currently loaded for the process.
fn loaded_layouts() -> Vec<HKL> {
    // SAFETY: a `None` list asks only for the count.
    let count = unsafe { GetKeyboardLayoutList(None) };
    let mut list = vec![HKL::default(); usize::try_from(count).unwrap_or(0)];
    // SAFETY: `list` is a writable slice of exactly the size the first call reported.
    let filled = unsafe { GetKeyboardLayoutList(Some(&mut list)) };
    list.truncate(usize::try_from(filled).unwrap_or(0));
    list
}

/// A keyboard layout loaded by KLID without being activated. The layout is unloaded when the
/// value is dropped, unless it was already loaded (the user's own layouts stay).
#[derive(Debug)]
pub struct LoadedLayout {
    hkl: HKL,
    already_loaded: bool,
}

impl LoadedLayout {
    /// Loads the layout `klid` (for example `"0000041F"`, Turkish Q) without activating it.
    ///
    /// # Errors
    /// [`KeyboardError::Load`] when the system rejects the layout id.
    pub fn load(klid: &str) -> Result<Self, KeyboardError> {
        let before = loaded_layouts();
        let wide: Vec<u16> = klid.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and outlives the call. Flags 0 never include
        // KLF_ACTIVATE, so the active layout does not change.
        let hkl = unsafe {
            LoadKeyboardLayoutW(PCWSTR(wide.as_ptr()), ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0))
        }
        .map_err(|source| KeyboardError::Load {
            klid: klid.to_owned(),
            source,
        })?;
        Ok(Self {
            hkl,
            already_loaded: before.contains(&hkl),
        })
    }

    #[must_use]
    pub fn hkl(&self) -> HKL {
        self.hkl
    }

    /// True when the layout was loaded before [`LoadedLayout::load`] (it is kept on drop).
    #[must_use]
    pub fn was_already_loaded(&self) -> bool {
        self.already_loaded
    }

    /// A probe for this layout.
    #[must_use]
    pub fn probe(&self) -> ToUnicodeCharProbe {
        ToUnicodeCharProbe::for_layout(self.hkl)
    }
}

impl Drop for LoadedLayout {
    fn drop(&mut self) {
        if !self.already_loaded {
            // SAFETY: the handle came from LoadKeyboardLayoutW and was not loaded before. An
            // unload refused because the layout is in use is harmless, so the result is ignored.
            let _ = unsafe { UnloadKeyboardLayout(self.hkl) };
        }
    }
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
    use std::sync::Mutex;

    /// Layout loading is process-wide; keep the tests that load and unload apart.
    static LAYOUTS: Mutex<()> = Mutex::new(());

    const TURKISH_Q: &str = "0000041F";
    const TURKISH_F: &str = "0001041F";
    const VK_Q: u16 = 0x51;
    const VK_F: u16 = 0x46;
    const VK_I: u16 = 0x49;
    const VK_7: u16 = 0x37;
    const VK_F5: u16 = 0x74;
    const SCAN_Q_POSITION: u16 = 0x10;
    const SCAN_I_POSITION: u16 = 0x17;
    const SCAN_7: u16 = 0x08;

    fn active_layout() -> HKL {
        current_layout()
    }

    #[test]
    fn uia_keyboard_turkish_q_altgr_layer_types_text() {
        let _guard = LAYOUTS.lock().unwrap_or_else(|e| e.into_inner());
        let active_before = active_layout();
        let layout = LoadedLayout::load(TURKISH_Q).unwrap_or_else(|e| panic!("{e}"));
        let probe = layout.probe();
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
        // Function keys type nothing, even with AltGr.
        assert!(!probe.produces_char(VK_F5, 0x3F, false, false));
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
        // Probing never changes the user's active layout.
        assert_eq!(active_layout(), active_before);
        drop(layout);
        assert_eq!(active_layout(), active_before);
    }

    #[test]
    fn uia_keyboard_turkish_f_layout_differs_from_q() {
        let _guard = LAYOUTS.lock().unwrap_or_else(|e| e.into_inner());
        let active_before = active_layout();
        let f_layout = LoadedLayout::load(TURKISH_F).unwrap_or_else(|e| panic!("{e}"));
        let q_layout = LoadedLayout::load(TURKISH_Q).unwrap_or_else(|e| panic!("{e}"));
        assert_ne!(f_layout.hkl(), q_layout.hkl());
        // The key at the Q position types 'f' on the F layout and 'q' on the Q layout.
        let f = f_layout
            .probe()
            .char_for(VK_F, SCAN_Q_POSITION, false, false, false);
        let q = q_layout
            .probe()
            .char_for(VK_Q, SCAN_Q_POSITION, false, false, false);
        assert_eq!(f.as_deref(), Some("f"));
        assert_eq!(q.as_deref(), Some("q"));
        assert!(!f_layout.probe().produces_char(VK_F5, 0x3F, false, false));
        assert_eq!(active_layout(), active_before);
    }

    #[test]
    fn uia_keyboard_already_loaded_layout_is_kept() {
        let _guard = LAYOUTS.lock().unwrap_or_else(|e| e.into_inner());
        let first = LoadedLayout::load(TURKISH_Q).unwrap_or_else(|e| panic!("{e}"));
        let second = LoadedLayout::load(TURKISH_Q).unwrap_or_else(|e| panic!("{e}"));
        // The second load sees the first one's layout in the list, or the user already had it.
        assert_eq!(first.hkl(), second.hkl());
        assert!(second.was_already_loaded() || loaded_layouts().contains(&second.hkl()));
        drop(second);
        assert!(loaded_layouts().contains(&first.hkl()) || first.was_already_loaded());
        assert!(
            first
                .probe()
                .char_for(VK_Q, SCAN_Q_POSITION, false, false, true)
                .is_some()
        );
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
            let name = key_name(vk, scan).unwrap_or_else(|e| panic!("{e}"));
            assert!(!name.is_empty(), "vk {vk:#x}");
        }
        // Deriving the scan code from the virtual key gives the same name as passing it.
        assert_eq!(
            key_name(0x0D, 0).unwrap_or_else(|e| panic!("{e}")),
            key_name(0x0D, 0x1C).unwrap_or_else(|e| panic!("{e}"))
        );
        // A virtual key with no scan code has no name.
        let err = key_name(0xFF, 0).unwrap_err();
        assert!(matches!(err, KeyboardError::NoName { vk: 0xFF }));
        assert!(err.to_string().contains("0xff"));
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

    #[test]
    fn uia_keyboard_load_error_reports_klid() {
        let error = KeyboardError::Load {
            klid: "00000000".to_owned(),
            source: windows::core::Error::from(windows::Win32::Foundation::E_INVALIDARG),
        };
        let text = error.to_string();
        assert!(text.contains("00000000"));
        assert!(std::error::Error::source(&error).is_some());
    }
}
