//! The owner of the message loop: [`UiShell`] (the production [`ShellHost`]) and [`UiApp`]
//! (the windows, requests and housekeeping it runs).
//!
//! Window procedures only push [`Request`]s ([`super::window`]); [`UiApp::pump`] carries them
//! out between messages: it makes the native windows, runs commands, shows the popup menus and
//! dialogs (modal loops never run inside a window procedure), negotiates closing, applies what
//! the widgets asked of the window (repaint, timers, cursor), pumps the application core
//! (worker results, watchers, autosave), opens launches forwarded by later processes and keeps
//! the Jump List in step with the recent files.
//!
//! Tests build a [`UiApp`] over scripted services with hidden windows and call
//! [`UiApp::pump`] themselves; the production [`UiShell::run`] calls it from the idle handler of
//! [`ssw_win::window::run_loop`].

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_core::i18n::Localizer;
use ssw_io::fs::RealFs;
use ssw_io::logging::{Level, Logger};
use ssw_io::net::{Http, WinHttp};
use ssw_io::session::{RecentKind, Session, WindowPlacement};
use ssw_ui::geometry::Point;
use ssw_ui::services::{Clock, Dialogs, Shell};
use ssw_ui::shell::titlebar::{MenuKind, TitleEvent};
use ssw_ui::theme::ThemeMode;
use ssw_ui::widgets::SharedClipboard;
use ssw_ui::widgets::anim::Motion;
use ssw_win::canvas::{CanvasAssets, ImageStore};
use ssw_win::com::WinError;
use ssw_win::dragdrop::OleGuard;
use ssw_win::dwrite::DWriteEngine;
use ssw_win::gfx::{DeviceStartup, GfxDevices, SharedDevices};
use ssw_win::instance::{Acquired, InstanceConfig, PrimaryInstance, Wake};
use ssw_win::jumplist::{JumpList, set_process_app_id};
use ssw_win::keyboard::ToUnicodeCharProbe;
use ssw_win::menus::{MenuTheme, PopupHost, PopupMenus, apply_menu_theme};
use ssw_win::services::clipboard::WindowsClipboard;
use ssw_win::services::clock::QpcClock;
use ssw_win::services::dialogs::{DialogStrings, WindowsDialogs};
use ssw_win::services::shell::WindowsShell;
use ssw_win::taskbar::{Progress, SystemTaskbars, TaskbarFactory};
use ssw_win::theme::{Win32Settings, read_system_theme};
use ssw_win::wic::Wic;
use ssw_win::window::{
    MessageWindow, ShowMode, WM_WAKE, WakeHandle, WindowHost, WindowOptions, apply_dark_mode,
};
use windows::Win32::Foundation::HWND;

use super::controller::{Controller, Request, WindowCell};
use super::menus::{build_menu, command_for_menu_id};
use super::platform::Platform;
use super::settings_window::{
    SettingsNative, SettingsSetup, WINDOW_SIZE, WindowsKeyNames, window_rect,
};
use super::shell_view::ShellAction;
use super::window::{WindowEnv, WindowGlue, WindowLink};
use crate::app::{AppCore, EXIT_NO_DEVICE, EXIT_NO_SHELL, ShellHost};
use crate::args::{Args, Launch};
use crate::startup::{InstanceError, InstanceHook, InstanceRole};
use crate::workers::{Waker, WindowId};

/// The AppUserModelID of SS Writer: the taskbar groups the windows and the Jump List under it,
/// and the single-instance mutex is named after it.
pub const APP_ID: &str = "isolmaz.SSWriter";

/// Longest the loop sleeps while it has no message: the autosave tick and the file watchers
/// are polled at least this often.
const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(250);

/// How many recent files the Jump List is asked to show.
const JUMP_LIST_RECENT: usize = 20;

/// Spelling checks run once no window asked for a repaint (scrolling, typing, animation) for
/// this long: a check calls into the system and must not delay a frame.
pub const SPELL_IDLE: Duration = Duration::from_millis(40);

/// The most time one loop turn spends on spelling checks.
pub const SPELL_BUDGET: Duration = Duration::from_millis(2);

/// How soon the loop wakes again while spelling checks wait.
pub const SPELL_TURN: Duration = Duration::from_millis(20);

/// Where an off-screen window (`--perf-offscreen`) has its top left corner, in both axes: far
/// outside every monitor arrangement.
pub const OFFSCREEN_ORIGIN: i32 = -20_000;

/// Wakes the message loop from a worker thread.
#[derive(Debug)]
struct LoopWaker(WakeHandle);

impl Waker for LoopWaker {
    fn wake(&self) {
        self.0.wake();
    }
}

/// The OS services the controller works with.
pub struct UiServices {
    /// File and message dialogs.
    pub dialogs: Box<dyn Dialogs>,
    /// Opening URLs and paths, revealing and recycling files.
    pub shell: Box<dyn Shell>,
    /// The clipboard.
    pub clipboard: SharedClipboard,
    /// The modal call that shows popup menus.
    pub popups: Box<dyn PopupHost>,
}

impl std::fmt::Debug for UiServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiServices").finish_non_exhaustive()
    }
}

