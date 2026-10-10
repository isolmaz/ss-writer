//! The banner host: the notification stack at the top of the editor slot (spec §4).
//!
//! Banners are one per kind: read-only file (Allow editing), external-change conflict (Keep mine /
//! Load theirs / Compare), import loss report (Details expands the list of what could not be
//! represented), remote images (Don't load remote images) and the recovery offer (Restore /
//! Discard). Showing a kind again replaces its banner. The host draws the kit's
//! [`Banner`](crate::widgets::banner::Banner) for each and reports what the user did
//! ([`HostEvent`]); the app answers by dismissing the banner or acting on the document.

use ssw_core::i18n::{Arg, Localizer};

use crate::access::{AccessAction, AccessNode, AccessTree, Role};
use crate::canvas::Canvas;
use crate::geometry::{CornerRadii, Insets, Rect, Size};
use crate::input::Event;
use crate::widget::{AccessCx, EventCx, EventResult, LayoutCx, PaintCx, Widget, WidgetId};
use crate::widgets::banner::{BANNER_HEIGHT, Banner, BannerEvent, Severity};
use crate::widgets::{Control, FONT_SIZE, draw_left_centered, layout_line};

/// Gap between stacked banners.
const GAP: f32 = 4.0;
/// Height of one line of the import details.
const DETAIL_ROW: f32 = 24.0;
/// Padding above and below the details lines.
const DETAIL_PADDING: f32 = 8.0;
/// Details lines shown before "… and n more".
pub const MAX_DETAIL_ROWS: usize = 6;
/// Inset of the details text from the panel's left edge.
const DETAIL_INSET: f32 = 32.0;

/// The kinds of banner; the host holds at most one of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BannerKind {
    /// "This file is read-only".
    ReadOnly,
    /// The file changed on disk while there are unsaved edits.
    Conflict,
    /// The import could not represent everything.
    ImportLoss,
    /// Remote images are loaded for this document.
    RemoteImages,
    /// Unsaved work from a previous session was recovered.
    Recovery,
    /// A newer release of the application exists.
    Update,
}

/// A banner button the app has to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerAction {
    /// Clear the read-only attribute.
    AllowEditing,
    /// Keep the edits and overwrite the file on the next save.
    KeepMine,
    /// Discard the edits and load the file's content.
    LoadTheirs,
    /// Show the differences.
    Compare,
    /// Stop loading remote images in this document.
    DontLoadRemoteImages,
    /// Open the recovered text.
    Restore,
    /// Delete the recovered text.
    Discard,
    /// Open the page of the newer release.
    Download,
}

/// What the user did to the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostEvent {
    /// An action button of the banner `kind` was activated.
    Action {
        kind: BannerKind,
        action: BannerAction,
    },
    /// The close button removed the banner `kind`.
    Dismissed(BannerKind),
    /// The stack's height changed (a banner was closed, details were expanded or collapsed):
    /// lay the window out again.
    Resized,
}

/// Localized strings of the banners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerStrings {
    /// Accessible name of the stack.
    pub label: String,
    /// Accessible name of the close buttons.
    pub close: String,
    /// The button that expands the import details.
    pub details: String,
    /// Accessible name of the details list.
    pub details_list: String,
    pub read_only_title: String,
    pub read_only_message: String,
    pub allow_editing: String,
    pub conflict_title: String,
    pub conflict_message: String,
    pub keep_mine: String,
    pub load_theirs: String,
    pub compare: String,
    pub import_title: String,
    pub remote_images_title: String,
    pub remote_images_message: String,
    pub dont_load_remote_images: String,
    pub recovery_title: String,
    pub recovery_message: String,
    pub restore: String,
    pub discard: String,
}

impl BannerStrings {
    /// Looks the strings up in `shell.ftl`.
    #[must_use]
    pub fn from_localizer(localizer: &Localizer) -> Self {
        let text = |id: &str| localizer.text(id);
        Self {
            label: text("banner-label"),
            close: text("banner-close"),
            details: text("banner-details"),
            details_list: text("banner-details-list"),
            read_only_title: text("banner-read-only-title"),
            read_only_message: text("banner-read-only-message"),
            allow_editing: text("banner-allow-editing"),
            conflict_title: text("banner-conflict-title"),
            conflict_message: text("banner-conflict-message"),
            keep_mine: text("banner-keep-mine"),
            load_theirs: text("banner-load-theirs"),
            compare: text("banner-compare"),
            import_title: text("banner-import-title"),
            remote_images_title: text("banner-remote-images-title"),
            remote_images_message: text("banner-remote-images-message"),
            dont_load_remote_images: text("banner-dont-load-remote-images"),
            recovery_title: text("banner-recovery-title"),
            recovery_message: text("banner-recovery-message"),
            restore: text("banner-restore"),
            discard: text("banner-discard"),
        }
    }
}

