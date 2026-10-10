//! The window procedure side of one shell window: [`WindowGlue`] implements
//! [`ssw_win::window::WindowHandler`] over a [`WindowCell`].
//!
//! The handler runs inside the window procedure, so it never runs commands, shows menus or
//! dialogs and never touches the native window: it feeds [`ssw_ui::input::Event`]s to the
//! [`super::shell_view::ShellView`], paints it, answers hit tests and pushes
//! [`Request`]s (commands, widget actions, close requests) for the owner of the message loop
//! ([`super::host::UiApp`]) to carry out. A view that is borrowed while a message arrives (a
//! command's modal dialog pumps messages) skips the paint or drops the input instead of
//! panicking inside the operating system's callback.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use ssw_io::net::Http;
use ssw_ui::canvas::Canvas;
use ssw_ui::geometry::Point;
use ssw_ui::input::{Event, KeyEvent};
use ssw_ui::shell::layout::{CaptionButton, ChromeHit, ResizeEdge};
use ssw_ui::shell::titlebar::{MenuKind, TitleEvent};
use ssw_ui::shortcuts::ShortcutOutcome;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::keyboard::{ToUnicodeCharProbe, caps_lock_on};
use ssw_win::menus::{MenuTheme, apply_menu_theme};
use ssw_win::taskbar::TaskbarFactory;
use ssw_win::theme::SystemTheme;
use ssw_win::window::ResizeEdge as WinEdge;
use ssw_win::window::{HitRegion, WindowHandler, WindowInfo};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::WM_MOVE;

use super::controller::{Controller, Request, WindowCell};
use super::platform::Platform;
use super::shell_view::ShellAction;

/// What every window of the process shares.
pub struct WindowEnv {
    /// Windows, tabs and commands.
    pub controller: Rc<Controller>,
    /// The text layout engine the views lay out and hit test with.
    pub text: DWriteEngine,
    /// The layout's character probe for the AltGr rule (spec §11).
    pub probe: ToUnicodeCharProbe,
    /// The backdrop is transparent (Mica): off when the system turned transparency off.
    pub mica: Cell<bool>,
    /// The network seam of the remote images (real WinHTTP, scripted in tests).
    pub http: Arc<dyn Http>,
    /// Makes the taskbar object of a window (the real taskbar, a recorder in tests).
    pub taskbar: Rc<dyn TaskbarFactory>,
}

impl std::fmt::Debug for WindowEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowEnv")
            .field("mica", &self.mica.get())
            .finish_non_exhaustive()
    }
}

/// Facts the window procedure side leaves for the loop owner.
#[derive(Debug, Default)]
pub struct WindowLink {
    /// The window moved or changed size: its stored placement is stale.
    pub placement_dirty: Cell<bool>,
    /// The native window was destroyed.
    pub destroyed: Cell<bool>,
}

/// The [`WindowHandler`] of one window.
pub struct WindowGlue {
    env: Rc<WindowEnv>,
    cell: Rc<WindowCell>,
    link: Rc<WindowLink>,
    platform: Rc<Platform>,
}

impl std::fmt::Debug for WindowGlue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowGlue")
            .field("window", &self.cell.id)
            .finish_non_exhaustive()
    }
}

impl WindowGlue {
    /// The handler of the window `cell`.
    #[must_use]
    pub fn new(
        env: Rc<WindowEnv>,
        cell: Rc<WindowCell>,
        link: Rc<WindowLink>,
        platform: Rc<Platform>,
    ) -> Self {
        Self {
            env,
            cell,
            link,
            platform,
        }
    }

    /// Where the popup of `menu` opens: the bottom-left corner of its button.
    fn menu_anchor(&self, menu: MenuKind) -> Option<Point> {
        let view = self.cell.view.try_borrow().ok()?;
        let rect = view.titlebar().menu_rect(menu);
        Some(Point::new(rect.x, rect.bottom()))
    }

    /// Asks the loop owner to open `menu`.
    fn request_menu(&self, menu: MenuKind) -> bool {
        let Some(anchor) = self.menu_anchor(menu) else {
            return false;
        };
        self.env.controller.push(Request::Action(
            self.cell.id,
            ShellAction::Title(TitleEvent::MenuRequested { menu, anchor }),
        ));
        true
    }

    /// The menu whose localized accelerator letter is `letter`.
    fn menu_for_mnemonic(&self, letter: char) -> Option<MenuKind> {
        let i18n = self.env.controller.i18n();
        let key = |id: &str| i18n.text(id).chars().next().map(|c| c.to_ascii_uppercase());
        [
            ("window-menu-key-file", MenuKind::File),
            ("window-menu-key-edit", MenuKind::Edit),
            ("window-menu-key-format", MenuKind::Format),
            ("window-menu-key-view", MenuKind::View),
            ("help-menu-key", MenuKind::Help),
        ]
        .into_iter()
        .find(|(id, _)| key(id) == Some(letter))
        .map(|(_, menu)| menu)
    }