/// A [`PopupHost`] behind a box.
struct BoxedPopupHost(Box<dyn PopupHost>);

impl PopupHost for BoxedPopupHost {
    fn track(
        &mut self,
        menu: windows::Win32::UI::WindowsAndMessaging::HMENU,
        request: &ssw_win::menus::PopupRequest,
    ) -> Option<u32> {
        self.0.track(menu, request)
    }
}

/// How a [`UiApp`] is set up.
#[derive(Debug, Default)]
pub struct UiOptions {
    /// Windows are created but never shown or activated (tests).
    pub hidden: bool,
    /// Services to use instead of the real ones (tests).
    pub services: Option<UiServices>,
    /// The single-instance state of this process (production).
    pub instance: Option<InstanceLink>,
    /// Keep the taskbar Jump List in step with the recent files (production).
    pub jump_list: bool,
    /// Start the weekly update check the settings allow (production).
    pub update_check: bool,
    /// The network to load remote images with instead of WinHTTP (tests).
    pub http: Option<Arc<dyn Http>>,
    /// The taskbar object of each window instead of the real taskbar (tests).
    pub taskbar: Option<Rc<dyn TaskbarFactory>>,
}

/// When [`UiApp::start`] takes the graphics devices.
#[derive(Debug)]
pub enum DeviceStart {
    /// Wait for the devices startup creates: every window has its swap chain from the first
    /// turn (tests that draw at once; [`UiApp::new`]).
    Wait,
    /// Open the windows first: a window shows its backdrop at once and gets its swap chain when
    /// the devices arrive, ready or not (the application).
    Defer,
    /// [`DeviceStart::Defer`] over `startup` instead of the one the process started (tests that
    /// hold the devices back or fail them).
    DeferOver(DeviceStartup),
}

/// What the instance hook and the shell share: the receiving window of later launches.
#[derive(Debug)]
struct InstanceInner {
    app_id: String,
    primary: RefCell<Option<PrimaryInstance>>,
    window: RefCell<Option<MessageWindow>>,
}

/// The shell's end of the single-instance state ([`WinInstance::link`]).
#[derive(Debug, Clone)]
pub struct InstanceLink(Rc<InstanceInner>);

impl Default for InstanceLink {
    fn default() -> Self {
        Self::named(APP_ID)
    }
}

impl InstanceLink {
    fn named(app_id: impl Into<String>) -> Self {
        Self(Rc::new(InstanceInner {
            app_id: app_id.into(),
            primary: RefCell::default(),
            window: RefCell::default(),
        }))
    }

    /// The AppUserModelID the single-instance state is named after: the process, its taskbar
    /// button and its Jump List take it ([`APP_ID`] unless tests ask for an id of their own).
    #[must_use]
    pub fn app_id(&self) -> &str {
        &self.0.app_id
    }

    /// The launches later processes forwarded since the last call.
    fn take_launches(&self) -> Vec<Launch> {
        self.0
            .primary
            .borrow()
            .as_ref()
            .map(PrimaryInstance::take_launches)
            .unwrap_or_default()
            .into_iter()
            .map(|launch| {
                Args {
                    paths: launch.paths,
                    new_window: launch.new_window,
                    ..Args::default()
                }
                .launch(&RealFs)
            })
            .collect()
    }

    fn take_message_window(&self) -> Option<MessageWindow> {
        self.0.window.borrow_mut().take()
    }
}

/// [`InstanceHook`] over `ssw_win::instance`: a named mutex decides who is first, later launches
/// travel to the first process with `WM_COPYDATA`.
#[derive(Debug, Default)]
pub struct WinInstance {
    link: InstanceLink,
}

impl WinInstance {
    /// A hook of the application ([`APP_ID`]) that has not claimed anything yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A hook of the application `app_id` instead of [`APP_ID`]: its single-instance mutex,
    /// receiving window, process id and Jump List are named after `app_id`, so a test never
    /// meets the running editor of the user.
    #[must_use]
    pub fn with_app_id(app_id: impl Into<String>) -> Self {
        Self {
            link: InstanceLink::named(app_id),
        }
    }

    /// The shell's end of this hook.
    #[must_use]
    pub fn link(&self) -> InstanceLink {
        self.link.clone()
    }
}

impl InstanceHook for WinInstance {
    fn claim(&mut self, launch: &Launch) -> Result<InstanceRole, InstanceError> {
        let fail = |error: WinError| InstanceError(error.to_string());
        let window = MessageWindow::new().map_err(fail)?;
        let wake = Wake {
            hwnd: window.hwnd(),
            message: WM_WAKE,
        };
        match InstanceConfig::new(self.link.app_id())
            .acquire(Some(wake))
            .map_err(fail)?
        {
            Acquired::Primary(primary) => {
                *self.link.0.primary.borrow_mut() = Some(primary);
                *self.link.0.window.borrow_mut() = Some(window);
                Ok(InstanceRole::Primary)
            }
            Acquired::Secondary(secondary) => {
                let forwarded = ssw_win::instance::Launch {
                    paths: launch
                        .items
                        .iter()
                        .map(|item| item.path().to_path_buf())
                        .collect(),
                    new_window: launch.new_window,
                };
                secondary.forward(&forwarded).map_err(fail)?;
                Ok(InstanceRole::Forwarded)
            }
        }
    }
}