/// What a banner button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Emit(BannerAction),
    ToggleDetails,
}

/// The expandable list of an import loss report.
#[derive(Debug, Clone)]
struct Details {
    id: WidgetId,
    /// What could not be represented, one line each.
    items: Vec<String>,
    item_ids: Vec<WidgetId>,
    /// "… and n more" when the list is longer than [`MAX_DETAIL_ROWS`].
    more: Option<String>,
    expanded: bool,
}

impl Details {
    fn rows(&self) -> usize {
        self.items.len().min(MAX_DETAIL_ROWS) + usize::from(self.more.is_some())
    }

    fn height(&self) -> f32 {
        self.rows() as f32 * DETAIL_ROW + 2.0 * DETAIL_PADDING
    }
}

#[derive(Debug, Clone)]
struct Entry {
    kind: BannerKind,
    banner: Banner,
    slots: Vec<Slot>,
    details: Option<Details>,
    details_rect: Rect,
}

impl Entry {
    fn height(&self) -> f32 {
        BANNER_HEIGHT
            + self
                .details
                .as_ref()
                .filter(|details| details.expanded)
                .map_or(0.0, |details| GAP + details.height())
    }
}

/// The notification stack.
#[derive(Debug, Clone)]
pub struct BannerHost {
    id: WidgetId,
    strings: BannerStrings,
    entries: Vec<Entry>,
    bounds: Rect,
    events: Vec<HostEvent>,
}

impl BannerHost {
    /// An empty stack.
    #[must_use]
    pub fn new(strings: BannerStrings) -> Self {
        Self {
            id: WidgetId::allocate(),
            strings,
            entries: Vec::new(),
            bounds: Rect::ZERO,
            events: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_id(mut self, id: WidgetId) -> Self {
        self.id = id;
        self
    }

    fn insert(&mut self, entry: Entry) {
        match self.entries.iter_mut().find(|e| e.kind == entry.kind) {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
        }
        self.events.push(HostEvent::Resized);
    }

    fn build(
        &self,
        kind: BannerKind,
        severity: Severity,
        title: &str,
        message: &str,
        actions: &[(&String, Slot)],
        closable: bool,
    ) -> Entry {
        let mut banner = Banner::new(severity, title, message);
        for (label, _) in actions {
            banner = banner.with_action(*label);
        }
        if closable {
            banner = banner.closable(&self.strings.close);
        }
        Entry {
            kind,
            banner,
            slots: actions.iter().map(|(_, slot)| *slot).collect(),
            details: None,
            details_rect: Rect::ZERO,
        }
    }

    /// Shows "This file is read-only" with Allow editing.
    pub fn show_read_only(&mut self) {
        let s = &self.strings;
        let entry = self.build(
            BannerKind::ReadOnly,
            Severity::Info,
            &s.read_only_title,
            &s.read_only_message,
            &[(&s.allow_editing, Slot::Emit(BannerAction::AllowEditing))],
            true,
        );
        self.insert(entry);
    }

    /// Shows the external-change conflict with Keep mine / Load theirs / Compare. It cannot be
    /// closed: the user has to choose.
    pub fn show_conflict(&mut self) {
        let s = &self.strings;
        let entry = self.build(
            BannerKind::Conflict,
            Severity::Warning,
            &s.conflict_title,
            &s.conflict_message,
            &[
                (&s.keep_mine, Slot::Emit(BannerAction::KeepMine)),
                (&s.load_theirs, Slot::Emit(BannerAction::LoadTheirs)),
                (&s.compare, Slot::Emit(BannerAction::Compare)),
            ],
            false,
        );
        self.insert(entry);
    }

    /// Shows the import loss report. `items` describes what could not be represented; the Details
    /// button expands them below the banner.
    pub fn show_import_loss(&mut self, items: Vec<String>, localizer: &Localizer) {
        let count = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let message = localizer.format("banner-import-message", &[("count", Arg::Int(count))]);
        let s = &self.strings;
        let mut entry = self.build(
            BannerKind::ImportLoss,
            Severity::Warning,
            &s.import_title,
            &message,
            &[(&s.details, Slot::ToggleDetails)],
            true,
        );
        let hidden = items.len().saturating_sub(MAX_DETAIL_ROWS);
        let more = (hidden > 0).then(|| {
            let hidden = i64::try_from(hidden).unwrap_or(i64::MAX);
            localizer.format("banner-details-more", &[("count", Arg::Int(hidden))])
        });
        entry.details = Some(Details {
            id: WidgetId::allocate(),
            item_ids: items.iter().map(|_| WidgetId::allocate()).collect(),
            items,
            more,
            expanded: false,
        });
        self.insert(entry);
    }

    /// Shows "Remote images are loaded" with Don't load remote images.
    pub fn show_remote_images(&mut self) {
        let s = &self.strings;
        let entry = self.build(
            BannerKind::RemoteImages,
            Severity::Info,
            &s.remote_images_title,
            &s.remote_images_message,
            &[(
                &s.dont_load_remote_images,
                Slot::Emit(BannerAction::DontLoadRemoteImages),
            )],
            true,
        );
        self.insert(entry);
    }

    /// Shows the recovery offer with Restore / Discard.
    pub fn show_recovery(&mut self) {
        let s = &self.strings;
        let entry = self.build(
            BannerKind::Recovery,
            Severity::Warning,
            &s.recovery_title,
            &s.recovery_message,
            &[
                (&s.restore, Slot::Emit(BannerAction::Restore)),
                (&s.discard, Slot::Emit(BannerAction::Discard)),
            ],
            true,
        );
        self.insert(entry);
    }

    /// Shows "a newer release exists" with a Download button. The app supplies the texts: the
    /// message names the release version, which only it knows.
    pub fn show_update(&mut self, title: &str, message: &str, download: &str) {
        let download = download.to_owned();
        let entry = self.build(
            BannerKind::Update,
            Severity::Info,
            title,
            message,
            &[(&download, Slot::Emit(BannerAction::Download))],
            true,
        );
        self.insert(entry);
    }

    /// Removes the banner of `kind`. Returns whether there was one.
    pub fn dismiss(&mut self, kind: BannerKind) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.kind != kind);
        let removed = self.entries.len() != before;
        if removed {
            self.events.push(HostEvent::Resized);
        }
        removed
    }

