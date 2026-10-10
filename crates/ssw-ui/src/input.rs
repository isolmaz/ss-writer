//! Input events delivered to widgets, in platform-neutral form.
//!
//! `ssw-win` translates `WM_*`, TSF, pointer and DirectManipulation messages into these types.
//! Positions are DIPs in window coordinates.

use std::ops::{BitOr, BitOrAssign, Range};
use std::time::Duration;

use crate::geometry::Point;
use crate::services::TimerId;

/// Windows virtual-key codes the widgets react to.
pub mod vk {
    pub const BACK: u16 = 0x08;
    pub const TAB: u16 = 0x09;
    pub const RETURN: u16 = 0x0D;
    pub const SHIFT: u16 = 0x10;
    pub const CONTROL: u16 = 0x11;
    /// The Alt key.
    pub const MENU: u16 = 0x12;
    pub const ESCAPE: u16 = 0x1B;
    pub const SPACE: u16 = 0x20;
    pub const PAGE_UP: u16 = 0x21;
    pub const PAGE_DOWN: u16 = 0x22;
    pub const END: u16 = 0x23;
    pub const HOME: u16 = 0x24;
    pub const LEFT: u16 = 0x25;
    pub const UP: u16 = 0x26;
    pub const RIGHT: u16 = 0x27;
    pub const DOWN: u16 = 0x28;
    pub const INSERT: u16 = 0x2D;
    pub const DELETE: u16 = 0x2E;
    /// The context-menu key.
    pub const APPS: u16 = 0x5D;
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
}

/// A set of held modifier keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const SHIFT: Modifiers = Modifiers(1);
    pub const CTRL: Modifiers = Modifiers(2);
    pub const ALT: Modifiers = Modifiers(4);
    /// The Windows key.
    pub const WIN: Modifiers = Modifiers(8);

    /// True when every modifier in `other` is held.
    #[must_use]
    pub const fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    #[must_use]
    pub const fn shift(self) -> bool {
        self.contains(Modifiers::SHIFT)
    }

    #[must_use]
    pub const fn ctrl(self) -> bool {
        self.contains(Modifiers::CTRL)
    }

    #[must_use]
    pub const fn alt(self) -> bool {
        self.contains(Modifiers::ALT)
    }

    #[must_use]
    pub const fn win(self) -> bool {
        self.contains(Modifiers::WIN)
    }
}

impl BitOr for Modifiers {
    type Output = Modifiers;

    fn bitor(self, rhs: Modifiers) -> Modifiers {
        Modifiers(self.0 | rhs.0)
    }
}

impl BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Modifiers) {
        self.0 |= rhs.0;
    }
}

/// A key press or release (`WM_KEYDOWN` / `WM_KEYUP` / `WM_SYSKEY*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// Virtual-key code (see [`vk`]).
    pub vk: u16,
    /// Hardware scan code.
    pub scan: u16,
    pub mods: Modifiers,
    /// Auto-repeat of a held key.
    pub repeat: bool,
    pub is_down: bool,
}

impl KeyEvent {
    /// A first (non-repeat) key-down of `vk` with `mods`.
    #[must_use]
    pub const fn press(vk: u16, mods: Modifiers) -> Self {
        Self {
            vk,
            scan: 0,
            mods,
            repeat: false,
            is_down: true,
        }
    }
}

/// Committed text typed or pasted by the platform (`WM_CHAR`, TSF commit). Never contains
/// control characters produced by shortcuts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput(pub String);

/// How an IME composition segment is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImeAttrStyle {
    Input,
    TargetConverted,
    Converted,
    TargetNotConverted,
    InputError,
    FixedConverted,
}

/// A styled segment of the composition string; `range` is in UTF-8 bytes of the composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImeAttr {
    pub range: Range<usize>,
    pub style: ImeAttrStyle,
}

/// IME composition lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeEvent {
    /// A composition began.
    Start,
    /// The composition string changed.
    Update {
        /// The current composition string.
        text: String,
        /// Caret position inside `text`, in UTF-8 bytes.
        caret: usize,
        attrs: Vec<ImeAttr>,
    },
    /// The composition was committed as this text.
    Commit(String),
    /// The composition ended (after commit or cancel).
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerEventKind {
    Down,
    Up,
    Move,
    Enter,
    Leave,
}

/// A pointer button. A pen's barrel button reports as [`PointerButton::Right`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerType {
    Mouse,
    Touch,
    Pen,
    Touchpad,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    pub kind: PointerEventKind,
    /// The button that changed for `Down`/`Up`; `None` for `Move`/`Enter`/`Leave`.
    pub button: Option<PointerButton>,
    pub position: Point,
    pub pointer_type: PointerType,
    /// 1 for a single click, 2 for a double click, 3 for a triple click; 0 when not a press.
    pub click_count: u8,
    pub mods: Modifiers,
}

impl PointerEvent {
    /// A mouse event without modifiers and with `click_count` 0.
    #[must_use]
    pub const fn mouse(
        kind: PointerEventKind,
        button: Option<PointerButton>,
        position: Point,
    ) -> Self {
        Self {
            kind,
            button,
            position,
            pointer_type: PointerType::Mouse,
            click_count: 0,
            mods: Modifiers::NONE,
        }
    }
}

