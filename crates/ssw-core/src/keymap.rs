//! Keyboard model: key chords (virtual key + modifiers) and the command keymap.
//!
//! Shortcuts are stored as a Windows virtual-key code plus modifier flags (spec §11) and are
//! displayed by the shell with `GetKeyNameTextW`; the `Display` form here (`Ctrl+Shift+P`) is
//! the portable fallback used by tests, logs and documentation.
//!
//! The keymap binds exactly the default table of spec §11. Standard text-editing keys that the
//! editor surface handles itself (Ctrl+X/C/V/A, caret movement, Enter, Backspace…) are not
//! commands here. No default binding uses Ctrl+Alt (AltGr); [`KeyChord::is_ctrl_alt`] lets the
//! shell apply the `ToUnicodeEx` rule from §11 to user-assigned chords.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::CommandId;

/// Windows virtual-key codes (`VK_*`) used by SS Writer.
pub mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    /// Page Up (`VK_PRIOR`).
    pub const PRIOR: u16 = 0x21;
    /// Page Down (`VK_NEXT`).
    pub const NEXT: u16 = 0x22;
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const INSERT: u16 = 0x2D;
    pub const DELETE: u16 = 0x2E;
    pub const D0: u16 = 0x30;
    pub const D1: u16 = 0x31;
    pub const D2: u16 = 0x32;
    pub const D3: u16 = 0x33;
    pub const D4: u16 = 0x34;
    pub const D5: u16 = 0x35;
    pub const D6: u16 = 0x36;
    pub const D7: u16 = 0x37;
    pub const D8: u16 = 0x38;
    pub const D9: u16 = 0x39;
    pub const A: u16 = 0x41;
    pub const B: u16 = 0x42;
    pub const C: u16 = 0x43;
    pub const D: u16 = 0x44;
    pub const E: u16 = 0x45;
    pub const F: u16 = 0x46;
    pub const G: u16 = 0x47;
    pub const H: u16 = 0x48;
    pub const I: u16 = 0x49;
    pub const J: u16 = 0x4A;
    pub const K: u16 = 0x4B;
    pub const L: u16 = 0x4C;
    pub const M: u16 = 0x4D;
    pub const N: u16 = 0x4E;
    pub const O: u16 = 0x4F;
    pub const P: u16 = 0x50;
    pub const Q: u16 = 0x51;
    pub const R: u16 = 0x52;
    pub const S: u16 = 0x53;
    pub const T: u16 = 0x54;
    pub const U: u16 = 0x55;
    pub const V: u16 = 0x56;
    pub const W: u16 = 0x57;
    pub const X: u16 = 0x58;
    pub const Y: u16 = 0x59;
    pub const Z: u16 = 0x5A;
    pub const F1: u16 = 0x70;
    pub const F2: u16 = 0x71;
    pub const F3: u16 = 0x72;
    pub const F4: u16 = 0x73;
    pub const F5: u16 = 0x74;
    pub const F6: u16 = 0x75;
    pub const F7: u16 = 0x76;
    pub const F8: u16 = 0x77;
    pub const F9: u16 = 0x78;
    pub const F10: u16 = 0x79;
    pub const F11: u16 = 0x7A;
    pub const F12: u16 = 0x7B;
    pub const F13: u16 = 0x7C;
    pub const F14: u16 = 0x7D;
    pub const F15: u16 = 0x7E;
    pub const F16: u16 = 0x7F;
    pub const F17: u16 = 0x80;
    pub const F18: u16 = 0x81;
    pub const F19: u16 = 0x82;
    pub const F20: u16 = 0x83;
    pub const F21: u16 = 0x84;
    pub const F22: u16 = 0x85;
    pub const F23: u16 = 0x86;
    pub const F24: u16 = 0x87;
    /// `;:` key (`VK_OEM_1`, US layout).
    pub const OEM_SEMICOLON: u16 = 0xBA;
    /// `=+` key (`VK_OEM_PLUS`).
    pub const OEM_PLUS: u16 = 0xBB;
    /// `,<` key (`VK_OEM_COMMA`).
    pub const OEM_COMMA: u16 = 0xBC;
    /// `-_` key (`VK_OEM_MINUS`).
    pub const OEM_MINUS: u16 = 0xBD;
    /// `.>` key (`VK_OEM_PERIOD`).
    pub const OEM_PERIOD: u16 = 0xBE;
    /// `/?` key (`VK_OEM_2`, US layout).
    pub const OEM_SLASH: u16 = 0xBF;
    /// `` `~ `` key (`VK_OEM_3`, US layout).
    pub const OEM_BACKTICK: u16 = 0xC0;
    /// `[{` key (`VK_OEM_4`, US layout).
    pub const OEM_LBRACKET: u16 = 0xDB;
    /// `\|` key (`VK_OEM_5`, US layout).
    pub const OEM_BACKSLASH: u16 = 0xDC;
    /// `]}` key (`VK_OEM_6`, US layout).
    pub const OEM_RBRACKET: u16 = 0xDD;
    /// `'"` key (`VK_OEM_7`, US layout).
    pub const OEM_QUOTE: u16 = 0xDE;
}