    /// Removes every banner (another document became active).
    pub fn clear(&mut self) {
        if !self.entries.is_empty() {
            self.entries.clear();
            self.events.push(HostEvent::Resized);
        }
    }

    #[must_use]
    pub fn contains(&self, kind: BannerKind) -> bool {
        self.entries.iter().any(|entry| entry.kind == kind)
    }

    /// The shown kinds, top to bottom.
    #[must_use]
    pub fn kinds(&self) -> Vec<BannerKind> {
        self.entries.iter().map(|entry| entry.kind).collect()
    }

    /// Whether the import details are expanded.
    #[must_use]
    pub fn details_expanded(&self) -> bool {
        self.entries
            .iter()
            .filter_map(|entry| entry.details.as_ref())
            .any(|details| details.expanded)
    }

    /// The height the stack needs for its banners and expanded details.
    #[must_use]
    pub fn content_height(&self) -> f32 {
        let gaps = self.entries.len().saturating_sub(1) as f32 * GAP;
        gaps + self.entries.iter().map(Entry::height).sum::<f32>()
    }

    #[must_use]
    pub fn bounds(&self) -> Rect {
        self.bounds
    }

    /// The rectangle of the banner of `kind` (valid after arrange).
    #[must_use]
    pub fn banner_rect(&self, kind: BannerKind) -> Option<Rect> {
        self.entries
            .iter()
            .find(|entry| entry.kind == kind)
            .map(|entry| entry.banner.bounds())
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<HostEvent> {
        std::mem::take(&mut self.events)
    }

    fn collect(&mut self) {
        let mut closed = Vec::new();
        let mut resized = false;
        for entry in &mut self.entries {
            for event in entry.banner.take_events() {
                match event {
                    BannerEvent::Action(index) => match entry.slots[index] {
                        Slot::Emit(action) => self.events.push(HostEvent::Action {
                            kind: entry.kind,
                            action,
                        }),
                        Slot::ToggleDetails => {
                            // Only the import banner has this slot, and it always has details.
                            resized |= entry
                                .details
                                .as_mut()
                                .map(|details| details.expanded = !details.expanded)
                                .is_some();
                        }
                    },
                    BannerEvent::Closed => closed.push(entry.kind),
                }
            }
        }
        for kind in closed {
            self.entries.retain(|entry| entry.kind != kind);
            self.events.push(HostEvent::Dismissed(kind));
            resized = true;
        }
        if resized {
            self.events.push(HostEvent::Resized);
        }
    }
}

impl Control for BannerHost {
    fn id(&self) -> WidgetId {
        self.id
    }
}

impl Widget for BannerHost {
    fn measure(&mut self, _cx: &mut LayoutCx, available: Size) -> Size {
        let width = if available.width.is_finite() {
            available.width
        } else {
            0.0
        };
        Size::new(width, self.content_height())
    }

