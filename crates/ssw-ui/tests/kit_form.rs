//! Widget-level integration: a form of controls laid out by a minimal host that routes Tab,
//! keyboard and pointer events to the focused or hit widget like the real window does.

use std::cell::RefCell;
use std::rc::Rc;

use ssw_ui::access::{AccessAction, AccessTree, Role, ToggleState};
use ssw_ui::canvas::RecordingCanvas;
use ssw_ui::geometry::{Point, Rect, Size};
use ssw_ui::input::{
    Event, FocusEvent, KeyEvent, Modifiers, PointerButton, PointerEvent, PointerEventKind,
    TextInput, vk,
};
use ssw_ui::services::MemoryClipboard;
use ssw_ui::text::MonoTextEngine;
use ssw_ui::theme::{DEFAULT_ACCENT, Theme};
use ssw_ui::widget::{
    AccessCx, EventCx, FocusManager, LayoutCx, PaintCx, PointerCapture, Requests, Widget, WidgetId,
};
use ssw_ui::widgets::Control;
use ssw_ui::widgets::anim::Motion;
use ssw_ui::widgets::button::{Button, ButtonKind};
use ssw_ui::widgets::checkbox::CheckBox;
use ssw_ui::widgets::combo::ComboBox;
use ssw_ui::widgets::list::{ListView, SelectionMode};
use ssw_ui::widgets::slider::Slider;
use ssw_ui::widgets::textbox::TextBox;
use ssw_ui::widgets::toggle::ToggleSwitch;

struct Form {
    button: Button,
    check: CheckBox,
    toggle: ToggleSwitch,
    slider: Slider,
    text: TextBox,
    combo: ComboBox,
    list: ListView,
}

impl Form {
    fn new() -> Self {
        let motion = Motion::reduced();
        Self {
            button: Button::new(ButtonKind::Accent, "Apply").with_motion(motion),
            check: CheckBox::new("Remember", ToggleState::Off).with_motion(motion),
            toggle: ToggleSwitch::new("Autosave", false).with_motion(motion),
            slider: Slider::new("Zoom", 0.0, 100.0, 50.0, 1.0)
                .with_large_step(10.0)
                .with_motion(motion),
            text: TextBox::new(
                "Name",
                "",
                Rc::new(RefCell::new(MemoryClipboard::default())),
            )
            .with_motion(motion),
            combo: ComboBox::new(
                "Theme",
                ["Light", "Dark", "System"].map(String::from).to_vec(),
                Some(0),
            )
            .with_motion(motion),
            list: ListView::new(
                "Recent",
                (0..6).map(|index| format!("File {index}")).collect(),
                SelectionMode::Single,
            )
            .with_motion(motion),
        }
    }

    fn controls(&mut self) -> [&mut dyn Control; 7] {
        [
            &mut self.button,
            &mut self.check,
            &mut self.toggle,
            &mut self.slider,
            &mut self.text,
            &mut self.combo,
            &mut self.list,
        ]
    }
}

struct Host {
    theme: Theme,
    engine: MonoTextEngine,
    focus: FocusManager,
    capture: PointerCapture,
    requests: Requests,
}

impl Host {
    fn new() -> Self {
        Self {
            theme: Theme::light(DEFAULT_ACCENT),
            engine: MonoTextEngine::default(),
            focus: FocusManager::new(),
            capture: PointerCapture::new(),
            requests: Requests::new(),
        }
    }

    /// Stacks the controls vertically, 8 DIPs apart, and commits the tab order.
    fn layout(&mut self, form: &mut Form) {
        self.focus.begin_tab_order();
        let mut y = 10.0;
        for control in form.controls() {
            let id = control.id();
            let size = {
                let mut cx = LayoutCx::new(
                    id,
                    &self.theme,
                    1.0,
                    &self.engine,
                    &mut self.focus,
                    &mut self.requests,
                );
                control.measure(&mut cx, Size::new(300.0, f32::INFINITY))
            };
            let mut cx = LayoutCx::new(
                id,
                &self.theme,
                1.0,
                &self.engine,
                &mut self.focus,
                &mut self.requests,
            );
            control.arrange(
                &mut cx,
                Rect::new(10.0, y, size.width.min(300.0), size.height),
            );
            y += size.height + 8.0;
        }
        self.focus.commit_tab_order();
        self.deliver_focus(form);
    }

