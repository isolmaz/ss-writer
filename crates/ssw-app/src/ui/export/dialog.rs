//! The export dialog in the window (spec §9).
//!
//! [`ExportHost`] puts the `ssw-ui` [`ExportDialog`] over the whole window while it is open and
//! routes every input to it (it is modal). The dialog is created when File → Export… opens it,
//! preset from the Export settings; its Export button ends as an [`ExportRequest`] that the
//! overlays hand to the controller ([`super::super::overlays::OverlayAction::Export`]), which
//! asks for the target file and runs the export with the chosen format and options.

use std::fmt;
use std::rc::Rc;

use ssw_core::i18n::Localizer;
use ssw_core::settings::ExportSettings;
use ssw_ui::access::AccessTree;
use ssw_ui::canvas::Canvas;
use ssw_ui::geometry::Rect;
use ssw_ui::input::Event;
use ssw_ui::shell::export_dialog::{ExportDialog, ExportEvent, ExportRequest};
use ssw_ui::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use ssw_ui::widgets::anim::Motion;
use ssw_ui::widgets::{Control, SharedClipboard};

use crate::ui::overlays::OverlayEnv;

/// The export dialog of one window: layout, painting, input and accessibility while it is open.
pub struct ExportHost {
    localizer: Rc<Localizer>,
    clipboard: SharedClipboard,
    dialog: Option<ExportDialog>,
    focus_request: Option<WidgetId>,
    refocus: bool,
    request: Option<ExportRequest>,
}

impl fmt::Debug for ExportHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportHost")
            .field("open", &self.dialog.is_some())
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl ExportHost {
    /// A host with no dialog open.
    #[must_use]
    pub fn new(localizer: Rc<Localizer>, clipboard: SharedClipboard) -> Self {
        Self {
            localizer,
            clipboard,
            dialog: None,
            focus_request: None,
            refocus: false,
            request: None,
        }
    }

    /// Whether the dialog is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// The dialog, while it is open.
    #[must_use]
    pub fn dialog(&self) -> Option<&ExportDialog> {
        self.dialog.as_ref()
    }

    /// Opens a dialog preset from `defaults` (the Export settings); a dialog that is already
    /// open starts over.
    pub fn open(&mut self, defaults: &ExportSettings, motion: Motion) {
        let dialog = ExportDialog::new(
            defaults,
            Rc::clone(&self.localizer),
            Rc::clone(&self.clipboard),
            motion,
        );
        self.focus_request = Some(dialog.focus_id());
        self.dialog = Some(dialog);
        self.request = None;
    }

    /// What the user chose with the Export button since the last call.
    pub fn take_request(&mut self) -> Option<ExportRequest> {
        self.request.take()
    }

    /// The control that should get the keyboard focus after the next layout (the dialog was
    /// just opened); once.
    pub fn take_focus_request(&mut self) -> Option<WidgetId> {
        self.focus_request.take()
    }

    /// Whether the focus should go back to the editor (the dialog closed); once.
    pub fn take_refocus(&mut self) -> bool {
        std::mem::take(&mut self.refocus)
    }

    /// Places the dialog over `rect` (the whole window).
    pub fn arrange(&mut self, cx: &mut LayoutCx, rect: Rect) {
        if let Some(dialog) = self.dialog.as_mut() {
            let id = dialog.id();
            dialog.arrange(&mut cx.for_widget(id), rect);
        }
    }

    /// Paints the dialog when it is open.
    pub fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if let Some(dialog) = &self.dialog {
            dialog.paint(&cx.for_widget(dialog.id()), canvas);
        }
    }

    /// Describes the dialog when it is open; returns its node id.
    pub fn access(&self, cx: &AccessCx, out: &mut AccessTree) -> Option<WidgetId> {
        self.dialog.as_ref().map(|dialog| {
            dialog.access(&cx.for_widget(dialog.id()), out);
            dialog.id()
        })
    }

    /// Routes `event` to the open dialog. It is modal: everything but timers, animation frames
    /// and focus changes is taken, so nothing typed or clicked reaches what is behind it.
    /// When the dialog ends, the host drops it and remembers its request.
    pub fn event(&mut self, event: &Event, env: &mut OverlayEnv<'_>) -> EventResult {
        let Some(dialog) = self.dialog.as_mut() else {
            return EventResult::ignored();
        };
        let result = route(dialog, event, env);
        for ended in dialog.take_events() {
            match ended {
                ExportEvent::Export(request) => self.request = Some(request),
                ExportEvent::Cancel => {}
            }
        }
        if !dialog.is_open() {
            self.dialog = None;
            self.refocus = true;
        }
        match event {
            Event::Timer(_) | Event::AnimationFrame(_) => result,
            Event::Focus(_) => EventResult::ignored(),
            _ => EventResult::handled(),
        }
    }
}

/// Runs `dialog.event` with a fresh context. Its controls are routed through `forward`, which
/// grants their focus requests, so the dialog never asks for focus itself.
fn route(dialog: &mut ExportDialog, event: &Event, env: &mut OverlayEnv<'_>) -> EventResult {
    let mut cx = EventCx::new(
        dialog.id(),
        env.theme,
        env.scale,
        env.text,
        env.focus,
        env.capture,
        env.requests,
    );
    dialog.event(&mut cx, event)
}
