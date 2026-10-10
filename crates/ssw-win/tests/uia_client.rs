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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Once, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use ssw_ui::access::{
    AccessAction, AccessNode, AccessState, AccessTree, Patterns, RangeValue, Role, ScrollInfo,
    TextPatternInfo, ToggleState as NodeToggle,
};
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::widget::WidgetId;
use ssw_win::com::ComGuard;
use ssw_win::uia::events::{PropertyValue, UiaChange, UiaProperty};
use ssw_win::uia::{ActionSink, TextSource, UiaBridge};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, SAFEARRAY};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound,
    SafeArrayGetUBound, SafeArrayUnaccessData,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
    ExpandCollapseState_LeafNode, IExpandCollapseProvider, IInvokeProvider, IRangeValueProvider,
    IRawElementProviderFragment, IRawElementProviderFragmentRoot, IRawElementProviderSimple,
    IScrollProvider, ISelectionItemProvider, ISelectionProvider, ITextProvider, ITextRangeProvider,
    IToggleProvider, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationElement,
    IUIAutomationEventHandler, IUIAutomationEventHandler_Impl, IUIAutomationExpandCollapsePattern,
    IUIAutomationInvokePattern, IUIAutomationRangeValuePattern, IUIAutomationTextPattern,
    IUIAutomationTogglePattern, IUIAutomationValuePattern, IValueProvider, NavigateDirection,
    NavigateDirection_FirstChild, NavigateDirection_LastChild, NavigateDirection_NextSibling,
    NavigateDirection_Parent, NavigateDirection_PreviousSibling,
    ProviderOptions_ServerSideProvider, ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement,
    ScrollAmount_NoAmount, ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement,
    SupportedTextSelection_Single, TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_Start,
    TextUnit_Character, TextUnit_Line, ToggleState_Indeterminate, ToggleState_Off, ToggleState_On,
    TreeScope_Children, TreeScope_Descendants, TreeScope_Subtree, UIA_AutomationIdPropertyId,
    UIA_ButtonControlTypeId, UIA_CONTROLTYPE_ID, UIA_CheckBoxControlTypeId,
    UIA_ComboBoxControlTypeId, UIA_ControlTypePropertyId, UIA_CustomControlTypeId,
    UIA_DataItemControlTypeId, UIA_DocumentControlTypeId, UIA_EVENT_ID, UIA_EditControlTypeId,
    UIA_ExpandCollapsePatternId, UIA_FontNameAttributeId, UIA_FrameworkIdPropertyId,
    UIA_GroupControlTypeId, UIA_HasKeyboardFocusPropertyId, UIA_HelpTextPropertyId,
    UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId, UIA_InvokePatternId,
    UIA_IsContentElementPropertyId, UIA_IsControlElementPropertyId, UIA_IsEnabledPropertyId,
    UIA_IsKeyboardFocusablePropertyId, UIA_IsOffscreenPropertyId, UIA_IsPasswordPropertyId,
    UIA_ListControlTypeId, UIA_ListItemControlTypeId, UIA_LocalizedControlTypePropertyId,
    UIA_MenuControlTypeId, UIA_MenuItemControlTypeId, UIA_NamePropertyId, UIA_PATTERN_ID,
    UIA_PROPERTY_ID, UIA_PaneControlTypeId, UIA_RangeValuePatternId, UIA_ScrollBarControlTypeId,
    UIA_ScrollPatternId, UIA_SelectionItemPatternId, UIA_SelectionPatternId,
    UIA_SeparatorControlTypeId, UIA_SliderControlTypeId, UIA_StatusBarControlTypeId,
    UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TableControlTypeId,
    UIA_Text_TextSelectionChangedEventId, UIA_TextControlTypeId, UIA_TextPatternId,
    UIA_TitleBarControlTypeId, UIA_TogglePatternId, UIA_ToolBarControlTypeId,
    UIA_ToolTipControlTypeId, UIA_TreeControlTypeId, UIA_TreeItemControlTypeId, UIA_ValuePatternId,
    UIA_WindowControlTypeId, UiaPoint, UiaRect,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage, WM_CLOSE, WM_DESTROY, WM_GETOBJECT,
    WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BSTR, HRESULT, IUnknown, Interface, PCWSTR, Ref, implement, w};

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

/// An editor text with hard line breaks as visual lines; records what UIA selects and scrolls
/// into view.
struct Editor {
    text: String,
    selected: Mutex<Vec<Range<usize>>>,
    scrolled: Mutex<Vec<(Range<usize>, bool)>>,
    /// When set, no point of the window is over any text.
    blind: AtomicBool,
}

impl Editor {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            selected: Mutex::default(),
            scrolled: Mutex::default(),
            blind: AtomicBool::new(false),
        }
    }
}

impl TextSource for Editor {
    fn text(&self) -> String {
        self.text.clone()
    }

    fn bounding_rects(&self, _range: Range<usize>) -> Vec<Rect> {
        vec![Rect::new(10.0, 200.0, 100.0, 20.0)]
    }

    fn offset_from_point(&self, _point: Point) -> Option<usize> {
        (!self.blind.load(Ordering::SeqCst)).then_some(0)
    }