    fn arrange(&mut self, cx: &mut LayoutCx, bounds: Rect) {
        self.bounds = bounds;
        let mut y = bounds.y;
        for entry in &mut self.entries {
            let rect = Rect::new(bounds.x, y, bounds.width, BANNER_HEIGHT);
            let id = entry.banner.id();
            entry.banner.arrange(&mut cx.for_widget(id), rect);
            y += BANNER_HEIGHT + GAP;
            entry.details_rect = Rect::ZERO;
            if let Some(details) = entry.details.as_ref().filter(|d| d.expanded) {
                entry.details_rect = Rect::new(bounds.x, y, bounds.width, details.height());
                y += details.height() + GAP;
            }
        }
    }

    fn paint(&self, cx: &PaintCx, canvas: &mut dyn Canvas) {
        if !cx.needs_paint(self.bounds) {
            return;
        }
        let tokens = &cx.theme.tokens;
        for entry in &self.entries {
            entry
                .banner
                .paint(&cx.for_widget(entry.banner.id()), canvas);
            let Some(details) = entry.details.as_ref().filter(|d| d.expanded) else {
                continue;
            };
            let panel = entry.details_rect;
            let radii = CornerRadii::uniform(tokens.metrics.control_radius);
            canvas.fill_rounded_rect(panel, radii, tokens.surface);
            canvas.stroke_rounded_rect(
                panel.inset(Insets::uniform(0.5)),
                radii,
                tokens.hairline,
                1.0,
            );
            let lines = details
                .items
                .iter()
                .take(MAX_DETAIL_ROWS)
                .chain(details.more.iter());
            for (row, text) in lines.enumerate() {
                let line = Rect::new(
                    panel.x,
                    panel.y + DETAIL_PADDING + row as f32 * DETAIL_ROW,
                    panel.width,
                    DETAIL_ROW,
                );
                let is_more = row >= MAX_DETAIL_ROWS.min(details.items.len());
                if !is_more {
                    canvas.fill_rounded_rect(
                        Rect::new(panel.x + 18.0, line.center().y - 2.0, 4.0, 4.0),
                        CornerRadii::uniform(2.0),
                        tokens.text_muted,
                    );
                }
                let layout = layout_line(cx.text, cx.theme, text, FONT_SIZE);
                let color = if is_more {
                    tokens.text_muted
                } else {
                    tokens.text
                };
                draw_left_centered(canvas, layout.as_ref(), panel.x + DETAIL_INSET, line, color);
            }
        }
    }

    fn event(&mut self, cx: &mut EventCx, e: &Event) -> EventResult {
        let mut result = EventResult::ignored();
        for entry in &mut self.entries {
            let id = entry.banner.id();
            result = result.or(entry.banner.event(&mut cx.for_widget(id), e));
        }
        self.collect();
        result
    }

    fn access(&self, cx: &AccessCx, out: &mut AccessTree) {
        let mut children = Vec::new();
        for entry in &self.entries {
            entry.banner.access(&cx.for_widget(entry.banner.id()), out);
            children.push(entry.banner.id());
            let Some(details) = entry.details.as_ref().filter(|d| d.expanded) else {
                continue;
            };
            let rows = details.items.iter().zip(&details.item_ids);
            let mut ids = Vec::new();
            for (row, (text, id)) in rows.enumerate() {
                let rect = Rect::new(
                    entry.details_rect.x,
                    entry.details_rect.y + DETAIL_PADDING + row as f32 * DETAIL_ROW,
                    entry.details_rect.width,
                    DETAIL_ROW,
                );
                out.insert(AccessNode::new(*id, Role::ListItem, text.clone(), rect));
                ids.push(*id);
            }
            out.insert(
                AccessNode::new(
                    details.id,
                    Role::List,
                    self.strings.details_list.clone(),
                    entry.details_rect,
                )
                .with_children(ids),
            );
            children.push(details.id);
        }
        out.insert(
            AccessNode::new(
                self.id,
                Role::Group,
                self.strings.label.clone(),
                self.bounds,
            )
            .with_children(children),
        );
    }

