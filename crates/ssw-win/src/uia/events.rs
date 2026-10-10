//! UIA events: change detection between two [`AccessTree`] snapshots and raising the matching
//! automation events (focus changed, property changed, structure changed, text selection
//! changed). Events are only raised while a UIA client is listening.

use std::ptr;
use std::sync::Arc;

use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    StructureChangeType_ChildrenInvalidated, UIA_AutomationFocusChangedEventId,
    UIA_ExpandCollapseExpandCollapseStatePropertyId, UIA_IsEnabledPropertyId,
    UIA_IsOffscreenPropertyId, UIA_NamePropertyId, UIA_PROPERTY_ID,
    UIA_SelectionItemIsSelectedPropertyId, UIA_Text_TextSelectionChangedEventId,
    UIA_ToggleToggleStatePropertyId, UIA_ValueValuePropertyId, UiaClientsAreListening,
    UiaRaiseAutomationEvent, UiaRaiseAutomationPropertyChangedEvent, UiaRaiseStructureChangedEvent,
};

use ssw_ui::access::{AccessNode, AccessTree, ToggleState};
use ssw_ui::widget::WidgetId;

use super::{Shared, UiaError};
use crate::fault::check;

/// A property whose change is reported to UIA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiaProperty {
    Name,
    Toggle,
    ExpandCollapse,
    Enabled,
    Value,
    Selected,
    Offscreen,
}

impl UiaProperty {
    fn id(self) -> UIA_PROPERTY_ID {
        match self {
            UiaProperty::Name => UIA_NamePropertyId,
            UiaProperty::Toggle => UIA_ToggleToggleStatePropertyId,
            UiaProperty::ExpandCollapse => UIA_ExpandCollapseExpandCollapseStatePropertyId,
            UiaProperty::Enabled => UIA_IsEnabledPropertyId,
            UiaProperty::Value => UIA_ValueValuePropertyId,
            UiaProperty::Selected => UIA_SelectionItemIsSelectedPropertyId,
            UiaProperty::Offscreen => UIA_IsOffscreenPropertyId,
        }
    }
}

/// An old or new property value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyValue {
    Text(String),
    /// Enumerated states (toggle and expand/collapse states use UIA's numbering).
    Number(i32),
    Flag(bool),
}

impl PropertyValue {
    fn variant(&self) -> VARIANT {
        match self {
            PropertyValue::Text(text) => VARIANT::from(text.as_str()),
            PropertyValue::Number(number) => VARIANT::from(*number),
            PropertyValue::Flag(flag) => VARIANT::from(*flag),
        }
    }
}

/// One detected difference between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiaChange {
    /// Keyboard focus moved to this node.
    FocusChanged(WidgetId),
    PropertyChanged {
        id: WidgetId,
        property: UiaProperty,
        old: PropertyValue,
        new: PropertyValue,
    },
    /// The children of this node changed.
    StructureChanged(WidgetId),
    /// The selection or caret of this text node moved.
    TextSelectionChanged(WidgetId),
}

fn toggle_number(state: Option<ToggleState>) -> i32 {
    match state {
        None | Some(ToggleState::Off) => 0,
        Some(ToggleState::On) => 1,
        Some(ToggleState::Indeterminate) => 2,
    }
}

/// UIA's ExpandCollapseState numbering: collapsed 0, expanded 1, leaf 3.
fn expand_number(expanded: Option<bool>) -> i32 {
    match expanded {
        Some(false) => 0,
        Some(true) => 1,
        None => 3,
    }
}

