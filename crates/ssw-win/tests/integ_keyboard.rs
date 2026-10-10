//! The user's own Turkish Q keyboard layout, probed (never loaded or activated) when the
//! session has it: the AltGr layer types text, so a Ctrl+Alt shortcut must not take those keys.
//! On a machine without the layout there is nothing to probe and the test has nothing to check.

use ssw_ui::shortcuts::CharProbe;
use ssw_win::keyboard::{ToUnicodeCharProbe, session_layouts};
use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;

/// The handle of Turkish Q (language 0x041F, layout 0x041F): `HKL` `0x041F041F`.
const TURKISH_Q_HKL: usize = 0x041F_041F;
const VK_Q: u16 = 0x51;
const VK_I: u16 = 0x49;
const VK_7: u16 = 0x37;
const VK_F5: u16 = 0x74;
const SCAN_Q_POSITION: u16 = 0x10;
const SCAN_I_POSITION: u16 = 0x17;
const SCAN_7: u16 = 0x08;

fn active_layout() -> usize {
    // SAFETY: GetKeyboardLayout has no preconditions; thread id 0 means the calling thread.
    unsafe { GetKeyboardLayout(0) }.0 as usize
}

#[test]
fn the_turkish_q_altgr_layer_types_text() {
    let active_before = active_layout();
    for hkl in session_layouts()
        .into_iter()
        .filter(|hkl| hkl.0 as usize == TURKISH_Q_HKL)
    {
        let probe = ToUnicodeCharProbe::for_layout(hkl);
        // Function keys type nothing, whatever the layout, even with AltGr.
        assert!(!probe.produces_char(VK_F5, 0x3F, false, false));
        // AltGr+Q is '@' and AltGr+7 is '{' on the Turkish Q layout.
        let altgr = |vk, scan| probe.char_for(vk, scan, false, false, true);
        assert_eq!(altgr(VK_Q, SCAN_Q_POSITION).as_deref(), Some("@"));
        assert_eq!(altgr(VK_7, SCAN_7).as_deref(), Some("{"));
        assert!(probe.produces_char(VK_Q, SCAN_Q_POSITION, false, false));
        assert!(probe.produces_char(VK_7, SCAN_7, false, false));
        // Without AltGr the same keys type their plain letters; Turkish I is dotless.
        let plain = |vk, scan, shift, caps| probe.char_for(vk, scan, shift, caps, false);
        assert_eq!(
            plain(VK_Q, SCAN_Q_POSITION, false, false).as_deref(),
            Some("q")
        );
        assert_eq!(
            plain(VK_I, SCAN_I_POSITION, false, false).as_deref(),
            Some("\u{131}")
        );
        assert_eq!(
            plain(VK_I, SCAN_I_POSITION, true, false).as_deref(),
            Some("I")
        );
        // Caps Lock upper-cases without Shift.
        assert_eq!(
            plain(VK_Q, SCAN_Q_POSITION, false, true).as_deref(),
            Some("Q")
        );
    }
    // Probing never changes the user's active layout.
    assert_eq!(active_layout(), active_before);
}