    /// Carries out `a` on the action or close button of a banner (the node with id `cx.id`):
    /// Invoke reports the same [`HostEvent`]s as a click, including expanding the import
    /// details and dismissing a closable banner.
    fn perform(&mut self, cx: &mut EventCx, a: &AccessAction) -> bool {
        let performed = self
            .entries
            .iter_mut()
            .any(|entry| entry.banner.perform(cx, a));
        self.collect();
        performed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::DrawOp;
    use crate::geometry::Point;
    use crate::input::{PointerButton, PointerEventKind, vk};
    use crate::widgets::testkit::Harness;

    const SHELL_FTL: &str = include_str!("../../../../assets/i18n/en/shell.ftl");

    fn localizer() -> Localizer {
        let localizer = Localizer::new("en", &[("en", SHELL_FTL)]);
        let diagnostics = localizer.diagnostics();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        localizer
    }

    fn host() -> (Harness, BannerHost) {
        let strings = BannerStrings::from_localizer(&localizer());
        (Harness::new(), BannerHost::new(strings))
    }

    fn layout(harness: &mut Harness, host: &mut BannerHost) {
        let height = host.content_height();
        harness.arrange(host, Rect::new(0.0, 80.0, 800.0, height));
    }

    fn items(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("Comment {n}")).collect()
    }

    #[test]
    fn strings_are_resolved_from_the_ftl_ids() {
        let strings = BannerStrings::from_localizer(&localizer());
        assert_eq!(strings.read_only_title, "This file is read-only");
        assert_eq!(strings.allow_editing, "Allow editing");
        assert_eq!(strings.keep_mine, "Keep mine");
        assert_eq!(strings.load_theirs, "Load theirs");
        assert_eq!(strings.compare, "Compare");
        assert_eq!(strings.dont_load_remote_images, "Don't load remote images");
        assert_eq!(strings.restore, "Restore");
        assert_eq!(strings.discard, "Discard");
        let all = [
            &strings.label,
            &strings.close,
            &strings.details,
            &strings.details_list,
            &strings.read_only_message,
            &strings.conflict_title,
            &strings.conflict_message,
            &strings.import_title,
            &strings.remote_images_title,
            &strings.remote_images_message,
            &strings.recovery_title,
            &strings.recovery_message,
        ];
        for text in all {
            assert!(!text.is_empty() && !text.starts_with("banner-"), "{text}");
        }
    }

    #[test]
    fn banners_stack_top_to_bottom_one_per_kind() {
        let (mut harness, mut host) = host();
        host.show_read_only();
        host.show_remote_images();
        assert_eq!(
            host.kinds(),
            vec![BannerKind::ReadOnly, BannerKind::RemoteImages]
        );
        assert!(host.contains(BannerKind::ReadOnly));
        assert!(!host.contains(BannerKind::Conflict));
        assert_eq!(host.content_height(), 48.0 + 4.0 + 48.0);
        layout(&mut harness, &mut host);
        assert_eq!(
            host.banner_rect(BannerKind::ReadOnly),
            Some(Rect::new(0.0, 80.0, 800.0, 48.0))
        );
        assert_eq!(
            host.banner_rect(BannerKind::RemoteImages),
            Some(Rect::new(0.0, 132.0, 800.0, 48.0))
        );
        assert_eq!(host.banner_rect(BannerKind::Recovery), None);
        // Showing a kind again replaces it in place.
        host.show_read_only();
        assert_eq!(
            host.kinds(),
            vec![BannerKind::ReadOnly, BannerKind::RemoteImages]
        );
        assert_eq!(host.bounds(), Rect::new(0.0, 80.0, 800.0, 100.0));
    }

