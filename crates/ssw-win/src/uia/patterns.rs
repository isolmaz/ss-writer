//! UIA pattern providers on [`ElementProvider`](super::provider): Invoke, Toggle,
//! ExpandCollapse, SelectionItem, Selection, Value, RangeValue and Scroll.
//!
//! Every mutating call becomes an [`AccessAction`] for the [`ActionSink`](super::ActionSink);
//! reads come from the node in the current snapshot.

// The `windows` crate names its UIA identifiers `ScrollAmount_LargeDecrement` and so on;
// matching on them is the idiomatic way to dispatch.
#![allow(non_upper_case_globals)]

use std::ffi::c_void;

use windows::Win32::Foundation::E_INVALIDARG;
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Variant::VT_UNKNOWN;
use windows::Win32::UI::Accessibility::{
    ExpandCollapseState, ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
    ExpandCollapseState_LeafNode, IExpandCollapseProvider_Impl, IInvokeProvider_Impl,
    IRangeValueProvider_Impl, IRawElementProviderSimple, IScrollProvider_Impl,
    ISelectionItemProvider_Impl, ISelectionProvider_Impl, IToggleProvider_Impl,
    IValueProvider_Impl, ScrollAmount, ScrollAmount_LargeDecrement, ScrollAmount_LargeIncrement,
    ScrollAmount_SmallDecrement, ScrollAmount_SmallIncrement, ToggleState,
    ToggleState_Indeterminate, ToggleState_Off, ToggleState_On,
};
use windows::core::{BOOL, BSTR, Error, Interface, PCWSTR};

use ssw_ui::access::{AccessAction, ScrollInfo, ToggleState as AccessToggle};

use super::provider::{ElementProvider_Impl, not_supported};
use super::{none, safe_array};

/// UIA's marker for "this axis does not scroll".
const NO_SCROLL: f64 = -1.0;
/// A small scroll step as a fraction of a page.
const SMALL_STEP_OF_PAGE: f32 = 0.25;

/// A SAFEARRAY of interface pointers (selection results).
pub(crate) fn safe_array_of<I: Interface>(items: &[I]) -> windows::core::Result<*mut SAFEARRAY> {
    safe_array(VT_UNKNOWN.0, items.len(), &mut |slots: *mut c_void| {
        let slots = slots.cast::<*mut c_void>();
        for (index, item) in items.iter().enumerate() {
            // SAFETY: `slots` has room for `items.len()` pointers (see `safe_array`). The array
            // owns one reference per slot and releases it when destroyed: it is the one taken
            // by cloning the interface and handing the clone over as a raw pointer.
            unsafe { slots.add(index).write(item.clone().into_raw()) };
        }
    })
}

/// The UIA toggle state of an access toggle state.
fn uia_toggle(state: AccessToggle) -> ToggleState {
    match state {
        AccessToggle::Off => ToggleState_Off,
        AccessToggle::On => ToggleState_On,
        AccessToggle::Indeterminate => ToggleState_Indeterminate,
    }
}

/// The scroll percentage change a [`ScrollAmount`] asks for on one axis, in percent of the
/// scrollable range, given the visible share `view` (percent) of the content; `None` for
/// "no amount". A page is the visible share expressed against the scrollable range.
pub(crate) fn scroll_delta(amount: ScrollAmount, view: f32) -> Option<f32> {
    let range = 100.0 - view;
    let page = if range > 0.0 {
        view / range * 100.0
    } else {
        0.0
    };
    match amount {
        ScrollAmount_LargeDecrement => Some(-page),
        ScrollAmount_SmallDecrement => Some(-page * SMALL_STEP_OF_PAGE),
        ScrollAmount_LargeIncrement => Some(page),
        ScrollAmount_SmallIncrement => Some(page * SMALL_STEP_OF_PAGE),
        _ => None,
    }
}

/// The new percentage of an axis after applying `amount`, or `None` when it does not scroll.
fn scrolled(current: f32, view: f32, amount: ScrollAmount, scrollable: bool) -> Option<f32> {
    if !scrollable {
        return None;
    }
    scroll_delta(amount, view).map(|delta| (current + delta).clamp(0.0, 100.0))
}

/// Percent value for UIA: `-1` when the axis does not scroll.
fn axis_percent(value: f32, scrollable: bool) -> f64 {
    if scrollable {
        f64::from(value)
    } else {
        NO_SCROLL
    }
}

impl IInvokeProvider_Impl for ElementProvider_Impl {
    fn Invoke(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Invoke)
    }
}