/// A virtual key plus modifier flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KeyChord {
    /// Windows virtual-key code.
    pub vk: u16,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub win: bool,
}

impl KeyChord {
    /// A chord without modifiers.
    #[must_use]
    pub const fn new(vk: u16) -> Self {
        Self {
            vk,
            ctrl: false,
            shift: false,
            alt: false,
            win: false,
        }
    }

    #[must_use]
    pub const fn with_ctrl(mut self) -> Self {
        self.ctrl = true;
        self
    }

    #[must_use]
    pub const fn with_shift(mut self) -> Self {
        self.shift = true;
        self
    }

    #[must_use]
    pub const fn with_alt(mut self) -> Self {
        self.alt = true;
        self
    }

    #[must_use]
    pub const fn with_win(mut self) -> Self {
        self.win = true;
        self
    }

    /// Ctrl and Alt together: on layouts with AltGr this may produce text instead of a command.
    #[must_use]
    pub const fn is_ctrl_alt(self) -> bool {
        self.ctrl && self.alt
    }
}

/// Key names that are neither a letter, a digit nor an F-key. The first name is canonical.
const NAMED_KEYS: &[(u16, &[&str])] = &[
    (vk::BACK, &["Backspace"]),
    (vk::TAB, &["Tab"]),
    (vk::RETURN, &["Enter", "Return"]),
    (vk::ESCAPE, &["Esc", "Escape"]),
    (vk::SPACE, &["Space"]),
    (vk::PRIOR, &["PgUp", "PageUp"]),
    (vk::NEXT, &["PgDn", "PageDown"]),
    (vk::END, &["End"]),
    (vk::HOME, &["Home"]),
    (vk::LEFT, &["Left"]),
    (vk::UP, &["Up"]),
    (vk::RIGHT, &["Right"]),
    (vk::DOWN, &["Down"]),
    (vk::INSERT, &["Insert", "Ins"]),
    (vk::DELETE, &["Delete", "Del"]),
    (vk::OEM_SEMICOLON, &[";"]),
    (vk::OEM_PLUS, &["="]),
    (vk::OEM_COMMA, &[","]),
    (vk::OEM_MINUS, &["-"]),
    (vk::OEM_PERIOD, &["."]),
    (vk::OEM_SLASH, &["/"]),
    (vk::OEM_BACKTICK, &["`"]),
    (vk::OEM_LBRACKET, &["["]),
    (vk::OEM_BACKSLASH, &["\\"]),
    (vk::OEM_RBRACKET, &["]"]),
    (vk::OEM_QUOTE, &["'"]),
];