    fn line_range(&self, offset: usize) -> Range<usize> {
        let bytes = self.text.as_bytes();
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

    fn scroll_into_view(&self, range: Range<usize>, align_to_top: bool) {
        lock(&self.scrolled).push((range, align_to_top));
    }
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
    let editor = Arc::new(Editor::new(DOC));
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

// ---- direct provider calls ---------------------------------------------------------------------
//
// The provider objects implement every pattern interface on one COM object, so the tests reach
// them through `UiaBridge::root_provider` and query the interfaces directly, without a UIA
// client in between: that is how the error results and null arguments a client never sends are
// exercised.

const E_INVALIDARG_BITS: u32 = 0x8007_0057;
const E_POINTER_BITS: u32 = 0x8000_4003;
const ELEMENT_NOT_AVAILABLE: u32 = 0x8004_0201;

const ROLES: &[(Role, UIA_CONTROLTYPE_ID)] = &[
    (Role::Window, UIA_WindowControlTypeId),
    (Role::Document, UIA_DocumentControlTypeId),
    (Role::Edit, UIA_EditControlTypeId),
    (Role::Text, UIA_TextControlTypeId),
    (Role::Button, UIA_ButtonControlTypeId),
    (Role::ToggleButton, UIA_ButtonControlTypeId),
    (Role::CheckBox, UIA_CheckBoxControlTypeId),
    (Role::ComboBox, UIA_ComboBoxControlTypeId),
    (Role::List, UIA_ListControlTypeId),
    (Role::ListItem, UIA_ListItemControlTypeId),
    (Role::Tree, UIA_TreeControlTypeId),
    (Role::TreeItem, UIA_TreeItemControlTypeId),
    (Role::Tab, UIA_TabControlTypeId),
    (Role::TabItem, UIA_TabItemControlTypeId),
    (Role::Menu, UIA_MenuControlTypeId),
    (Role::MenuItem, UIA_MenuItemControlTypeId),
    (Role::ScrollBar, UIA_ScrollBarControlTypeId),
    (Role::Slider, UIA_SliderControlTypeId),
    (Role::ToolBar, UIA_ToolBarControlTypeId),
    (Role::StatusBar, UIA_StatusBarControlTypeId),
    (Role::Pane, UIA_PaneControlTypeId),
    (Role::Group, UIA_GroupControlTypeId),
    (Role::Hyperlink, UIA_HyperlinkControlTypeId),
    (Role::Image, UIA_ImageControlTypeId),
    (Role::Table, UIA_TableControlTypeId),
    (Role::DataItem, UIA_DataItemControlTypeId),
    (Role::Separator, UIA_SeparatorControlTypeId),
    (Role::ToolTip, UIA_ToolTipControlTypeId),
    (Role::TitleBar, UIA_TitleBarControlTypeId),
    (Role::Custom, UIA_CustomControlTypeId),
];

/// Every node the provider tests need: one per pattern, a node of every role under `group`.
struct Rich {
    root: WidgetId,
    run: WidgetId,
    off: WidgetId,
    tri: WidgetId,
    plain: WidgetId,
    branch: WidgetId,
    leaf: WidgetId,
    list: WidgetId,
    alpha: WidgetId,
    beta: WidgetId,
    scroller: WidgetId,
    vscroller: WidgetId,
    secret: WidgetId,
    slider: WidgetId,
    group: WidgetId,
    roles: Vec<WidgetId>,
}

impl Rich {
    fn new() -> Self {
        Self {
            root: WidgetId::allocate(),
            run: WidgetId::allocate(),
            off: WidgetId::allocate(),
            tri: WidgetId::allocate(),
            plain: WidgetId::allocate(),
            branch: WidgetId::allocate(),
            leaf: WidgetId::allocate(),
            list: WidgetId::allocate(),
            alpha: WidgetId::allocate(),
            beta: WidgetId::allocate(),
            scroller: WidgetId::allocate(),
            vscroller: WidgetId::allocate(),
            secret: WidgetId::allocate(),
            slider: WidgetId::allocate(),
            group: WidgetId::allocate(),
            roles: ROLES.iter().map(|_| WidgetId::allocate()).collect(),
        }
    }

    fn tree(&self, focused: Option<WidgetId>) -> AccessTree {
        let mut tree = AccessTree::new(self.root);
        let node = |id, role, name: &str, x, y, w, h| {
            AccessNode::new(id, role, name, Rect::new(x, y, w, h))
        };
        let with = |patterns: Patterns| patterns;
        tree.insert(
            node(self.root, Role::Window, "Root", 0.0, 0.0, 640.0, 480.0).with_children([
                self.run,
                self.off,
                self.tri,
                self.plain,
                self.branch,
                self.leaf,
                self.list,
                self.scroller,
                self.vscroller,
                self.secret,
                self.slider,
                self.group,
            ]),
        );
        tree.insert(
            node(self.run, Role::Button, "Run", 10.0, 10.0, 80.0, 30.0).with_patterns(with(
                Patterns {
                    invoke: true,
                    ..Patterns::default()
                },
            )),
        );
        tree.insert(
            node(self.off, Role::Button, "Off", 100.0, 10.0, 80.0, 30.0)
                .with_state(AccessState {
                    disabled: true,
                    ..AccessState::default()
                })
                .with_patterns(Patterns {
                    invoke: true,
                    ..Patterns::default()
                }),
        );
        tree.insert(
            node(self.tri, Role::CheckBox, "Tri", 190.0, 10.0, 80.0, 30.0)
                .with_state(AccessState {
                    toggle: Some(NodeToggle::Indeterminate),
                    ..AccessState::default()
                })
                .with_patterns(Patterns {
                    toggle: true,
                    ..Patterns::default()
                }),
        );
        tree.insert(node(
            self.plain,
            Role::Button,
            "Plain",
            280.0,
            10.0,
            80.0,
            30.0,
        ));
        tree.insert(
            node(
                self.branch,
                Role::TreeItem,
                "Branch",
                10.0,
                50.0,
                100.0,
                20.0,
            )
            .with_state(AccessState {
                expanded: Some(true),
                ..AccessState::default()
            })
            .with_patterns(Patterns {
                expand_collapse: true,
                ..Patterns::default()
            }),
        );
        tree.insert(
            node(self.leaf, Role::TreeItem, "Leaf", 120.0, 50.0, 100.0, 20.0).with_patterns(
                Patterns {
                    expand_collapse: true,
                    ..Patterns::default()
                },
            ),
        );
        tree.insert(
            node(self.list, Role::List, "Items", 10.0, 80.0, 200.0, 70.0)
                .with_children([self.alpha, self.beta]),
        );
        tree.insert(
            node(self.alpha, Role::ListItem, "Alpha", 10.0, 80.0, 200.0, 30.0)
                .with_state(AccessState {
                    selected: true,
                    ..AccessState::default()
                })
                .with_patterns(Patterns {
                    selection_item: true,
                    ..Patterns::default()
                }),
        );
        tree.insert(
            node(self.beta, Role::ListItem, "Beta", 10.0, 110.0, 200.0, 30.0).with_patterns(
                Patterns {
                    selection_item: true,
                    ..Patterns::default()
                },
            ),
        );
        let scroll = |info: ScrollInfo| Patterns {
            scroll: Some(info),
            ..Patterns::default()
        };
        tree.insert(
            node(
                self.scroller,
                Role::Pane,
                "Scroller",
                230.0,
                80.0,
                200.0,
                60.0,
            )
            .with_patterns(scroll(ScrollInfo {
                horizontal_percent: 25.0,
                vertical_percent: 50.0,
                horizontal_view_percent: 50.0,
                vertical_view_percent: 20.0,
                horizontally_scrollable: true,
                vertically_scrollable: true,
            })),
        );
        tree.insert(
            node(
                self.vscroller,
                Role::Pane,
                "VScroller",
                230.0,
                150.0,
                200.0,
                60.0,
            )
            .with_patterns(scroll(ScrollInfo {
                horizontal_percent: 0.0,
                vertical_percent: 10.0,
                horizontal_view_percent: 100.0,
                vertical_view_percent: 50.0,
                horizontally_scrollable: false,
                vertically_scrollable: true,
            })),
        );
        tree.insert(
            node(self.secret, Role::Edit, "Secret", 10.0, 160.0, 200.0, 30.0)
                .with_description("Only for you")
                .with_value("hunter2")
                .with_state(AccessState {
                    focusable: true,
                    focused: focused == Some(self.secret),
                    read_only: true,
                    offscreen: true,
                    password: true,
                    ..AccessState::default()
                })
                .with_patterns(Patterns {
                    value: true,
                    ..Patterns::default()
                }),
        );
        tree.insert(
            node(self.slider, Role::Slider, "Level", 10.0, 200.0, 200.0, 24.0)
                .with_state(AccessState {
                    read_only: true,
                    ..AccessState::default()
                })
                .with_patterns(Patterns {
                    range_value: Some(RangeValue {
                        value: 5.0,
                        min: 0.0,
                        max: 10.0,
                        step: 0.5,
                    }),
                    ..Patterns::default()
                }),
        );
        tree.insert(
            node(self.group, Role::Group, "Roles", 10.0, 240.0, 600.0, 200.0)
                .with_children(self.roles.iter().copied()),
        );
        for (index, ((role, _), id)) in ROLES.iter().zip(&self.roles).enumerate() {
            let x = 10.0 + (index % 10) as f32 * 55.0;
            let y = 250.0 + (index / 10) as f32 * 30.0;
            tree.insert(node(
                *id,
                *role,
                &format!("role:{role:?}"),
                x,
                y,
                50.0,
                25.0,
            ));
        }
        tree
    }

    /// A tree that kept only the root.
    fn stripped(&self) -> AccessTree {
        let mut tree = AccessTree::new(self.root);
        tree.insert(AccessNode::new(
            self.root,
            Role::Window,
            "Root",
            Rect::new(0.0, 0.0, 640.0, 480.0),
        ));
        tree
    }
}

fn publish(fixture: &Fixture, tree: AccessTree) {
    fixture.bridge.update(tree).expect("snapshot published");
}

fn property(provider: &IRawElementProviderSimple, id: UIA_PROPERTY_ID) -> VARIANT {
    uia!(provider.GetPropertyValue(id)).expect("property value")
}

fn text_property(provider: &IRawElementProviderSimple, id: UIA_PROPERTY_ID) -> String {
    BSTR::try_from(&property(provider, id))
        .expect("a string")
        .to_string()
}

fn flag_property(provider: &IRawElementProviderSimple, id: UIA_PROPERTY_ID) -> bool {
    bool::try_from(&property(provider, id)).expect("a boolean")
}

fn int_property(provider: &IRawElementProviderSimple, id: UIA_PROPERTY_ID) -> i32 {
    i32::try_from(&property(provider, id)).expect("an integer")
}

fn name_of_provider(provider: &IRawElementProviderSimple) -> String {
    text_property(provider, UIA_NamePropertyId)
}

fn fragment_of(provider: &IRawElementProviderSimple) -> IRawElementProviderFragment {
    provider.cast().expect("the provider is a fragment")
}

fn navigate(
    provider: &IRawElementProviderSimple,
    direction: NavigateDirection,
) -> windows::core::Result<IRawElementProviderSimple> {
    uia!(fragment_of(provider).Navigate(direction)).map(|found| found.cast().expect("simple"))
}

fn child_named(parent: &IRawElementProviderSimple, name: &str) -> IRawElementProviderSimple {
    let mut next = navigate(parent, NavigateDirection_FirstChild).ok();
    while let Some(child) = next {
        if name_of_provider(&child) == name {
            return child;
        }
        next = navigate(&child, NavigateDirection_NextSibling).ok();
    }
    panic!("no child named {name}");
}

/// "No object, no error": S_OK with a null result.
fn assert_no_object<T: std::fmt::Debug>(result: windows::core::Result<T>) {
    let error = result.expect_err("no object");
    assert_eq!(error.code(), HRESULT(0));
}

fn assert_code<T: std::fmt::Debug>(result: windows::core::Result<T>, bits: u32) {
    let error = result.expect_err("the call fails");
    assert_eq!(code(&error), bits);
}

fn array_len(array: *mut SAFEARRAY) -> usize {
    let lower = uia!(SafeArrayGetLBound(array, 1)).expect("lower bound");
    let upper = uia!(SafeArrayGetUBound(array, 1)).expect("upper bound");
    usize::try_from(upper - lower + 1).expect("a length")
}

/// The values of a SAFEARRAY of plain values; destroys the array.
fn take_values<T: Copy>(array: *mut SAFEARRAY) -> Vec<T> {
    let len = array_len(array);
    let mut values = Vec::new();
    if len > 0 {
        let mut data: *mut c_void = std::ptr::null_mut();
        uia!(SafeArrayAccessData(array, &mut data)).expect("data");
        // SAFETY: the locked vector holds `len` elements of type `T`.
        values = unsafe { std::slice::from_raw_parts(data.cast::<T>(), len) }.to_vec();
        uia!(SafeArrayUnaccessData(array)).expect("unlock");
    }
    uia!(SafeArrayDestroy(array)).expect("destroy");
    values
}

/// The interfaces of a SAFEARRAY of `IUnknown`; destroys the array.
fn take_interfaces<I: Interface>(array: *mut SAFEARRAY) -> Vec<I> {
    let items = (0..array_len(array))
        .map(|position| {
            let index = i32::try_from(position).expect("a small index");
            let mut element: Option<IUnknown> = None;
            uia!(SafeArrayGetElement(
                array,
                &index,
                (&raw mut element).cast::<c_void>()
            ))
            .expect("element");
            element
                .expect("an element")
                .cast::<I>()
                .expect("the element's interface")
        })
        .collect();
    uia!(SafeArrayDestroy(array)).expect("destroy");
    items
}

fn client_origin(fixture: &Fixture) -> POINT {
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: `origin` is a valid POINT and the handle is the fixture's window.
    let _ = unsafe { ClientToScreen(fixture.hwnd(), &mut origin) };
    origin
}

fn near(actual: f64, expected: f64) -> bool {
    (actual - expected).abs() < 1e-3
}

#[test]
fn uia_provider_roles_map_to_control_types_and_only_containers_offer_selection() {
    let fixture = Fixture::new();
    let rich = Rich::new();
    publish(&fixture, rich.tree(None));
    let root = fixture.bridge.root_provider();
    let group = child_named(&root, "Roles");
    let containers = [
        Role::List,
        Role::Tree,
        Role::Tab,
        Role::ComboBox,
        Role::Menu,
    ];
    for (role, control_type) in ROLES {
        let node = child_named(&group, &format!("role:{role:?}"));
        assert_eq!(
            int_property(&node, UIA_ControlTypePropertyId),
            control_type.0,
            "{role:?}"
        );
        let selection = uia!(node.GetPatternProvider(UIA_SelectionPatternId));
        assert_eq!(selection.is_ok(), containers.contains(role), "{role:?}");
        // No other pattern is declared.
        for pattern in [
            UIA_InvokePatternId,
            UIA_TogglePatternId,
            UIA_ExpandCollapsePatternId,
            UIA_SelectionItemPatternId,
            UIA_ValuePatternId,
            UIA_RangeValuePatternId,
            UIA_ScrollPatternId,
            UIA_TextPatternId,
        ] {
            assert_no_object(uia!(node.GetPatternProvider(pattern)));
        }
    }
}

#[test]
fn uia_provider_properties_report_the_node_state() {
    let fixture = Fixture::new();
    let rich = Rich::new();
    publish(&fixture, rich.tree(Some(rich.secret)));
    let root = fixture.bridge.root_provider();
    assert_eq!(
        uia!(root.ProviderOptions()).expect("options"),
        ProviderOptions_ServerSideProvider
    );
    let secret = child_named(&root, "Secret");
    assert_eq!(
        int_property(&secret, UIA_ControlTypePropertyId),
        UIA_EditControlTypeId.0
    );
    assert_eq!(
        text_property(&secret, UIA_AutomationIdPropertyId),
        format!("ssw-{}", rich.secret.get())
    );
    assert!(flag_property(&secret, UIA_HasKeyboardFocusPropertyId));
    assert!(flag_property(&secret, UIA_IsKeyboardFocusablePropertyId));
    assert!(flag_property(&secret, UIA_IsEnabledPropertyId));
    assert!(flag_property(&secret, UIA_IsOffscreenPropertyId));
    assert!(flag_property(&secret, UIA_IsPasswordPropertyId));
    assert!(flag_property(&secret, UIA_IsControlElementPropertyId));
    assert!(flag_property(&secret, UIA_IsContentElementPropertyId));
    assert_eq!(
        text_property(&secret, UIA_FrameworkIdPropertyId),
        "SSWriter"
    );
    assert_eq!(
        text_property(&secret, UIA_HelpTextPropertyId),
        "Only for you"
    );
    // A node without description or state: UIA falls back to its defaults.
    let plain = child_named(&root, "Plain");
    assert!(property(&plain, UIA_HelpTextPropertyId).is_empty());
    assert!(property(&plain, UIA_LocalizedControlTypePropertyId).is_empty());
    assert!(!flag_property(&plain, UIA_IsPasswordPropertyId));
    assert!(!flag_property(&plain, UIA_IsOffscreenPropertyId));
    assert!(!flag_property(&plain, UIA_HasKeyboardFocusPropertyId));
    assert!(!flag_property(&plain, UIA_IsKeyboardFocusablePropertyId));
    let off = child_named(&root, "Off");
    assert!(!flag_property(&off, UIA_IsEnabledPropertyId));
}

#[test]
fn uia_provider_navigation_geometry_focus_and_hit_testing() {
    let fixture = Fixture::new();
    let rich = Rich::new();
    publish(&fixture, rich.tree(Some(rich.secret)));
    let root = fixture.bridge.root_provider();
    let first = navigate(&root, NavigateDirection_FirstChild).expect("first child");
    assert_eq!(name_of_provider(&first), "Run");
    let last = navigate(&root, NavigateDirection_LastChild).expect("last child");
    assert_eq!(name_of_provider(&last), "Roles");
    let second = navigate(&first, NavigateDirection_NextSibling).expect("next sibling");
    assert_eq!(name_of_provider(&second), "Off");
    let back = navigate(&second, NavigateDirection_PreviousSibling).expect("previous sibling");
    assert_eq!(name_of_provider(&back), "Run");
    let parent = navigate(&first, NavigateDirection_Parent).expect("parent");
    assert_eq!(name_of_provider(&parent), "Root");
    assert_no_object(navigate(&first, NavigateDirection_PreviousSibling));
    assert_no_object(navigate(&last, NavigateDirection_NextSibling));
    assert_no_object(navigate(&root, NavigateDirection_Parent));
    assert_no_object(navigate(&first, NavigateDirection_FirstChild));
    assert_no_object(navigate(&first, NavigateDirection(99)));
    // Runtime ids: the window host provides the root's, every other node appends its own.
    assert!(
        uia!(fragment_of(&root).GetRuntimeId())
            .expect("root runtime id")
            .is_null()
    );
    let id = rich.run.get();
    let runtime = take_values::<i32>(uia!(fragment_of(&first).GetRuntimeId()).expect("runtime id"));
    assert_eq!(runtime, [3, (id & 0xFFFF_FFFF) as i32, (id >> 32) as i32]);
    assert!(
        uia!(fragment_of(&first).GetEmbeddedFragmentRoots())
            .expect("no embedded roots")
            .is_null()
    );
    // Geometry in screen pixels.
    let (origin, scale) = (client_origin(&fixture), scale(&fixture));
    let beta = child_named(&child_named(&root, "Items"), "Beta");
    let rect: UiaRect = uia!(fragment_of(&beta).BoundingRectangle()).expect("rectangle");
    assert!(
        near(rect.left, f64::from(origin.x) + 10.0 * scale),
        "{rect:?}"
    );
    assert!(
        near(rect.top, f64::from(origin.y) + 110.0 * scale),
        "{rect:?}"
    );
    assert!(near(rect.width, 200.0 * scale), "{rect:?}");
    assert!(near(rect.height, 30.0 * scale), "{rect:?}");
    // The fragment root of any node is the window.
    let window = uia!(fragment_of(&beta).FragmentRoot()).expect("fragment root");
    let window: IRawElementProviderSimple = window.cast().expect("simple");
    assert_eq!(name_of_provider(&window), "Root");
    // Hit testing converts screen pixels to DIPs.
    let from_point = |x: f32, y: f32| {
        let screen = root
            .cast::<IRawElementProviderFragmentRoot>()
            .expect("root");
        uia!(screen.ElementProviderFromPoint(
            f64::from(origin.x) + f64::from(x) * scale,
            f64::from(origin.y) + f64::from(y) * scale
        ))
        .map(|found| name_of_provider(&found.cast().expect("simple")))
    };
    assert_eq!(from_point(110.0, 125.0).expect("over Beta"), "Beta");
    assert_eq!(from_point(600.0, 60.0).expect("over the window"), "Root");
    assert_no_object(from_point(700.0, 10.0));
    // The focus is the focused node, if any.
    let screen = root
        .cast::<IRawElementProviderFragmentRoot>()
        .expect("root");
    let focused = uia!(screen.GetFocus()).expect("focus");
    assert_eq!(name_of_provider(&focused.cast().expect("simple")), "Secret");
    // The host provider belongs to the window alone.
    assert!(uia!(root.HostRawElementProvider()).is_ok());
    assert_no_object(uia!(first.HostRawElementProvider()));
    // Focus requests reach the sink, unless the node is disabled.
    uia!(fragment_of(&first).SetFocus()).expect("focus request");
    assert_eq!(fixture.sink.take(), [(rich.run, AccessAction::Focus)]);
    assert_code(uia!(fragment_of(&second).SetFocus()), ELEMENT_NOT_ENABLED);
    assert!(fixture.sink.take().is_empty());
    publish(&fixture, rich.tree(None));
    assert_no_object(uia!(screen.GetFocus()));
}

#[test]
fn uia_provider_patterns_read_the_node_and_post_actions() {
    let fixture = Fixture::new();
    let rich = Rich::new();
    publish(&fixture, rich.tree(None));
    let root = fixture.bridge.root_provider();
    let sink = &fixture.sink;
    // Toggle.
    let tri = child_named(&root, "Tri");
    let toggle: IToggleProvider = tri.cast().expect("toggle");
    assert_eq!(
        uia!(toggle.ToggleState()).expect("state"),
        ToggleState_Indeterminate
    );
    uia!(toggle.Toggle()).expect("toggle");
    assert_eq!(sink.take(), [(rich.tri, AccessAction::Toggle)]);
    let plain = child_named(&root, "Plain");
    assert_code(
        uia!(
            plain
                .cast::<IToggleProvider>()
                .expect("toggle")
                .ToggleState()
        ),
        NOT_SUPPORTED,
    );
    // Expand / collapse.
    let branch = child_named(&root, "Branch");
    let expand: IExpandCollapseProvider = branch.cast().expect("expand");
    assert_eq!(
        uia!(expand.ExpandCollapseState()).expect("state"),
        ExpandCollapseState_Expanded
    );
    uia!(expand.Collapse()).expect("collapse");
    assert_eq!(sink.take(), [(rich.branch, AccessAction::Collapse)]);
    let leaf = child_named(&root, "Leaf");
    let leaf_expand: IExpandCollapseProvider = leaf.cast().expect("expand");
    assert_eq!(
        uia!(leaf_expand.ExpandCollapseState()).expect("state"),
        ExpandCollapseState_LeafNode
    );
    // Selection items and their container.
    let items = child_named(&root, "Items");
    let alpha = child_named(&items, "Alpha");
    let beta = child_named(&items, "Beta");
    let alpha_item: ISelectionItemProvider = alpha.cast().expect("item");
    let beta_item: ISelectionItemProvider = beta.cast().expect("item");
    assert!(uia!(alpha_item.IsSelected()).expect("selected").as_bool());
    assert!(!uia!(beta_item.IsSelected()).expect("selected").as_bool());
    uia!(beta_item.Select()).expect("select");
    uia!(beta_item.AddToSelection()).expect("add");
    assert_eq!(
        sink.take(),
        [
            (rich.beta, AccessAction::Select),
            (rich.beta, AccessAction::Select)
        ]
    );
    assert_code(uia!(beta_item.RemoveFromSelection()), NOT_SUPPORTED);
    let container = uia!(alpha_item.SelectionContainer()).expect("container");
    assert_eq!(name_of_provider(&container), "Items");
    assert_no_object(uia!(
        root.cast::<ISelectionItemProvider>()
            .expect("item")
            .SelectionContainer()
    ));
    let selection: ISelectionProvider = items.cast().expect("selection");
    let selected = take_interfaces::<IRawElementProviderSimple>(
        uia!(selection.GetSelection()).expect("selection"),
    );
    assert_eq!(
        selected.iter().map(name_of_provider).collect::<Vec<_>>(),
        ["Alpha"]
    );
    assert!(
        !uia!(selection.CanSelectMultiple())
            .expect("multiple")
            .as_bool()
    );
    assert!(
        !uia!(selection.IsSelectionRequired())
            .expect("required")
            .as_bool()
    );
    // A container without selected children selects nothing.
    let roles = child_named(&root, "Roles");
    let empty_list: ISelectionProvider =
        child_named(&roles, "role:List").cast().expect("selection");
    assert!(
        take_interfaces::<IRawElementProviderSimple>(
            uia!(empty_list.GetSelection()).expect("selection")
        )
        .is_empty()
    );
    // Value.
    let secret = child_named(&root, "Secret");
    let value: IValueProvider = secret.cast().expect("value");
    assert_eq!(uia!(value.Value()).expect("value").to_string(), "hunter2");
    assert!(uia!(value.IsReadOnly()).expect("read only").as_bool());
    uia!(value.SetValue(w!("changed"))).expect("set value");
    assert_eq!(
        sink.take(),
        [(rich.secret, AccessAction::SetValue("changed".to_owned()))]
    );
    // A lone surrogate is not text.
    let lone = [0xD800u16, 0];
    assert_code(
        uia!(value.SetValue(PCWSTR(lone.as_ptr()))),
        E_INVALIDARG_BITS,
    );
    assert!(sink.take().is_empty());
    let plain_value: IValueProvider = plain.cast().expect("value");
    assert_eq!(uia!(plain_value.Value()).expect("value").to_string(), "");
    assert!(!uia!(plain_value.IsReadOnly()).expect("read only").as_bool());
    // Range value.
    let slider = child_named(&root, "Level");
    let range: IRangeValueProvider = slider.cast().expect("range");
    assert_eq!(uia!(range.Value()).expect("value"), 5.0);
    assert_eq!(uia!(range.Minimum()).expect("minimum"), 0.0);
    assert_eq!(uia!(range.Maximum()).expect("maximum"), 10.0);
    assert_eq!(uia!(range.LargeChange()).expect("large change"), 1.0);
    assert_eq!(uia!(range.SmallChange()).expect("small change"), 0.5);
    assert!(uia!(range.IsReadOnly()).expect("read only").as_bool());
    uia!(range.SetValue(7.5)).expect("in range");
    assert_eq!(
        sink.take(),
        [(rich.slider, AccessAction::SetRangeValue(7.5))]
    );
    assert_code(uia!(range.SetValue(10.5)), E_INVALIDARG_BITS);
    assert_code(uia!(range.SetValue(-0.5)), E_INVALIDARG_BITS);
    let none: IRangeValueProvider = plain.cast().expect("range");
    assert_code(uia!(none.Value()), NOT_SUPPORTED);
    assert_code(uia!(none.Minimum()), NOT_SUPPORTED);
    assert_code(uia!(none.Maximum()), NOT_SUPPORTED);
    assert_code(uia!(none.LargeChange()), NOT_SUPPORTED);
    assert_code(uia!(none.SmallChange()), NOT_SUPPORTED);
    assert_code(uia!(none.SetValue(1.0)), NOT_SUPPORTED);
    // The patterns a node declares are offered, nothing else.
    for (node, pattern) in [
        (&child_named(&root, "Run"), UIA_InvokePatternId),
        (&tri, UIA_TogglePatternId),
        (&branch, UIA_ExpandCollapsePatternId),
        (&alpha, UIA_SelectionItemPatternId),
        (&items, UIA_SelectionPatternId),
        (&secret, UIA_ValuePatternId),
        (&slider, UIA_RangeValuePatternId),
        (&child_named(&root, "Scroller"), UIA_ScrollPatternId),
    ] {
        assert!(uia!(node.GetPatternProvider(pattern)).is_ok());
    }
    let invoke: IInvokeProvider = child_named(&root, "Run").cast().expect("invoke");
    uia!(invoke.Invoke()).expect("invoke");
    assert_eq!(sink.take(), [(rich.run, AccessAction::Invoke)]);
}

#[test]
fn uia_provider_scroll_pattern_reads_and_requests_percentages() {
    let fixture = Fixture::new();
    let rich = Rich::new();
    publish(&fixture, rich.tree(None));
    let root = fixture.bridge.root_provider();
    let scroller: IScrollProvider = child_named(&root, "Scroller").cast().expect("scroll");
    let vscroller: IScrollProvider = child_named(&root, "VScroller").cast().expect("scroll");
    assert_eq!(
        uia!(scroller.HorizontalScrollPercent()).expect("percent"),
        25.0
    );
    assert_eq!(
        uia!(scroller.VerticalScrollPercent()).expect("percent"),
        50.0
    );
    assert_eq!(uia!(scroller.HorizontalViewSize()).expect("size"), 50.0);
    assert_eq!(uia!(scroller.VerticalViewSize()).expect("size"), 20.0);
    assert!(
        uia!(scroller.HorizontallyScrollable())
            .expect("flag")
            .as_bool()
    );
    assert!(
        uia!(scroller.VerticallyScrollable())
            .expect("flag")
            .as_bool()
    );
    // An axis that does not scroll reports -1.
    assert_eq!(
        uia!(vscroller.HorizontalScrollPercent()).expect("percent"),
        -1.0
    );
    assert_eq!(
        uia!(vscroller.VerticalScrollPercent()).expect("percent"),
        10.0
    );
    assert!(
        !uia!(vscroller.HorizontallyScrollable())
            .expect("flag")
            .as_bool()
    );
    // A third of a page-sized step: pages are the visible share against the scrollable range.
    uia!(scroller.Scroll(ScrollAmount_SmallIncrement, ScrollAmount_LargeIncrement))
        .expect("scroll");
    uia!(scroller.Scroll(ScrollAmount_LargeDecrement, ScrollAmount_SmallDecrement))
        .expect("scroll");
    uia!(scroller.Scroll(ScrollAmount_NoAmount, ScrollAmount_NoAmount)).expect("scroll");
    uia!(vscroller.Scroll(ScrollAmount_LargeIncrement, ScrollAmount_LargeIncrement))
        .expect("scroll");
    assert_eq!(
        fixture.sink.take(),
        [
            (
                rich.scroller,
                AccessAction::ScrollTo {
                    horizontal: Some(50.0),
                    vertical: Some(75.0)
                }
            ),
            (
                rich.scroller,
                AccessAction::ScrollTo {
                    horizontal: Some(0.0),
                    vertical: Some(43.75)
                }
            ),
            (
                rich.vscroller,
                AccessAction::ScrollTo {
                    horizontal: None,
                    vertical: Some(100.0)
                }
            ),
        ]
    );
    uia!(scroller.SetScrollPercent(40.0, 60.0)).expect("set");
    uia!(scroller.SetScrollPercent(-1.0, 30.0)).expect("set");
    assert_eq!(
        fixture.sink.take(),
        [
            (
                rich.scroller,
                AccessAction::ScrollTo {
                    horizontal: Some(40.0),
                    vertical: Some(60.0)
                }
            ),
            (
                rich.scroller,
                AccessAction::ScrollTo {
                    horizontal: None,
                    vertical: Some(30.0)
                }
            ),
        ]
    );
    assert_code(
        uia!(vscroller.SetScrollPercent(10.0, 20.0)),
        E_INVALIDARG_BITS,
    );
    assert_code(
        uia!(scroller.SetScrollPercent(-1.0, 120.0)),
        E_INVALIDARG_BITS,
    );
    assert!(fixture.sink.take().is_empty());
    let plain: IScrollProvider = child_named(&root, "Plain").cast().expect("scroll");
    assert_code(uia!(plain.HorizontalScrollPercent()), NOT_SUPPORTED);
    assert_code(
        uia!(plain.Scroll(ScrollAmount_NoAmount, ScrollAmount_NoAmount)),
        NOT_SUPPORTED,
    );
    assert_code(uia!(plain.SetScrollPercent(0.0, 0.0)), NOT_SUPPORTED);
}

#[test]
fn uia_providers_of_nodes_that_left_the_tree_report_element_not_available() {
    let fixture = Fixture::new();
    let rich = Rich::new();
    publish(&fixture, rich.tree(None));
    let root = fixture.bridge.root_provider();
    let run = child_named(&root, "Run");
    let tri = child_named(&root, "Tri");
    let items = child_named(&root, "Items");
    let alpha = child_named(&items, "Alpha");
    let secret = child_named(&root, "Secret");
    let slider = child_named(&root, "Level");
    let scroller = child_named(&root, "Scroller");
    let branch = child_named(&root, "Branch");
    publish(&fixture, rich.stripped());
    assert_code(
        uia!(run.GetPatternProvider(UIA_InvokePatternId)),
        ELEMENT_NOT_AVAILABLE,
    );
    assert_code(
        uia!(run.GetPropertyValue(UIA_NamePropertyId)),
        ELEMENT_NOT_AVAILABLE,
    );
    assert_code(
        navigate(&run, NavigateDirection_Parent),
        ELEMENT_NOT_AVAILABLE,
    );
    assert_code(
        uia!(fragment_of(&run).BoundingRectangle()),
        ELEMENT_NOT_AVAILABLE,
    );
    assert_code(uia!(fragment_of(&run).SetFocus()), ELEMENT_NOT_AVAILABLE);
    assert_code(
        uia!(run.cast::<IInvokeProvider>().expect("invoke").Invoke()),
        ELEMENT_NOT_AVAILABLE,
    );
    assert_code(
        uia!(tri.cast::<IToggleProvider>().expect("toggle").ToggleState()),
        ELEMENT_NOT_AVAILABLE,
    );
    assert_code(
        uia!(
            branch
                .cast::<IExpandCollapseProvider>()
                .expect("expand")
                .ExpandCollapseState()
        ),
        ELEMENT_NOT_AVAILABLE,
    );
    let item: ISelectionItemProvider = alpha.cast().expect("item");
    assert_code(uia!(item.IsSelected()), ELEMENT_NOT_AVAILABLE);
    assert_code(uia!(item.SelectionContainer()), ELEMENT_NOT_AVAILABLE);
    assert_code(
        uia!(
            items
                .cast::<ISelectionProvider>()
                .expect("selection")
                .GetSelection()
        ),
        ELEMENT_NOT_AVAILABLE,
    );
    let value: IValueProvider = secret.cast().expect("value");
    assert_code(uia!(value.Value()), ELEMENT_NOT_AVAILABLE);
    assert_code(uia!(value.IsReadOnly()), ELEMENT_NOT_AVAILABLE);
    let range: IRangeValueProvider = slider.cast().expect("range");
    assert_code(uia!(range.Value()), ELEMENT_NOT_AVAILABLE);
    assert_code(uia!(range.IsReadOnly()), ELEMENT_NOT_AVAILABLE);
    assert_code(
        uia!(
            scroller
                .cast::<IScrollProvider>()
                .expect("scroll")
                .VerticalScrollPercent()
        ),
        ELEMENT_NOT_AVAILABLE,
    );
}

#[test]
fn uia_text_provider_ranges_compare_move_scroll_and_report_geometry() {
    let fixture = Fixture::new();
    let element = child_named(&fixture.bridge.root_provider(), "Editor");
    let text: ITextProvider = element.cast().expect("text");
    // Without a registered source the editor offers no text.
    assert_code(uia!(text.DocumentRange()), NOT_SUPPORTED);
    assert_code(uia!(text.GetSelection()), NOT_SUPPORTED);
    let button: ITextProvider = child_named(&fixture.bridge.root_provider(), "Save")
        .cast()
        .expect("text");
    assert_code(uia!(button.GetSelection()), NOT_SUPPORTED);
    assert_code(uia!(button.GetVisibleRanges()), NOT_SUPPORTED);
    let editor = Arc::new(Editor::new(DOC));
    fixture
        .bridge
        .set_text_source(fixture.ids.editor, editor.clone());
    assert_eq!(
        uia!(text.SupportedTextSelection()).expect("selection mode"),
        SupportedTextSelection_Single
    );
    let whole = uia!(text.DocumentRange()).expect("document range");
    let get = |range: &ITextRangeProvider| uia!(range.GetText(-1)).expect("text").to_string();
    assert_eq!(get(&whole), DOC);
    let visible =
        take_interfaces::<ITextRangeProvider>(uia!(text.GetVisibleRanges()).expect("visible"));
    assert_eq!(visible.iter().map(get).collect::<Vec<_>>(), [DOC]);
    let from_child =
        uia!(text.RangeFromChild(None::<&IRawElementProviderSimple>)).expect("child range");
    assert_eq!(get(&from_child), DOC);
    let selected =
        take_interfaces::<ITextRangeProvider>(uia!(text.GetSelection()).expect("selection"))
            .remove(0);
    assert_eq!(get(&selected), "wörld");
    // A point over the text is an empty range at the offset the editor reports.
    let at_point = uia!(text.RangeFromPoint(UiaPoint { x: 0.0, y: 0.0 })).expect("point range");
    assert_eq!(get(&at_point), "");
    assert_eq!(
        uia!(at_point.CompareEndpoints(
            TextPatternRangeEndpoint_End,
            &whole,
            TextPatternRangeEndpoint_End
        ))
        .expect("compare"),
        -1
    );
    editor.blind.store(true, Ordering::SeqCst);
    assert_no_object(uia!(text.RangeFromPoint(UiaPoint { x: 0.0, y: 0.0 })));
    // Comparisons.
    let endpoints = |a, b| uia!(selected.CompareEndpoints(a, &whole, b)).expect("compare");
    assert_eq!(
        endpoints(
            TextPatternRangeEndpoint_Start,
            TextPatternRangeEndpoint_Start
        ),
        1
    );
    assert_eq!(
        endpoints(TextPatternRangeEndpoint_End, TextPatternRangeEndpoint_End),
        -1
    );
    assert_eq!(
        uia!(selected.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            &selected,
            TextPatternRangeEndpoint_Start
        ))
        .expect("compare"),
        0
    );
    assert_code(
        uia!(selected.Compare(None::<&ITextRangeProvider>)),
        E_POINTER_BITS,
    );
    assert_code(
        uia!(selected.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            None::<&ITextRangeProvider>,
            TextPatternRangeEndpoint_Start
        )),
        E_POINTER_BITS,
    );
    // Text, limited by length.
    assert_eq!(uia!(selected.GetText(3)).expect("text").to_string(), "wör");
    assert_eq!(uia!(selected.GetText(0)).expect("text").to_string(), "");
    assert_code(uia!(selected.GetText(-5)), E_INVALIDARG_BITS);
    // Geometry: the editor's rectangle in screen pixels.
    let (origin, scale) = (client_origin(&fixture), scale(&fixture));
    let rects = take_values::<f64>(uia!(selected.GetBoundingRectangles()).expect("rectangles"));
    assert_eq!(rects.len(), 4);
    assert!(
        near(rects[0], f64::from(origin.x) + 10.0 * scale),
        "{rects:?}"
    );
    assert!(
        near(rects[1], f64::from(origin.y) + 200.0 * scale),
        "{rects:?}"
    );
    assert!(
        near(rects[2], 100.0 * scale) && near(rects[3], 20.0 * scale),
        "{rects:?}"
    );
    let enclosing = uia!(selected.GetEnclosingElement()).expect("element");
    assert_eq!(name_of_provider(&enclosing), "Editor");
    assert_eq!(
        array_len(uia!(selected.GetChildren()).expect("children")),
        0
    );
    // Moving by lines.
    let line = uia!(selected.Clone()).expect("clone");
    uia!(line.ExpandToEnclosingUnit(TextUnit_Line)).expect("expand");
    assert_eq!(get(&line), "Hello wörld\n");
    assert_eq!(uia!(line.Move(TextUnit_Line, 0)).expect("move"), 0);
    assert_eq!(uia!(line.Move(TextUnit_Line, 2)).expect("move"), 2);
    assert_eq!(get(&line), "Third 👍 end");
    // Moving past the last line stays on it.
    uia!(line.Move(TextUnit_Line, 1)).expect("move");
    assert_eq!(get(&line), "Third 👍 end");
    assert_eq!(uia!(line.Move(TextUnit_Line, -1)).expect("move"), -1);
    assert_eq!(get(&line), "Second line\n");
    // Moving endpoints by unit and to other ranges.
    let longer = uia!(selected.Clone()).expect("clone");
    assert_eq!(
        uia!(longer.MoveEndpointByUnit(TextPatternRangeEndpoint_End, TextUnit_Character, 2))
            .expect("move"),
        2
    );
    assert_eq!(get(&longer), "wörld\nS");
    let widened = uia!(selected.Clone()).expect("clone");
    uia!(widened.MoveEndpointByRange(
        TextPatternRangeEndpoint_Start,
        &whole,
        TextPatternRangeEndpoint_Start
    ))
    .expect("move");
    assert_eq!(get(&widened), "Hello wörld");
    uia!(widened.MoveEndpointByRange(
        TextPatternRangeEndpoint_End,
        &whole,
        TextPatternRangeEndpoint_End
    ))
    .expect("move");
    assert_eq!(get(&widened), DOC);
    assert_code(
        uia!(widened.MoveEndpointByRange(
            TextPatternRangeEndpoint_End,
            None::<&ITextRangeProvider>,
            TextPatternRangeEndpoint_End
        )),
        E_POINTER_BITS,
    );
    // Scrolling reaches the editor in byte offsets; other operations are not supported.
    uia!(selected.ScrollIntoView(true)).expect("scroll");
    uia!(selected.ScrollIntoView(false)).expect("scroll");
    assert_eq!(*lock(&editor.scrolled), [(6..12, true), (6..12, false)]);
    assert_code(uia!(selected.AddToSelection()), NOT_SUPPORTED);
    assert_code(uia!(selected.RemoveFromSelection()), NOT_SUPPORTED);
    assert_code(
        uia!(selected.FindAttribute(UIA_FontNameAttributeId, &VARIANT::default(), false)),
        NOT_SUPPORTED,
    );
    assert_code(
        uia!(selected.GetAttributeValue(UIA_FontNameAttributeId)),
        NOT_SUPPORTED,
    );
}

