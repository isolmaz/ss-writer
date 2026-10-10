//! A real in-process UI Automation client (`CUIAutomation8`) against the bridge.
//!
//! The provider lives in a window that is created and never shown, activated or focused:
//! `WS_POPUP` with `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` (no taskbar button), at
//! (-32000, -32000) off every monitor, and `ShowWindow` is never called. It runs on its own
//! thread with its own message loop, so UIA's `WM_GETOBJECT` is answered while the client call
//! blocks on the test thread. Actions land in a recording sink instead of widgets.
//!
//! The tests are serialized by a process-wide lock held for the whole test: several UIA
//! clients and provider windows alive at once in one process make `ElementFromHandle` fail with
//! `E_FAIL`.

use std::ffi::c_void;
use std::ops::Range;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::thread::JoinHandle;

use ssw_ui::access::{
    AccessAction, AccessNode, AccessState, AccessTree, Patterns, RangeValue, Role, TextPatternInfo,
    ToggleState as NodeToggle,
};
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::widget::WidgetId;
use ssw_win::com::ComGuard;
use ssw_win::uia::{ActionSink, TextSource, UiaBridge};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, ExpandCollapseState_Collapsed, IUIAutomation, IUIAutomationElement,
    IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern, IUIAutomationRangeValuePattern,
    IUIAutomationTextPattern, IUIAutomationTogglePattern, IUIAutomationValuePattern,
    TextPatternRangeEndpoint_Start, TextUnit_Character, TextUnit_Line, ToggleState_Off,
    ToggleState_On, TreeScope_Children, TreeScope_Descendants, UIA_ButtonControlTypeId,
    UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_ExpandCollapsePatternId,
    UIA_InvokePatternId, UIA_NamePropertyId, UIA_PATTERN_ID, UIA_RangeValuePatternId,
    UIA_SliderControlTypeId, UIA_TextPatternId, UIA_TogglePatternId, UIA_ValuePatternId,
    UIA_WindowControlTypeId,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_GETOBJECT,
    WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{IUnknown, Interface, PCWSTR, w};

/// UIA's "element not enabled" and "not supported" HRESULT bit patterns.
const ELEMENT_NOT_ENABLED: u32 = 0x8004_0200;
const NOT_SUPPORTED: u32 = 0x8004_0204;

const CLASS_NAME: PCWSTR = w!("SswUiaTestWindow");

/// The document the Text pattern reads: 'ö' is 2 bytes and '👍' is 2 UTF-16 units.
const DOC: &str = "Hello wörld\nSecond line\nThird 👍 end";

/// Calls into the UIA client interfaces. Every call is made on a live interface owned by the
/// test, with arguments that outlive the call.
macro_rules! uia {
    ($call:expr) => {
        // SAFETY: a COM call on a live interface pointer owned by the test; arguments are valid
        // for the duration of the call.
        unsafe { $call }
    };
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Held by every [`Fixture`] for the whole test: one provider window and UIA client at a time.
static SERIAL: Mutex<()> = Mutex::new(());

static BRIDGES: Mutex<Vec<(isize, Arc<UiaBridge>)>> = Mutex::new(Vec::new());
static REGISTER: Once = Once::new();

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_GETOBJECT => {
            let bridge = lock(&BRIDGES)
                .iter()
                .find(|(raw, _)| *raw == hwnd.0 as isize)
                .map(|(_, bridge)| bridge.clone());
            if let Some(result) = bridge.and_then(|bridge| bridge.handle_get_object(wparam, lparam))
            {
                return result;
            }
        }
        WM_DESTROY => {
            // SAFETY: ends the message loop of the window's own thread.
            unsafe { PostQuitMessage(0) };
        }
        _ => {}
    }
    // SAFETY: forwards the original arguments to the default procedure.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn instance() -> HINSTANCE {
    // SAFETY: querying the handle of the current module.
    unsafe { GetModuleHandleW(None) }
        .map(|module| HINSTANCE(module.0))
        .unwrap_or_default()
}

fn register_class() {
    let class = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance(),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: the class structure is fully initialized and outlives the call.
    unsafe { RegisterClassExW(&class) };
}