const FALLBACK_PREFIX: &str = "VK_0x";

fn is_letter(vk: u16) -> bool {
    (vk::A..=vk::Z).contains(&vk)
}

fn is_digit(vk: u16) -> bool {
    (vk::D0..=vk::D9).contains(&vk)
}

fn is_fkey(vk: u16) -> bool {
    (vk::F1..=vk::F24).contains(&vk)
}

fn write_key(f: &mut fmt::Formatter<'_>, key: u16) -> fmt::Result {
    if is_letter(key) || is_digit(key) {
        // Letters and digits: the VK code is the ASCII code.
        return fmt::Write::write_char(f, char::from(u8::try_from(key).unwrap_or(b'?')));
    }
    if is_fkey(key) {
        return write!(f, "F{}", key - vk::F1 + 1);
    }
    match NAMED_KEYS.iter().find(|(code, _)| *code == key) {
        Some((_, names)) => f.write_str(names[0]),
        None => write!(f, "{FALLBACK_PREFIX}{key:02X}"),
    }
}

impl fmt::Display for KeyChord {
    /// `Ctrl+Shift+Alt+Win+Key`, modifiers in that order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (on, name) in [
            (self.ctrl, "Ctrl"),
            (self.shift, "Shift"),
            (self.alt, "Alt"),
            (self.win, "Win"),
        ] {
            if on {
                f.write_str(name)?;
                f.write_str("+")?;
            }
        }
        write_key(f, self.vk)
    }
}

/// Error parsing a chord such as `"Ctrl+Shift+P"`.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("invalid key chord `{0}`")]
pub struct ChordParseError(pub String);

fn parse_key(token: &str) -> Option<u16> {
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(u16::from(c.to_ascii_uppercase() as u8));
        }
        if c.is_ascii_digit() {
            return Some(u16::from(c as u8));
        }
    }
    let lower = token.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix('f')
        && let Ok(n) = rest.parse::<u16>()
        && (1..=24).contains(&n)
    {
        return Some(vk::F1 + n - 1);
    }
    if let Some(hex) = lower.strip_prefix(&FALLBACK_PREFIX.to_ascii_lowercase())
        && let Ok(code) = u16::from_str_radix(hex, 16)
    {
        return Some(code);
    }
    NAMED_KEYS
        .iter()
        .find(|(_, names)| names.iter().any(|n| n.eq_ignore_ascii_case(token)))
        .map(|(code, _)| *code)
}

impl FromStr for KeyChord {
    type Err = ChordParseError;

    /// Parses the [`Display`](fmt::Display) form; modifier order and case are free.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ChordParseError(s.to_owned());
        let mut tokens = s.split('+').map(str::trim);
        let key_token = tokens.next_back().ok_or_else(err)?;
        let mut chord = KeyChord::new(parse_key(key_token).ok_or_else(err)?);
        for token in tokens {
            let flag =
                if token.eq_ignore_ascii_case("ctrl") || token.eq_ignore_ascii_case("control") {
                    &mut chord.ctrl
                } else if token.eq_ignore_ascii_case("shift") {
                    &mut chord.shift
                } else if token.eq_ignore_ascii_case("alt") {
                    &mut chord.alt
                } else if token.eq_ignore_ascii_case("win") {
                    &mut chord.win
                } else {
                    return Err(err());
                };
            if *flag {
                return Err(err());
            }
            *flag = true;
        }
        Ok(chord)
    }
}

const fn ctrl(key: u16) -> KeyChord {
    KeyChord::new(key).with_ctrl()
}

const fn ctrl_shift(key: u16) -> KeyChord {
    KeyChord::new(key).with_ctrl().with_shift()
}

const fn plain(key: u16) -> KeyChord {
    KeyChord::new(key)
}

const fn shift(key: u16) -> KeyChord {
    KeyChord::new(key).with_shift()
}