    /// Routes a key through the shortcut table. Returns whether the key was consumed.
    fn route_key(&self, key: &KeyEvent) -> bool {
        let outcome = self
            .env
            .controller
            .route_key(key, &self.env.probe, caps_lock_on());
        match outcome {
            ShortcutOutcome::Command(command) => {
                self.env
                    .controller
                    .push(Request::Command(self.cell.id, command));
                true
            }
            // Ctrl+Alt types a character here: it arrives as WM_CHAR, the key itself is not
            // a shortcut and not an editor key either.
            ShortcutOutcome::Text => true,
            ShortcutOutcome::OpenMenu => self.request_menu(MenuKind::File),
            ShortcutOutcome::Mnemonic(letter) => self
                .menu_for_mnemonic(letter)
                .is_some_and(|menu| self.request_menu(menu)),
            ShortcutOutcome::Unhandled => false,
        }
    }

    /// Delivers `event` to the view and queues what the user asked for.
    fn deliver(&self, event: &Event) {
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        let actions = view.event(event, &self.env.text);
        drop(view);
        for action in actions {
            self.env
                .controller
                .push(Request::Action(self.cell.id, action));
        }
    }
}

fn win_edge(edge: ResizeEdge) -> WinEdge {
    match edge {
        ResizeEdge::Left => WinEdge::Left,
        ResizeEdge::Right => WinEdge::Right,
        ResizeEdge::Top => WinEdge::Top,
        ResizeEdge::Bottom => WinEdge::Bottom,
        ResizeEdge::TopLeft => WinEdge::TopLeft,
        ResizeEdge::TopRight => WinEdge::TopRight,
        ResizeEdge::BottomLeft => WinEdge::BottomLeft,
        ResizeEdge::BottomRight => WinEdge::BottomRight,
    }
}

/// The `WM_NCHITTEST` region of a layout hit.
#[must_use]
pub fn hit_region(hit: ChromeHit) -> HitRegion {
    match hit {
        ChromeHit::Client => HitRegion::Client,
        ChromeHit::Caption => HitRegion::Caption,
        ChromeHit::CaptionButton(CaptionButton::Minimize) => HitRegion::Minimize,
        // HTMAXBUTTON: Snap Layouts appear over the maximize button.
        ChromeHit::CaptionButton(CaptionButton::Maximize) => HitRegion::Maximize,
        ChromeHit::CaptionButton(CaptionButton::Close) => HitRegion::Close,
        ChromeHit::Resize(edge) => HitRegion::Resize(win_edge(edge)),
    }
}

impl WindowHandler for WindowGlue {
    fn hit_test(&self, point: Point, _info: &WindowInfo) -> HitRegion {
        match self.cell.view.try_borrow() {
            Ok(view) => hit_region(view.chrome_hit(point)),
            Err(_) => HitRegion::Client,
        }
    }

    fn on_event(&mut self, _info: &WindowInfo, event: Event) {
        if let Event::Key(key) = &event
            && self.route_key(key)
        {
            return;
        }
        match &event {
            // The platform animates the editor's wheel scrolling.
            Event::Wheel(wheel) => self.platform.wheel(wheel, || self.deliver(&event)),
            // A touch that DirectManipulation scrolls with does not select text as well.
            Event::Pointer(pointer) if self.platform.is_scrolling_contact(pointer) => {}
            _ => self.deliver(&event),
        }
    }

    fn on_paint(&mut self, _info: &WindowInfo, canvas: &mut dyn Canvas) {
        let Ok(mut view) = self.cell.view.try_borrow_mut() else {
            return;
        };
        view.paint(
            canvas,
            &self.env.text,
            self.env.mica.get() && self.env.controller.mica(),
        );
        // Painted: what the widgets asked to repaint meanwhile is drawn now.
        view.requests_mut().take_invalidation();
        // The editor of the active tab was part of this frame (a document is on screen).
        ssw_win::perf::content_painted(view.active_tab().is_some());
    }

    fn on_resize(&mut self, info: &WindowInfo) {
        if let Ok(mut view) = self.cell.view.try_borrow_mut() {
            view.resize(info.size_dip(), info.scale());
        }
        self.link.placement_dirty.set(true);
        self.platform.layout_changed();
    }

    fn on_activate(&mut self, _info: &WindowInfo, active: bool) {
        if active {
            self.env.controller.set_active_window(self.cell.id);
            // Files may have changed while the user was elsewhere: the loop owner checks them.
            self.env.controller.push(Request::Activated(self.cell.id));
        }
        self.platform.activation_changed(active);
    }

    fn on_theme_changed(&mut self, _info: &WindowInfo, theme: &SystemTheme) {
        self.env.mica.set(theme.transparency_effects);
        let _ = apply_menu_theme(MenuTheme::System);
        self.env.controller.set_system_theme(*theme);
    }

    fn on_close_requested(&mut self, _info: &WindowInfo) -> bool {
        // The negotiation (unsaved documents) runs in the loop, outside of the window
        // procedure: keep the window open until it decides.
        self.env
            .controller
            .push(Request::CloseRequested(self.cell.id));
        false
    }

    fn on_destroy(&mut self, _info: &WindowInfo) {
        self.link.destroyed.set(true);
        self.platform.disconnect();
    }

    fn on_message(
        &mut self,
        _info: &WindowInfo,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        if msg == WM_MOVE {
            self.link.placement_dirty.set(true);
            return None;
        }
        self.platform.on_message(msg, wparam, lparam)
    }
}
