//! UI Automation bridge: maps an [`AccessTree`] snapshot to UIA providers.
//!
//! [`UiaBridge`] owns the latest tree snapshot. The UI thread publishes a new snapshot with
//! [`UiaBridge::update`] after every frame that changed the tree; while a client listens, that
//! call also raises the UIA events for what changed (focus, properties, structure, text
//! selection). UIA calls the providers on its own threads, so every provider reads the current
//! snapshot behind a lock and never touches widgets: requests (Invoke, Toggle, SetValue, …) go
//! to an [`ActionSink`], which hands them to the UI thread (a channel plus `PostMessage` in the
//! app) and reports whether the action is supported.
//!
//! The window procedure answers `WM_GETOBJECT` with [`UiaBridge::handle_get_object`].
//!
//! # Modules
//!
//! - [`provider`]: the element provider (simple, fragment and fragment root) and properties.
//! - [`patterns`]: Invoke, Toggle, ExpandCollapse, SelectionItem, Selection, Value, RangeValue
//!   and Scroll.
//! - [`text_pattern`]: the Text pattern and text ranges over the editor document.
//! - [`events`]: change detection between snapshots and UIA event raising.

pub mod events;
pub mod patterns;
pub mod provider;
pub mod text_pattern;

use std::collections::HashMap;
use std::ffi::c_void;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use thiserror::Error;
use windows::Win32::Foundation::{E_FAIL, E_OUTOFMEMORY, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::SafeArrayCreateVector;
use windows::Win32::System::Variant::{VT_I4, VT_R8, VT_UNKNOWN};
use windows::Win32::UI::Accessibility::{
    IRawElementProviderSimple, UIA_ButtonControlTypeId, UIA_CONTROLTYPE_ID,
    UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_CustomControlTypeId,
    UIA_DataItemControlTypeId, UIA_DocumentControlTypeId, UIA_EditControlTypeId,
    UIA_GroupControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
    UIA_ListControlTypeId, UIA_ListItemControlTypeId, UIA_MenuControlTypeId,
    UIA_MenuItemControlTypeId, UIA_PaneControlTypeId, UIA_ScrollBarControlTypeId,
    UIA_SeparatorControlTypeId, UIA_SliderControlTypeId, UIA_StatusBarControlTypeId,
    UIA_TabControlTypeId, UIA_TabItemControlTypeId, UIA_TableControlTypeId, UIA_TextControlTypeId,
    UIA_TitleBarControlTypeId, UIA_ToolBarControlTypeId, UIA_ToolTipControlTypeId,
    UIA_TreeControlTypeId, UIA_TreeItemControlTypeId, UIA_WindowControlTypeId,
    UiaReturnRawElementProvider, UiaRootObjectId,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::core::{ComObject, Error, HRESULT};

use ssw_ui::access::{AccessAction, AccessNode, AccessTree, Role};
use ssw_ui::geometry::{Point, Rect};
use ssw_ui::widget::WidgetId;

use provider::ElementProvider;

/// Failures of the UIA bridge.
#[derive(Debug, Error)]
pub enum UiaError {
    /// A COM call failed.
    #[error("UI Automation call failed: {0}")]
    Com(#[from] windows::core::Error),
}

/// Receives the actions assistive technology requests. Called on a UIA thread; implementations
/// queue the action for the UI thread and return whether the widget supports it.
pub trait ActionSink: Send + Sync {
    /// Requests `action` on widget `id`. Returns false when it is not supported.
    fn perform(&self, id: WidgetId, action: AccessAction) -> bool;
}

/// The text of a document node (the editor), for the Text pattern. Offsets are UTF-8 byte
/// offsets; rectangles are in window DIPs. Called on UIA threads.
pub trait TextSource: Send + Sync {
    /// The whole document text.
    fn text(&self) -> String;
    /// One rectangle per visual line covered by `range` (empty when the range is not visible).
    fn bounding_rects(&self, range: Range<usize>) -> Vec<Rect>;
    /// The offset of the character at the window point, if any.
    fn offset_from_point(&self, point: Point) -> Option<usize>;
    /// The visual (soft-wrapped) line containing `offset`, including its line break.
    fn line_range(&self, offset: usize) -> Range<usize>;
    /// Selects `range` in the editor.
    fn select(&self, range: Range<usize>);
    /// Scrolls `range` into view.
    fn scroll_into_view(&self, range: Range<usize>, align_to_top: bool);
}

/// UIA's "element not available" (the element left the tree since the client got it).
pub(crate) const ELEMENT_NOT_AVAILABLE: u32 = 0x8004_0201;
/// UIA's "element not enabled".
pub(crate) const ELEMENT_NOT_ENABLED: u32 = 0x8004_0200;
/// UIA's "not supported".
pub(crate) const NOT_SUPPORTED: u32 = 0x8004_0204;

/// An `Error` carrying a UIA HRESULT.
pub(crate) fn uia_error(code: u32) -> Error {
    // The HRESULT bit pattern is the code; the cast reinterprets, it does not truncate.
    Error::from(HRESULT(code as i32))
}

/// "No object, no error": S_OK with a null interface or array result.
pub(crate) fn none() -> Error {
    Error::empty()
}

/// State shared by the bridge and every provider it hands out.
pub(crate) struct Shared {
    hwnd: isize,
    tree: Mutex<Arc<AccessTree>>,
    sink: Arc<dyn ActionSink>,
    text_sources: Mutex<HashMap<WidgetId, Arc<dyn TextSource>>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    pub(crate) fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut c_void)
    }

    pub(crate) fn tree(&self) -> Arc<AccessTree> {
        locked(&self.tree).clone()
    }

    /// A copy of the node `id`, or "element not available".
    pub(crate) fn node(&self, id: WidgetId) -> windows::core::Result<AccessNode> {
        self.tree()
            .node(id)
            .cloned()
            .ok_or_else(|| uia_error(ELEMENT_NOT_AVAILABLE))
    }

    pub(crate) fn text_source(&self, id: WidgetId) -> windows::core::Result<Arc<dyn TextSource>> {
        locked(&self.text_sources)
            .get(&id)
            .cloned()
            .ok_or_else(|| uia_error(NOT_SUPPORTED))
    }

    /// Sends `action` to the sink: disabled nodes refuse, unsupported actions fail.
    pub(crate) fn act(&self, id: WidgetId, action: AccessAction) -> windows::core::Result<()> {
        let node = self.node(id)?;
        if node.state.disabled {
            return Err(uia_error(ELEMENT_NOT_ENABLED));
        }
        if self.sink.perform(id, action) {
            Ok(())
        } else {
            Err(uia_error(NOT_SUPPORTED))
        }
    }

    /// DIPs per physical pixel factor of the window (`dpi / 96`, 1 when unknown).
    pub(crate) fn scale(&self) -> f32 {
        // SAFETY: GetDpiForWindow accepts any handle and returns 0 for an invalid one.
        let dpi = unsafe { GetDpiForWindow(self.hwnd()) };
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }

    /// Window DIP rectangle → screen pixels `(left, top, width, height)`.
    pub(crate) fn to_screen(&self, rect: Rect) -> (f64, f64, f64, f64) {
        let scale = f64::from(self.scale());
        let mut origin = windows::Win32::Foundation::POINT { x: 0, y: 0 };
        // SAFETY: `origin` is a valid POINT; a failing call leaves it at the window origin.
        let _ = unsafe { ClientToScreen(self.hwnd(), &mut origin) };
        (
            f64::from(origin.x) + f64::from(rect.x) * scale,
            f64::from(origin.y) + f64::from(rect.y) * scale,
            f64::from(rect.width) * scale,
            f64::from(rect.height) * scale,
        )
    }

    /// Screen pixels → window DIPs.
    pub(crate) fn screen_to_window(&self, x: f64, y: f64) -> Point {
        let mut point = windows::Win32::Foundation::POINT {
            x: x as i32,
            y: y as i32,
        };
        // SAFETY: `point` is a valid POINT; a failing call leaves screen coordinates.
        let _ = unsafe { ScreenToClient(self.hwnd(), &mut point) };
        let scale = self.scale();
        Point::new(point.x as f32 / scale, point.y as f32 / scale)
    }

    /// The provider for node `id`, as a simple provider.
    pub(crate) fn provider(self: &Arc<Self>, id: WidgetId) -> IRawElementProviderSimple {
        ComObject::new(ElementProvider::new(self.clone(), id))
            .to_interface::<IRawElementProviderSimple>()
    }
}

/// The UIA control type of a role.
pub(crate) fn control_type(role: Role) -> UIA_CONTROLTYPE_ID {
    match role {
        Role::Window => UIA_WindowControlTypeId,
        Role::Document => UIA_DocumentControlTypeId,
        Role::Edit => UIA_EditControlTypeId,
        Role::Text => UIA_TextControlTypeId,
        Role::Button | Role::ToggleButton => UIA_ButtonControlTypeId,
        Role::CheckBox => UIA_CheckBoxControlTypeId,
        Role::ComboBox => UIA_ComboBoxControlTypeId,
        Role::List => UIA_ListControlTypeId,
        Role::ListItem => UIA_ListItemControlTypeId,
        Role::Tree => UIA_TreeControlTypeId,
        Role::TreeItem => UIA_TreeItemControlTypeId,
        Role::Tab => UIA_TabControlTypeId,
        Role::TabItem => UIA_TabItemControlTypeId,
        Role::Menu => UIA_MenuControlTypeId,
        Role::MenuItem => UIA_MenuItemControlTypeId,
        Role::ScrollBar => UIA_ScrollBarControlTypeId,
        Role::Slider => UIA_SliderControlTypeId,
        Role::ToolBar => UIA_ToolBarControlTypeId,
        Role::StatusBar => UIA_StatusBarControlTypeId,
        Role::Pane => UIA_PaneControlTypeId,
        Role::Group => UIA_GroupControlTypeId,
        Role::Hyperlink => UIA_HyperlinkControlTypeId,
        Role::Image => UIA_ImageControlTypeId,
        Role::Table => UIA_TableControlTypeId,
        Role::DataItem => UIA_DataItemControlTypeId,
        Role::Separator => UIA_SeparatorControlTypeId,
        Role::ToolTip => UIA_ToolTipControlTypeId,
        Role::TitleBar => UIA_TitleBarControlTypeId,
        Role::Custom => UIA_CustomControlTypeId,
    }
}

/// A SAFEARRAY of `i32` values (runtime ids).
pub(crate) fn safe_array_i32(values: &[i32]) -> windows::core::Result<*mut SAFEARRAY> {
    safe_array(VT_I4.0, values.len(), &mut |data: *mut c_void| {
        let data = data.cast::<i32>();
        for (index, value) in values.iter().enumerate() {
            // SAFETY: `data` points at `values.len()` writable i32 slots (see `safe_array`).
            unsafe { data.add(index).write(*value) };
        }
    })
}

/// A SAFEARRAY of `f64` values (bounding rectangles).
pub(crate) fn safe_array_f64(values: &[f64]) -> windows::core::Result<*mut SAFEARRAY> {
    safe_array(VT_R8.0, values.len(), &mut |data: *mut c_void| {
        let data = data.cast::<f64>();
        for (index, value) in values.iter().enumerate() {
            // SAFETY: `data` points at `values.len()` writable f64 slots (see `safe_array`).
            unsafe { data.add(index).write(*value) };
        }
    })
}

/// The result of a SAFEARRAY allocation: `E_OUTOFMEMORY` when the allocator returned null.
fn allocated(array: *mut SAFEARRAY) -> windows::core::Result<*mut SAFEARRAY> {
    if array.is_null() {
        Err(Error::from(E_OUTOFMEMORY))
    } else {
        Ok(array)
    }
}

/// An empty SAFEARRAY of `IUnknown` (no children).
pub(crate) fn safe_array_empty_unknown() -> windows::core::Result<*mut SAFEARRAY> {
    // SAFETY: creating a zero-length vector has no preconditions.
    allocated(unsafe { SafeArrayCreateVector(VT_UNKNOWN, 0, 0) })
}

/// A new SAFEARRAY vector of `len` elements of the type `vt` names that `fill` initializes
/// through a pointer to its first element (the caller casts it to the element type).
fn safe_array(
    vt: u16,
    len: usize,
    fill: &mut dyn FnMut(*mut c_void),
) -> windows::core::Result<*mut SAFEARRAY> {
    let count = u32::try_from(len).map_err(|_| Error::from(E_FAIL))?;
    // SAFETY: creating a vector of `count` elements of the given element type.
    let array = allocated(unsafe {
        SafeArrayCreateVector(windows::Win32::System::Variant::VARENUM(vt), 0, count)
    });
    array.inspect(|array| {
        // SAFETY: the vector was just created and nobody else holds it, so its data pointer
        // (`count` elements of the array's type) can be written without locking it.
        let data = unsafe { (**array).pvData };
        fill(data);
    })
}

/// The bridge between the widget tree and UI Automation, one per window.
pub struct UiaBridge {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for UiaBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UiaBridge")
            .field("hwnd", &self.shared.hwnd)
            .finish_non_exhaustive()
    }
}