/// The default bindings of spec §11, in table order. Commands may appear more than once.
const DEFAULTS: &[(CommandId, KeyChord)] = &[
    (CommandId::FileNew, ctrl(vk::N)),
    (CommandId::FileNewWindow, ctrl_shift(vk::N)),
    (CommandId::FileOpen, ctrl(vk::O)),
    (CommandId::FileOpenFolder, ctrl_shift(vk::O)),
    (CommandId::FileSave, ctrl(vk::S)),
    (CommandId::FileSaveAs, ctrl_shift(vk::S)),
    (CommandId::FilePrint, ctrl(vk::P)),
    (CommandId::TabClose, ctrl(vk::W)),
    (CommandId::TabReopenClosed, ctrl_shift(vk::T)),
    (CommandId::TabNew, ctrl(vk::T)),
    (CommandId::TabNext, ctrl(vk::TAB)),
    (CommandId::TabNext, ctrl(vk::NEXT)),
    (CommandId::TabPrevious, ctrl_shift(vk::TAB)),
    (CommandId::TabPrevious, ctrl(vk::PRIOR)),
    (CommandId::EditUndo, ctrl(vk::Z)),
    (CommandId::EditRedo, ctrl(vk::Y)),
    (CommandId::EditRedo, ctrl_shift(vk::Z)),
    (CommandId::EditPastePlain, ctrl_shift(vk::V)),
    (CommandId::SearchFind, ctrl(vk::F)),
    (CommandId::SearchReplace, ctrl(vk::H)),
    (CommandId::SearchGoToLine, ctrl(vk::G)),
    (CommandId::SearchFindNext, plain(vk::F3)),
    (CommandId::SearchFindPrevious, shift(vk::F3)),
    (CommandId::SearchQuickOpen, ctrl(vk::E)),
    (CommandId::SearchCommandPalette, ctrl_shift(vk::P)),
    (CommandId::SearchInFolder, ctrl_shift(vk::F)),
    (CommandId::FormatBold, ctrl(vk::B)),
    (CommandId::FormatItalic, ctrl(vk::I)),
    (CommandId::FormatUnderline, ctrl(vk::U)),
    (CommandId::FormatHighlight, ctrl_shift(vk::H)),
    (CommandId::FormatStrikethrough, ctrl_shift(vk::X)),
    (CommandId::FormatInlineCode, ctrl_shift(vk::C)),
    (CommandId::FormatCodeBlock, ctrl_shift(vk::K)),
    (CommandId::FormatLink, ctrl(vk::K)),
    (CommandId::FormatTaskToggle, ctrl(vk::OEM_PERIOD)),
    (CommandId::FormatHeading1, ctrl(vk::D1)),
    (CommandId::FormatHeading2, ctrl(vk::D2)),
    (CommandId::FormatHeading3, ctrl(vk::D3)),
    (CommandId::FormatHeading4, ctrl(vk::D4)),
    (CommandId::FormatHeading5, ctrl(vk::D5)),
    (CommandId::FormatHeading6, ctrl(vk::D6)),
    (CommandId::ViewToggleSidebar, ctrl_shift(vk::E)),
    (CommandId::ViewToggleInspector, ctrl_shift(vk::I)),
    (CommandId::ViewTypewriterMode, plain(vk::F9)),
    (CommandId::ViewFocusMode, shift(vk::F9)),
    (CommandId::ViewFullScreen, plain(vk::F11)),
    (CommandId::ViewZenMode, shift(vk::F11)),
    (CommandId::EditInsertDate, plain(vk::F5)),
    (CommandId::ViewZoomIn, ctrl(vk::OEM_PLUS)),
    (CommandId::ViewZoomOut, ctrl(vk::OEM_MINUS)),
    (CommandId::ViewZoomReset, ctrl(vk::D0)),
    (CommandId::ViewRecenterCaret, ctrl(vk::L)),
    (CommandId::SettingsOpen, ctrl(vk::OEM_COMMA)),
    (CommandId::ViewNextPane, plain(vk::F6)),
];