    #[test]
    fn showing_and_dismissing_reports_resizes() {
        let (_, mut host) = host();
        host.show_conflict();
        host.show_recovery();
        assert_eq!(
            host.take_events(),
            vec![HostEvent::Resized, HostEvent::Resized]
        );
        assert!(host.dismiss(BannerKind::Conflict));
        assert!(!host.dismiss(BannerKind::Conflict));
        assert_eq!(host.take_events(), vec![HostEvent::Resized]);
        host.clear();
        assert!(host.kinds().is_empty());
        host.clear();
        assert_eq!(host.take_events(), vec![HostEvent::Resized]);
        assert_eq!(host.content_height(), 0.0);
    }

    #[test]
    fn action_buttons_report_their_action_and_kind() {
        let (mut harness, mut host) = host();
        host.show_conflict();
        host.show_read_only();
        host.take_events();
        layout(&mut harness, &mut host);
        // Banner buttons are tab stops in layout order: three conflict actions, then the read-only
        // action and its close button.
        let order = harness.focus.tab_order().to_vec();
        assert_eq!(order.len(), 5);
        let mut presses = Vec::new();
        for id in &order[..4] {
            harness.focus.focus(*id);
            presses.push(harness.key(&mut host, vk::RETURN).handled);
        }
        assert_eq!(presses, vec![true; 4]);
        assert_eq!(
            host.take_events(),
            vec![
                HostEvent::Action {
                    kind: BannerKind::Conflict,
                    action: BannerAction::KeepMine
                },
                HostEvent::Action {
                    kind: BannerKind::Conflict,
                    action: BannerAction::LoadTheirs
                },
                HostEvent::Action {
                    kind: BannerKind::Conflict,
                    action: BannerAction::Compare
                },
                HostEvent::Action {
                    kind: BannerKind::ReadOnly,
                    action: BannerAction::AllowEditing
                },
            ]
        );
    }

    #[test]
    fn the_conflict_banner_cannot_be_closed_but_the_others_can() {
        let (mut harness, mut host) = host();
        host.show_conflict();
        host.show_read_only();
        host.show_remote_images();
        host.show_recovery();
        host.take_events();
        layout(&mut harness, &mut host);
        // Conflict has 3 buttons, the other three 2 each (action + close; recovery 3).
        assert_eq!(harness.focus.tab_order().len(), 3 + 2 + 2 + 3);
        // Click the close button of the read-only banner (right edge, 8 DIPs inside).
        let rect = host.banner_rect(BannerKind::ReadOnly).expect("rect");
        let close = Point::new(rect.right() - 8.0 - 16.0, rect.center().y);
        harness.click(&mut host, close);
        assert_eq!(
            host.take_events(),
            vec![
                HostEvent::Dismissed(BannerKind::ReadOnly),
                HostEvent::Resized
            ]
        );
        assert_eq!(
            host.kinds(),
            vec![
                BannerKind::Conflict,
                BannerKind::RemoteImages,
                BannerKind::Recovery
            ]
        );
    }

    #[test]
    fn import_details_expand_below_the_banner_and_truncate_long_lists() {
        let localizer = localizer();
        let (mut harness, mut host) = host();
        host.show_import_loss(items(9), &localizer);
        host.take_events();
        assert!(!host.details_expanded());
        assert_eq!(host.content_height(), 48.0);
        layout(&mut harness, &mut host);

        // The Details button is the first tab stop.
        let details = harness.focus.tab_order()[0];
        harness.focus.focus(details);
        assert!(harness.key(&mut host, vk::SPACE).handled);
        assert_eq!(host.take_events(), vec![HostEvent::Resized]);
        assert!(host.details_expanded());
        // 6 lines + "… and 3 more", 24 DIPs each, 8 DIPs padding top and bottom, 4 DIP gap.
        assert_eq!(host.content_height(), 48.0 + 4.0 + 7.0 * 24.0 + 16.0);
        layout(&mut harness, &mut host);

        let canvas = harness.paint(&host);
        let texts = canvas.texts();
        assert!(texts.contains(&"Comment 1"));
        assert!(texts.contains(&"Comment 6"));
        assert!(!texts.contains(&"Comment 7"));
        assert!(texts.contains(&"… and 3 more"));
        assert!(texts.contains(&"9 things could not be represented"));

        // Collapse again.
        harness.focus.focus(details);
        harness.key(&mut host, vk::RETURN);
        assert_eq!(host.take_events(), vec![HostEvent::Resized]);
        assert_eq!(host.content_height(), 48.0);
    }