#[test]
fn uia_text_find_matches_case_insensitively_and_exactly_when_folding_changes_lengths() {
    let fixture = Fixture::new();
    let element = child_named(&fixture.bridge.root_provider(), "Editor");
    let text: ITextProvider = element.cast().expect("text");
    let find = |range: &ITextRangeProvider, needle: &str, backward: bool, ignore_case: bool| {
        uia!(range.FindText(&BSTR::from(needle), backward, ignore_case))
    };
    let get = |range: &ITextRangeProvider| uia!(range.GetText(-1)).expect("text").to_string();
    let starts_after = |range: &ITextRangeProvider, whole: &ITextRangeProvider| {
        uia!(range.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            whole,
            TextPatternRangeEndpoint_Start
        ))
        .expect("compare")
    };
    // Same lengths when lowercased: fold and match in the folded text.
    fixture
        .bridge
        .set_text_source(fixture.ids.editor, Arc::new(Editor::new("Ab ab Cd")));
    let whole = uia!(text.DocumentRange()).expect("document range");
    let first = find(&whole, "AB", false, true).expect("found");
    assert_eq!(
        (get(&first), starts_after(&first, &whole)),
        ("Ab".to_owned(), 0)
    );
    let last = find(&whole, "AB", true, true).expect("found");
    assert_eq!(
        (get(&last), starts_after(&last, &whole)),
        ("ab".to_owned(), 1)
    );
    assert_no_object(find(&whole, "AB", false, false));
    assert_code(find(&whole, "", false, true), E_INVALIDARG_BITS);
    // Searching a sub-range reports offsets in the whole document.
    let tail = uia!(whole.Clone()).expect("clone");
    uia!(tail.MoveEndpointByUnit(TextPatternRangeEndpoint_Start, TextUnit_Character, 3))
        .expect("move");
    let b = find(&tail, "b", false, false).expect("found");
    assert_eq!((get(&b), starts_after(&b, &whole)), ("b".to_owned(), 1));
    assert_eq!(
        uia!(b.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            &tail,
            TextPatternRangeEndpoint_Start
        ))
        .expect("compare"),
        1
    );
    // 'İ' lowercases to a longer string: the folded text would not line up with the original,
    // so the match is exact.
    fixture
        .bridge
        .set_text_source(fixture.ids.editor, Arc::new(Editor::new("xİyİ")));
    let whole = uia!(text.DocumentRange()).expect("document range");
    let forward = find(&whole, "İ", false, true).expect("found");
    let backward = find(&whole, "İ", true, true).expect("found");
    assert_eq!(
        (get(&forward), get(&backward)),
        ("İ".to_owned(), "İ".to_owned())
    );
    assert_eq!(starts_after(&forward, &whole), 1);
    assert_eq!(
        uia!(backward.CompareEndpoints(
            TextPatternRangeEndpoint_Start,
            &forward,
            TextPatternRangeEndpoint_Start
        ))
        .expect("compare"),
        1
    );
}