/// One native window with everything that belongs to it.
struct NativeWindow {
    cell: Rc<WindowCell>,
    link: Rc<WindowLink>,
    platform: Rc<Platform>,
    // Declared last: the host destroys the window (and drops its handler) first.
    host: WindowHost,
}

/// Whether the graphics devices have reached the windows.
enum Arrival {
    /// Still being created: windows open without a swap chain and get it on arrival.
    Waiting,
    /// Here: every window has (or is given) a swap chain on them.
    Here(GfxDevices),
    /// Their creation failed; the application is leaving.
    Failed,
}

impl Arrival {
    /// The devices, once they are here.
    fn devices(&self) -> Option<GfxDevices> {
        match self {
            Arrival::Here(devices) => Some(devices.clone()),
            Arrival::Waiting | Arrival::Failed => None,
        }
    }
}

/// Gives `host` its swap chain on `devices`.
fn attach_graphics(host: &WindowHost, devices: GfxDevices) -> Result<(), WinError> {
    host.attach_graphics(devices)?;
    ssw_win::perf::mark(crate::perf::GRAPHICS_ATTACHED);
    Ok(())
}

/// The windows of the process and the loop that serves them.
pub struct UiApp {
    controller: Rc<Controller>,
    env: Rc<WindowEnv>,
    devices: SharedDevices,
    arrival: RefCell<Arrival>,
    wic: Wic,
    windows: RefCell<Vec<NativeWindow>>,
    settings: RefCell<Option<SettingsNative>>,
    popups: RefCell<PopupMenus<BoxedPopupHost>>,
    logger: Arc<Logger>,
    hidden: bool,
    offscreen: Cell<bool>,
    instance: Option<InstanceLink>,
    jump_list: Option<JumpList>,
    shown_recent: RefCell<Vec<PathBuf>>,
    recent_stale: Cell<bool>,
    frame_interval: Duration,
    /// When a window last asked for a repaint (the spelling checks wait for a quiet moment).
    last_repaint: Cell<Instant>,
    final_session: RefCell<Option<Session>>,
    exit: Cell<Option<i32>>,
    // Declared last: every native window is gone before the message window.
    message_window: MessageWindow,
}

impl std::fmt::Debug for UiApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiApp")
            .field("windows", &self.windows.borrow().len())
            .field("hidden", &self.hidden)
            .finish_non_exhaustive()
    }
}

impl UiApp {
    /// [`UiApp::start`] that waits for the graphics devices ([`DeviceStart::Wait`]): every
    /// window has its swap chain from the first turn.
    ///
    /// # Errors
    /// See [`UiApp::start`].
    pub fn new(core: AppCore, options: UiOptions) -> Result<Self, WinError> {
        Self::start(core, options, DeviceStart::Wait)
    }

    /// Builds the application over `core`: connects the workers' waker to the loop, makes the
    /// controller and queues the startup windows. With a deferred [`DeviceStart`] the graphics
    /// devices are still being created by their worker: the windows open first and
    /// [`UiApp::pump`] gives them their swap chains when the devices arrive (the worker wakes
    /// the loop).
    ///
    /// COM and OLE must be initialized on the calling thread ([`OleGuard`]) and outlive the
    /// returned value.
    ///
    /// # Errors
    /// The graphics device (when waited for), WIC, DirectWrite or the message window cannot be
    /// created.
    pub fn start(
        mut core: AppCore,
        options: UiOptions,
        start: DeviceStart,
    ) -> Result<Self, WinError> {
        let (startup, defer) = match start {
            DeviceStart::Wait => (core.take_devices()?, false),
            DeviceStart::Defer => (core.take_devices()?, true),
            DeviceStart::DeferOver(startup) => (startup, true),
        };
        let waker = Arc::clone(&core.waker);
        startup.notify_on_ready(move || waker.wake());
        let devices = SharedDevices::new(startup);
        let arrival = if defer {
            Arrival::Waiting
        } else {
            let here = devices.get()?;
            ssw_win::perf::mark(crate::perf::DEVICES_RECEIVED);
            Arrival::Here(here)
        };
        let wic = Wic::new()?;
        let text = DWriteEngine::new()?;
        let message_window = match options
            .instance
            .as_ref()
            .and_then(InstanceLink::take_message_window)
        {
            Some(window) => window,
            None => MessageWindow::new()?,
        };
        core.waker
            .connect(Arc::new(LoopWaker(WakeHandle::new(message_window.hwnd()))));
        let plan = core.plan.clone();
        let logger = Arc::clone(&core.logger);
        let i18n = Rc::clone(&core.i18n);
        let services = match options.services {
            Some(services) => services,
            None => real_services(message_window.hwnd(), &wic, &i18n),
        };
        let clock: Rc<dyn Clock> = Rc::new(QpcClock::new());
        let controller = Controller::new(
            core,
            services.dialogs,
            services.shell,
            services.clipboard,
            clock,
        );
        let system = read_system_theme(&Win32Settings);
        controller.set_system_theme(system);
        let _ = apply_menu_theme(MenuTheme::System);
        controller.set_printing(Box::new(super::export::SystemPrinting::new(
            devices.clone(),
            wic.clone(),
            text.clone(),
        )));
        controller.set_sharing(Box::new(super::export::SystemSharing::default()));
        let env = Rc::new(WindowEnv {
            controller: Rc::clone(&controller),
            text,
            probe: Box::new(ToUnicodeCharProbe::current()),
            mica: Cell::new(system.transparency_effects),
            http: options.http.unwrap_or_else(|| Arc::new(WinHttp)),
            taskbar: options.taskbar.unwrap_or_else(|| Rc::new(SystemTaskbars)),
        });
        let app_id = options
            .instance
            .as_ref()
            .map_or(APP_ID, InstanceLink::app_id);
        let jump_list = options.jump_list.then(|| {
            let exe = std::env::current_exe().unwrap_or_default();
            JumpList::new(
                app_id,
                exe,
                i18n.text("window-jumplist-recent"),
                i18n.text("window-jumplist-new-window"),
            )
        });
        let app = Self {
            controller,
            env,
            devices,
            arrival: RefCell::new(arrival),
            wic,
            windows: RefCell::new(Vec::new()),
            settings: RefCell::new(None),
            popups: RefCell::new(PopupMenus::with_host(BoxedPopupHost(services.popups))),
            logger,
            hidden: options.hidden,
            offscreen: Cell::new(false),
            instance: options.instance,
            jump_list,
            shown_recent: RefCell::new(Vec::new()),
            recent_stale: Cell::new(true),
            frame_interval: ssw_win::services::timers::refresh_interval(),
            last_repaint: Cell::new(Instant::now()),
            final_session: RefCell::new(None),
            exit: Cell::new(None),
            message_window,
        };
        app.controller.open_plan(&plan);
        if options.update_check {
            // Weekly, setting-gated, notification only (spec §13).
            app.controller.start_update_check();
        }
        Ok(app)
    }

