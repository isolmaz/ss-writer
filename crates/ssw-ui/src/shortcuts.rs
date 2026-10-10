//! Keyboard shortcut routing: [`KeyEvent`] → [`CommandId`] through the [`Keymap`], the AltGr
//! rule of spec §11, and the menu keys (Alt alone, F10, Alt+mnemonic).
//!
//! The AltGr rule: on layouts that use AltGr the OS reports it as Ctrl+Alt. When Ctrl+Alt+key
//! produces a character in the active layout ([`CharProbe`], `ToUnicodeEx` with flag `0x4` in
//! `ssw-win`), the event is text and never a command, whatever the keymap says.

use ssw_core::command::CommandId;
use ssw_core::keymap::{KeyChord, Keymap};

use crate::input::{KeyEvent, vk};

/// Left and right Alt (`VK_LMENU`, `VK_RMENU`) as sent by `WM_SYSKEYUP`.
const LMENU: u16 = 0xA4;
const RMENU: u16 = 0xA5;

/// Asks the active keyboard layout whether a key combination produces a character.
pub trait CharProbe {
    /// True when pressing `vk` (hardware `scan`) with Ctrl+Alt (AltGr) and the given Shift and
    /// Caps Lock state produces a character. Must not change the keyboard state (dead keys).
    fn produces_char(&self, vk: u16, scan: u16, shift: bool, caps: bool) -> bool;
}

/// What the router decided for a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutOutcome {
    /// Run the command; the event is consumed.
    Command(CommandId),
    /// Ctrl+Alt produces a character in this layout: deliver it as text, never as a command.
    Text,
    /// Alt pressed and released alone, or F10: focus the menu bar.
    OpenMenu,
    /// Alt plus a letter or digit that no command uses: open the menu with this accelerator
    /// (uppercase ASCII).
    Mnemonic(char),
    /// Not a shortcut; route the event to the focused widget.
    Unhandled,
}

/// Maps key events to commands.
#[derive(Debug, Clone)]
pub struct ShortcutRouter {
    keymap: Keymap,
    /// Alt went down and nothing else happened since: releasing it opens the menu.
    alt_pending: bool,
}

impl ShortcutRouter {
    #[must_use]
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap,
            alt_pending: false,
        }
    }

    #[must_use]
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Replaces the keymap (after the user edited shortcuts).
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
    }

    /// The chord of a key event: its virtual key and held modifiers.
    #[must_use]
    pub fn chord_of(event: &KeyEvent) -> KeyChord {
        KeyChord {
            vk: event.vk,
            ctrl: event.mods.ctrl(),
            shift: event.mods.shift(),
            alt: event.mods.alt(),
            win: event.mods.win(),
        }
    }

    /// Routes one key event. `caps` is the Caps Lock state, which the layout needs to decide
    /// whether Ctrl+Alt+key produces a character.
    pub fn route(
        &mut self,
        event: &KeyEvent,
        probe: &dyn CharProbe,
        caps: bool,
    ) -> ShortcutOutcome {
        let is_alt = matches!(event.vk, vk::MENU | LMENU | RMENU);
        if !event.is_down {
            let open = is_alt && std::mem::take(&mut self.alt_pending);
            return if open {
                ShortcutOutcome::OpenMenu
            } else {
                ShortcutOutcome::Unhandled
            };
        }
        if is_alt {
            // Auto-repeat of a held Alt keeps the pending state; a first press starts it.
            self.alt_pending = true;
            return ShortcutOutcome::Unhandled;
        }
        self.alt_pending = false;
        if matches!(event.vk, vk::SHIFT | vk::CONTROL) {
            return ShortcutOutcome::Unhandled;
        }
        let chord = Self::chord_of(event);
        if chord.is_ctrl_alt() && probe.produces_char(event.vk, event.scan, chord.shift, caps) {
            return ShortcutOutcome::Text;
        }
        if chord == KeyChord::new(vk::F10) {
            return ShortcutOutcome::OpenMenu;
        }
        if let Some(command) = self.keymap.lookup(chord) {
            return ShortcutOutcome::Command(command);
        }
        if chord.alt
            && !chord.ctrl
            && !chord.shift
            && !chord.win
            && let Some(accelerator) = accelerator_char(event.vk)
        {
            return ShortcutOutcome::Mnemonic(accelerator);
        }
        ShortcutOutcome::Unhandled
    }
}

