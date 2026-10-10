//! The accessibility tree.
//!
//! Every widget describes itself as an [`AccessNode`] (role, name, bounds, state, patterns);
//! `ssw-win` maps the [`AccessTree`] to UI Automation providers and routes UIA calls back as
//! [`AccessAction`]s. No widget ships without its node.

use std::collections::HashMap;
use std::ops::Range;

use smallvec::SmallVec;

use crate::geometry::{Point, Rect};
use crate::widget::WidgetId;

/// What a node is, mapped to a UIA control type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Window,
    Document,
    Edit,
    Text,
    Button,
    ToggleButton,
    CheckBox,
    ComboBox,
    List,
    ListItem,
    Tree,
    TreeItem,
    Tab,
    TabItem,
    Menu,
    MenuItem,
    ScrollBar,
    Slider,
    ToolBar,
    StatusBar,
    Pane,
    Group,
    Hyperlink,
    Image,
    Table,
    DataItem,
    Separator,
    ToolTip,
    TitleBar,
    /// A control with no standard role; its `description` explains it.
    Custom,
}

/// Check state of toggles and check boxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToggleState {
    Off,
    On,
    Indeterminate,
}

/// State flags of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AccessState {
    /// Can receive keyboard focus.
    pub focusable: bool,
    /// Currently has keyboard focus.
    pub focused: bool,
    pub disabled: bool,
    /// Selected item of a list, tree or tab strip.
    pub selected: bool,
    pub read_only: bool,
    /// Scrolled out of the visible area.
    pub offscreen: bool,
    pub multiline: bool,
    pub password: bool,
    /// `Some` for toggle buttons and check boxes.
    pub toggle: Option<ToggleState>,
    /// `Some(true)` expanded, `Some(false)` collapsed, `None` not expandable.
    pub expanded: Option<bool>,
}

/// UIA RangeValue pattern data (sliders, scroll bars).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangeValue {
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
}

/// UIA Scroll pattern data; percentages are `0.0..=100.0`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollInfo {
    pub horizontal_percent: f32,
    pub vertical_percent: f32,
    /// Visible share of the content, horizontally.
    pub horizontal_view_percent: f32,
    /// Visible share of the content, vertically.
    pub vertical_view_percent: f32,
    pub horizontally_scrollable: bool,
    pub vertically_scrollable: bool,
}

/// UIA Text pattern data of a document or edit; all offsets are UTF-8 bytes of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPatternInfo {
    /// Length of the text in bytes.
    pub len: usize,
    /// Selected ranges (a single empty range at the caret when nothing is selected).
    pub selection: Vec<Range<usize>>,
    /// Caret offset.
    pub caret: usize,
    /// The range currently visible in the viewport.
    pub visible: Range<usize>,
}

/// The UIA patterns a node supports.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Patterns {
    pub invoke: bool,
    /// Toggle state is in [`AccessState::toggle`].
    pub toggle: bool,
    /// Expanded state is in [`AccessState::expanded`].
    pub expand_collapse: bool,
    /// Selected state is in [`AccessState::selected`].
    pub selection_item: bool,
    /// The value is in [`AccessNode::value`].
    pub value: bool,
    pub range_value: Option<RangeValue>,
    pub scroll: Option<ScrollInfo>,
    pub text: Option<TextPatternInfo>,
}

/// One element of the accessibility tree.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessNode {
    pub id: WidgetId,
    pub role: Role,
    /// The accessible name (localized by the caller).
    pub name: String,
    pub description: String,
    /// Bounds in window DIPs.
    pub bounds: Rect,
    pub state: AccessState,
    pub value: Option<String>,
    /// Child ids in z-order, topmost last.
    pub children: SmallVec<[WidgetId; 8]>,
    pub patterns: Patterns,
}