/// The differences between two snapshots, in a stable order: focus, then per node (in the new
/// tree's insertion order) properties, structure and text selection.
#[must_use]
pub fn diff(old: &AccessTree, new: &AccessTree) -> Vec<UiaChange> {
    let mut changes = Vec::new();
    let new_focus = new.focused().map(|node| node.id);
    if new_focus != old.focused().map(|node| node.id)
        && let Some(id) = new_focus
    {
        changes.push(UiaChange::FocusChanged(id));
    }
    for node in new.nodes() {
        let Some(before) = old.node(node.id) else {
            continue;
        };
        let id = node.id;
        let pairs = [
            (
                UiaProperty::Name,
                PropertyValue::Text(before.name.clone()),
                PropertyValue::Text(node.name.clone()),
            ),
            (
                UiaProperty::Toggle,
                PropertyValue::Number(toggle_number(before.state.toggle)),
                PropertyValue::Number(toggle_number(node.state.toggle)),
            ),
            (
                UiaProperty::ExpandCollapse,
                PropertyValue::Number(expand_number(before.state.expanded)),
                PropertyValue::Number(expand_number(node.state.expanded)),
            ),
            (
                UiaProperty::Enabled,
                PropertyValue::Flag(!before.state.disabled),
                PropertyValue::Flag(!node.state.disabled),
            ),
            (
                UiaProperty::Value,
                PropertyValue::Text(before.value.clone().unwrap_or_default()),
                PropertyValue::Text(node.value.clone().unwrap_or_default()),
            ),
            (
                UiaProperty::Selected,
                PropertyValue::Flag(before.state.selected),
                PropertyValue::Flag(node.state.selected),
            ),
            (
                UiaProperty::Offscreen,
                PropertyValue::Flag(before.state.offscreen),
                PropertyValue::Flag(node.state.offscreen),
            ),
        ];
        for (property, old_value, new_value) in pairs {
            if old_value != new_value {
                changes.push(UiaChange::PropertyChanged {
                    id,
                    property,
                    old: old_value,
                    new: new_value,
                });
            }
        }
        if before.children != node.children {
            changes.push(UiaChange::StructureChanged(id));
        }
        let selection = |node: &AccessNode| {
            node.patterns
                .text
                .as_ref()
                .map(|text| (text.selection.clone(), text.caret))
        };
        if selection(before) != selection(node) {
            changes.push(UiaChange::TextSelectionChanged(id));
        }
    }
    changes
}