    /// The controller (windows, tabs, commands).
    #[must_use]
    pub fn controller(&self) -> &Rc<Controller> {
        &self.controller
    }

    /// The exit code once the application decided to leave the loop.
    #[must_use]
    pub fn exit_code(&self) -> Option<i32> {
        self.exit.get()
    }

    /// The session captured when the application quit.
    #[must_use]
    pub fn final_session(&self) -> Option<Session> {
        self.final_session.borrow().clone()
    }

    /// The native handle of window `id`.
    #[must_use]
    pub fn hwnd(&self, id: WindowId) -> Option<HWND> {
        self.windows
            .borrow()
            .iter()
            .find(|native| native.cell.id == id)
            .map(|native| native.host.hwnd())
    }

    /// The message window that wakes the loop.
    #[must_use]
    pub fn message_hwnd(&self) -> HWND {
        self.message_window.hwnd()
    }

    /// Windows made from now on are placed at ([`OFFSCREEN_ORIGIN`], [`OFFSCREEN_ORIGIN`]) and
    /// shown without activation (`SW_SHOWNOACTIVATE`, `WS_EX_NOACTIVATE`): frames are really
    /// presented, nothing appears on a screen and nothing takes the focus
    /// (`--perf-offscreen`). A hidden application stays hidden.
    pub fn set_offscreen(&self, offscreen: bool) {
        self.offscreen.set(offscreen);
    }

    /// Mirrors the recent files to `jump_list` from the next turn on (tests give it an
    /// application id of their own, or a program the shell refuses).
    pub fn set_jump_list(&mut self, jump_list: JumpList) {
        self.jump_list = Some(jump_list);
        self.recent_stale.set(true);
        self.shown_recent.borrow_mut().clear();
    }

    /// The platform wiring (text services, caret, accessibility) of window `id`.
    #[must_use]
    pub fn platform(&self, id: WindowId) -> Option<Rc<Platform>> {
        self.windows
            .borrow()
            .iter()
            .find(|native| native.cell.id == id)
            .map(|native| Rc::clone(&native.platform))
    }

    // ----- native windows -----------------------------------------------------------------