/// Wheel or touchpad scrolling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WheelEvent {
    /// Scroll amount. Positive `y` is wheel-up (content moves down); positive `x` is
    /// wheel-right. In DIPs when `precise`, otherwise in wheel notches (1.0 = one detent).
    pub delta: Point,
    /// Pixel-precise input (touchpad, touch, pen) rather than detented wheel notches.
    pub precise: bool,
    pub position: Point,
    pub mods: Modifiers,
}

/// Keyboard focus moved into or out of a widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusEvent {
    Gained,
    Lost,
}

/// Everything the host can deliver to a widget.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Key(KeyEvent),
    Text(TextInput),
    Ime(ImeEvent),
    Pointer(PointerEvent),
    Wheel(WheelEvent),
    Focus(FocusEvent),
    /// A timer set through `Requests::set_timer` fired.
    Timer(TimerId),
    /// A requested animation frame; carries the monotonic time of the frame.
    AnimationFrame(Duration),
}

impl Event {
    /// The pointer position of pointer and wheel events.
    #[must_use]
    pub fn position(&self) -> Option<Point> {
        match self {
            Event::Pointer(event) => Some(event.position),
            Event::Wheel(event) => Some(event.position),
            _ => None,
        }
    }
}

/// The mouse cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Cursor {
    #[default]
    Arrow,
    IBeam,
    Hand,
    Cross,
    SizeWE,
    SizeNS,
    SizeNWSE,
    SizeNESW,
    SizeAll,
    NotAllowed,
    Wait,
    Help,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_combine_and_query() {
        let both = Modifiers::CTRL | Modifiers::SHIFT;
        assert!(both.ctrl() && both.shift());
        assert!(!both.alt() && !both.win());
        assert!(both.contains(Modifiers::CTRL));
        assert!(both.contains(both));
        assert!(!Modifiers::CTRL.contains(both));
        assert!(Modifiers::NONE.is_empty());
        assert!(!both.is_empty());
        assert!(Modifiers::NONE.contains(Modifiers::NONE));

        let mut held = Modifiers::default();
        assert_eq!(held, Modifiers::NONE);
        held |= Modifiers::ALT;
        held |= Modifiers::WIN;
        assert!(held.alt() && held.win() && !held.ctrl());
    }

    #[test]
    fn key_press_is_a_fresh_key_down() {
        let key = KeyEvent::press(vk::RETURN, Modifiers::CTRL);
        assert_eq!(key.vk, 0x0D);
        assert!(key.is_down && !key.repeat);
        assert!(key.mods.ctrl());
        assert_eq!(key.scan, 0);
    }

    #[test]
    fn mouse_events_start_without_modifiers_or_clicks() {
        let event = PointerEvent::mouse(
            PointerEventKind::Down,
            Some(PointerButton::Left),
            Point::new(3.0, 4.0),
        );
        assert_eq!(event.pointer_type, PointerType::Mouse);
        assert_eq!(event.click_count, 0);
        assert!(event.mods.is_empty());
        assert_eq!(event.button, Some(PointerButton::Left));
        let double = PointerEvent {
            click_count: 2,
            ..event
        };
        assert_eq!(double.click_count, 2);
        assert_eq!(double.position, Point::new(3.0, 4.0));
    }

    #[test]
    fn only_pointer_and_wheel_events_have_positions() {
        let at = Point::new(10.0, 20.0);
        let pointer = Event::Pointer(PointerEvent::mouse(PointerEventKind::Move, None, at));
        assert_eq!(pointer.position(), Some(at));
        let wheel = Event::Wheel(WheelEvent {
            delta: Point::new(0.0, 1.0),
            precise: false,
            position: Point::new(1.0, 2.0),
            mods: Modifiers::CTRL,
        });
        assert_eq!(wheel.position(), Some(Point::new(1.0, 2.0)));
        assert_eq!(
            Event::Key(KeyEvent::press(vk::TAB, Modifiers::NONE)).position(),
            None
        );
        assert_eq!(Event::Text(TextInput("é".to_owned())).position(), None);
        assert_eq!(Event::Ime(ImeEvent::Start).position(), None);
        assert_eq!(Event::Focus(FocusEvent::Gained).position(), None);
        assert_eq!(Event::Timer(TimerId::new(1)).position(), None);
        assert_eq!(
            Event::AnimationFrame(Duration::from_millis(16)).position(),
            None
        );
    }

    #[test]
    fn ime_updates_carry_composition_text_and_attributes() {
        let update = ImeEvent::Update {
            text: "かな".to_owned(),
            caret: 3,
            attrs: vec![
                ImeAttr {
                    range: 0..3,
                    style: ImeAttrStyle::TargetConverted,
                },
                ImeAttr {
                    range: 3..6,
                    style: ImeAttrStyle::Input,
                },
            ],
        };
        assert!(matches!(
            &update,
            ImeEvent::Update { text, caret: 3, attrs }
                if text.len() == 6 && attrs.len() == 2 && attrs[1].range == (3..6)
        ));
        assert_ne!(update, ImeEvent::Commit("かな".to_owned()));
        assert_ne!(ImeEvent::Start, ImeEvent::End);
    }

    #[test]
    fn default_cursor_is_the_arrow() {
        assert_eq!(Cursor::default(), Cursor::Arrow);
    }
}
