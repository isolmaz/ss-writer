//! What the banners of a window do (spec §3.3, §4.1, §4.3, §4.6).
//!
//! [`Controller::sync_banners`] decides which banners the active document needs (read-only,
//! conflict, import loss, remote images) and the window the recovery offer was made in. A banner
//! the user closed stays closed for that document ([`FlowState`]); it comes back only when the
//! reason for it ended and returned. The actions of the banners are carried out here too:
//! Allow editing, Keep mine / Load theirs / Compare (the diff against the file on disk is the
//! history panel, see [`super::history`]), Don't load remote images, and the recovery offer's
//! Restore / Discard. [`Controller::check_external_changes`] is the window activation check:
//! files that another program changed meanwhile reload (or raise the conflict banner). A reload
//! that fails, found by that check or by a watcher, is reported once per document, until a check
//! of the document gets through again.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use ssw_core::document::Document;
use ssw_core::i18n::Arg;
use ssw_core::links::{LinkSource, LinkTarget, extract_links};
use ssw_io::fs::FsError;
use ssw_io::recovery::RecoveryEntry;
use ssw_ui::services::{MessageBoxButtons, MessageBoxIcon, MessageBoxOptions, MessageBoxResult};
use ssw_ui::shell::banners::{BannerAction, BannerHost, BannerKind};

use super::controller::{Controller, WindowCell};
use super::remote_images::is_remote;
use super::shell_view::ShellView;
use crate::documents::{
    DocEntry, DocError, DocId, DocMode, ExternalAction, ExternalOutcome, ExternalState, Resolution,
    loss_message_id,
};
use crate::workers::WindowId;

/// The banners that belong to the active document.
const DOCUMENT_BANNERS: [BannerKind; 4] = [
    BannerKind::ReadOnly,
    BannerKind::Conflict,
    BannerKind::ImportLoss,
    BannerKind::RemoteImages,
];

/// The documents a crashed session left behind, offered in one window.
#[derive(Debug)]
struct RecoveryOffer {
    /// The window that shows the banner.
    window: WindowId,
    /// What can be restored.
    entries: Vec<RecoveryEntry>,
}

/// What the banner flows remember between two refreshes of a window.
#[derive(Debug, Default)]
pub struct FlowState {
    /// Banners the user closed, per document.
    dismissed: RefCell<HashSet<(DocId, BannerKind)>>,
    /// Whether a document shows an image from the network, with the document version it was
    /// found for (the scan runs again only after an edit).
    remote: RefCell<HashMap<DocId, (u64, bool)>>,
    /// The recovery offer, until the user answered it.
    recovery: RefCell<Option<RecoveryOffer>>,
    /// Documents whose reload after an external change failed and was reported; the failure is
    /// not reported again until a later check of the document does not fail.
    reload_failed: RefCell<HashSet<DocId>>,
}

impl FlowState {
    /// Whether the user closed the banner `kind` of `doc` and its reason still holds.
    fn is_dismissed(&self, doc: DocId, kind: BannerKind) -> bool {
        self.dismissed.borrow().contains(&(doc, kind))
    }

    /// The user closed the banner `kind` of `doc`.
    fn dismiss(&self, doc: DocId, kind: BannerKind) {
        self.dismissed.borrow_mut().insert((doc, kind));
    }

    /// The reason for the banner `kind` of `doc` ended: if it returns, it is news again.
    fn forget(&self, doc: DocId, kind: BannerKind) {
        self.dismissed.borrow_mut().remove(&(doc, kind));
    }

    /// Drops what is remembered about documents that are not open any more.
    fn prune(&self, open: &[Rc<DocEntry>]) {
        let is_open = |doc: &DocId| open.iter().any(|entry| entry.id() == *doc);
        self.dismissed.borrow_mut().retain(|(doc, _)| is_open(doc));
        self.remote.borrow_mut().retain(|doc, _| is_open(doc));
        self.reload_failed.borrow_mut().retain(is_open);
    }

    /// Whether the text of `entry` shows an image from the network.
    fn has_remote_images(&self, entry: &DocEntry) -> bool {
        let document = entry.document().borrow();
        let version = document.version();
        if let Some(&(seen, found)) = self.remote.borrow().get(&entry.id())
            && seen == version
        {
            return found;
        }
        let found = shows_remote_image(&document);
        self.remote
            .borrow_mut()
            .insert(entry.id(), (version, found));
        found
    }
}

/// Whether the Markdown of `document` shows an image whose address is `http` or `https`.
fn shows_remote_image(document: &Document) -> bool {
    extract_links(document.ast()).iter().any(|link| {
        link.source == LinkSource::Image
            && matches!(&link.target, LinkTarget::External(url) if is_remote(url))
    })
}