    fn create_native(&self, cell: Rc<WindowCell>) -> Result<NativeWindow, WinError> {
        let link = Rc::new(WindowLink::default());
        let platform = Platform::new(Rc::clone(&cell), Rc::clone(&self.env));
        let glue = WindowGlue::new(
            Rc::clone(&self.env),
            Rc::clone(&cell),
            Rc::clone(&link),
            Rc::clone(&platform),
        );
        let placement = cell.placement.get();
        let theme = self.controller.theme();
        let offscreen = self.offscreen.get();
        let (x, y) = if offscreen {
            (OFFSCREEN_ORIGIN, OFFSCREEN_ORIGIN)
        } else {
            (placement.x, placement.y)
        };
        let options = WindowOptions {
            title: self.controller.i18n().text("window-title-app"),
            rect: Some((x, y, placement.width, placement.height)),
            dark: theme.mode == ThemeMode::Dark,
            no_activate: self.hidden || offscreen,
        };
        let host = WindowHost::create(&options, Box::new(glue), theme)?;
        // The assets (images, math scenes) need no device: registrations made before the
        // swap chain exists are kept.
        host.set_assets(CanvasAssets::with_scene_renderer(
            ImageStore::new(self.wic.clone()),
            self.env.text.clone(),
            self.wic.clone(),
        )?);
        // Devices still being created: the window shows its backdrop without a swap chain and
        // `attach_devices` gives it one on arrival.
        let devices = self.arrival.borrow().devices();
        if let Some(devices) = devices {
            attach_graphics(&host, devices)?;
        }
        cell.hwnd.set(Some(host.hwnd()));
        let info = host.info();
        cell.view.borrow_mut().resize(info.size_dip(), info.scale());
        platform.attach(&host, &self.logger);
        host.show_mode(ShowMode::for_new_window(
            self.hidden,
            offscreen,
            placement.maximized,
        ));
        ssw_win::perf::mark(crate::perf::WINDOW_SHOWN);
        Ok(NativeWindow {
            cell,
            link,
            platform,
            host,
        })
    }

    fn destroy_native(&self, id: WindowId) {
        let removed = {
            let mut windows = self.windows.borrow_mut();
            windows
                .iter()
                .position(|native| native.cell.id == id)
                .map(|index| windows.remove(index))
        };
        // Dropping the native window destroys it (outside of any borrow of the list).
        drop(removed);
    }

    // ----- requests ------------------------------------------------------------------------

    fn drain_requests(&self) {
        while let Some(request) = self.controller.take_request() {
            self.recent_stale.set(true);
            self.handle(request);
        }
    }

    fn handle(&self, request: Request) {
        match request {
            Request::NewWindow(id) => {
                let Some(cell) = self.controller.window(id) else {
                    return;
                };
                match self.create_native(cell) {
                    Ok(native) => self.windows.borrow_mut().push(native),
                    Err(error) => {
                        self.logger.log(
                            Level::Error,
                            "window",
                            &format!("cannot create window {}: {error}", id.0),
                        );
                        self.controller.close_window(id);
                    }
                }
            }
            Request::Command(id, command) => {
                self.controller.run_command(id, command);
            }
            Request::Action(id, action) => self.action(id, action),
            Request::CloseRequested(id) => self.close_requested(id),
            Request::CloseWindow(id) => self.destroy_native(id),
            Request::Quit => self.quit(),
            Request::TaskbarProgress(id, progress) => self.set_progress(id, progress),
            Request::OpenSettings => self.open_settings(),
            Request::Settings(event) => self.controller.settings_event(event),
            Request::CloseSettings => self.close_settings(),
            Request::Activated(id) => self.controller.check_external_changes(id),
        }
    }

    fn action(&self, id: WindowId, action: ShellAction) {
        match action {
            ShellAction::Title(TitleEvent::MenuRequested { menu, anchor }) => {
                self.show_menu(id, menu, anchor);
            }
            other => match self.controller.popup_for(id, &other) {
                // Tab, sidebar, editor, spelling, table and callout menus: shown here, outside
                // of any window procedure, and the chosen item is carried out.
                Some(popup) => {
                    if let Some(chosen) = self.show_popup(id, &popup.model, popup.at) {
                        self.controller.run_popup_choice(id, &popup, chosen);
                    }
                }
                None => self.controller.handle_action(id, other),
            },
        }
    }

    fn set_progress(&self, id: WindowId, progress: Progress) {
        if let Some(platform) = self.platform(id) {
            platform.set_progress(progress);
        }
    }

    // ----- menus ---------------------------------------------------------------------------

    /// Shows the popup of `menu` below its button and runs the chosen command.
    fn show_menu(&self, id: WindowId, menu: MenuKind, anchor: Point) {
        let Some(cell) = self.controller.window(id) else {
            return;
        };
        let model = build_menu(
            menu,
            self.controller.i18n(),
            &self.controller.keymap(),
            &self.controller.menu_state(&cell),
        );
        let Some(chosen) = self.show_popup(id, &model, anchor) else {
            return;
        };
        if let Some(command) = command_for_menu_id(chosen) {
            self.controller.run_command(id, command);
        }
    }

    /// Shows `model` with its corner at `anchor` (window DIPs) and returns the chosen item id.
    pub fn show_popup(
        &self,
        id: WindowId,
        model: &ssw_win::menus::MenuModel,
        anchor: Point,
    ) -> Option<u32> {
        let (hwnd, x, y) = {
            let windows = self.windows.borrow();
            let native = windows.iter().find(|native| native.cell.id == id)?;
            let scale = native.host.info().scale();
            let (x, y) = native.host.client_to_screen(
                (anchor.x * scale).round() as i32,
                (anchor.y * scale).round() as i32,
            );
            (native.host.hwnd(), x, y)
        };
        match self.popups.borrow_mut().show(model, hwnd, x, y) {
            Ok(chosen) => chosen,
            Err(error) => {
                self.logger.log(
                    Level::Error,
                    "menu",
                    &format!("cannot show the menu: {error}"),
                );
                None
            }
        }
    }

    // ----- closing -------------------------------------------------------------------------