/// User shortcut overrides as stored in `settings.json`: command id string → its chords
/// (an empty list unbinds the command). Keys are strings so ids written by a newer version
/// survive a load/save round trip.
pub type Overrides = BTreeMap<String, Vec<KeyChord>>;

/// One chord → command binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub chord: KeyChord,
    pub command: CommandId,
}

/// A chord bound to more than one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub chord: KeyChord,
    /// The competing commands, in command order.
    pub commands: Vec<CommandId>,
}

/// The effective keymap: defaults plus user overrides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Keymap {
    bindings: Vec<Binding>,
    by_chord: HashMap<KeyChord, Vec<CommandId>>,
}

impl Default for Keymap {
    /// The spec §11 defaults.
    fn default() -> Self {
        Self::from_bindings(
            DEFAULTS
                .iter()
                .map(|&(command, chord)| Binding { chord, command }),
        )
    }
}

impl Keymap {
    fn from_bindings(bindings: impl IntoIterator<Item = Binding>) -> Self {
        let mut seen = HashSet::new();
        let bindings: Vec<Binding> = bindings
            .into_iter()
            .filter(|b| seen.insert((b.command, b.chord)))
            .collect();
        let mut by_chord: HashMap<KeyChord, Vec<CommandId>> = HashMap::new();
        for b in &bindings {
            by_chord.entry(b.chord).or_default().push(b.command);
        }
        for commands in by_chord.values_mut() {
            commands.sort_unstable();
        }
        Self { bindings, by_chord }
    }

    /// The default bindings of one command (empty when it has none).
    pub fn default_chords(command: CommandId) -> impl Iterator<Item = KeyChord> {
        DEFAULTS
            .iter()
            .filter(move |(c, _)| *c == command)
            .map(|(_, chord)| *chord)
    }

    /// Defaults with `overrides` applied: an overridden command loses its default chords and
    /// gets exactly the listed ones. Ids that name no known command are ignored.
    #[must_use]
    pub fn with_overrides(overrides: &Overrides) -> Self {
        let overridden: HashMap<CommandId, &Vec<KeyChord>> = overrides
            .iter()
            .filter_map(|(id, chords)| id.parse::<CommandId>().ok().map(|c| (c, chords)))
            .collect();
        let kept = DEFAULTS
            .iter()
            .filter(|(command, _)| !overridden.contains_key(command))
            .map(|&(command, chord)| Binding { chord, command });
        let custom = CommandId::all().flat_map(|command| {
            overridden
                .get(&command)
                .into_iter()
                .flat_map(|chords| chords.iter())
                .map(move |&chord| Binding { chord, command })
        });
        Self::from_bindings(kept.chain(custom))
    }

    /// The overrides that turn the defaults into this keymap (empty for the defaults).
    #[must_use]
    pub fn to_overrides(&self) -> Overrides {
        let mut out = Overrides::new();
        for command in CommandId::all() {
            let mut current: Vec<KeyChord> = self.chords_for(command).collect();
            let mut default: Vec<KeyChord> = Self::default_chords(command).collect();
            current.sort_unstable();
            default.sort_unstable();
            if current != default {
                out.insert(
                    command.as_str().to_owned(),
                    self.chords_for(command).collect(),
                );
            }
        }
        out
    }

    /// All bindings, in command order then definition order.
    #[must_use]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// The command a chord triggers. `None` when the chord is unbound **or ambiguous**
    /// (bound to several commands, see [`Keymap::conflicts`]): a conflicting chord must not
    /// run an arbitrary one of them.
    #[must_use]
    pub fn lookup(&self, chord: KeyChord) -> Option<CommandId> {
        match self.by_chord.get(&chord)?.as_slice() {
            [only] => Some(*only),
            _ => None,
        }
    }