impl IToggleProvider_Impl for ElementProvider_Impl {
    fn Toggle(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Toggle)
    }

    fn ToggleState(&self) -> windows::core::Result<ToggleState> {
        let node = self.shared.node(self.id)?;
        node.state.toggle.map(uia_toggle).ok_or_else(not_supported)
    }
}

impl IExpandCollapseProvider_Impl for ElementProvider_Impl {
    fn Expand(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Expand)
    }

    fn Collapse(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Collapse)
    }

    fn ExpandCollapseState(&self) -> windows::core::Result<ExpandCollapseState> {
        let node = self.shared.node(self.id)?;
        Ok(match node.state.expanded {
            Some(true) => ExpandCollapseState_Expanded,
            Some(false) => ExpandCollapseState_Collapsed,
            None => ExpandCollapseState_LeafNode,
        })
    }
}

impl ISelectionItemProvider_Impl for ElementProvider_Impl {
    fn Select(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Select)
    }

    fn AddToSelection(&self) -> windows::core::Result<()> {
        self.shared.act(self.id, AccessAction::Select)
    }

    fn RemoveFromSelection(&self) -> windows::core::Result<()> {
        Err(not_supported())
    }

    fn IsSelected(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(self.shared.node(self.id)?.state.selected))
    }

    fn SelectionContainer(&self) -> windows::core::Result<IRawElementProviderSimple> {
        self.shared.node(self.id)?;
        match self.shared.tree().parent(self.id) {
            Some(parent) => Ok(self.shared.provider(parent.id)),
            None => Err(none()),
        }
    }
}

impl ISelectionProvider_Impl for ElementProvider_Impl {
    fn GetSelection(&self) -> windows::core::Result<*mut SAFEARRAY> {
        self.shared.node(self.id)?;
        let tree = self.shared.tree();
        let selected: Vec<IRawElementProviderSimple> = tree
            .children(self.id)
            .filter(|child| child.state.selected)
            .map(|child| self.shared.provider(child.id))
            .collect();
        safe_array_of(&selected)
    }

    fn CanSelectMultiple(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(false))
    }

    fn IsSelectionRequired(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(false))
    }
}

impl IValueProvider_Impl for ElementProvider_Impl {
    fn SetValue(&self, value: &PCWSTR) -> windows::core::Result<()> {
        // SAFETY: UIA passes a NUL-terminated UTF-16 string that lives for the call.
        let text = unsafe { value.to_string() }.map_err(|_| Error::from(E_INVALIDARG))?;
        self.shared.act(self.id, AccessAction::SetValue(text))
    }

    fn Value(&self) -> windows::core::Result<BSTR> {
        let node = self.shared.node(self.id)?;
        Ok(BSTR::from(node.value.unwrap_or_default().as_str()))
    }

    fn IsReadOnly(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(self.shared.node(self.id)?.state.read_only))
    }
}

impl ElementProvider_Impl {
    fn range_value(&self) -> windows::core::Result<ssw_ui::access::RangeValue> {
        self.shared
            .node(self.id)?
            .patterns
            .range_value
            .ok_or_else(not_supported)
    }

    fn scroll_info(&self) -> windows::core::Result<ScrollInfo> {
        self.shared
            .node(self.id)?
            .patterns
            .scroll
            .ok_or_else(not_supported)
    }
}

impl IRangeValueProvider_Impl for ElementProvider_Impl {
    fn SetValue(&self, value: f64) -> windows::core::Result<()> {
        let range = self.range_value()?;
        if value < range.min || value > range.max {
            return Err(Error::from(E_INVALIDARG));
        }
        self.shared.act(self.id, AccessAction::SetRangeValue(value))
    }

    fn Value(&self) -> windows::core::Result<f64> {
        Ok(self.range_value()?.value)
    }

    fn IsReadOnly(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(self.shared.node(self.id)?.state.read_only))
    }

    fn Maximum(&self) -> windows::core::Result<f64> {
        Ok(self.range_value()?.max)
    }

    fn Minimum(&self) -> windows::core::Result<f64> {
        Ok(self.range_value()?.min)
    }

    fn LargeChange(&self) -> windows::core::Result<f64> {
        let range = self.range_value()?;
        Ok((range.max - range.min) / 10.0)
    }

    fn SmallChange(&self) -> windows::core::Result<f64> {
        Ok(self.range_value()?.step)
    }
}