/// Creates the hidden window on a new thread with a message loop. Returns the handle value.
fn spawn_window() -> (isize, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel::<isize>();
    let thread = std::thread::spawn(move || {
        REGISTER.call_once(register_class);
        // SAFETY: creates a window of the registered class; it is never shown.
        let created = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!(""),
                WS_POPUP,
                -32000,
                -32000,
                640,
                480,
                None,
                None,
                Some(instance()),
                None,
            )
        };
        let raw = created.as_ref().map_or(0, |hwnd| hwnd.0 as isize);
        let _ = sender.send(raw);
        if raw == 0 {
            return;
        }
        let mut message = MSG::default();
        // SAFETY: the standard message loop of the thread that owns the window.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
            // SAFETY: `message` was filled in by GetMessageW.
            unsafe {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }
        }
    });
    let raw = receiver.recv().expect("window thread started");
    assert_ne!(raw, 0, "the hidden window was not created");
    (raw, thread)
}

/// Records every action UIA requests; refuses those for the ids in `refused`.
#[derive(Default)]
struct Recorder {
    actions: Mutex<Vec<(WidgetId, AccessAction)>>,
    refused: Mutex<Vec<WidgetId>>,
}

impl Recorder {
    fn take(&self) -> Vec<(WidgetId, AccessAction)> {
        std::mem::take(&mut *lock(&self.actions))
    }
}

impl ActionSink for Recorder {
    fn perform(&self, id: WidgetId, action: AccessAction) -> bool {
        if lock(&self.refused).contains(&id) {
            return false;
        }
        lock(&self.actions).push((id, action));
        true
    }
}

/// The editor text with hard line breaks as visual lines; records what UIA selects.
#[derive(Default)]
struct Editor {
    selected: Mutex<Vec<Range<usize>>>,
}

impl TextSource for Editor {
    fn text(&self) -> String {
        DOC.to_owned()
    }

    fn bounding_rects(&self, _range: Range<usize>) -> Vec<Rect> {
        vec![Rect::new(10.0, 200.0, 100.0, 20.0)]
    }

    fn offset_from_point(&self, _point: Point) -> Option<usize> {
        Some(0)
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        let bytes = DOC.as_bytes();
        let offset = offset.min(bytes.len());
        let start = bytes[..offset]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |at| at + 1);
        let end = bytes[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |at| offset + at + 1);
        start..end
    }

    fn select(&self, range: Range<usize>) {
        lock(&self.selected).push(range);
    }

    fn scroll_into_view(&self, _range: Range<usize>, _align_to_top: bool) {}
}

#[derive(Clone, Copy)]
struct Ids {
    root: WidgetId,
    save: WidgetId,
    remember: WidgetId,
    name: WidgetId,
    theme: WidgetId,
    zoom: WidgetId,
    editor: WidgetId,
    delete: WidgetId,
}

impl Ids {
    fn new() -> Self {
        Self {
            root: WidgetId::allocate(),
            save: WidgetId::allocate(),
            remember: WidgetId::allocate(),
            name: WidgetId::allocate(),
            theme: WidgetId::allocate(),
            zoom: WidgetId::allocate(),
            editor: WidgetId::allocate(),
            delete: WidgetId::allocate(),
        }
    }
}

/// What the app publishes: the check box state, the name field's text and the focused node.
struct Snapshot {
    remember: NodeToggle,
    name: &'static str,
    focused: Option<WidgetId>,
}

fn focusable(id: WidgetId, focused: Option<WidgetId>) -> AccessState {
    AccessState {
        focusable: true,
        focused: focused == Some(id),
        ..AccessState::default()
    }
}