    /// Sends the queued focus changes to the widgets involved.
    fn deliver_focus(&mut self, form: &mut Form) {
        for change in self.focus.take_changes() {
            for (id, event) in [
                (change.lost, FocusEvent::Lost),
                (change.gained, FocusEvent::Gained),
            ] {
                if let Some(id) = id {
                    self.send_to(form, id, &Event::Focus(event));
                }
            }
        }
    }

    fn send_to(&mut self, form: &mut Form, id: WidgetId, event: &Event) -> bool {
        for control in form.controls() {
            if control.id() == id {
                let mut cx = EventCx::new(
                    id,
                    &self.theme,
                    1.0,
                    &self.engine,
                    &mut self.focus,
                    &mut self.capture,
                    &mut self.requests,
                );
                return control.event(&mut cx, event).handled;
            }
        }
        false
    }

    /// A key press: Tab and Shift+Tab move focus, everything else goes to the focused control.
    fn press(&mut self, form: &mut Form, key: u16, mods: Modifiers) -> bool {
        if key == vk::TAB {
            let moved = if mods.shift() {
                self.focus.focus_previous()
            } else {
                self.focus.focus_next()
            };
            self.deliver_focus(form);
            return moved;
        }
        let Some(focused) = self.focus.focused() else {
            return false;
        };
        self.send_to(form, focused, &Event::Key(KeyEvent::press(key, mods)))
    }

    fn type_text(&mut self, form: &mut Form, text: &str) -> bool {
        let Some(focused) = self.focus.focused() else {
            return false;
        };
        self.send_to(form, focused, &Event::Text(TextInput(text.to_owned())))
    }

    /// A left click: the pointer-capturing control gets both events, otherwise the control under
    /// the pointer.
    fn click(&mut self, form: &mut Form, at: Point) {
        let tree = self.tree(form);
        let order = ids(form);
        for kind in [PointerEventKind::Down, PointerEventKind::Up] {
            let event = Event::Pointer(PointerEvent {
                click_count: 1,
                ..PointerEvent::mouse(kind, Some(PointerButton::Left), at)
            });
            let under = order
                .iter()
                .copied()
                .find(|id| tree.node(*id).is_some_and(|node| node.bounds.contains(at)));
            let Some(id) = self.capture.owner().or(under) else {
                return;
            };
            for control in form.controls() {
                if control.id() == id {
                    let mut cx = EventCx::new(
                        id,
                        &self.theme,
                        1.0,
                        &self.engine,
                        &mut self.focus,
                        &mut self.capture,
                        &mut self.requests,
                    );
                    let result = control.event(&mut cx, &event);
                    if result.request_focus {
                        self.focus.focus(id);
                    }
                }
            }
            self.deliver_focus(form);
        }
    }

    fn tree(&self, form: &mut Form) -> AccessTree {
        let root = WidgetId::allocate();
        let mut tree = AccessTree::new(root);
        for control in form.controls() {
            let cx = AccessCx::new(control.id(), &self.theme, 1.0, &self.engine, &self.focus);
            control.access(&cx, &mut tree);
        }
        tree
    }

    fn paint(&self, form: &mut Form) -> RecordingCanvas {
        let mut canvas = RecordingCanvas::new();
        for control in form.controls() {
            let cx = PaintCx::new(
                control.id(),
                &self.theme,
                1.0,
                &self.engine,
                &self.focus,
                &self.capture,
                None,
            );
            control.paint(&cx, &mut canvas);
        }
        assert!(canvas.is_balanced());
        canvas
    }
}

fn ids(form: &mut Form) -> Vec<WidgetId> {
    form.controls().iter().map(|control| control.id()).collect()
}

#[test]
fn kit_form_tab_visits_every_control_in_layout_order_and_wraps() {
    let mut form = Form::new();
    let mut host = Host::new();
    host.layout(&mut form);
    let expected = ids(&mut form);
    assert_eq!(host.focus.tab_order(), expected.as_slice());
    let mut visited = Vec::new();
    for _ in 0..expected.len() {
        assert!(host.press(&mut form, vk::TAB, Modifiers::NONE));
        visited.push(host.focus.focused().expect("focused"));
    }
    assert_eq!(visited, expected);
    // Tab from the last control wraps to the first; Shift+Tab goes back to the last.
    host.press(&mut form, vk::TAB, Modifiers::NONE);
    assert_eq!(host.focus.focused(), Some(expected[0]));
    host.press(&mut form, vk::TAB, Modifiers::SHIFT);
    assert_eq!(host.focus.focused(), Some(expected[6]));
    host.press(&mut form, vk::TAB, Modifiers::SHIFT);
    assert_eq!(host.focus.focused(), Some(expected[5]));
}