impl IScrollProvider_Impl for ElementProvider_Impl {
    fn Scroll(
        &self,
        horizontal: ScrollAmount,
        vertical: ScrollAmount,
    ) -> windows::core::Result<()> {
        let info = self.scroll_info()?;
        let horizontal = scrolled(
            info.horizontal_percent,
            info.horizontal_view_percent,
            horizontal,
            info.horizontally_scrollable,
        );
        let vertical = scrolled(
            info.vertical_percent,
            info.vertical_view_percent,
            vertical,
            info.vertically_scrollable,
        );
        if horizontal.is_none() && vertical.is_none() {
            return Ok(());
        }
        self.shared.act(
            self.id,
            AccessAction::ScrollTo {
                horizontal,
                vertical,
            },
        )
    }

    fn SetScrollPercent(&self, horizontal: f64, vertical: f64) -> windows::core::Result<()> {
        let info = self.scroll_info()?;
        let axis = |percent: f64, scrollable: bool| {
            if percent == NO_SCROLL {
                Ok(None)
            } else if !scrollable || !(0.0..=100.0).contains(&percent) {
                Err(Error::from(E_INVALIDARG))
            } else {
                Ok(Some(percent as f32))
            }
        };
        let horizontal = axis(horizontal, info.horizontally_scrollable)?;
        let vertical = axis(vertical, info.vertically_scrollable)?;
        self.shared.act(
            self.id,
            AccessAction::ScrollTo {
                horizontal,
                vertical,
            },
        )
    }

    fn HorizontalScrollPercent(&self) -> windows::core::Result<f64> {
        let info = self.scroll_info()?;
        Ok(axis_percent(
            info.horizontal_percent,
            info.horizontally_scrollable,
        ))
    }

    fn VerticalScrollPercent(&self) -> windows::core::Result<f64> {
        let info = self.scroll_info()?;
        Ok(axis_percent(
            info.vertical_percent,
            info.vertically_scrollable,
        ))
    }

    fn HorizontalViewSize(&self) -> windows::core::Result<f64> {
        Ok(f64::from(self.scroll_info()?.horizontal_view_percent))
    }

    fn VerticalViewSize(&self) -> windows::core::Result<f64> {
        Ok(f64::from(self.scroll_info()?.vertical_view_percent))
    }

    fn HorizontallyScrollable(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(self.scroll_info()?.horizontally_scrollable))
    }

    fn VerticallyScrollable(&self) -> windows::core::Result<BOOL> {
        Ok(BOOL::from(self.scroll_info()?.vertically_scrollable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Accessibility::ScrollAmount_NoAmount;

    #[test]
    fn uia_scroll_delta_pages_against_the_scrollable_range() {
        // A third of the content visible: a page is half of the scrollable range.
        let view = 100.0 / 3.0;
        let page = scroll_delta(ScrollAmount_LargeIncrement, view).unwrap_or_default();
        assert!((page - 50.0).abs() < 1e-3);
        assert_eq!(scroll_delta(ScrollAmount_LargeDecrement, view), Some(-page));
        assert_eq!(
            scroll_delta(ScrollAmount_SmallIncrement, view),
            Some(page * 0.25)
        );
        assert_eq!(
            scroll_delta(ScrollAmount_SmallDecrement, view),
            Some(-page * 0.25)
        );
        assert_eq!(scroll_delta(ScrollAmount_NoAmount, view), None);
        // Everything visible: nothing to scroll.
        assert_eq!(scroll_delta(ScrollAmount_LargeIncrement, 100.0), Some(0.0));
    }

    #[test]
    fn uia_scrolled_clamps_and_respects_scrollability() {
        assert_eq!(
            scrolled(90.0, 50.0, ScrollAmount_LargeIncrement, true),
            Some(100.0)
        );
        assert_eq!(
            scrolled(10.0, 50.0, ScrollAmount_LargeDecrement, true),
            Some(0.0)
        );
        assert_eq!(
            scrolled(10.0, 50.0, ScrollAmount_LargeIncrement, false),
            None
        );
        assert_eq!(scrolled(10.0, 50.0, ScrollAmount_NoAmount, true), None);
        assert_eq!(axis_percent(40.0, true), 40.0);
        assert_eq!(axis_percent(40.0, false), -1.0);
    }

    #[test]
    fn uia_toggle_states_map_one_to_one() {
        assert_eq!(uia_toggle(AccessToggle::Off), ToggleState_Off);
        assert_eq!(uia_toggle(AccessToggle::On), ToggleState_On);
        assert_eq!(
            uia_toggle(AccessToggle::Indeterminate),
            ToggleState_Indeterminate
        );
    }
}