impl UiaBridge {
    /// A bridge for `hwnd` with an empty tree; publish the first snapshot with
    /// [`UiaBridge::update`].
    #[must_use]
    pub fn new(hwnd: HWND, sink: Arc<dyn ActionSink>, root: WidgetId) -> Self {
        Self {
            shared: Arc::new(Shared {
                hwnd: hwnd.0 as isize,
                tree: Mutex::new(Arc::new(AccessTree::new(root))),
                sink,
                text_sources: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Registers the text of the document node `id` (the editor).
    pub fn set_text_source(&self, id: WidgetId, source: Arc<dyn TextSource>) {
        locked(&self.shared.text_sources).insert(id, source);
    }

    /// The current snapshot.
    #[must_use]
    pub fn tree(&self) -> Arc<AccessTree> {
        self.shared.tree()
    }

    /// Publishes `tree`. While a UIA client is listening, the differences from the previous
    /// snapshot are raised as UIA events and returned; without a listener nothing compares the
    /// snapshots and the result is empty.
    ///
    /// # Errors
    /// Fails when raising an event fails.
    pub fn update(&self, tree: AccessTree) -> Result<Vec<events::UiaChange>, UiaError> {
        self.publish(tree, events::clients_listening())
    }

    fn publish(
        &self,
        tree: AccessTree,
        listening: bool,
    ) -> Result<Vec<events::UiaChange>, UiaError> {
        let new = Arc::new(tree);
        let old = std::mem::replace(&mut *locked(&self.shared.tree), new.clone());
        if !listening {
            return Ok(Vec::new());
        }
        let changes = events::diff(&old, &new);
        events::raise(&self.shared, &changes)?;
        Ok(changes)
    }

    /// The root provider (the window's fragment root).
    #[must_use]
    pub fn root_provider(&self) -> IRawElementProviderSimple {
        self.shared.provider(self.shared.tree().root_id())
    }

    /// Answers `WM_GETOBJECT`: `Some(result)` when `lparam` asks for the UIA root object, which
    /// the window procedure returns; `None` for any other object id.
    #[must_use]
    pub fn handle_get_object(&self, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        // WM_GETOBJECT carries the object id in the low 32 bits of lparam.
        if lparam.0 as i32 != UiaRootObjectId {
            return None;
        }
        let root = self.root_provider();
        // SAFETY: `hwnd` is the window this bridge was created for; `root` is a live provider.
        Some(unsafe { UiaReturnRawElementProvider(self.shared.hwnd(), wparam, lparam, &root) })
    }

    /// Detaches every provider from UIA; call when the window is destroyed.
    pub fn disconnect(&self) {
        // SAFETY: a null provider with zero ids is the documented way to drop all providers of
        // the window.
        let _ = unsafe {
            UiaReturnRawElementProvider(
                self.shared.hwnd(),
                WPARAM(0),
                LPARAM(0),
                None::<&IRawElementProviderSimple>,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Ole::SafeArrayDestroy;

    #[test]
    fn a_null_allocation_is_out_of_memory_and_a_real_array_passes_through() {
        let error = allocated(std::ptr::null_mut()).expect_err("null is a failed allocation");
        assert_eq!(error.code(), E_OUTOFMEMORY);
        let array = safe_array_empty_unknown().expect("an empty array");
        // SAFETY: the array was just created and is destroyed exactly once.
        unsafe { SafeArrayDestroy(array) }.expect("the array is destroyed");
    }

    #[test]
    fn a_vector_is_allocated_for_its_elements_and_filled_in_place() {
        let array = safe_array(VT_I4.0, 3, &mut |data| {
            let elements = data.cast::<i32>();
            for (index, value) in [7, 8, 9].into_iter().enumerate() {
                // SAFETY: the vector has room for three 32-bit elements and nobody else holds
                // it yet.
                unsafe { elements.add(index).write(value) };
            }
        })
        .expect("a vector of three elements");
        // SAFETY: the array was just created; its data is three initialized 32-bit elements.
        let (bound, elements) = unsafe {
            (
                (*array).rgsabound[0].cElements,
                std::slice::from_raw_parts((*array).pvData.cast::<i32>(), 3).to_vec(),
            )
        };
        assert_eq!(bound, 3);
        assert_eq!(elements, [7, 8, 9]);
        // SAFETY: the array was just created and is destroyed exactly once.
        unsafe { SafeArrayDestroy(array) }.expect("the array is destroyed");
    }

    /// A sink that supports no action.
    struct Refusing;

    impl ActionSink for Refusing {
        fn perform(&self, _id: WidgetId, _action: AccessAction) -> bool {
            false
        }
    }

    fn named(root: WidgetId, name: &str) -> AccessTree {
        let mut tree = AccessTree::new(root);
        tree.insert(AccessNode::new(
            root,
            Role::Window,
            name,
            Rect::new(0.0, 0.0, 10.0, 10.0),
        ));
        tree
    }

    #[test]
    fn a_snapshot_nobody_listens_to_is_published_without_being_compared() {
        let root = WidgetId::new(1);
        let bridge = UiaBridge::new(HWND::default(), Arc::new(Refusing), root);
        let publish = |name: &str| bridge.publish(named(root, name), false).unwrap();
        let current = || bridge.tree().node(root).map(|node| node.name.clone());
        assert!(publish("one").is_empty());
        assert_eq!(current().as_deref(), Some("one"));
        // The rename is not reported later either: nothing compared the two snapshots.
        assert!(publish("two").is_empty());
        assert_eq!(current().as_deref(), Some("two"));
        assert!(!bridge.shared.sink.perform(root, AccessAction::Invoke));
    }

    #[test]
    fn an_array_with_more_elements_than_a_safearray_can_hold_is_refused() {
        // Refused before anything is allocated or filled.
        let too_many = u32::MAX as usize + 1;
        let error =
            safe_array(VT_I4.0, too_many, &mut drop).expect_err("too many elements for a vector");
        assert_eq!(error.code(), E_FAIL);
    }
}