impl Controller {
    // ----- which banners a window shows ----------------------------------------------------

    /// Brings the banners of the window `window` (whose content is `view`) up to date with its
    /// active document: shows what applies and was not closed, removes what no longer applies.
    /// The history panel closes when the active document is not the one it shows (another tab
    /// was activated).
    pub(super) fn sync_banners(&self, window: WindowId, view: &mut ShellView) {
        let before = view.banners_mut().kinds();
        let active = view.active_tab().map(|tab| Rc::clone(&tab.entry));
        let stale = view.history().entry().is_some_and(|shown| {
            !active
                .as_ref()
                .is_some_and(|entry| Rc::ptr_eq(shown, entry))
        });
        if stale {
            view.history_mut().close();
            view.requests_mut().invalidate_all();
        }
        self.flows.prune(&self.docs().all());
        match &active {
            Some(entry) => self.sync_document_banners(entry, view.banners_mut()),
            None => {
                for kind in DOCUMENT_BANNERS {
                    view.banners_mut().dismiss(kind);
                }
            }
        }
        self.sync_recovery_banner(window, view.banners_mut());
        if view.banners_mut().kinds() != before {
            view.requests_mut().invalidate_all();
        }
    }

    fn sync_document_banners(&self, entry: &DocEntry, banners: &mut BannerHost) {
        let doc = entry.id();
        self.sync_one(
            doc,
            BannerKind::ReadOnly,
            entry.is_read_only(),
            banners,
            BannerHost::show_read_only,
        );
        self.sync_one(
            doc,
            BannerKind::Conflict,
            entry.external() == ExternalState::Conflict,
            banners,
            BannerHost::show_conflict,
        );
        let losses = entry.losses();
        self.sync_one(
            doc,
            BannerKind::ImportLoss,
            !losses.is_empty(),
            banners,
            |banners| {
                let items = losses
                    .iter()
                    .map(|loss| {
                        let what = self.i18n().text(loss_message_id(loss.kind));
                        match &loss.detail {
                            Some(detail) => format!("{what} ({detail})"),
                            None => what,
                        }
                    })
                    .collect();
                banners.show_import_loss(items, self.i18n());
            },
        );
        self.sync_one(
            doc,
            BannerKind::RemoteImages,
            self.wants_remote_images_banner(entry),
            banners,
            BannerHost::show_remote_images,
        );
    }

    /// One banner of a document: `show` it when it is `wanted`, not shown and not closed by the
    /// user; remove it (and forget that it was closed) when it is not wanted.
    fn sync_one(
        &self,
        doc: DocId,
        kind: BannerKind,
        wanted: bool,
        banners: &mut BannerHost,
        show: impl FnOnce(&mut BannerHost),
    ) {
        if !wanted {
            banners.dismiss(kind);
            self.flows.forget(doc, kind);
        } else if !banners.contains(kind) && !self.flows.is_dismissed(doc, kind) {
            show(banners);
        }
    }

    /// Whether "Remote images are loaded" applies: the setting is on, the document did not opt
    /// out and its Markdown shows an image from the network.
    fn wants_remote_images_banner(&self, entry: &DocEntry) -> bool {
        entry.mode() == DocMode::Markdown
            && self.settings().attachments.load_remote_images
            && !self.remote_images().is_disabled(entry.id())
            && self.flows.has_remote_images(entry)
    }

    fn sync_recovery_banner(&self, window: WindowId, banners: &mut BannerHost) {
        let offered = self
            .flows
            .recovery
            .borrow()
            .as_ref()
            .is_some_and(|offer| offer.window == window);
        if !offered {
            banners.dismiss(BannerKind::Recovery);
        } else if !banners.contains(BannerKind::Recovery) {
            banners.show_recovery();
        }
    }

    // ----- what the banners do -------------------------------------------------------------

    /// Carries out the button `action` of a banner of window `cell`.
    pub(super) fn banner_action(&self, cell: &Rc<WindowCell>, action: BannerAction) {
        let entry = cell
            .view
            .borrow()
            .active_tab()
            .map(|tab| Rc::clone(&tab.entry));
        match (action, entry) {
            (BannerAction::Restore, _) => self.restore_recovered_work(cell),
            (BannerAction::Discard, _) => self.discard_recovered_work(),
            (BannerAction::AllowEditing, Some(entry)) => self.allow_editing(&entry),
            (BannerAction::KeepMine, Some(entry)) => {
                self.resolve_conflict(cell, &entry, Resolution::KeepMine);
            }
            (BannerAction::LoadTheirs, Some(entry)) => {
                self.resolve_conflict(cell, &entry, Resolution::LoadTheirs);
            }
            (BannerAction::Compare, Some(entry)) => self.compare_with_disk(cell, &entry),
            (BannerAction::DontLoadRemoteImages, Some(entry)) => {
                self.remote_images().set_disabled(entry.id(), true);
                self.remote_images_changed(cell);
            }
            // The update banner's buttons need no document.
            (BannerAction::Download, _) => self.start_update_download(),
            (BannerAction::Restart, _) => self.install_update(),
            (_, None) => {}
        }
    }