/// The uppercase ASCII letter or digit of a virtual key.
fn accelerator_char(key: u16) -> Option<char> {
    let byte = u8::try_from(key).ok()?;
    byte.is_ascii_uppercase()
        .then_some(char::from(byte))
        .or_else(|| byte.is_ascii_digit().then_some(char::from(byte)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Modifiers;
    use ssw_core::keymap::vk as key;

    /// The AltGr layer of the Turkish Q layout: these keys produce a character with Ctrl+Alt.
    struct TurkishQ;

    impl CharProbe for TurkishQ {
        fn produces_char(&self, vk: u16, _scan: u16, shift: bool, caps: bool) -> bool {
            match vk {
                // AltGr+Q @, AltGr+E €, AltGr+7 {, 8 [, 9 ], 0 }, AltGr+T ₺ , AltGr+W (none)
                key::Q | key::E | key::T | key::D7 | key::D8 | key::D9 | key::D0 => true,
                // AltGr+Shift+A produces Æ only with Caps/Shift state.
                key::A => shift || caps,
                _ => false,
            }
        }
    }

    /// A layout without an AltGr layer (US).
    struct UsLayout;

    impl CharProbe for UsLayout {
        fn produces_char(&self, _: u16, _: u16, _: bool, _: bool) -> bool {
            false
        }
    }

    fn event(chord: &str) -> KeyEvent {
        let chord: KeyChord = chord.parse().expect("a valid key chord");
        let mut mods = Modifiers::NONE;
        if chord.ctrl {
            mods |= Modifiers::CTRL;
        }
        if chord.shift {
            mods |= Modifiers::SHIFT;
        }
        if chord.alt {
            mods |= Modifiers::ALT;
        }
        if chord.win {
            mods |= Modifiers::WIN;
        }
        KeyEvent::press(chord.vk, mods)
    }

    fn router() -> ShortcutRouter {
        ShortcutRouter::new(Keymap::default())
    }

    /// Spec §11 table: (chord, command).
    fn spec_table() -> Vec<(&'static str, CommandId)> {
        use CommandId::*;
        vec![
            ("Ctrl+N", FileNew),
            ("Ctrl+Shift+N", FileNewWindow),
            ("Ctrl+O", FileOpen),
            ("Ctrl+Shift+O", FileOpenFolder),
            ("Ctrl+S", FileSave),
            ("Ctrl+Shift+S", FileSaveAs),
            ("Ctrl+P", FilePrint),
            ("Ctrl+W", TabClose),
            ("Ctrl+Shift+T", TabReopenClosed),
            ("Ctrl+T", TabNew),
            ("Ctrl+Tab", TabNext),
            ("Ctrl+PgDn", TabNext),
            ("Ctrl+Shift+Tab", TabPrevious),
            ("Ctrl+PgUp", TabPrevious),
            ("Ctrl+Z", EditUndo),
            ("Ctrl+Y", EditRedo),
            ("Ctrl+Shift+Z", EditRedo),
            ("Ctrl+Shift+V", EditPastePlain),
            ("Ctrl+F", SearchFind),
            ("Ctrl+H", SearchReplace),
            ("Ctrl+G", SearchGoToLine),
            ("F3", SearchFindNext),
            ("Shift+F3", SearchFindPrevious),
            ("Ctrl+E", SearchQuickOpen),
            ("Ctrl+Shift+P", SearchCommandPalette),
            ("Ctrl+Shift+F", SearchInFolder),
            ("Ctrl+B", FormatBold),
            ("Ctrl+I", FormatItalic),
            ("Ctrl+U", FormatUnderline),
            ("Ctrl+Shift+H", FormatHighlight),
            ("Ctrl+Shift+X", FormatStrikethrough),
            ("Ctrl+Shift+C", FormatInlineCode),
            ("Ctrl+Shift+K", FormatCodeBlock),
            ("Ctrl+K", FormatLink),
            ("Ctrl+.", FormatTaskToggle),
            ("Ctrl+1", FormatHeading1),
            ("Ctrl+2", FormatHeading2),
            ("Ctrl+3", FormatHeading3),
            ("Ctrl+4", FormatHeading4),
            ("Ctrl+5", FormatHeading5),
            ("Ctrl+6", FormatHeading6),
            ("Ctrl+Shift+E", ViewToggleSidebar),
            ("Ctrl+Shift+I", ViewToggleInspector),
            ("F9", ViewTypewriterMode),
            ("Shift+F9", ViewFocusMode),
            ("F11", ViewFullScreen),
            ("Shift+F11", ViewZenMode),
            ("F5", EditInsertDate),
            ("Ctrl+=", ViewZoomIn),
            ("Ctrl+-", ViewZoomOut),
            ("Ctrl+0", ViewZoomReset),
            ("Ctrl+L", ViewRecenterCaret),
            ("Ctrl+,", SettingsOpen),
            ("F6", ViewNextPane),
        ]
    }

    #[test]
    fn kit_shortcuts_every_spec_default_routes_to_its_command() {
        let mut router = router();
        for (chord, command) in spec_table() {
            assert_eq!(
                router.route(&event(chord), &UsLayout, false),
                ShortcutOutcome::Command(command),
                "{chord}"
            );
        }
    }

    #[test]
    fn kit_shortcuts_spec_table_covers_every_default_binding() {
        let router = router();
        let table = spec_table();
        assert_eq!(table.len(), router.keymap().bindings().len());
        for binding in router.keymap().bindings() {
            assert!(
                table
                    .iter()
                    .any(|(chord, command)| *command == binding.command
                        && chord.parse::<KeyChord>().ok() == Some(binding.chord)),
                "{} is not in the spec table",
                binding.chord
            );
        }
    }

    #[test]
    fn kit_shortcuts_altgr_character_is_text_not_command() {
        let mut router = router();
        // User bound Ctrl+Alt+E to a command; on Turkish Q it types the euro sign.
        let mut overrides = ssw_core::keymap::Overrides::new();
        overrides.insert(
            CommandId::SearchQuickOpen.as_str().to_owned(),
            vec![KeyChord::new(key::E).with_ctrl().with_alt()],
        );
        router.set_keymap(Keymap::with_overrides(&overrides));
        let altgr_e = event("Ctrl+Alt+E");
        assert_eq!(
            router.route(&altgr_e, &TurkishQ, false),
            ShortcutOutcome::Text
        );
        // The same chord is a command on a layout without AltGr text.
        assert_eq!(
            router.route(&altgr_e, &UsLayout, false),
            ShortcutOutcome::Command(CommandId::SearchQuickOpen)
        );
        for chord in [
            "Ctrl+Alt+Q",
            "Ctrl+Alt+7",
            "Ctrl+Alt+8",
            "Ctrl+Alt+9",
            "Ctrl+Alt+0",
            "Ctrl+Alt+T",
        ] {
            assert_eq!(
                router.route(&event(chord), &TurkishQ, false),
                ShortcutOutcome::Text,
                "{chord}"
            );
        }
    }

    #[test]
    fn kit_shortcuts_altgr_probe_receives_shift_and_caps() {
        let mut router = router();
        // On the Turkish layout Ctrl+Alt+A only produces a character with Shift or Caps Lock.
        assert_eq!(
            router.route(&event("Ctrl+Alt+A"), &TurkishQ, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            router.route(&event("Ctrl+Alt+A"), &TurkishQ, true),
            ShortcutOutcome::Text
        );
        assert_eq!(
            router.route(&event("Ctrl+Alt+Shift+A"), &TurkishQ, false),
            ShortcutOutcome::Text
        );
    }

    #[test]
    fn kit_shortcuts_ctrl_alt_without_text_falls_back_to_keymap() {
        let mut router = router();
        assert_eq!(
            router.route(&event("Ctrl+Alt+Z"), &TurkishQ, false),
            ShortcutOutcome::Unhandled
        );
        // The probe is only consulted for Ctrl+Alt; plain Ctrl+Q never becomes text.
        assert_eq!(
            router.route(&event("Ctrl+E"), &TurkishQ, false),
            ShortcutOutcome::Command(CommandId::SearchQuickOpen)
        );
    }

    #[test]
    fn kit_shortcuts_alt_alone_opens_menu_on_release() {
        let mut router = router();
        let alt_down = KeyEvent::press(vk::MENU, Modifiers::ALT);
        let alt_up = KeyEvent {
            is_down: false,
            ..alt_down
        };
        assert_eq!(
            router.route(&alt_down, &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            router.route(&alt_up, &UsLayout, false),
            ShortcutOutcome::OpenMenu
        );
        // The second release has no pending Alt.
        assert_eq!(
            router.route(&alt_up, &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        // Left and right Alt behave the same.
        for code in [LMENU, RMENU] {
            let down = KeyEvent::press(code, Modifiers::ALT);
            let up = KeyEvent {
                is_down: false,
                ..down
            };
            router.route(&down, &UsLayout, false);
            assert_eq!(
                router.route(&up, &UsLayout, false),
                ShortcutOutcome::OpenMenu
            );
        }
    }

    #[test]
    fn kit_shortcuts_alt_combined_with_a_key_does_not_open_menu() {
        let mut router = router();
        let alt_down = KeyEvent::press(vk::MENU, Modifiers::ALT);
        let alt_up = KeyEvent {
            is_down: false,
            ..alt_down
        };
        router.route(&alt_down, &UsLayout, false);
        // Alt+Tab-like: some other key was pressed while Alt was down.
        router.route(&KeyEvent::press(vk::TAB, Modifiers::ALT), &UsLayout, false);
        assert_eq!(
            router.route(&alt_up, &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        // A non-Alt key release never opens the menu.
        router.route(&alt_down, &UsLayout, false);
        let up = KeyEvent {
            is_down: false,
            ..KeyEvent::press(key::A, Modifiers::NONE)
        };
        assert_eq!(
            router.route(&up, &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        // Held Alt repeats keep the pending state.
        let repeat = KeyEvent {
            repeat: true,
            ..alt_down
        };
        router.route(&repeat, &UsLayout, false);
        assert_eq!(
            router.route(&alt_up, &UsLayout, false),
            ShortcutOutcome::OpenMenu
        );
    }

    #[test]
    fn kit_shortcuts_f10_and_mnemonics() {
        let mut router = router();
        assert_eq!(
            router.route(&event("F10"), &UsLayout, false),
            ShortcutOutcome::OpenMenu
        );
        // Shift+F10 is the context menu key, not the menu bar.
        assert_eq!(
            router.route(&event("Shift+F10"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            router.route(&event("Alt+F"), &UsLayout, false),
            ShortcutOutcome::Mnemonic('F')
        );
        assert_eq!(
            router.route(&event("Alt+5"), &UsLayout, false),
            ShortcutOutcome::Mnemonic('5')
        );
        // Alt plus a non-alphanumeric key, or with more modifiers, is no mnemonic.
        assert_eq!(
            router.route(&event("Alt+Left"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            router.route(&event("Ctrl+Alt+F"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            router.route(&event("Shift+Alt+F"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        // A command bound to Alt+X wins over the mnemonic.
        let mut overrides = ssw_core::keymap::Overrides::new();
        overrides.insert(
            CommandId::FormatBold.as_str().to_owned(),
            vec![KeyChord::new(key::X).with_alt()],
        );
        router.set_keymap(Keymap::with_overrides(&overrides));
        assert_eq!(
            router.route(&event("Alt+X"), &UsLayout, false),
            ShortcutOutcome::Command(CommandId::FormatBold)
        );
    }

    #[test]
    fn kit_shortcuts_modifier_keys_alone_and_unbound_keys_are_unhandled() {
        let mut router = router();
        for code in [vk::SHIFT, vk::CONTROL] {
            let down = KeyEvent::press(code, Modifiers::CTRL);
            assert_eq!(
                router.route(&down, &UsLayout, false),
                ShortcutOutcome::Unhandled
            );
        }
        assert_eq!(
            router.route(&event("Ctrl+J"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            router.route(&event("A"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        // Key releases of ordinary keys are never commands.
        let release = KeyEvent {
            is_down: false,
            ..event("Ctrl+S")
        };
        assert_eq!(
            router.route(&release, &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        // Windows-key chords are matched with the Win flag.
        assert_eq!(
            router.route(&event("Win+S"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        assert_eq!(
            ShortcutRouter::chord_of(&event("Ctrl+Shift+Alt+Win+P")).to_string(),
            "Ctrl+Shift+Alt+Win+P"
        );
    }

    #[test]
    fn kit_shortcuts_conflicting_chord_runs_nothing() {
        let mut overrides = ssw_core::keymap::Overrides::new();
        overrides.insert(
            CommandId::FormatBold.as_str().to_owned(),
            vec![KeyChord::new(key::I).with_ctrl()],
        );
        let mut router = ShortcutRouter::new(Keymap::with_overrides(&overrides));
        // Ctrl+I now belongs to Italic and Bold: ambiguous, so neither runs.
        assert_eq!(
            router.route(&event("Ctrl+I"), &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
    }

    #[test]
    fn kit_shortcuts_an_alt_key_code_beyond_a_byte_is_no_mnemonic() {
        let mut router = router();
        // 0x146 would read as 'F' if its high byte were dropped.
        let wide = KeyEvent::press(0x146, Modifiers::ALT);
        assert_eq!(
            router.route(&wide, &UsLayout, false),
            ShortcutOutcome::Unhandled
        );
        let letter = KeyEvent::press(key::F, Modifiers::ALT);
        assert_eq!(
            router.route(&letter, &UsLayout, false),
            ShortcutOutcome::Mnemonic('F')
        );
    }
}