    /// Every command bound to `chord` (more than one means a conflict).
    #[must_use]
    pub fn commands_for(&self, chord: KeyChord) -> &[CommandId] {
        self.by_chord.get(&chord).map_or(&[], Vec::as_slice)
    }

    /// The chords bound to `command`, in definition order.
    pub fn chords_for(&self, command: CommandId) -> impl Iterator<Item = KeyChord> + '_ {
        self.bindings
            .iter()
            .filter(move |b| b.command == command)
            .map(|b| b.chord)
    }

    /// Chords bound to more than one command, sorted by chord.
    #[must_use]
    pub fn conflicts(&self) -> Vec<Conflict> {
        let mut out: Vec<Conflict> = self
            .by_chord
            .iter()
            .filter(|(_, commands)| commands.len() > 1)
            .map(|(chord, commands)| Conflict {
                chord: *chord,
                commands: commands.clone(),
            })
            .collect();
        out.sort_unstable_by_key(|c| c.chord);
        out
    }

    /// Commands other than `command` that already use `chord` — what a rebinding of `command`
    /// to `chord` would conflict with.
    #[must_use]
    pub fn conflicts_with(&self, command: CommandId, chord: KeyChord) -> Vec<CommandId> {
        self.commands_for(chord)
            .iter()
            .copied()
            .filter(|c| *c != command)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(s: &str) -> KeyChord {
        s.parse().unwrap_or_else(|e| panic!("{e}"))
    }

    /// Spec §11 table, one row per command: (command, default chords).
    fn spec_table() -> Vec<(CommandId, Vec<&'static str>)> {
        use CommandId::*;
        vec![
            (FileNew, vec!["Ctrl+N"]),
            (FileNewWindow, vec!["Ctrl+Shift+N"]),
            (FileOpen, vec!["Ctrl+O"]),
            (FileOpenFolder, vec!["Ctrl+Shift+O"]),
            (FileSave, vec!["Ctrl+S"]),
            (FileSaveAs, vec!["Ctrl+Shift+S"]),
            (FilePrint, vec!["Ctrl+P"]),
            (TabClose, vec!["Ctrl+W"]),
            (TabReopenClosed, vec!["Ctrl+Shift+T"]),
            (TabNew, vec!["Ctrl+T"]),
            (TabNext, vec!["Ctrl+Tab", "Ctrl+PgDn"]),
            (TabPrevious, vec!["Ctrl+Shift+Tab", "Ctrl+PgUp"]),
            (EditUndo, vec!["Ctrl+Z"]),
            (EditRedo, vec!["Ctrl+Y", "Ctrl+Shift+Z"]),
            (EditPastePlain, vec!["Ctrl+Shift+V"]),
            (SearchFind, vec!["Ctrl+F"]),
            (SearchReplace, vec!["Ctrl+H"]),
            (SearchGoToLine, vec!["Ctrl+G"]),
            (SearchFindNext, vec!["F3"]),
            (SearchFindPrevious, vec!["Shift+F3"]),
            (SearchQuickOpen, vec!["Ctrl+E"]),
            (SearchCommandPalette, vec!["Ctrl+Shift+P"]),
            (SearchInFolder, vec!["Ctrl+Shift+F"]),
            (FormatBold, vec!["Ctrl+B"]),
            (FormatItalic, vec!["Ctrl+I"]),
            (FormatUnderline, vec!["Ctrl+U"]),
            (FormatHighlight, vec!["Ctrl+Shift+H"]),
            (FormatStrikethrough, vec!["Ctrl+Shift+X"]),
            (FormatInlineCode, vec!["Ctrl+Shift+C"]),
            (FormatCodeBlock, vec!["Ctrl+Shift+K"]),
            (FormatLink, vec!["Ctrl+K"]),
            (FormatTaskToggle, vec!["Ctrl+."]),
            (FormatHeading1, vec!["Ctrl+1"]),
            (FormatHeading2, vec!["Ctrl+2"]),
            (FormatHeading3, vec!["Ctrl+3"]),
            (FormatHeading4, vec!["Ctrl+4"]),
            (FormatHeading5, vec!["Ctrl+5"]),
            (FormatHeading6, vec!["Ctrl+6"]),
            (ViewToggleSidebar, vec!["Ctrl+Shift+E"]),
            (ViewToggleInspector, vec!["Ctrl+Shift+I"]),
            (ViewTypewriterMode, vec!["F9"]),
            (ViewFocusMode, vec!["Shift+F9"]),
            (ViewFullScreen, vec!["F11"]),
            (ViewZenMode, vec!["Shift+F11"]),
            (EditInsertDate, vec!["F5"]),
            (ViewZoomIn, vec!["Ctrl+="]),
            (ViewZoomOut, vec!["Ctrl+-"]),
            (ViewZoomReset, vec!["Ctrl+0"]),
            (ViewRecenterCaret, vec!["Ctrl+L"]),
            (SettingsOpen, vec!["Ctrl+,"]),
            (ViewNextPane, vec!["F6"]),
        ]
    }

    #[test]
    fn defaults_match_spec_table() {
        let keymap = Keymap::default();
        let table = spec_table();
        for (command, chords) in &table {
            let mut want: Vec<KeyChord> = chords.iter().map(|c| chord(c)).collect();
            let mut got: Vec<KeyChord> = keymap.chords_for(*command).collect();
            want.sort_unstable();
            got.sort_unstable();
            assert_eq!(got, want, "{command}");
            for c in want {
                assert_eq!(keymap.lookup(c), Some(*command), "{c}");
            }
        }
        let total: usize = table.iter().map(|(_, chords)| chords.len()).sum();
        assert_eq!(
            keymap.bindings().len(),
            total,
            "default keymap has extra bindings"
        );
    }

    #[test]
    fn raw_virtual_keys_of_oem_defaults() {
        let keymap = Keymap::default();
        let zoom_in = KeyChord {
            vk: 0xBB,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        let zoom_out = KeyChord {
            vk: 0xBD,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        let task = KeyChord {
            vk: 0xBE,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        let settings = KeyChord {
            vk: 0xBC,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        assert_eq!(keymap.lookup(zoom_in), Some(CommandId::ViewZoomIn));
        assert_eq!(keymap.lookup(zoom_out), Some(CommandId::ViewZoomOut));
        assert_eq!(keymap.lookup(task), Some(CommandId::FormatTaskToggle));
        assert_eq!(keymap.lookup(settings), Some(CommandId::SettingsOpen));
        let pgdn = KeyChord {
            vk: 0x22,
            ctrl: true,
            shift: false,
            alt: false,
            win: false,
        };
        assert_eq!(keymap.lookup(pgdn), Some(CommandId::TabNext));
    }

    #[test]
    fn no_default_uses_ctrl_alt() {
        for b in Keymap::default().bindings() {
            assert!(!b.chord.is_ctrl_alt(), "{} uses Ctrl+Alt", b.command);
        }
        assert!(chord("Ctrl+Alt+Q").is_ctrl_alt());
    }

    #[test]
    fn defaults_have_no_conflicts() {
        assert!(Keymap::default().conflicts().is_empty());
    }

    #[test]
    fn chord_text_round_trips() {
        for b in Keymap::default().bindings() {
            let text = b.chord.to_string();
            assert_eq!(text.parse::<KeyChord>(), Ok(b.chord), "{text}");
        }
        assert_eq!(chord("shift+ctrl+p"), chord("Ctrl+Shift+P"));
        assert_eq!(
            chord("Ctrl+Alt+Win+Shift+F12").to_string(),
            "Ctrl+Shift+Alt+Win+F12"
        );
        let odd = KeyChord::new(0xFA);
        assert_eq!(odd.to_string().parse::<KeyChord>(), Ok(odd));
        for bad in ["", "Ctrl+", "Ctrl+Ctrl+A", "Hyper+A", "Ctrl+F25", "Ctrl+AB"] {
            assert!(bad.parse::<KeyChord>().is_err(), "{bad}");
        }
    }

    #[test]
    fn overrides_replace_defaults_and_conflicts_are_detected() {
        let mut overrides = Overrides::new();
        overrides.insert("format.bold".into(), vec![chord("Ctrl+Shift+B")]);
        overrides.insert("tab.next".into(), vec![]);
        overrides.insert("format.italic".into(), vec![chord("Ctrl+B")]);
        overrides.insert("from.the_future".into(), vec![chord("F2")]);
        let keymap = Keymap::with_overrides(&overrides);

        assert_eq!(
            keymap.chords_for(CommandId::FormatBold).collect::<Vec<_>>(),
            [chord("Ctrl+Shift+B")]
        );
        assert_eq!(keymap.chords_for(CommandId::TabNext).count(), 0);
        assert_eq!(keymap.lookup(chord("Ctrl+Tab")), None);
        assert_eq!(
            keymap.lookup(chord("F2")),
            None,
            "unknown command id is ignored"
        );

        // Italic now collides with nothing (bold moved away), so Ctrl+B is italic.
        assert_eq!(
            keymap.lookup(chord("Ctrl+B")),
            Some(CommandId::FormatItalic)
        );
        assert!(keymap.conflicts().is_empty());

        // Bind underline to Ctrl+B as well: a conflict, and the chord becomes ambiguous.
        overrides.insert("format.underline".into(), vec![chord("Ctrl+B")]);
        let keymap = Keymap::with_overrides(&overrides);
        assert_eq!(
            keymap.conflicts(),
            vec![Conflict {
                chord: chord("Ctrl+B"),
                commands: vec![CommandId::FormatItalic, CommandId::FormatUnderline],
            }]
        );
        assert_eq!(keymap.lookup(chord("Ctrl+B")), None);
        assert_eq!(
            keymap.conflicts_with(CommandId::FormatBold, chord("Ctrl+B")),
            vec![CommandId::FormatItalic, CommandId::FormatUnderline]
        );
        assert!(
            Keymap::default()
                .conflicts_with(CommandId::FormatBold, chord("Ctrl+B"))
                .is_empty()
        );
        assert_eq!(
            Keymap::default().conflicts_with(CommandId::FormatItalic, chord("Ctrl+B")),
            vec![CommandId::FormatBold]
        );
    }

    #[test]
    fn overrides_round_trip_through_keymap_and_json() {
        assert!(Keymap::default().to_overrides().is_empty());
        assert_eq!(Keymap::with_overrides(&Overrides::new()), Keymap::default());

        let mut overrides = Overrides::new();
        overrides.insert("view.fold_all".into(), vec![chord("Ctrl+Shift+Alt+F")]);
        overrides.insert("edit.redo".into(), vec![chord("Ctrl+Shift+Z")]);
        overrides.insert("file.print".into(), vec![]);
        let keymap = Keymap::with_overrides(&overrides);
        assert_eq!(keymap.to_overrides(), overrides);

        let json = serde_json::to_string(&overrides).expect("serialize");
        let back: Overrides = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, overrides);
        assert_eq!(Keymap::with_overrides(&back), keymap);
    }

    #[test]
    fn chord_serde_is_vk_plus_modifiers() {
        let json = serde_json::to_string(&chord("Ctrl+Shift+P")).expect("serialize");
        assert_eq!(
            json,
            r#"{"vk":80,"ctrl":true,"shift":true,"alt":false,"win":false}"#
        );
        let parsed: KeyChord =
            serde_json::from_str(r#"{"vk":117}"#).expect("modifiers default to off");
        assert_eq!(parsed, chord("F6"));
    }
}