/// Raises the UIA events for `changes`, if a UIA client is listening.
pub(crate) fn raise(shared: &Arc<Shared>, changes: &[UiaChange]) -> Result<(), UiaError> {
    // SAFETY: UiaClientsAreListening has no preconditions.
    if changes.is_empty() || !unsafe { UiaClientsAreListening() }.as_bool() {
        return Ok(());
    }
    for change in changes {
        match change {
            UiaChange::FocusChanged(id) => {
                let provider = shared.provider(*id);
                // SAFETY: `provider` is a live provider created by this bridge.
                check(unsafe {
                    UiaRaiseAutomationEvent(&provider, UIA_AutomationFocusChangedEventId)
                })?;
            }
            UiaChange::PropertyChanged {
                id,
                property,
                old,
                new,
            } => {
                let provider = shared.provider(*id);
                // SAFETY: `provider` is live and both VARIANTs outlive the call.
                check(unsafe {
                    UiaRaiseAutomationPropertyChangedEvent(
                        &provider,
                        property.id(),
                        &old.variant(),
                        &new.variant(),
                    )
                })?;
            }
            UiaChange::StructureChanged(id) => {
                let provider = shared.provider(*id);
                // SAFETY: a null runtime id with length 0 is allowed for this change type.
                check(unsafe {
                    UiaRaiseStructureChangedEvent(
                        &provider,
                        StructureChangeType_ChildrenInvalidated,
                        ptr::null_mut(),
                        0,
                    )
                })?;
            }
            UiaChange::TextSelectionChanged(id) => {
                let provider = shared.provider(*id);
                // SAFETY: `provider` is a live provider created by this bridge.
                check(unsafe {
                    UiaRaiseAutomationEvent(&provider, UIA_Text_TextSelectionChangedEventId)
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssw_ui::access::{AccessState, Role, TextPatternInfo};
    use ssw_ui::geometry::Rect;

    const ROOT: WidgetId = WidgetId::new(1);
    const BUTTON: WidgetId = WidgetId::new(2);
    const EDIT: WidgetId = WidgetId::new(3);

    fn tree(button: AccessNode, edit: AccessNode, children: &[WidgetId]) -> AccessTree {
        let mut tree = AccessTree::new(ROOT);
        tree.insert(
            AccessNode::new(ROOT, Role::Window, "w", Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_children(children.iter().copied()),
        );
        tree.insert(button);
        tree.insert(edit);
        tree
    }

    fn button(name: &str, state: AccessState) -> AccessNode {
        AccessNode::new(BUTTON, Role::Button, name, Rect::new(0.0, 0.0, 10.0, 10.0))
            .with_state(state)
    }

    fn edit(selection: std::ops::Range<usize>) -> AccessNode {
        let mut node = AccessNode::new(
            EDIT,
            Role::Document,
            "doc",
            Rect::new(0.0, 10.0, 90.0, 80.0),
        );
        node.patterns.text = Some(TextPatternInfo {
            len: 10,
            caret: selection.end,
            selection: vec![selection],
            visible: 0..10,
        });
        node
    }

    #[test]
    fn uia_diff_reports_nothing_for_identical_trees() {
        let a = tree(
            button("Save", AccessState::default()),
            edit(0..0),
            &[BUTTON, EDIT],
        );
        assert!(diff(&a, &a.clone()).is_empty());
    }

    #[test]
    fn uia_diff_reports_focus_properties_structure_and_selection() {
        let old = tree(
            button("Save", AccessState::default()),
            edit(0..0),
            &[BUTTON, EDIT],
        );
        let focused = AccessState {
            focused: true,
            disabled: true,
            toggle: Some(ToggleState::On),
            expanded: Some(true),
            selected: true,
            offscreen: true,
            ..AccessState::default()
        };
        let new_button = button("Saved", focused).with_value("v");
        let new = tree(new_button, edit(2..5), &[BUTTON]);
        let changes = diff(&old, &new);
        assert_eq!(changes[0], UiaChange::FocusChanged(BUTTON));
        let properties: Vec<(UiaProperty, PropertyValue, PropertyValue)> = changes
            .iter()
            .filter_map(|change| match change {
                UiaChange::PropertyChanged {
                    id,
                    property,
                    old,
                    new,
                } if *id == BUTTON => Some((*property, old.clone(), new.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            properties,
            vec![
                (
                    UiaProperty::Name,
                    PropertyValue::Text("Save".into()),
                    PropertyValue::Text("Saved".into())
                ),
                (
                    UiaProperty::Toggle,
                    PropertyValue::Number(0),
                    PropertyValue::Number(1)
                ),
                (
                    UiaProperty::ExpandCollapse,
                    PropertyValue::Number(3),
                    PropertyValue::Number(1)
                ),
                (
                    UiaProperty::Enabled,
                    PropertyValue::Flag(true),
                    PropertyValue::Flag(false)
                ),
                (
                    UiaProperty::Value,
                    PropertyValue::Text(String::new()),
                    PropertyValue::Text("v".into())
                ),
                (
                    UiaProperty::Selected,
                    PropertyValue::Flag(false),
                    PropertyValue::Flag(true)
                ),
                (
                    UiaProperty::Offscreen,
                    PropertyValue::Flag(false),
                    PropertyValue::Flag(true)
                ),
            ]
        );
        assert!(changes.contains(&UiaChange::StructureChanged(ROOT)));
        assert!(changes.contains(&UiaChange::TextSelectionChanged(EDIT)));
    }

    #[test]
    fn uia_diff_focus_loss_alone_raises_no_focus_event_and_new_nodes_are_structure_only() {
        let focused = AccessState {
            focused: true,
            ..AccessState::default()
        };
        let old = tree(button("Save", focused), edit(0..0), &[BUTTON]);
        let new = tree(
            button("Save", AccessState::default()),
            edit(0..0),
            &[BUTTON, EDIT],
        );
        let changes = diff(&old, &new);
        assert_eq!(changes, vec![UiaChange::StructureChanged(ROOT)]);
        // A node absent from the old tree produces no property changes of its own.
        let empty = AccessTree::new(ROOT);
        assert!(diff(&empty, &new).is_empty());
    }

    #[test]
    fn uia_property_values_and_numbering() {
        assert_eq!(toggle_number(None), 0);
        assert_eq!(toggle_number(Some(ToggleState::Indeterminate)), 2);
        assert_eq!(expand_number(Some(false)), 0);
        assert_eq!(expand_number(Some(true)), 1);
        assert_eq!(expand_number(None), 3);
        for property in [
            UiaProperty::Name,
            UiaProperty::Toggle,
            UiaProperty::ExpandCollapse,
            UiaProperty::Enabled,
            UiaProperty::Value,
            UiaProperty::Selected,
            UiaProperty::Offscreen,
        ] {
            assert_ne!(property.id().0, 0);
        }
        assert_eq!(
            i32::try_from(&PropertyValue::Number(7).variant()).ok(),
            Some(7)
        );
        assert_eq!(
            bool::try_from(&PropertyValue::Flag(true).variant()).ok(),
            Some(true)
        );
        assert!(!PropertyValue::Text("x".into()).variant().is_empty());
    }
}