fn build_tree(ids: Ids, snapshot: &Snapshot) -> AccessTree {
    let mut tree = AccessTree::new(ids.root);
    tree.insert(
        AccessNode::new(
            ids.root,
            Role::Window,
            "SS Writer",
            Rect::new(0.0, 0.0, 640.0, 480.0),
        )
        .with_children([
            ids.save,
            ids.remember,
            ids.name,
            ids.theme,
            ids.zoom,
            ids.editor,
            ids.delete,
        ]),
    );
    tree.insert(
        AccessNode::new(
            ids.save,
            Role::Button,
            "Save",
            Rect::new(10.0, 10.0, 80.0, 32.0),
        )
        .with_state(focusable(ids.save, snapshot.focused))
        .with_patterns(Patterns {
            invoke: true,
            ..Patterns::default()
        }),
    );
    tree.insert(
        AccessNode::new(
            ids.remember,
            Role::CheckBox,
            "Remember",
            Rect::new(10.0, 50.0, 120.0, 24.0),
        )
        .with_state(AccessState {
            toggle: Some(snapshot.remember),
            ..focusable(ids.remember, snapshot.focused)
        })
        .with_patterns(Patterns {
            toggle: true,
            ..Patterns::default()
        }),
    );
    tree.insert(
        AccessNode::new(
            ids.name,
            Role::Edit,
            "Name",
            Rect::new(10.0, 80.0, 200.0, 32.0),
        )
        .with_state(focusable(ids.name, snapshot.focused))
        .with_value(snapshot.name)
        .with_patterns(Patterns {
            value: true,
            ..Patterns::default()
        }),
    );
    tree.insert(
        AccessNode::new(
            ids.theme,
            Role::ComboBox,
            "Theme",
            Rect::new(10.0, 120.0, 160.0, 32.0),
        )
        .with_state(AccessState {
            expanded: Some(false),
            ..focusable(ids.theme, snapshot.focused)
        })
        .with_value("Light")
        .with_patterns(Patterns {
            expand_collapse: true,
            value: true,
            ..Patterns::default()
        }),
    );
    tree.insert(
        AccessNode::new(
            ids.zoom,
            Role::Slider,
            "Zoom",
            Rect::new(10.0, 160.0, 200.0, 24.0),
        )
        .with_state(focusable(ids.zoom, snapshot.focused))
        .with_patterns(Patterns {
            range_value: Some(RangeValue {
                value: 50.0,
                min: 0.0,
                max: 100.0,
                step: 1.0,
            }),
            ..Patterns::default()
        }),
    );
    tree.insert(
        AccessNode::new(
            ids.editor,
            Role::Document,
            "Editor",
            Rect::new(10.0, 200.0, 300.0, 100.0),
        )
        .with_state(AccessState {
            multiline: true,
            ..focusable(ids.editor, snapshot.focused)
        })
        .with_patterns(Patterns {
            text: Some(TextPatternInfo {
                len: DOC.len(),
                selection: vec![std::ops::Range { start: 6, end: 12 }],
                caret: 12,
                visible: 0..DOC.len(),
            }),
            ..Patterns::default()
        }),
    );
    tree.insert(
        AccessNode::new(
            ids.delete,
            Role::Button,
            "Delete",
            Rect::new(10.0, 320.0, 80.0, 32.0),
        )
        .with_state(AccessState {
            focusable: true,
            disabled: true,
            ..AccessState::default()
        })
        .with_patterns(Patterns {
            invoke: true,
            ..Patterns::default()
        }),
    );
    tree
}

fn initial() -> Snapshot {
    Snapshot {
        remember: NodeToggle::Off,
        name: "Ada",
        focused: None,
    }
}