#[test]
fn uia_bridge_exposes_its_snapshot_debug_output_and_leaves_other_objects_alone() {
    let fixture = Fixture::new();
    assert_eq!(
        format!("{:?}", fixture.bridge),
        format!("UiaBridge {{ hwnd: {}, .. }}", fixture.raw)
    );
    let tree = fixture.bridge.tree();
    assert_eq!(tree.root_id(), fixture.ids.root);
    assert_eq!(
        tree.node(fixture.ids.save).map(|node| node.name.as_str()),
        Some("Save")
    );
    // Only the UIA root object id (-25) is answered; the default procedure gets the rest.
    assert!(
        fixture
            .bridge
            .handle_get_object(WPARAM(0), LPARAM(0))
            .is_none()
    );
    assert!(
        fixture
            .bridge
            .handle_get_object(WPARAM(0), LPARAM(-4))
            .is_none()
    );
}

/// Receives UIA events and forwards their ids.
#[implement(IUIAutomationEventHandler)]
struct EventLog {
    sender: Mutex<mpsc::Sender<i32>>,
}

#[allow(non_snake_case)]
impl IUIAutomationEventHandler_Impl for EventLog_Impl {
    fn HandleAutomationEvent(
        &self,
        _sender: Ref<IUIAutomationElement>,
        eventid: UIA_EVENT_ID,
    ) -> windows::core::Result<()> {
        let _ = lock(&self.sender).send(eventid.0);
        Ok(())
    }
}