    #[test]
    fn short_import_lists_show_every_line_without_a_more_row() {
        let localizer = localizer();
        let (mut harness, mut host) = host();
        host.show_import_loss(items(1), &localizer);
        layout(&mut harness, &mut host);
        let details = harness.focus.tab_order()[0];
        harness.focus.focus(details);
        harness.key(&mut host, vk::RETURN);
        layout(&mut harness, &mut host);
        assert_eq!(host.content_height(), 48.0 + 4.0 + 24.0 + 16.0);
        let canvas = harness.paint(&host);
        assert!(canvas.texts().contains(&"Comment 1"));
        assert!(canvas.texts().contains(&"1 thing could not be represented"));
        assert!(!canvas.texts().iter().any(|text| text.contains("more")));
    }

    #[test]
    fn access_tree_lists_banners_and_the_details_list() {
        let localizer = localizer();
        let (mut harness, mut host) = host();
        host.show_read_only();
        host.show_import_loss(items(2), &localizer);
        layout(&mut harness, &mut host);
        let before = harness.tree(&host);
        assert_eq!(
            before.root().map(|n| n.name.as_str()),
            Some("Notifications")
        );
        assert_eq!(before.children(host.id()).count(), 2);
        assert!(before.nodes().iter().all(|n| n.role != Role::List));

        let details = harness.focus.tab_order()[2];
        harness.focus.focus(details);
        harness.key(&mut host, vk::RETURN);
        layout(&mut harness, &mut host);
        let tree = harness.tree(&host);
        let children: Vec<_> = tree.children(host.id()).collect();
        assert_eq!(children.len(), 3);
        assert_eq!(children[0].name, "This file is read-only");
        assert_eq!(children[1].name, "Imported with changes");
        assert_eq!(children[2].role, Role::List);
        assert_eq!(children[2].name, "Details");
        let rows: Vec<&str> = tree
            .children(children[2].id)
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(rows, vec!["Comment 1", "Comment 2"]);
        assert_eq!(tree.walk().len(), tree.len());
        let mut cx = harness.event_cx(host.id());
        assert!(!host.perform(&mut cx, &AccessAction::Focus));
    }

    #[test]
    fn access_invoke_on_banner_buttons_does_what_clicking_does() {
        let localizer = localizer();
        let (mut harness, mut host) = host();
        host.show_conflict();
        host.show_read_only();
        host.show_import_loss(items(2), &localizer);
        host.take_events();
        layout(&mut harness, &mut host);
        // Tab order: Keep mine, Load theirs, Compare, Allow editing, its Close, Details, Close.
        let order = harness.focus.tab_order().to_vec();
        assert_eq!(order.len(), 7);
        let tree = harness.tree(&host);
        for id in &order {
            assert!(
                tree.nodes()
                    .iter()
                    .any(|node| node.id == *id && node.patterns.invoke),
                "{id:?} is an invokable node"
            );
        }
        for id in &order[..4] {
            let mut cx = harness.event_cx(*id);
            assert!(host.perform(&mut cx, &AccessAction::Invoke));
        }
        assert_eq!(
            host.take_events(),
            vec![
                HostEvent::Action {
                    kind: BannerKind::Conflict,
                    action: BannerAction::KeepMine
                },
                HostEvent::Action {
                    kind: BannerKind::Conflict,
                    action: BannerAction::LoadTheirs
                },
                HostEvent::Action {
                    kind: BannerKind::Conflict,
                    action: BannerAction::Compare
                },
                HostEvent::Action {
                    kind: BannerKind::ReadOnly,
                    action: BannerAction::AllowEditing
                },
            ]
        );
        // Details expands the list.
        let mut cx = harness.event_cx(order[5]);
        assert!(host.perform(&mut cx, &AccessAction::Invoke));
        assert_eq!(host.take_events(), vec![HostEvent::Resized]);
        assert!(host.details_expanded());
        // Close dismisses the read-only banner.
        let mut cx = harness.event_cx(order[4]);
        assert!(host.perform(&mut cx, &AccessAction::Invoke));
        assert_eq!(
            host.take_events(),
            vec![
                HostEvent::Dismissed(BannerKind::ReadOnly),
                HostEvent::Resized
            ]
        );
        assert_eq!(
            host.kinds(),
            vec![BannerKind::Conflict, BannerKind::ImportLoss]
        );
        // The dismissed banner's buttons are gone and unsupported actions are refused.
        let mut cx = harness.event_cx(order[4]);
        assert!(!host.perform(&mut cx, &AccessAction::Invoke));
        let mut cx = harness.event_cx(order[0]);
        assert!(!host.perform(&mut cx, &AccessAction::Toggle));
        assert!(host.take_events().is_empty());
    }