    /// The close button of the banner `kind` of window `cell`.
    pub(super) fn banner_dismissed(&self, cell: &Rc<WindowCell>, kind: BannerKind) {
        match kind {
            BannerKind::Recovery => self.confirm_discard_recovered_work(),
            // The conflict banner has no close button (the user has to choose); the update
            // banner's close button is the help module's.
            BannerKind::Conflict | BannerKind::Update => {}
            BannerKind::ReadOnly | BannerKind::ImportLoss | BannerKind::RemoteImages => {
                let doc = cell.view.borrow().active_tab().map(|tab| tab.entry.id());
                if let Some(doc) = doc {
                    self.flows.dismiss(doc, kind);
                }
            }
        }
    }

    /// "Allow editing": clears the read-only attribute. When the permissions still refuse
    /// writing the user is told so (the banner stays).
    fn allow_editing(&self, entry: &DocEntry) {
        match self.docs().allow_editing(entry) {
            Ok(()) => {}
            Err(DocError::Fs(FsError::AccessDenied(_))) => {
                self.show_error(self.i18n().text("app-error-access-denied"));
            }
            Err(error) => self.report(&error),
        }
    }

    /// Keep mine / Load theirs: settles the conflict; the comparison with the file, if open,
    /// has served its purpose.
    fn resolve_conflict(&self, cell: &Rc<WindowCell>, entry: &DocEntry, resolution: Resolution) {
        match self.docs().resolve_conflict(entry, resolution) {
            Ok(()) => {
                let comparing = cell.view.borrow().history().disk_text().is_some();
                if comparing {
                    self.close_history(cell);
                }
            }
            Err(error) => self.report(&error),
        }
    }

    /// Compare: the history panel with the file as it is on disk selected.
    fn compare_with_disk(&self, cell: &Rc<WindowCell>, entry: &DocEntry) {
        match self.docs().external_text(entry) {
            Ok(text) => self.open_history(cell.id, Some(text)),
            Err(error) => self.report(&error),
        }
    }

    // ----- files changed by other programs -------------------------------------------------

    /// The window `window` was activated: its files are compared with the disk now (the
    /// watchers may have missed a change, spec §4.3). A clean document reloads, one with
    /// unsaved edits raises the conflict banner; a file that cannot be read is reported once
    /// ([`Controller::note_reload`]), not at every activation.
    pub fn check_external_changes(&self, window: WindowId) {
        let Some(cell) = self.window(window) else {
            return;
        };
        let mut seen = HashSet::new();
        let entries: Vec<Rc<DocEntry>> = cell
            .view
            .borrow()
            .tabs()
            .iter()
            .map(|tab| Rc::clone(&tab.entry))
            .filter(|entry| seen.insert(entry.id()))
            .collect();
        let mut changed = false;
        for entry in &entries {
            let outcome = self.docs().check_external(entry);
            self.note_reload(entry.id(), outcome.as_ref());
            changed |= outcome.is_some();
        }
        if changed {
            for other in self.windows() {
                other.dirty.set(true);
            }
            self.refresh_all();
        }
    }

    /// What the latest check of the document `doc` found (`None`: nothing changed on disk). A
    /// failed reload is reported the first time only: a file that stays unreadable (locked by a
    /// sync client, a share that is offline) would otherwise bring the box back at every check,
    /// and closing the box activates the window, which checks again. Any other outcome ends the
    /// failure, so the next one is news.
    pub(super) fn note_reload(&self, doc: DocId, outcome: Option<&ExternalOutcome>) {
        match outcome.map(|outcome| &outcome.action) {
            Some(ExternalAction::ReloadFailed(error)) => {
                if self.flows.reload_failed.borrow_mut().insert(doc) {
                    self.report(error);
                }
            }
            _ => {
                self.flows.reload_failed.borrow_mut().remove(&doc);
            }
        }
    }

    // ----- the recovery offer --------------------------------------------------------------

    /// Offers the documents a crashed session left in the recovery folder in the active window
    /// (a banner with Restore / Discard). Called when the startup windows exist.
    pub(super) fn offer_recovery(&self) {
        let Some(cell) = self.active_window() else {
            return;
        };
        let entries = std::mem::take(&mut self.core().borrow_mut().recoverable);
        if entries.is_empty() {
            return;
        }
        *self.flows.recovery.borrow_mut() = Some(RecoveryOffer {
            window: cell.id,
            entries,
        });
        self.refresh(&cell);
    }

