//! The UIA element provider: one COM object per [`AccessNode`](ssw_ui::access::AccessNode),
//! implementing the simple, fragment and fragment-root provider interfaces and every pattern
//! interface. `GetPatternProvider` only hands out the patterns the node declares.
//!
//! A provider holds only the node id and the shared snapshot; it looks the node up on every
//! call, so a provider kept by a client after its widget left the tree reports "element not
//! available" instead of stale data.

// The `windows` crate names its UIA identifiers `UIA_NamePropertyId` and so on; matching on them
// is the idiomatic way to dispatch.
#![allow(non_upper_case_globals)]

use std::sync::Arc;

use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IExpandCollapseProvider, IInvokeProvider, IRangeValueProvider, IRawElementProviderFragment,
    IRawElementProviderFragment_Impl, IRawElementProviderFragmentRoot,
    IRawElementProviderFragmentRoot_Impl, IRawElementProviderSimple,
    IRawElementProviderSimple_Impl, IScrollProvider, ISelectionItemProvider, ISelectionProvider,
    ITextProvider, IToggleProvider, IValueProvider, NavigateDirection,
    NavigateDirection_FirstChild, NavigateDirection_LastChild, NavigateDirection_NextSibling,
    NavigateDirection_Parent, NavigateDirection_PreviousSibling, ProviderOptions,
    ProviderOptions_ServerSideProvider, UIA_AutomationIdPropertyId, UIA_ControlTypePropertyId,
    UIA_ExpandCollapsePatternId, UIA_FrameworkIdPropertyId, UIA_HasKeyboardFocusPropertyId,
    UIA_HelpTextPropertyId, UIA_InvokePatternId, UIA_IsContentElementPropertyId,
    UIA_IsControlElementPropertyId, UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId,
    UIA_IsOffscreenPropertyId, UIA_IsPasswordPropertyId, UIA_NamePropertyId, UIA_PATTERN_ID,
    UIA_PROPERTY_ID, UIA_RangeValuePatternId, UIA_ScrollPatternId, UIA_SelectionItemPatternId,
    UIA_SelectionPatternId, UIA_TextPatternId, UIA_TogglePatternId, UIA_ValuePatternId,
    UiaHostProviderFromHwnd, UiaRect,
};
use windows::core::{IUnknown, IUnknownImpl, Interface, implement};

use ssw_ui::access::{AccessAction, Role};
use ssw_ui::widget::WidgetId;

use super::{NOT_SUPPORTED, Shared, control_type, none, safe_array_i32, uia_error};

/// The framework id reported to clients.
const FRAMEWORK_ID: &str = "SSWriter";
/// First element of a runtime id that UIA completes with the window's own id.
const APPEND_RUNTIME_ID: i32 = 3;

/// The provider of one node.
#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot,
    IInvokeProvider,
    IToggleProvider,
    IExpandCollapseProvider,
    ISelectionItemProvider,
    ISelectionProvider,
    IValueProvider,
    IRangeValueProvider,
    IScrollProvider,
    ITextProvider
)]
pub(crate) struct ElementProvider {
    pub(crate) shared: Arc<Shared>,
    pub(crate) id: WidgetId,
}

impl ElementProvider {
    pub(crate) fn new(shared: Arc<Shared>, id: WidgetId) -> Self {
        Self { shared, id }
    }
}

/// True for roles that contain selectable children.
pub(crate) fn is_selection_container(role: Role) -> bool {
    matches!(
        role,
        Role::List | Role::Tree | Role::Tab | Role::ComboBox | Role::Menu
    )
}