impl AccessNode {
    /// A node with no description, value, children, state flags or patterns.
    #[must_use]
    pub fn new(id: WidgetId, role: Role, name: impl Into<String>, bounds: Rect) -> Self {
        Self {
            id,
            role,
            name: name.into(),
            description: String::new(),
            bounds,
            state: AccessState::default(),
            value: None,
            children: SmallVec::new(),
            patterns: Patterns::default(),
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    #[must_use]
    pub fn with_state(mut self, state: AccessState) -> Self {
        self.state = state;
        self
    }

    #[must_use]
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        self.value = Some(value.into());
        self
    }

    #[must_use]
    pub fn with_children(mut self, children: impl IntoIterator<Item = WidgetId>) -> Self {
        self.children = children.into_iter().collect();
        self
    }

    #[must_use]
    pub fn with_patterns(mut self, patterns: Patterns) -> Self {
        self.patterns = patterns;
        self
    }
}

/// A request from assistive technology, delivered to `Widget::perform`.
#[derive(Debug, Clone, PartialEq)]
pub enum AccessAction {
    Invoke,
    Toggle,
    Expand,
    Collapse,
    Select,
    Focus,
    SetValue(String),
    SetRangeValue(f64),
    /// Scrolls to the given percentages (`0.0..=100.0`); `None` leaves that axis alone.
    ScrollTo {
        horizontal: Option<f32>,
        vertical: Option<f32>,
    },
    /// Scrolls the node into view of its scrolling ancestor.
    ScrollIntoView,
}

/// A snapshot of the accessibility tree. Nodes must form a tree below the root: every child
/// id appears in exactly one node's `children`.
///
/// Widgets call [`AccessTree::insert`] for their own node and let their children do the same;
/// the order of insertion does not matter. Inserting an id twice replaces the node.
#[derive(Debug, Clone)]
pub struct AccessTree {
    root: WidgetId,
    /// In insertion order.
    nodes: Vec<AccessNode>,
    /// Id to position in `nodes`.
    index: HashMap<WidgetId, usize>,
    /// Child id to (parent id, position among the parent's children).
    parents: HashMap<WidgetId, (WidgetId, usize)>,
}

impl AccessTree {
    /// An empty tree whose root will be the node with id `root`.
    #[must_use]
    pub fn new(root: WidgetId) -> Self {
        Self {
            root,
            nodes: Vec::new(),
            index: HashMap::new(),
            parents: HashMap::new(),
        }
    }

    /// Adds `node`, or replaces the node with the same id, and links its children to it.
    pub fn insert(&mut self, node: AccessNode) {
        let id = node.id;
        let slot = self.index.get(&id).copied();
        if let Some(slot) = slot {
            for old_child in &self.nodes[slot].children {
                if self
                    .parents
                    .get(old_child)
                    .is_some_and(|(parent, _)| *parent == id)
                {
                    self.parents.remove(old_child);
                }
            }
        }
        for (position, child) in node.children.iter().enumerate() {
            self.parents.insert(*child, (id, position));
        }
        match slot {
            Some(slot) => self.nodes[slot] = node,
            None => {
                self.index.insert(id, self.nodes.len());
                self.nodes.push(node);
            }
        }
    }

    #[must_use]
    pub fn root_id(&self) -> WidgetId {
        self.root
    }

    /// The root node, once inserted.
    #[must_use]
    pub fn root(&self) -> Option<&AccessNode> {
        self.node(self.root)
    }