    /// Restore: every offered document opens in `cell`; a file that is open already has its
    /// tab activated instead. Documents that cannot be restored are reported and stay offered.
    fn restore_recovered_work(&self, cell: &Rc<WindowCell>) {
        let Some(offer) = self.flows.recovery.borrow_mut().take() else {
            return;
        };
        let recovery = &self.docs().env().recovery;
        let mut failed = Vec::new();
        for entry in offer.entries {
            match recovery.restore(&entry.id) {
                Ok(restored) => {
                    let document = self.docs().restore_recovered(restored);
                    self.show_recovered(cell, document);
                }
                Err(error) => {
                    self.show_error(self.i18n().format(
                        "flows-recovery-failed",
                        &[
                            ("name", Arg::Str(&entry.display_name)),
                            ("reason", Arg::Str(&error.to_string())),
                        ],
                    ));
                    failed.push(entry);
                }
            }
        }
        if !failed.is_empty() {
            *self.flows.recovery.borrow_mut() = Some(RecoveryOffer {
                window: offer.window,
                entries: failed,
            });
        }
    }

    /// Puts a restored document on screen: the tab that shows the same file (in any window) is
    /// activated, otherwise a new tab opens in `cell`.
    fn show_recovered(&self, cell: &Rc<WindowCell>, document: Rc<DocEntry>) {
        let existing = self.windows().into_iter().find_map(|window| {
            let key = window
                .view
                .borrow()
                .tabs()
                .iter()
                .find(|tab| Rc::ptr_eq(&tab.entry, &document))
                .map(|tab| tab.key);
            key.map(|key| (window, key))
        });
        match existing {
            Some((window, key)) => {
                // Restoring counted a user for this call; the tab that shows the file has one.
                self.docs().release(&document);
                self.activate_tab(&window, key);
                self.set_active_window(window.id);
            }
            None => {
                let workspace = cell
                    .view
                    .borrow()
                    .active_tab()
                    .and_then(|tab| tab.workspace.clone());
                cell.view.borrow_mut().add_tab(document, workspace);
            }
        }
    }

    /// Discard: the recovery folder is emptied. When that fails the offer stays.
    fn discard_recovered_work(&self) {
        let Some(offer) = self.flows.recovery.borrow_mut().take() else {
            return;
        };
        if let Err(error) = self.docs().env().recovery.discard_all() {
            *self.flows.recovery.borrow_mut() = Some(offer);
            self.show_error(self.i18n().format(
                "flows-recovery-discard-failed",
                &[("reason", Arg::Str(&error.to_string()))],
            ));
        }
    }

    /// The close button of the recovery banner would delete the recovered text: asks first.
    /// Without a Yes the offer stays (the banner comes back with the next refresh).
    fn confirm_discard_recovered_work(&self) {
        let answer = self.dialogs.borrow_mut().message_box(&MessageBoxOptions {
            title: self.i18n().text("flows-recovery-discard-title"),
            message: self.i18n().text("flows-recovery-discard-message"),
            buttons: MessageBoxButtons::YesNo,
            icon: Some(MessageBoxIcon::Warning),
        });
        if matches!(answer, Ok(MessageBoxResult::Yes)) {
            self.discard_recovered_work();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_from_the_network_are_found_in_every_image_syntax() {
        let cases = [
            ("![a](https://example.com/a.png)\n", true),
            ("![a](http://example.com/a.png \"title\")\n", true),
            ("![a][r]\n\n[r]: https://example.com/a.png\n", true),
            ("<img src=\"https://example.com/a.png\">\n", true),
            ("![a](images/a.png)\n", false),
            ("![a](file:///C:/pictures/a.png)\n", false),
            ("[a](https://example.com/page)\n", false),
            ("```\n![a](https://example.com/a.png)\n```\n", false),
            ("no images at all\n", false),
        ];
        for (text, expected) in cases {
            let document = Document::new(text);
            assert_eq!(shows_remote_image(&document), expected, "{text:?}");
        }
    }

    #[test]
    fn a_closed_banner_is_remembered_until_its_reason_ends() {
        let flows = FlowState::default();
        let (doc, other) = (DocId(7), DocId(8));
        assert!(!flows.is_dismissed(doc, BannerKind::ReadOnly));
        flows.dismiss(doc, BannerKind::ReadOnly);
        assert!(flows.is_dismissed(doc, BannerKind::ReadOnly));
        assert!(!flows.is_dismissed(doc, BannerKind::ImportLoss));
        assert!(!flows.is_dismissed(other, BannerKind::ReadOnly));
        flows.forget(doc, BannerKind::ReadOnly);
        assert!(!flows.is_dismissed(doc, BannerKind::ReadOnly));
    }
}