    #[test]
    fn painting_draws_titles_messages_and_buttons() {
        let (mut harness, mut host) = host();
        host.show_recovery();
        layout(&mut harness, &mut host);
        let canvas = harness.paint(&host);
        let texts = canvas.texts();
        for expected in ["Recovered unsaved work", "Restore", "Discard"] {
            assert!(texts.contains(&expected), "missing {expected}: {texts:?}");
        }
        let tokens = harness.theme.tokens;
        let warning = tokens.callout.get(crate::theme::CalloutKind::Warning);
        assert!(canvas.ops().any(|op| matches!(
            op,
            DrawOp::FillRoundedRect { color, .. } if *color == warning.background
        )));
    }

    #[test]
    fn the_update_banner_offers_download_and_can_be_closed() {
        let (mut harness, mut host) = host();
        host.show_update("Update available", "Version 1.2.0 is out.", "Download");
        host.take_events();
        assert_eq!(host.kinds(), vec![BannerKind::Update]);
        layout(&mut harness, &mut host);
        let canvas = harness.paint(&host);
        let texts = canvas.texts();
        for expected in ["Update available", "Version 1.2.0 is out.", "Download"] {
            assert!(texts.contains(&expected), "missing {expected}: {texts:?}");
        }
        // The Download button and the close button.
        let order = harness.focus.tab_order();
        assert_eq!(order.len(), 2);
        harness.focus.focus(order[0]);
        assert!(harness.key(&mut host, vk::SPACE).handled);
        assert_eq!(
            host.take_events(),
            vec![HostEvent::Action {
                kind: BannerKind::Update,
                action: BannerAction::Download
            }]
        );
        let rect = host.banner_rect(BannerKind::Update).expect("rect");
        harness.click(
            &mut host,
            Point::new(rect.right() - 8.0 - 16.0, rect.center().y),
        );
        assert_eq!(
            host.take_events(),
            vec![HostEvent::Dismissed(BannerKind::Update), HostEvent::Resized]
        );
        assert!(host.kinds().is_empty());
    }

    #[test]
    fn a_custom_id_names_the_host_in_the_access_tree() {
        let id = WidgetId::allocate();
        let host = BannerHost::new(BannerStrings::from_localizer(&localizer())).with_id(id);
        assert_eq!(host.id(), id);
        assert_eq!(Harness::new().node(&host).id, id);
    }

    #[test]
    fn painting_outside_the_damaged_area_draws_nothing() {
        let (mut harness, mut host) = host();
        host.show_read_only();
        layout(&mut harness, &mut host);
        let away = Rect::new(0.0, 400.0, 800.0, 100.0);
        assert_eq!(harness.paint_damaged(&host, Some(away)).ops().count(), 0);
        let over = host.bounds();
        assert!(harness.paint_damaged(&host, Some(over)).ops().count() > 0);
    }

    #[test]
    fn measuring_reports_the_stack_height() {
        let (mut harness, mut host) = host();
        host.show_read_only();
        let mut cx = LayoutCx::new(
            host.id(),
            &harness.theme,
            1.0,
            &harness.engine,
            &mut harness.focus,
            &mut harness.requests,
        );
        assert_eq!(
            host.measure(&mut cx, Size::new(640.0, 400.0)),
            Size::new(640.0, 48.0)
        );
        assert_eq!(host.measure(&mut cx, Size::UNBOUNDED), Size::new(0.0, 48.0));
    }

    #[test]
    fn hover_over_a_banner_does_not_report_anything() {
        let (mut harness, mut host) = host();
        host.show_read_only();
        host.take_events();
        layout(&mut harness, &mut host);
        harness.pointer(
            &mut host,
            PointerEventKind::Move,
            None,
            Point::new(100.0, 100.0),
        );
        harness.pointer(
            &mut host,
            PointerEventKind::Down,
            Some(PointerButton::Right),
            Point::new(100.0, 100.0),
        );
        assert!(host.take_events().is_empty());
    }
}