/// The hidden window, its bridge and the recording sink. Holding it holds [`SERIAL`] until the
/// window is gone (the guard is the last field, so it drops after `Drop::drop` closed the
/// window), so a test must create its UIA client after the fixture and drop it first.
struct Fixture {
    ids: Ids,
    bridge: Arc<UiaBridge>,
    sink: Arc<Recorder>,
    raw: isize,
    thread: Option<JoinHandle<()>>,
    _serial: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        let serial = lock(&SERIAL);
        let ids = Ids::new();
        let (raw, thread) = spawn_window();
        let sink = Arc::new(Recorder::default());
        let bridge = Arc::new(UiaBridge::new(
            HWND(raw as *mut c_void),
            sink.clone(),
            ids.root,
        ));
        lock(&BRIDGES).push((raw, bridge.clone()));
        bridge
            .update(build_tree(ids, &initial()))
            .expect("first snapshot");
        Self {
            ids,
            bridge,
            sink,
            raw,
            thread: Some(thread),
            _serial: serial,
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.raw as *mut c_void)
    }

    fn republish(&self, snapshot: &Snapshot) -> usize {
        self.bridge
            .update(build_tree(self.ids, snapshot))
            .expect("snapshot published")
            .len()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.bridge.disconnect();
        lock(&BRIDGES).retain(|(raw, _)| *raw != self.raw);
        // SAFETY: posting to a window of this process from another thread is allowed.
        let _ = unsafe { PostMessageW(Some(self.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The UIA client: automation object and the element of the hidden window.
struct Client {
    automation: IUIAutomation,
    root: IUIAutomationElement,
    _com: ComGuard,
}

impl Client {
    fn new(fixture: &Fixture) -> Self {
        let com = ComGuard::multi_threaded().expect("COM on the client thread");
        let automation: IUIAutomation = uia!(CoCreateInstance(
            &CUIAutomation8,
            None::<&IUnknown>,
            CLSCTX_INPROC_SERVER
        ))
        .expect("CUIAutomation8");
        let root =
            uia!(automation.ElementFromHandle(fixture.hwnd())).expect("element of the window");
        Self {
            automation,
            root,
            _com: com,
        }
    }

    fn try_find(&self, name: &str) -> windows::core::Result<IUIAutomationElement> {
        let condition = uia!(
            self.automation
                .CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(name))
        )?;
        uia!(self.root.FindFirst(TreeScope_Descendants, &condition))
    }

    fn find(&self, name: &str) -> IUIAutomationElement {
        self.try_find(name)
            .unwrap_or_else(|error| panic!("{name} not found: {error}"))
    }

    fn children(&self) -> Vec<IUIAutomationElement> {
        let all = uia!(self.automation.CreateTrueCondition()).expect("true condition");
        let array = uia!(self.root.FindAll(TreeScope_Children, &all)).expect("children");
        let count = uia!(array.Length()).expect("length");
        (0..count)
            .map(|index| uia!(array.GetElement(index)).expect("child"))
            .collect()
    }
}

fn name_of(element: &IUIAutomationElement) -> String {
    uia!(element.CurrentName()).expect("name").to_string()
}

fn type_of(element: &IUIAutomationElement) -> UIA_CONTROLTYPE_ID {
    uia!(element.CurrentControlType()).expect("control type")
}

fn pattern<T: Interface>(element: &IUIAutomationElement, id: UIA_PATTERN_ID) -> T {
    uia!(element.GetCurrentPatternAs::<T>(id)).expect("pattern")
}

fn code(error: &windows::core::Error) -> u32 {
    // The HRESULT bit pattern is the code.
    error.code().0 as u32
}

fn scale(fixture: &Fixture) -> f64 {
    // SAFETY: GetDpiForWindow accepts any handle.
    let dpi = unsafe { GetDpiForWindow(fixture.hwnd()) };
    if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 }
}

#[test]
fn uia_client_sees_the_window_children_with_names_roles_and_geometry() {
    let fixture = Fixture::new();
    let client = Client::new(&fixture);
    assert_eq!(type_of(&client.root), UIA_WindowControlTypeId);
    let children = client.children();
    let names: Vec<String> = children.iter().map(name_of).collect();
    assert_eq!(
        names,
        [
            "Save", "Remember", "Name", "Theme", "Zoom", "Editor", "Delete"
        ]
    );
    let types: Vec<UIA_CONTROLTYPE_ID> = children.iter().map(type_of).collect();
    assert_eq!(
        types,
        [
            UIA_ButtonControlTypeId,
            UIA_CheckBoxControlTypeId,
            UIA_EditControlTypeId,
            UIA_ComboBoxControlTypeId,
            UIA_SliderControlTypeId,
            UIA_DocumentControlTypeId,
            UIA_ButtonControlTypeId,
        ]
    );
    // The control view walker agrees with FindAll and knows the parent.
    let walker = uia!(client.automation.ControlViewWalker()).expect("walker");
    let mut walked = Vec::new();
    let mut next = uia!(walker.GetFirstChildElement(&client.root)).ok();
    while let Some(element) = next {
        walked.push(name_of(&element));
        next = uia!(walker.GetNextSiblingElement(&element)).ok();
    }
    assert_eq!(walked, names);
    let parent = uia!(walker.GetParentElement(&children[0])).expect("parent");
    assert_eq!(type_of(&parent), UIA_WindowControlTypeId);
    // Geometry: sizes and offsets are the DIP bounds scaled by the window DPI.
    let scale = scale(&fixture);
    let save = uia!(children[0].CurrentBoundingRectangle()).expect("save rect");
    let remember = uia!(children[1].CurrentBoundingRectangle()).expect("remember rect");
    let near = |actual: i32, expected: f64| (f64::from(actual) - expected).abs() <= 1.0;
    assert!(near(save.right - save.left, 80.0 * scale), "{save:?}");
    assert!(near(save.bottom - save.top, 32.0 * scale), "{save:?}");
    assert!(
        near(remember.top - save.top, 40.0 * scale),
        "{save:?} {remember:?}"
    );
    assert!(
        near(remember.left - save.left, 0.0),
        "{save:?} {remember:?}"
    );
    // State properties.
    assert!(
        uia!(children[0].CurrentIsEnabled())
            .expect("enabled")
            .as_bool()
    );
    assert!(
        !uia!(children[6].CurrentIsEnabled())
            .expect("enabled")
            .as_bool()
    );
    assert!(
        !uia!(children[0].CurrentHasKeyboardFocus())
            .expect("focus")
            .as_bool()
    );
    let id = uia!(children[0].CurrentAutomationId())
        .expect("id")
        .to_string();
    assert_eq!(id, format!("ssw-{}", fixture.ids.save.get()));
    assert!(client.try_find("Nonexistent").is_err());
}

#[test]
fn uia_client_patterns_reach_the_action_sink() {
    let fixture = Fixture::new();
    let client = Client::new(&fixture);
    let ids = fixture.ids;
    // Invoke.
    let save = client.find("Save");
    let invoke: IUIAutomationInvokePattern = pattern(&save, UIA_InvokePatternId);
    uia!(invoke.Invoke()).expect("invoke");
    assert_eq!(fixture.sink.take(), [(ids.save, AccessAction::Invoke)]);
    // Toggle: the request goes to the app; the state changes when the app publishes it.
    let remember = client.find("Remember");
    let toggle: IUIAutomationTogglePattern = pattern(&remember, UIA_TogglePatternId);
    assert_eq!(
        uia!(toggle.CurrentToggleState()).expect("state"),
        ToggleState_Off
    );
    uia!(toggle.Toggle()).expect("toggle");
    assert_eq!(fixture.sink.take(), [(ids.remember, AccessAction::Toggle)]);
    assert_eq!(
        uia!(toggle.CurrentToggleState()).expect("state"),
        ToggleState_Off
    );
    let changes = fixture.republish(&Snapshot {
        remember: NodeToggle::On,
        name: "Ada",
        focused: None,
    });
    assert!(changes > 0, "the toggle change is detected");
    assert_eq!(
        uia!(toggle.CurrentToggleState()).expect("state"),
        ToggleState_On
    );
    // Value.
    let name = client.find("Name");
    let value: IUIAutomationValuePattern = pattern(&name, UIA_ValuePatternId);
    assert_eq!(
        uia!(value.CurrentValue()).expect("value").to_string(),
        "Ada"
    );
    uia!(value.SetValue(&windows::core::BSTR::from("Grace"))).expect("set value");
    assert_eq!(
        fixture.sink.take(),
        [(ids.name, AccessAction::SetValue("Grace".to_owned()))]
    );
    // Range value: in range goes to the sink, out of range is rejected before it.
    let zoom = client.find("Zoom");
    let range: IUIAutomationRangeValuePattern = pattern(&zoom, UIA_RangeValuePatternId);
    assert_eq!(uia!(range.CurrentValue()).expect("range value"), 50.0);
    uia!(range.SetValue(75.0)).expect("set range value");
    assert_eq!(
        fixture.sink.take(),
        [(ids.zoom, AccessAction::SetRangeValue(75.0))]
    );
    assert!(uia!(range.SetValue(150.0)).is_err());
    assert!(fixture.sink.take().is_empty());
    // Expand / collapse.
    let theme = client.find("Theme");
    let expand: IUIAutomationExpandCollapsePattern = pattern(&theme, UIA_ExpandCollapsePatternId);
    assert_eq!(
        uia!(expand.CurrentExpandCollapseState()).expect("state"),
        ExpandCollapseState_Collapsed
    );
    uia!(expand.Expand()).expect("expand");
    assert_eq!(fixture.sink.take(), [(ids.theme, AccessAction::Expand)]);
}

#[test]
fn uia_client_unsupported_patterns_disabled_nodes_and_refusals_fail() {
    let fixture = Fixture::new();
    let client = Client::new(&fixture);
    // A button has no toggle pattern; a check box has no invoke pattern.
    let save = client.find("Save");
    assert!(
        uia!(save.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)).is_err()
    );
    let remember = client.find("Remember");
    assert!(
        uia!(remember.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId))
            .is_err()
    );
    // A disabled element refuses actions without involving the sink.
    let delete = client.find("Delete");
    let invoke: IUIAutomationInvokePattern = pattern(&delete, UIA_InvokePatternId);
    let error = uia!(invoke.Invoke()).expect_err("disabled");
    assert_eq!(code(&error), ELEMENT_NOT_ENABLED);
    assert!(fixture.sink.take().is_empty());
    // When the app does not support the action the client sees "not supported".
    lock(&fixture.sink.refused).push(fixture.ids.save);
    let invoke: IUIAutomationInvokePattern = pattern(&save, UIA_InvokePatternId);
    let error = uia!(invoke.Invoke()).expect_err("refused");
    assert_eq!(code(&error), NOT_SUPPORTED);
}