#[test]
fn kit_form_disabled_controls_are_skipped_by_tab() {
    let mut form = Form::new();
    form.toggle.set_enabled(false);
    form.slider.set_enabled(false);
    let mut host = Host::new();
    host.layout(&mut form);
    let all = ids(&mut form);
    let mut visited = Vec::new();
    for _ in 0..5 {
        host.press(&mut form, vk::TAB, Modifiers::NONE);
        visited.push(host.focus.focused().expect("focused"));
    }
    assert_eq!(visited, vec![all[0], all[1], all[4], all[5], all[6]]);
    // Disabling the focused control removes it from the order and clears focus.
    form.text.set_enabled(false);
    host.layout(&mut form);
    host.focus.focus(all[5]);
    host.press(&mut form, vk::TAB, Modifiers::SHIFT);
    assert_eq!(host.focus.focused(), Some(all[1]));
}

#[test]
fn kit_form_keys_reach_only_the_focused_control() {
    let mut form = Form::new();
    let mut host = Host::new();
    host.layout(&mut form);
    // Nothing is focused yet: keys and text go nowhere.
    assert!(!host.press(&mut form, vk::SPACE, Modifiers::NONE));
    assert!(!host.type_text(&mut form, "a"));
    host.press(&mut form, vk::TAB, Modifiers::NONE); // button
    host.press(&mut form, vk::SPACE, Modifiers::NONE);
    assert!(form.button.take_activated());
    assert_eq!(form.check.state(), ToggleState::Off);
    host.press(&mut form, vk::TAB, Modifiers::NONE); // check box
    host.press(&mut form, vk::SPACE, Modifiers::NONE);
    assert_eq!(form.check.state(), ToggleState::On);
    assert_eq!(form.check.take_changed(), Some(ToggleState::On));
    assert!(
        !form.button.take_activated(),
        "the button is not focused any more"
    );
    host.press(&mut form, vk::TAB, Modifiers::NONE); // toggle
    host.press(&mut form, vk::SPACE, Modifiers::NONE);
    assert!(form.toggle.is_on());
    host.press(&mut form, vk::TAB, Modifiers::NONE); // slider
    host.press(&mut form, vk::RIGHT, Modifiers::NONE);
    host.press(&mut form, vk::PAGE_UP, Modifiers::NONE);
    assert_eq!(form.slider.value(), 61.0);
    host.press(&mut form, vk::END, Modifiers::NONE);
    assert_eq!(form.slider.take_changed(), Some(100.0));
    host.press(&mut form, vk::TAB, Modifiers::NONE); // text box
    assert!(host.type_text(&mut form, "Ada"));
    host.press(&mut form, vk::LEFT, Modifiers::NONE);
    host.type_text(&mut form, "-");
    assert_eq!(form.text.text(), "Ad-a");
    host.press(&mut form, vk::TAB, Modifiers::NONE); // combo box
    host.press(&mut form, vk::DOWN, Modifiers::NONE);
    assert_eq!(form.combo.selected_text(), Some("Dark"));
    host.press(&mut form, vk::TAB, Modifiers::NONE); // list
    host.press(&mut form, vk::DOWN, Modifiers::NONE);
    host.press(&mut form, vk::DOWN, Modifiers::NONE);
    assert_eq!(form.list.selected(), vec![1]);
    // Typing characters in the list type-ahead searches instead of editing the text box.
    host.type_text(&mut form, "f");
    assert_eq!(form.text.text(), "Ad-a");
    assert_eq!(form.list.selected(), vec![2]);
}

#[test]
fn kit_form_enter_in_the_text_box_submits_and_arrows_stay_inside_it() {
    let mut form = Form::new();
    let mut host = Host::new();
    host.layout(&mut form);
    for _ in 0..5 {
        host.press(&mut form, vk::TAB, Modifiers::NONE);
    }
    host.type_text(&mut form, "hello");
    assert!(host.press(&mut form, vk::RETURN, Modifiers::NONE));
    assert!(form.text.take_submitted());
    assert!(host.press(&mut form, vk::HOME, Modifiers::NONE));
    assert_eq!(form.text.caret(), 0);
    // Up/Down are not the text box's: they bubble (the host could use them for something else).
    assert!(!host.press(&mut form, vk::DOWN, Modifiers::NONE));
    assert_eq!(form.slider.value(), 50.0);
}