    #[must_use]
    pub fn node(&self, id: WidgetId) -> Option<&AccessNode> {
        self.index.get(&id).map(|&slot| &self.nodes[slot])
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// All nodes in insertion order.
    #[must_use]
    pub fn nodes(&self) -> &[AccessNode] {
        &self.nodes
    }

    /// The parent node and the position of `id` among the parent's children.
    fn position(&self, id: WidgetId) -> Option<(&AccessNode, usize)> {
        self.parents
            .get(&id)
            .and_then(|&(parent, position)| self.node(parent).map(|node| (node, position)))
    }

    #[must_use]
    pub fn parent(&self, id: WidgetId) -> Option<&AccessNode> {
        self.position(id).map(|(parent, _)| parent)
    }

    /// The children of `id` that are present in the tree, in order.
    pub fn children(&self, id: WidgetId) -> impl Iterator<Item = &AccessNode> {
        self.node(id).into_iter().flat_map(move |node| {
            node.children
                .iter()
                .filter_map(move |child| self.node(*child))
        })
    }

    #[must_use]
    pub fn first_child(&self, id: WidgetId) -> Option<&AccessNode> {
        self.children(id).next()
    }

    #[must_use]
    pub fn last_child(&self, id: WidgetId) -> Option<&AccessNode> {
        self.children(id).last()
    }

    /// The next sibling present in the tree.
    #[must_use]
    pub fn next_sibling(&self, id: WidgetId) -> Option<&AccessNode> {
        let (parent, position) = self.position(id)?;
        parent.children[position + 1..]
            .iter()
            .find_map(|sibling| self.node(*sibling))
    }

    /// The previous sibling present in the tree.
    #[must_use]
    pub fn previous_sibling(&self, id: WidgetId) -> Option<&AccessNode> {
        let (parent, position) = self.position(id)?;
        parent.children[..position]
            .iter()
            .rev()
            .find_map(|sibling| self.node(*sibling))
    }

    /// The nodes reachable from the root in depth-first pre-order.
    #[must_use]
    pub fn walk(&self) -> Vec<&AccessNode> {
        let mut visited = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<&AccessNode> = self.root().into_iter().collect();
        // Bounded by the node count so a malformed (cyclic) tree cannot loop forever.
        while visited.len() < self.nodes.len() {
            let Some(node) = stack.pop() else { break };
            visited.push(node);
            for child in node.children.iter().rev() {
                if let Some(child) = self.node(*child) {
                    stack.push(child);
                }
            }
        }
        visited
    }

    /// The node with keyboard focus, if any.
    #[must_use]
    pub fn focused(&self) -> Option<&AccessNode> {
        self.nodes.iter().find(|node| node.state.focused)
    }

    /// The deepest, topmost node whose bounds contain `point`.
    #[must_use]
    pub fn hit_test(&self, point: Point) -> Option<&AccessNode> {
        self.root().and_then(|root| self.hit_test_in(root, point))
    }

    fn hit_test_in<'a>(&'a self, node: &'a AccessNode, point: Point) -> Option<&'a AccessNode> {
        if !node.bounds.contains(point) {
            return None;
        }
        let deeper = node
            .children
            .iter()
            .rev()
            .filter_map(|child| self.node(*child))
            .find_map(|child| self.hit_test_in(child, point));
        Some(deeper.unwrap_or(node))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(raw: u64) -> WidgetId {
        WidgetId::new(raw)
    }

    fn names(nodes: &[&AccessNode]) -> Vec<String> {
        nodes.iter().map(|node| node.name.clone()).collect()
    }

    /// window(1) -> [toolbar(2) -> [bold(3), italic(4)], document(5), status(6)]
    fn sample() -> AccessTree {
        let mut tree = AccessTree::new(id(1));
        tree.insert(
            AccessNode::new(
                id(1),
                Role::Window,
                "SS Writer",
                Rect::new(0.0, 0.0, 800.0, 600.0),
            )
            .with_children([id(2), id(5), id(6)]),
        );
        tree.insert(
            AccessNode::new(
                id(2),
                Role::ToolBar,
                "Formatting",
                Rect::new(0.0, 0.0, 800.0, 40.0),
            )
            .with_children([id(3), id(4)]),
        );
        tree.insert(AccessNode::new(
            id(3),
            Role::Button,
            "Bold",
            Rect::new(0.0, 0.0, 40.0, 40.0),
        ));
        tree.insert(AccessNode::new(
            id(4),
            Role::Button,
            "Italic",
            Rect::new(40.0, 0.0, 40.0, 40.0),
        ));
        tree.insert(AccessNode::new(
            id(5),
            Role::Document,
            "Editor",
            Rect::new(0.0, 40.0, 800.0, 520.0),
        ));
        tree.insert(AccessNode::new(
            id(6),
            Role::StatusBar,
            "Status",
            Rect::new(0.0, 560.0, 800.0, 40.0),
        ));
        tree
    }

    #[test]
    fn root_and_lookup() {
        let tree = sample();
        assert_eq!(tree.len(), 6);
        assert!(!tree.is_empty());
        assert_eq!(tree.root_id(), id(1));
        assert_eq!(tree.root().map(|n| n.role), Some(Role::Window));
        assert_eq!(tree.node(id(3)).map(|n| n.name.as_str()), Some("Bold"));
        assert!(tree.node(id(99)).is_none());
        assert_eq!(tree.nodes().len(), 6);
        assert_eq!(tree.nodes()[0].id, id(1));

        let empty = AccessTree::new(id(7));
        assert!(empty.is_empty());
        assert!(empty.root().is_none());
        assert!(empty.walk().is_empty());
        assert!(empty.hit_test(Point::new(1.0, 1.0)).is_none());
        assert!(empty.focused().is_none());
    }

    #[test]
    fn parent_and_child_navigation() {
        let tree = sample();
        assert_eq!(tree.parent(id(3)).map(|n| n.id), Some(id(2)));
        assert_eq!(tree.parent(id(2)).map(|n| n.id), Some(id(1)));
        assert!(tree.parent(id(1)).is_none());
        assert!(tree.parent(id(99)).is_none());

        let toolbar_children: Vec<WidgetId> = tree.children(id(2)).map(|n| n.id).collect();
        assert_eq!(toolbar_children, vec![id(3), id(4)]);
        assert_eq!(tree.children(id(3)).count(), 0);
        assert_eq!(tree.children(id(99)).count(), 0);
        assert_eq!(tree.first_child(id(1)).map(|n| n.id), Some(id(2)));
        assert_eq!(tree.last_child(id(1)).map(|n| n.id), Some(id(6)));
        assert!(tree.first_child(id(3)).is_none());
        assert!(tree.last_child(id(99)).is_none());
    }

    #[test]
    fn sibling_navigation() {
        let tree = sample();
        assert_eq!(tree.next_sibling(id(2)).map(|n| n.id), Some(id(5)));
        assert_eq!(tree.next_sibling(id(5)).map(|n| n.id), Some(id(6)));
        assert!(tree.next_sibling(id(6)).is_none());
        assert_eq!(tree.previous_sibling(id(6)).map(|n| n.id), Some(id(5)));
        assert_eq!(tree.previous_sibling(id(4)).map(|n| n.id), Some(id(3)));
        assert!(tree.previous_sibling(id(2)).is_none());
        // The root and unknown ids have no siblings.
        assert!(tree.next_sibling(id(1)).is_none());
        assert!(tree.previous_sibling(id(1)).is_none());
        assert!(tree.next_sibling(id(99)).is_none());
    }

    #[test]
    fn siblings_skip_children_missing_from_the_tree() {
        let mut tree = AccessTree::new(id(1));
        tree.insert(
            AccessNode::new(id(1), Role::List, "List", Rect::new(0.0, 0.0, 10.0, 30.0))
                .with_children([id(2), id(3), id(4)]),
        );
        tree.insert(AccessNode::new(
            id(2),
            Role::ListItem,
            "a",
            Rect::new(0.0, 0.0, 10.0, 10.0),
        ));
        // id 3 is virtualized away and never inserted.
        tree.insert(AccessNode::new(
            id(4),
            Role::ListItem,
            "c",
            Rect::new(0.0, 20.0, 10.0, 10.0),
        ));
        assert_eq!(tree.next_sibling(id(2)).map(|n| n.id), Some(id(4)));
        assert_eq!(tree.previous_sibling(id(4)).map(|n| n.id), Some(id(2)));
        assert_eq!(tree.children(id(1)).count(), 2);
        assert_eq!(tree.last_child(id(1)).map(|n| n.id), Some(id(4)));
        // A walk steps over the child that is not in the tree.
        assert_eq!(names(&tree.walk()), vec!["List", "a", "c"]);
    }

    #[test]
    fn walk_is_depth_first_preorder_and_ignores_orphans() {
        let mut tree = sample();
        assert_eq!(
            names(&tree.walk()),
            vec![
                "SS Writer",
                "Formatting",
                "Bold",
                "Italic",
                "Editor",
                "Status"
            ]
        );
        tree.insert(AccessNode::new(id(50), Role::Pane, "Orphan", Rect::ZERO));
        assert_eq!(tree.len(), 7);
        assert_eq!(tree.walk().len(), 6);
    }

    #[test]
    fn replacing_a_node_relinks_its_children() {
        let mut tree = sample();
        // The toolbar now only has "Italic"; "Bold" is no longer its child.
        tree.insert(
            AccessNode::new(
                id(2),
                Role::ToolBar,
                "Formatting",
                Rect::new(0.0, 0.0, 800.0, 40.0),
            )
            .with_children([id(4)]),
        );
        assert_eq!(tree.len(), 6);
        assert!(tree.parent(id(3)).is_none());
        assert_eq!(tree.parent(id(4)).map(|n| n.id), Some(id(2)));
        assert_eq!(tree.children(id(2)).count(), 1);

        // A child moved to another parent keeps the newer link when the old parent is replaced.
        tree.insert(
            AccessNode::new(
                id(6),
                Role::StatusBar,
                "Status",
                Rect::new(0.0, 560.0, 800.0, 40.0),
            )
            .with_children([id(4)]),
        );
        assert_eq!(tree.parent(id(4)).map(|n| n.id), Some(id(6)));
        tree.insert(AccessNode::new(
            id(2),
            Role::ToolBar,
            "Formatting",
            Rect::new(0.0, 0.0, 800.0, 40.0),
        ));
        assert_eq!(tree.parent(id(4)).map(|n| n.id), Some(id(6)));
    }

    #[test]
    fn hit_test_returns_the_deepest_topmost_node() {
        let tree = sample();
        assert_eq!(
            tree.hit_test(Point::new(50.0, 10.0)).map(|n| n.id),
            Some(id(4))
        );
        assert_eq!(
            tree.hit_test(Point::new(10.0, 10.0)).map(|n| n.id),
            Some(id(3))
        );
        assert_eq!(
            tree.hit_test(Point::new(300.0, 10.0)).map(|n| n.id),
            Some(id(2))
        );
        assert_eq!(
            tree.hit_test(Point::new(300.0, 300.0)).map(|n| n.id),
            Some(id(5))
        );
        assert_eq!(tree.hit_test(Point::new(5000.0, 5.0)).map(|n| n.id), None);

        // With overlapping children the later (topmost) one wins.
        let mut overlap = AccessTree::new(id(1));
        overlap.insert(
            AccessNode::new(id(1), Role::Pane, "root", Rect::new(0.0, 0.0, 100.0, 100.0))
                .with_children([id(2), id(3)]),
        );
        overlap.insert(AccessNode::new(
            id(2),
            Role::Pane,
            "below",
            Rect::new(0.0, 0.0, 60.0, 60.0),
        ));
        overlap.insert(AccessNode::new(
            id(3),
            Role::Pane,
            "above",
            Rect::new(30.0, 30.0, 60.0, 60.0),
        ));
        assert_eq!(
            overlap.hit_test(Point::new(40.0, 40.0)).map(|n| n.id),
            Some(id(3))
        );
        assert_eq!(
            overlap.hit_test(Point::new(10.0, 10.0)).map(|n| n.id),
            Some(id(2))
        );
        assert_eq!(
            overlap.hit_test(Point::new(95.0, 5.0)).map(|n| n.id),
            Some(id(1))
        );
    }

    #[test]
    fn focused_node_is_found_by_state() {
        let mut tree = sample();
        assert!(tree.focused().is_none());
        let focus = AccessState {
            focusable: true,
            focused: true,
            ..AccessState::default()
        };
        tree.insert(
            AccessNode::new(
                id(5),
                Role::Document,
                "Editor",
                Rect::new(0.0, 40.0, 800.0, 520.0),
            )
            .with_state(focus),
        );
        assert_eq!(tree.focused().map(|n| n.id), Some(id(5)));
    }

    #[test]
    fn node_builders_set_every_field() {
        let patterns = Patterns {
            invoke: true,
            text: Some(TextPatternInfo {
                len: 12,
                selection: vec![std::ops::Range { start: 3, end: 3 }],
                caret: 3,
                visible: 0..12,
            }),
            range_value: Some(RangeValue {
                value: 1.0,
                min: 0.0,
                max: 10.0,
                step: 0.5,
            }),
            scroll: Some(ScrollInfo {
                horizontal_percent: 0.0,
                vertical_percent: 25.0,
                horizontal_view_percent: 100.0,
                vertical_view_percent: 40.0,
                horizontally_scrollable: false,
                vertically_scrollable: true,
            }),
            ..Patterns::default()
        };
        let state = AccessState {
            toggle: Some(ToggleState::Indeterminate),
            expanded: Some(false),
            ..AccessState::default()
        };
        let node = AccessNode::new(
            id(9),
            Role::CheckBox,
            String::from("Wrap"),
            Rect::new(1.0, 2.0, 3.0, 4.0),
        )
        .with_description("Wraps long lines")
        .with_value("on")
        .with_state(state)
        .with_children(vec![id(10), id(11)])
        .with_patterns(patterns.clone());
        assert_eq!(node.id, id(9));
        assert_eq!(node.role, Role::CheckBox);
        assert_eq!(node.name, "Wrap");
        assert_eq!(node.description, "Wraps long lines");
        assert_eq!(node.value.as_deref(), Some("on"));
        assert_eq!(node.bounds, Rect::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(node.state, state);
        assert_eq!(node.children.as_slice(), &[id(10), id(11)]);
        assert_eq!(node.patterns, patterns);

        let bare = AccessNode::new(id(1), Role::Text, "x", Rect::ZERO);
        assert!(bare.description.is_empty() && bare.value.is_none() && bare.children.is_empty());
        assert_eq!(bare.state, AccessState::default());
        assert_eq!(bare.patterns, Patterns::default());
        assert_eq!(bare.clone(), bare);
    }

    #[test]
    fn actions_compare_by_payload() {
        assert_eq!(
            AccessAction::SetValue("a".to_owned()),
            AccessAction::SetValue("a".to_owned())
        );
        assert_ne!(
            AccessAction::SetValue("a".to_owned()),
            AccessAction::SetValue("b".to_owned())
        );
        assert_ne!(AccessAction::Expand, AccessAction::Collapse);
        let scroll = AccessAction::ScrollTo {
            horizontal: None,
            vertical: Some(50.0),
        };
        assert_eq!(scroll.clone(), scroll);
        assert_ne!(
            AccessAction::SetRangeValue(1.0),
            AccessAction::SetRangeValue(2.0)
        );
    }
}