#[test]
fn uia_client_republished_values_and_focus_are_visible() {
    let fixture = Fixture::new();
    let client = Client::new(&fixture);
    let name = client.find("Name");
    let save = client.find("Save");
    assert!(
        !uia!(save.CurrentHasKeyboardFocus())
            .expect("focus")
            .as_bool()
    );
    let changes = fixture.republish(&Snapshot {
        remember: NodeToggle::Off,
        name: "Grace",
        focused: Some(fixture.ids.save),
    });
    assert!(changes > 0);
    assert!(
        uia!(save.CurrentHasKeyboardFocus())
            .expect("focus")
            .as_bool()
    );
    let value: IUIAutomationValuePattern = pattern(&name, UIA_ValuePatternId);
    assert_eq!(
        uia!(value.CurrentValue()).expect("value").to_string(),
        "Grace"
    );
    // An identical snapshot raises nothing.
    let again = fixture.republish(&Snapshot {
        remember: NodeToggle::Off,
        name: "Grace",
        focused: Some(fixture.ids.save),
    });
    assert_eq!(again, 0);
}

#[test]
fn uia_client_text_pattern_reads_text_selection_and_ranges() {
    let fixture = Fixture::new();
    let editor = Arc::new(Editor::default());
    fixture
        .bridge
        .set_text_source(fixture.ids.editor, editor.clone());
    let client = Client::new(&fixture);
    let element = client.find("Editor");
    let text: IUIAutomationTextPattern = pattern(&element, UIA_TextPatternId);
    // The whole document, lossless across UTF-8 → UTF-16.
    let document = uia!(text.DocumentRange()).expect("document range");
    assert_eq!(uia!(document.GetText(-1)).expect("text").to_string(), DOC);
    assert_eq!(
        uia!(document.GetText(5)).expect("text").to_string(),
        "Hello"
    );
    // The selection is "wörld": bytes 6..12, UTF-16 units 6..11.
    let selection = uia!(text.GetSelection()).expect("selection");
    assert_eq!(uia!(selection.Length()).expect("length"), 1);
    let selected = uia!(selection.GetElement(0)).expect("selected range");
    assert_eq!(
        uia!(selected.GetText(-1)).expect("text").to_string(),
        "wörld"
    );
    // Expanding to the line takes the whole visual line including its break; moving by a line
    // goes to the next one.
    let line = uia!(selected.Clone()).expect("clone");
    uia!(line.ExpandToEnclosingUnit(TextUnit_Line)).expect("expand");
    assert_eq!(
        uia!(line.GetText(-1)).expect("text").to_string(),
        "Hello wörld\n"
    );
    assert_eq!(uia!(line.Move(TextUnit_Line, 1)).expect("move"), 1);
    assert_eq!(
        uia!(line.GetText(-1)).expect("text").to_string(),
        "Second line\n"
    );
    // Ranges compare by endpoints.
    let same = uia!(selected.Clone()).expect("clone");
    assert!(uia!(selected.Compare(&same)).expect("compare").as_bool());
    assert!(!uia!(selected.Compare(&line)).expect("compare").as_bool());
    // Moving the start endpoint back six characters reaches the start of the line.
    let wider = uia!(selected.Clone()).expect("clone");
    let moved =
        uia!(wider.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, -6))
            .expect("move endpoint");
    assert_eq!(moved, -6);
    assert_eq!(
        uia!(wider.GetText(-1)).expect("text").to_string(),
        "Hello wörld"
    );
    // Selecting a range reaches the editor in byte offsets.
    uia!(selected.Select()).expect("select");
    assert_eq!(
        *lock(&editor.selected),
        [std::ops::Range { start: 6, end: 12 }]
    );
}