    fn close_requested(&self, id: WindowId) {
        if !self.controller.request_close_window(id) {
            return;
        }
        if self.controller.windows().len() <= 1 {
            self.finish_quit();
        } else {
            self.controller.close_window(id);
        }
    }

    /// File → Exit: asks about every window, then leaves.
    fn quit(&self) {
        for cell in self.controller.windows() {
            if !self.controller.request_close_window(cell.id) {
                return;
            }
        }
        self.finish_quit();
    }

    /// Every window agreed to close: the session is captured while the tabs still exist, then
    /// the windows go and the loop ends.
    fn finish_quit(&self) {
        *self.final_session.borrow_mut() = Some(self.controller.session());
        for cell in self.controller.windows() {
            self.controller.close_window(cell.id);
        }
        self.close_settings();
        self.exit.set(Some(0));
        ssw_win::window::quit(0);
    }

    // ----- housekeeping --------------------------------------------------------------------

    /// Applies what the widgets asked of the native windows: repaint, timers, cursor, the
    /// stored placement, the scrolling, the caret and the accessibility tree. Returns whether
    /// a window is still scrolling and needs another frame.
    fn sync_windows(&self) -> bool {
        let mut scrolling = false;
        for native in self.windows.borrow().iter() {
            let cell = &native.cell;
            if native.link.destroyed.get() {
                continue;
            }
            if native.link.placement_dirty.take() {
                self.store_placement(native);
            }
            // Before the invalidation is read: scrolling invalidates the editor.
            scrolling |= native.platform.scroll_frame();
            let (repaint, cursor) = {
                // The loop owner's turn never runs inside a handler of the view.
                let mut view = cell.view.borrow_mut();
                let requests = view.requests_mut();
                let repaint = !requests.take_invalidation().is_clean();
                let cursor = requests.take_cursor();
                requests.flush_timers(&mut native.host.timers());
                (repaint, cursor)
            };
            if let Some(cursor) = cursor {
                cell.cursor.set(cursor);
            }
            native.host.set_cursor(cell.cursor.get());
            if repaint || cell.dirty.take() {
                native.host.invalidate();
                self.last_repaint.set(Instant::now());
            }
            native.platform.sync(&native.host, repaint);
        }
        scrolling
    }

    fn store_placement(&self, native: &NativeWindow) {
        if let Some((x, y, width, height)) = native.host.normal_placement() {
            native.cell.placement.set(WindowPlacement {
                x,
                y,
                width,
                height,
                maximized: native.host.info().maximized,
            });
        }
    }

    /// Delivers the pending animation frames; returns whether any window had one.
    fn deliver_frames(&self) -> bool {
        let mut delivered = false;
        for native in self.windows.borrow().iter() {
            delivered |= native.host.deliver_animation_frame();
        }
        if let Some(settings) = self.settings.borrow().as_ref() {
            delivered |= settings.deliver_frame();
        }
        delivered
    }

    fn open_launches(&self) {
        let Some(link) = &self.instance else {
            return;
        };
        for launch in link.take_launches() {
            self.controller.open_launch(&launch);
            self.drain_requests();
            // A hidden application (tests) has nothing to bring to the foreground.
            if !self.hidden
                && let Some(cell) = self.controller.active_window()
                && let Some(native) = self
                    .windows
                    .borrow()
                    .iter()
                    .find(|native| native.cell.id == cell.id)
            {
                native.host.bring_to_front();
            }
        }
    }

    fn refresh_jump_list(&self) {
        let Some(jump_list) = &self.jump_list else {
            return;
        };
        if !self.recent_stale.take() {
            return;
        }
        let recent: Vec<PathBuf> = self
            .controller
            .docs()
            .env()
            .store
            .borrow()
            .recent_items(RecentKind::File)
            .map(|items| {
                items
                    .into_iter()
                    .take(JUMP_LIST_RECENT)
                    .map(|item| item.path)
                    .collect()
            })
            .unwrap_or_default();
        if *self.shown_recent.borrow() == recent {
            return;
        }
        match jump_list.update(&recent) {
            Ok(_) => *self.shown_recent.borrow_mut() = recent,
            Err(error) => self.logger.log(
                Level::Warn,
                "jumplist",
                &format!("cannot update the Jump List: {error}"),
            ),
        }
    }

    /// Takes the graphics devices once their worker is done and gives every open window its
    /// swap chain and a repaint: the window that was showing only its backdrop gets its content.
    /// A window that cannot take them is closed. Without devices there is nothing to show, so
    /// the application leaves with [`EXIT_NO_DEVICE`].
    fn attach_devices(&self) {
        if !matches!(*self.arrival.borrow(), Arrival::Waiting) {
            return;
        }
        let devices = match self.devices.poll() {
            Ok(Some(devices)) => devices,
            Ok(None) => return,
            Err(error) => {
                *self.arrival.borrow_mut() = Arrival::Failed;
                self.logger.log(
                    Level::Error,
                    "startup",
                    &format!("device creation failed: {error}"),
                );
                self.exit.set(Some(EXIT_NO_DEVICE));
                ssw_win::window::quit(EXIT_NO_DEVICE);
                return;
            }
        };
        ssw_win::perf::mark(crate::perf::DEVICES_RECEIVED);
        *self.arrival.borrow_mut() = Arrival::Here(devices.clone());
        let failed: Vec<(WindowId, WinError)> = self
            .windows
            .borrow()
            .iter()
            .filter_map(
                |native| match attach_graphics(&native.host, devices.clone()) {
                    Ok(()) => {
                        native.host.invalidate();
                        None
                    }
                    Err(error) => Some((native.cell.id, error)),
                },
            )
            .collect();
        for (id, error) in failed {
            self.logger.log(
                Level::Error,
                "window",
                &format!("cannot give window {} its graphics: {error}", id.0),
            );
            self.controller.close_window(id);
        }
    }