#[test]
fn kit_form_pointer_clicks_focus_the_clicked_control_and_activate_it() {
    let mut form = Form::new();
    let mut host = Host::new();
    host.layout(&mut form);
    let tree = host.tree(&mut form);
    let center = |id: WidgetId| tree.node(id).expect("node").bounds.center();
    let all = ids(&mut form);
    host.click(&mut form, center(all[1]));
    assert_eq!(host.focus.focused(), Some(all[1]));
    assert_eq!(form.check.state(), ToggleState::On);
    host.click(&mut form, center(all[2]));
    assert_eq!(host.focus.focused(), Some(all[2]));
    assert!(form.toggle.is_on());
    host.click(&mut form, center(all[0]));
    assert!(form.button.take_activated());
    assert_eq!(host.focus.focused(), Some(all[0]));
    // Clicking the text box focuses it so typing goes there.
    host.click(&mut form, center(all[4]));
    host.type_text(&mut form, "x");
    assert_eq!(form.text.text(), "x");
    // Clicking a list row selects it.
    host.click(&mut form, center(all[6]));
    assert_eq!(host.focus.focused(), Some(all[6]));
    assert!(!form.list.selected().is_empty());
}

#[test]
fn kit_form_access_tree_describes_every_control_with_role_state_and_value() {
    let mut form = Form::new();
    let mut host = Host::new();
    host.layout(&mut form);
    host.press(&mut form, vk::TAB, Modifiers::NONE);
    host.press(&mut form, vk::TAB, Modifiers::NONE);
    host.press(&mut form, vk::SPACE, Modifiers::NONE);
    let tree = host.tree(&mut form);
    let all = ids(&mut form);
    let roles: Vec<Role> = all
        .iter()
        .map(|id| tree.node(*id).expect("node").role)
        .collect();
    assert_eq!(
        roles,
        vec![
            Role::Button,
            Role::CheckBox,
            Role::ToggleButton,
            Role::Slider,
            Role::Edit,
            Role::ComboBox,
            Role::List
        ]
    );
    let names: Vec<&str> = all
        .iter()
        .map(|id| tree.node(*id).expect("node").name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "Apply", "Remember", "Autosave", "Zoom", "Name", "Theme", "Recent"
        ]
    );
    let check = tree.node(all[1]).expect("check");
    assert_eq!(check.state.toggle, Some(ToggleState::On));
    assert!(check.state.focused, "focus is on the check box");
    assert_eq!(
        tree.node(all[3])
            .expect("slider")
            .patterns
            .range_value
            .map(|r| r.value),
        Some(50.0)
    );
    assert_eq!(
        tree.node(all[5]).expect("combo").value.as_deref(),
        Some("Light")
    );
    // Controls do not overlap and are stacked in layout order.
    let tops: Vec<f32> = all
        .iter()
        .map(|id| tree.node(*id).expect("node").bounds.y)
        .collect();
    assert!(tops.windows(2).all(|pair| pair[0] < pair[1]));
    // Assistive technology can invoke what the keyboard can.
    let id = all[0];
    let mut cx = EventCx::new(
        id,
        &host.theme,
        1.0,
        &host.engine,
        &mut host.focus,
        &mut host.capture,
        &mut host.requests,
    );
    assert!(form.button.perform(&mut cx, &AccessAction::Invoke));
    assert!(form.button.take_activated());
}

#[test]
fn kit_form_paints_a_balanced_scene_with_one_focus_ring() {
    let mut form = Form::new();
    let mut host = Host::new();
    host.layout(&mut form);
    let canvas = host.paint(&mut form);
    let texts = canvas.texts();
    for expected in ["Apply", "Remember", "Autosave", "Light", "File 0"] {
        assert!(texts.contains(&expected), "{expected} missing in {texts:?}");
    }
    let rings = |canvas: &RecordingCanvas| {
        canvas
            .ops()
            .filter(|op| matches!(op, ssw_ui::canvas::DrawOp::StrokeRoundedRect { width, .. } if *width == 2.0))
            .count()
    };
    assert_eq!(rings(&canvas), 0);
    host.press(&mut form, vk::TAB, Modifiers::NONE);
    assert_eq!(rings(&host.paint(&mut form)), 1);
    // Moving focus moves the ring, never duplicates it.
    host.press(&mut form, vk::TAB, Modifiers::NONE);
    assert_eq!(rings(&host.paint(&mut form)), 1);
}