#[test]
fn uia_listening_clients_receive_text_selection_events_for_published_changes() {
    let fixture = Fixture::new();
    let client = Client::new(&fixture);
    let (sender, receiver) = mpsc::channel();
    let handler: IUIAutomationEventHandler = EventLog {
        sender: Mutex::new(sender),
    }
    .into();
    // A registered handler is what makes UIA report that clients are listening.
    uia!(client.automation.AddAutomationEventHandler(
        UIA_Text_TextSelectionChangedEventId,
        &client.root,
        TreeScope_Subtree,
        None::<&IUIAutomationCacheRequest>,
        &handler
    ))
    .expect("handler registered");
    let ids = fixture.ids;
    let mut tree = build_tree(
        ids,
        &Snapshot {
            remember: NodeToggle::On,
            name: "Ada",
            focused: Some(ids.save),
        },
    );
    // The selection moves, a child appears, and a toggle changes in one snapshot.
    let mut editor = tree.node(ids.editor).cloned().expect("editor node");
    editor
        .patterns
        .text
        .as_mut()
        .expect("text pattern")
        .selection = vec![0..5];
    tree.insert(editor);
    let extra = WidgetId::allocate();
    let mut window = tree.root().cloned().expect("root node");
    window.children.push(extra);
    tree.insert(window);
    tree.insert(AccessNode::new(
        extra,
        Role::Button,
        "Extra",
        Rect::new(10.0, 400.0, 50.0, 20.0),
    ));
    let changes = fixture.bridge.update(tree).expect("snapshot published");
    assert!(changes.contains(&UiaChange::FocusChanged(ids.save)));
    assert!(changes.contains(&UiaChange::StructureChanged(ids.root)));
    assert!(changes.contains(&UiaChange::TextSelectionChanged(ids.editor)));
    assert!(changes.contains(&UiaChange::PropertyChanged {
        id: ids.remember,
        property: UiaProperty::Toggle,
        old: PropertyValue::Number(0),
        new: PropertyValue::Number(1),
    }));
    let event = receiver
        .recv_timeout(Duration::from_secs(15))
        .expect("the selection event reaches the client");
    assert_eq!(event, UIA_Text_TextSelectionChangedEventId.0);
    uia!(client.automation.RemoveAllEventHandlers()).expect("handlers removed");
}