    /// One turn of the loop: requests, the arrival of the graphics devices, worker results,
    /// watchers, autosave, forwarded launches, then the window synchronization. Returns whether
    /// another frame is wanted: an animation frame was delivered or a window is scrolling (wheel
    /// spring, touch, inertia).
    pub fn pump(&self) -> bool {
        self.drain_requests();
        // After the requests: a window made in this turn is shown before the devices count.
        self.attach_devices();
        let report = self.controller.core().borrow_mut().pump();
        self.controller.pump_files();
        if report.jobs > 0 || !report.external.is_empty() || !report.tick.saved.is_empty() {
            self.controller.refresh_all();
        }
        for (_, error) in &report.tick.failed {
            self.controller.report(error);
        }
        self.open_launches();
        self.drain_requests();
        let frames = self.deliver_frames();
        self.controller.pump_settings();
        let scrolling = self.sync_windows();
        self.pump_spell();
        self.sync_settings();
        self.refresh_jump_list();
        frames || scrolling
    }

    /// Runs the spelling checks the painted blocks wait for, a slice of [`SPELL_BUDGET`] per
    /// turn, once no window asked for a repaint for [`SPELL_IDLE`] (so a check never delays a
    /// frame of scrolling or typing), and repaints the windows when a check answered: the
    /// squiggles change what is drawn, not the layout. Returns whether the windows were asked
    /// to repaint.
    pub fn pump_spell(&self) -> bool {
        if self.last_repaint.get().elapsed() < SPELL_IDLE
            || !self.controller.pump_spell(SPELL_BUDGET)
        {
            return false;
        }
        for native in self.windows.borrow().iter() {
            native.host.invalidate();
        }
        true
    }

    /// The idle handler of the message loop: how long it may sleep.
    pub fn idle(&self) -> Option<Duration> {
        let frames = self.pump();
        if self.exit.get().is_some() {
            return None;
        }
        Some(if frames {
            self.frame_interval
        } else if self.controller.spell_pending() {
            SPELL_TURN
        } else {
            HOUSEKEEPING_INTERVAL
        })
    }

    /// Leaves the application cleanly: stores the session of the moment the user quit (or of
    /// the windows that are left) and flushes autosave.
    pub fn finish(&self) {
        let session = self
            .final_session
            .borrow()
            .clone()
            .unwrap_or_else(|| self.controller.session());
        self.controller.flush_settings();
        self.controller.core().borrow_mut().shutdown(&session);
    }
}

impl UiApp {
    // ----- the Settings window -------------------------------------------------------------

    /// The native handle of the Settings window, while it is open.
    #[must_use]
    pub fn settings_hwnd(&self) -> Option<HWND> {
        self.settings.borrow().as_ref().map(SettingsNative::hwnd)
    }

    /// Opens the Settings window, or brings the open one to the front (one instance).
    fn open_settings(&self) {
        if let Some(native) = self.settings.borrow().as_ref() {
            if !self.hidden {
                native.bring_to_front();
            }
            return;
        }
        let owner = self.controller.active_window().and_then(|cell| {
            let scale = self
                .windows
                .borrow()
                .iter()
                .find(|native| native.cell.id == cell.id)
                .map(|native| native.host.info().scale())?;
            let placement = cell.placement.get();
            Some((
                (placement.x, placement.y, placement.width, placement.height),
                scale,
            ))
        });
        let scale = owner.map_or(1.0, |(_, scale)| scale);
        let rect = window_rect(owner.map(|(rect, _)| rect), scale);
        let animations = read_system_theme(&Win32Settings).client_animations;
        let cell = self.controller.create_settings(SettingsSetup {
            probe: Box::new(ToUnicodeCharProbe::current()),
            key_names: Box::new(WindowsKeyNames),
            motion: Motion::new(!animations),
            size: WINDOW_SIZE,
            scale,
        });
        // Opened before the devices arrived (the first moments of a launch): waits for them.
        let created = self.devices.get().and_then(|devices| {
            SettingsNative::create(
                &self.env,
                &devices,
                &self.wic,
                &self.logger,
                cell,
                rect,
                self.hidden,
            )
        });
        match created {
            Ok(native) => *self.settings.borrow_mut() = Some(native),
            Err(error) => {
                self.logger.log(
                    Level::Error,
                    "settings",
                    &format!("cannot create the settings window: {error}"),
                );
                self.controller.close_settings();
            }
        }
    }