impl IRawElementProviderSimple_Impl for ElementProvider_Impl {
    fn ProviderOptions(&self) -> windows::core::Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, pattern: UIA_PATTERN_ID) -> windows::core::Result<IUnknown> {
        let node = self.shared.node(self.id)?;
        let patterns = &node.patterns;
        let this = self.to_object();
        match pattern {
            UIA_InvokePatternId if patterns.invoke => {
                this.to_interface::<IInvokeProvider>().cast::<IUnknown>()
            }
            UIA_TogglePatternId if patterns.toggle => {
                this.to_interface::<IToggleProvider>().cast::<IUnknown>()
            }
            UIA_ExpandCollapsePatternId if patterns.expand_collapse => this
                .to_interface::<IExpandCollapseProvider>()
                .cast::<IUnknown>(),
            UIA_SelectionItemPatternId if patterns.selection_item => this
                .to_interface::<ISelectionItemProvider>()
                .cast::<IUnknown>(),
            UIA_SelectionPatternId if is_selection_container(node.role) => {
                this.to_interface::<ISelectionProvider>().cast::<IUnknown>()
            }
            UIA_ValuePatternId if patterns.value => {
                this.to_interface::<IValueProvider>().cast::<IUnknown>()
            }
            UIA_RangeValuePatternId if patterns.range_value.is_some() => this
                .to_interface::<IRangeValueProvider>()
                .cast::<IUnknown>(),
            UIA_ScrollPatternId if patterns.scroll.is_some() => {
                this.to_interface::<IScrollProvider>().cast::<IUnknown>()
            }
            UIA_TextPatternId if patterns.text.is_some() => {
                this.to_interface::<ITextProvider>().cast::<IUnknown>()
            }
            // S_OK with a null interface: the pattern is not supported.
            _ => Err(none()),
        }
    }

    fn GetPropertyValue(&self, property: UIA_PROPERTY_ID) -> windows::core::Result<VARIANT> {
        let node = self.shared.node(self.id)?;
        Ok(match property {
            UIA_NamePropertyId => VARIANT::from(node.name.as_str()),
            UIA_ControlTypePropertyId => VARIANT::from(control_type(node.role).0),
            UIA_AutomationIdPropertyId => VARIANT::from(format!("ssw-{}", self.id.get()).as_str()),
            UIA_HasKeyboardFocusPropertyId => VARIANT::from(node.state.focused),
            UIA_IsKeyboardFocusablePropertyId => VARIANT::from(node.state.focusable),
            UIA_IsEnabledPropertyId => VARIANT::from(!node.state.disabled),
            UIA_IsOffscreenPropertyId => VARIANT::from(node.state.offscreen),
            UIA_IsPasswordPropertyId => VARIANT::from(node.state.password),
            UIA_IsControlElementPropertyId | UIA_IsContentElementPropertyId => VARIANT::from(true),
            UIA_FrameworkIdPropertyId => VARIANT::from(FRAMEWORK_ID),
            UIA_HelpTextPropertyId if !node.description.is_empty() => {
                VARIANT::from(node.description.as_str())
            }
            // VT_EMPTY: UIA falls back to its default for the property.
            _ => VARIANT::default(),
        })
    }

    fn HostRawElementProvider(&self) -> windows::core::Result<IRawElementProviderSimple> {
        if self.id == self.shared.tree().root_id() {
            // SAFETY: the handle is the bridge's window; a destroyed window yields an error.
            unsafe { UiaHostProviderFromHwnd(self.shared.hwnd()) }
        } else {
            Err(none())
        }
    }
}

impl IRawElementProviderFragment_Impl for ElementProvider_Impl {
    fn Navigate(
        &self,
        direction: NavigateDirection,
    ) -> windows::core::Result<IRawElementProviderFragment> {
        self.shared.node(self.id)?;
        let tree = self.shared.tree();
        let target = match direction {
            NavigateDirection_Parent => tree.parent(self.id),
            NavigateDirection_NextSibling => tree.next_sibling(self.id),
            NavigateDirection_PreviousSibling => tree.previous_sibling(self.id),
            NavigateDirection_FirstChild => tree.first_child(self.id),
            NavigateDirection_LastChild => tree.last_child(self.id),
            _ => None,
        };
        match target {
            Some(node) => self
                .shared
                .provider(node.id)
                .cast::<IRawElementProviderFragment>(),
            None => Err(none()),
        }
    }

    fn GetRuntimeId(&self) -> windows::core::Result<*mut SAFEARRAY> {
        if self.id == self.shared.tree().root_id() {
            // The window host provides the root's runtime id.
            return Err(none());
        }
        let raw = self.id.get();
        // The id is split into two 32-bit halves; truncation is the intent.
        safe_array_i32(&[
            APPEND_RUNTIME_ID,
            (raw & 0xFFFF_FFFF) as i32,
            (raw >> 32) as i32,
        ])
    }

    fn BoundingRectangle(&self) -> windows::core::Result<UiaRect> {
        let node = self.shared.node(self.id)?;
        let (left, top, width, height) = self.shared.to_screen(node.bounds);
        Ok(UiaRect {
            left,
            top,
            width,
            height,
        })
    }

    fn GetEmbeddedFragmentRoots(&self) -> windows::core::Result<*mut SAFEARRAY> {
        Err(none())
    }

    fn SetFocus(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Focus)
    }

    fn FragmentRoot(&self) -> windows::core::Result<IRawElementProviderFragmentRoot> {
        self.shared
            .provider(self.shared.tree().root_id())
            .cast::<IRawElementProviderFragmentRoot>()
    }
}

impl IRawElementProviderFragmentRoot_Impl for ElementProvider_Impl {
    fn ElementProviderFromPoint(
        &self,
        x: f64,
        y: f64,
    ) -> windows::core::Result<IRawElementProviderFragment> {
        let point = self.shared.screen_to_window(x, y);
        let tree = self.shared.tree();
        match tree.hit_test(point) {
            Some(node) => self
                .shared
                .provider(node.id)
                .cast::<IRawElementProviderFragment>(),
            None => Err(none()),
        }
    }

    fn GetFocus(&self) -> windows::core::Result<IRawElementProviderFragment> {
        let tree = self.shared.tree();
        match tree.focused() {
            Some(node) => self
                .shared
                .provider(node.id)
                .cast::<IRawElementProviderFragment>(),
            None => Err(none()),
        }
    }
}

/// `UIA_E_NOTSUPPORTED` as an error, for pattern operations a node cannot perform.
pub(crate) fn not_supported() -> windows::core::Error {
    uia_error(NOT_SUPPORTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uia_selection_containers_are_the_selectable_roles() {
        for role in [
            Role::List,
            Role::Tree,
            Role::Tab,
            Role::ComboBox,
            Role::Menu,
        ] {
            assert!(is_selection_container(role), "{role:?}");
        }
        for role in [Role::Button, Role::ListItem, Role::Document, Role::Window] {
            assert!(!is_selection_container(role), "{role:?}");
        }
    }
}