    /// Destroys the Settings window; pending changes are written first.
    fn close_settings(&self) {
        let native = self.settings.borrow_mut().take();
        self.controller.close_settings();
        drop(native);
    }

    /// Follows the controller's theme in every native window, then applies what the widgets of
    /// the Settings window asked of it.
    fn sync_settings(&self) {
        if let Some(theme) = self.controller.take_theme_change() {
            for native in self.windows.borrow().iter() {
                apply_dark_mode(native.host.hwnd(), theme.mode == ThemeMode::Dark);
                native.host.set_theme(theme);
            }
            if let Some(settings) = self.settings.borrow().as_ref() {
                settings.apply_theme(theme);
            }
        }
        let events = self
            .settings
            .borrow()
            .as_ref()
            .map(SettingsNative::sync)
            .unwrap_or_default();
        self.controller.push_settings(events);
        let gone = self
            .settings
            .borrow()
            .as_ref()
            .is_some_and(SettingsNative::destroyed);
        if gone {
            self.close_settings();
        }
    }
}

/// The real Windows services, owned by `owner`'s thread.
fn real_services(owner: HWND, wic: &Wic, i18n: &Localizer) -> UiServices {
    UiServices {
        dialogs: Box::new(WindowsDialogs::new(
            None,
            DialogStrings {
                save: i18n.text("app-dialog-save"),
                dont_save: i18n.text("app-dialog-dont-save"),
            },
        )),
        shell: Box::new(WindowsShell::new(None)),
        clipboard: Rc::new(RefCell::new(WindowsClipboard::new(owner, wic.clone()))),
        popups: Box::new(ssw_win::menus::SystemPopupHost),
    }
}

/// The production [`ShellHost`]: windows, tabs, editor, menus and dialogs of the application.
#[derive(Debug, Default)]
pub struct UiShell {
    instance: Option<InstanceLink>,
}

impl UiShell {
    /// A shell that receives the launches of later processes through `instance` (`None`: no
    /// single-instance handshake, as in test runs).
    #[must_use]
    pub fn new(instance: Option<InstanceLink>) -> Self {
        Self { instance }
    }
}

impl ShellHost for UiShell {
    fn run(&mut self, mut core: AppCore, args: &Args) -> i32 {
        if args.test.exit_after_start {
            return match core.wait_devices() {
                Ok(_) => 0,
                Err(error) => {
                    core.logger.log(
                        Level::Error,
                        "startup",
                        &format!("device creation failed: {error}"),
                    );
                    EXIT_NO_DEVICE
                }
            };
        }
        let logger = Arc::clone(&core.logger);
        // Bound first, so it is dropped after every COM object of the application.
        let _ole = match OleGuard::initialize() {
            Ok(guard) => guard,
            Err(error) => {
                logger.log(Level::Error, "startup", &format!("OLE failed: {error}"));
                return EXIT_NO_SHELL;
            }
        };
        let app_id = self
            .instance
            .as_ref()
            .map_or_else(|| APP_ID.to_owned(), |link| link.app_id().to_owned());
        if let Err(error) = set_process_app_id(&app_id) {
            logger.log(
                Level::Warn,
                "startup",
                &format!("AppUserModelID not set: {error}"),
            );
        }
        let options = UiOptions {
            hidden: args.test.hidden,
            services: None,
            instance: self.instance.take(),
            jump_list: !args.test.any(),
            // Never in test runs.
            update_check: !args.test.any(),
            http: None,
            taskbar: None,
        };
        // The window first: its backdrop is up while the worker still creates the devices, and
        // the swap chain follows on arrival (`UiApp::pump`).
        let app = match UiApp::start(core, options, DeviceStart::Defer) {
            Ok(app) => app,
            Err(error) => {
                logger.log(
                    Level::Error,
                    "startup",
                    &format!("device creation failed: {error}"),
                );
                return EXIT_NO_DEVICE;
            }
        };
        app.set_offscreen(args.test.perf_offscreen);
        let mut driver = super::perf_driver::PerfDriver::new(args);
        let code = ssw_win::window::run_loop(|| {
            let wait = app.idle();
            match driver.as_mut() {
                Some(driver) => driver.drive(&app, wait),
                None => wait,
            }
        });
        if let Some(driver) = driver.as_mut() {
            driver.finish();
        }
        app.finish();
        code
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::platform::tests::with_app;

    #[test]
    fn a_window_the_system_cannot_describe_keeps_the_placement_it_had() {
        with_app("text\n", |app, _, _| {
            let windows = app.windows.borrow();
            let native = &windows[0];
            let placement = &native.cell.placement;
            placement.set(WindowPlacement {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
                maximized: true,
            });

            // A live window reports where it is, and that is what gets stored.
            app.store_placement(native);
            let stored = placement.get();
            assert_eq!(
                Some((stored.x, stored.y, stored.width, stored.height)),
                native.host.normal_placement()
            );
            assert!(!stored.maximized);

            // Once the window is gone it reports nothing: the last placement stays.
            native.host.destroy();
            assert_eq!(native.host.normal_placement(), None);
            app.store_placement(native);
            assert_eq!(placement.get(), stored);
        });
    }
}
