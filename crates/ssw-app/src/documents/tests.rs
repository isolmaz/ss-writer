use std::path::PathBuf;

use ssw_core::edit::{Edit, Transaction};
use ssw_core::i18n::Localizer;
use ssw_io::encoding::TextEncoding;
use ssw_io::fs::{FaultFs, FsFailure, FsOp, RealFs};
use ssw_io::save::ThreadSleeper;
use ssw_io::session::ViewState;
use ssw_io::watch::{FsEvent, FsEventKind};

use super::*;
use crate::clock::ManualClock;

struct Fixture {
    tmp: tempfile::TempDir,
    docs: Documents,
    store: SharedStore,
}

fn fixture() -> Fixture {
    fixture_on(Arc::new(RealFs))
}

/// A fixture whose documents reach the disk through `fs`.
fn fixture_on(fs: Arc<dyn FileSystem>) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("notes")).unwrap();
    let store: SharedStore = Rc::new(RefCell::new(
        Store::open(&tmp.path().join("state").join("state.db")).unwrap(),
    ));
    let clock: Rc<dyn AppClock> = Rc::new(ManualClock::new(1_000_000));
    let env = DocEnv {
        fs,
        sleeper: Arc::new(ThreadSleeper),
        clock,
        store: Rc::clone(&store),
        recovery: Arc::new(RecoveryStore::new(
            Arc::new(RealFs),
            tmp.path().join("recovery"),
        )),
        tld: None,
    };
    Fixture {
        docs: Documents::new(env),
        store,
        tmp,
    }
}

impl Fixture {
    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.tmp.path().join("notes").join(name);
        std::fs::write(&path, bytes).unwrap();
        canonical_path(&path)
    }

    fn event(path: &Path, kind: FsEventKind) -> FsEvent {
        FsEvent {
            path: path.to_path_buf(),
            kind,
        }
    }
}

fn type_at_end(entry: &DocEntry, text: &str) {
    let mut document = entry.document().borrow_mut();
    let end = document.buffer().len();
    let transaction = Transaction::single(
        Edit::insert(end, text),
        Selection::caret(end),
        Selection::caret(end + text.len()),
    );
    document.apply(transaction).unwrap();
}

/// What an external change did to a document, as a comparable name.
fn action_name(action: &ExternalAction) -> &'static str {
    match action {
        ExternalAction::Reloaded => "reloaded",
        ExternalAction::Conflict => "conflict",
        ExternalAction::Deleted => "deleted",
        ExternalAction::Renamed { .. } => "renamed",
        ExternalAction::ReloadFailed(_) => "reload failed",
    }
}

fn fixture_file(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join(relative)
}

#[test]
fn two_tabs_share_one_document_until_the_last_one_closes() {
    let f = fixture();
    let path = f.write("a.md", b"hello\n");
    let a = f.docs.open(&path).unwrap();
    let dotted = f
        .tmp
        .path()
        .join("notes")
        .join("..")
        .join("notes")
        .join("a.md");
    let b = f.docs.open(&dotted).unwrap();
    assert!(Rc::ptr_eq(&a, &b));
    assert!(Rc::ptr_eq(a.document(), b.document()));
    assert_eq!(a.users(), 2);
    type_at_end(&a, "more\n");
    assert_eq!(b.document().borrow().text(), "hello\nmore\n");
    assert!(a.is_dirty() && b.is_dirty());
    assert!(Rc::ptr_eq(&f.docs.find(&path).unwrap(), &a));

    assert!(!f.docs.release(&a));
    assert_eq!(f.docs.all().len(), 1);
    assert!(f.docs.release(&b));
    assert!(f.docs.all().is_empty());
    let history = f
        .store
        .borrow()
        .history_list(&canonical_path(&path))
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].reason, SnapshotReason::Close);
}

#[test]
fn saving_restores_bom_and_line_endings_and_own_writes_are_not_external() {
    let f = fixture();
    let path = f.write("a.md", b"\xEF\xBB\xBFone\r\ntwo\r\n");
    let entry = f.docs.open(&path).unwrap();
    assert!(entry.format().bom);
    assert_eq!(entry.document().borrow().text(), "one\ntwo\n");
    type_at_end(&entry, "three\n");
    let saved = f.docs.save(&entry).unwrap();
    assert_eq!(saved.path, canonical_path(&path));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"\xEF\xBB\xBFone\r\ntwo\r\nthree\r\n"
    );
    assert!(!entry.is_dirty());
    let events = [Fixture::event(&path, FsEventKind::Modified)];
    assert!(f.docs.handle_fs_events(&events).is_empty());
    assert!(f.docs.check_external(&entry).is_none());
}

#[test]
fn an_external_change_reloads_a_clean_document_and_keeps_the_caret() {
    let f = fixture();
    let path = f.write("a.md", b"alpha\nbeta\ngamma\n");
    let entry = f.docs.open(&path).unwrap();
    let caret = "alpha\nbeta\nga".len();
    entry
        .document()
        .borrow_mut()
        .set_selections(Selection::caret(caret), &[]);
    std::fs::write(&path, b"intro\nalpha\nbeta\ngamma\n").unwrap();
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Modified)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(action_name(&outcomes[0].action), "reloaded");
    assert_eq!(
        entry.document().borrow().text(),
        "intro\nalpha\nbeta\ngamma\n"
    );
    assert_eq!(
        entry.document().borrow().primary_selection(),
        Selection::caret(caret + "intro\n".len())
    );
    assert!(!entry.is_dirty());
    let history = f
        .store
        .borrow()
        .history_list(&canonical_path(&path))
        .unwrap();
    assert!(
        history
            .iter()
            .any(|snapshot| snapshot.reason == SnapshotReason::BeforeExternalReload)
    );
}

#[test]
fn an_external_change_with_unsaved_edits_is_a_conflict() {
    let f = fixture();
    let path = f.write("a.md", b"base\n");
    let entry = f.docs.open(&path).unwrap();
    type_at_end(&entry, "mine\n");
    std::fs::write(&path, b"theirs\n").unwrap();
    let event = [Fixture::event(&path, FsEventKind::Modified)];
    let outcomes = f.docs.handle_fs_events(&event);
    assert_eq!(action_name(&outcomes[0].action), "conflict");
    assert_eq!(entry.external(), ExternalState::Conflict);
    assert_eq!(f.docs.external_text(&entry).unwrap(), "theirs\n");

    // Keep mine: the next save overwrites their version.
    f.docs
        .resolve_conflict(&entry, Resolution::KeepMine)
        .unwrap();
    assert_eq!(entry.external(), ExternalState::InSync);
    assert!(entry.is_dirty());
    f.docs.save(&entry).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"base\nmine\n");
    assert!(f.docs.handle_fs_events(&event).is_empty());

    // Load theirs: the text is replaced, the edits stay in the history.
    type_at_end(&entry, "again\n");
    std::fs::write(&path, b"theirs two\n").unwrap();
    assert_eq!(
        action_name(&f.docs.handle_fs_events(&event)[0].action),
        "conflict"
    );
    f.docs
        .resolve_conflict(&entry, Resolution::LoadTheirs)
        .unwrap();
    assert_eq!(entry.document().borrow().text(), "theirs two\n");
    assert!(!entry.is_dirty());
    assert_eq!(entry.external(), ExternalState::InSync);
}

#[test]
fn deleted_and_renamed_files_are_followed() {
    let f = fixture();
    let path = f.write("a.md", b"text\n");
    let entry = f.docs.open(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Removed)]);
    assert_eq!(action_name(&outcomes[0].action), "deleted");
    assert_eq!(entry.external(), ExternalState::Deleted);
    // The file comes back with other content: a clean document reloads.
    std::fs::write(&path, b"text again, longer\n").unwrap();
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Created)]);
    assert_eq!(action_name(&outcomes[0].action), "reloaded");
    assert_eq!(entry.external(), ExternalState::InSync);

    let moved = path.with_file_name("b.md");
    std::fs::rename(&path, &moved).unwrap();
    let outcomes = f.docs.handle_fs_events(&[Fixture::event(
        &moved,
        FsEventKind::Renamed { from: path.clone() },
    )]);
    assert_eq!(action_name(&outcomes[0].action), "renamed");
    assert_eq!(entry.path(), Some(canonical_path(&moved)));
    assert!(f.docs.find(&moved).is_some());
}

#[test]
fn saving_back_to_word_snapshots_first_and_round_trips_the_edit() {
    let f = fixture();
    let bytes = std::fs::read(fixture_file("docx/poi_heading123.docx")).unwrap();
    let path = f.write("report.docx", &bytes);
    let entry = f.docs.open(&path).unwrap();
    assert_eq!(entry.source(), Source::Converted(ConvertedFormat::Docx));
    assert!(entry.can_save_back());
    type_at_end(&entry, "\nTyped by the test.\n");
    assert_eq!(
        f.docs.save(&entry).unwrap_err().message_id(),
        "app-error-needs-save-as"
    );
    f.docs.save_back(&entry).unwrap();
    assert!(!entry.is_dirty());
    let history = f
        .store
        .borrow()
        .history_list(&canonical_path(&path))
        .unwrap();
    assert!(
        history
            .iter()
            .any(|snapshot| snapshot.reason == SnapshotReason::BeforeSaveBack)
    );
    let reloaded = load_file(&RealFs, &path, None).unwrap();
    assert!(
        reloaded.text.contains("Typed by the test."),
        "{}",
        reloaded.text
    );
    assert!(reloaded.text.starts_with("# First paragraph"));

    let doc = f.write(
        "old.doc",
        &std::fs::read(fixture_file("doc/simple-list.doc")).unwrap(),
    );
    let old = f.docs.open(&doc).unwrap();
    // Neither the import-only .doc nor a text file nor an untitled buffer can be written back.
    assert!(!old.can_save_back());
    let plain = f.docs.open(&f.write("plain.md", b"text\n")).unwrap();
    assert!(!plain.can_save_back());
    assert!(!f.docs.new_untitled().can_save_back());
    assert_eq!(
        f.docs.save_back(&old).unwrap_err().message_id(),
        "app-error-unsupported"
    );
    let target = f.tmp.path().join("notes").join("old.doc.docx");
    f.docs.save_as(&old, &target).unwrap();
    assert_eq!(old.source(), Source::Converted(ConvertedFormat::Docx));
}

#[test]
fn an_untitled_buffer_becomes_a_file_with_save_as() {
    let f = fixture();
    let i18n = Localizer::new(
        "en",
        &[(
            "en",
            "app-untitled = Untitled\napp-untitled-numbered = Untitled { $n }\n",
        )],
    );
    let first = f.docs.new_untitled();
    let second = f.docs.new_untitled();
    assert_eq!(first.title(&i18n), "Untitled");
    assert_eq!(second.title(&i18n), "Untitled 2");
    assert!(first.is_untitled() && !first.is_dirty());
    assert_eq!(
        f.docs.save(&first).unwrap_err().message_id(),
        "app-error-untitled"
    );
    assert!(first.is_pristine_untitled());
    // A restored buffer counts as unsaved even when it is empty; a file is no scratch buffer.
    assert!(
        !f.docs
            .restore_untitled("restored", "")
            .is_pristine_untitled()
    );
    let file = f.docs.open(&f.write("file.md", b"x\n")).unwrap();
    assert!(!file.is_pristine_untitled());
    type_at_end(&first, "# Draft\n");
    assert!(!first.is_pristine_untitled(), "it has text now");
    // Typing and undoing leaves an empty buffer that is not the one nobody touched.
    let scratch = f.docs.new_untitled();
    type_at_end(&scratch, "x");
    assert!(scratch.document().borrow_mut().undo().unwrap());
    assert!(scratch.document().borrow().buffer().is_empty());
    assert!(!scratch.is_pristine_untitled());
    let id = first.untitled_id().unwrap();
    f.store.borrow().untitled_put(&id, "# Draft\n", 5).unwrap();

    let target = f.tmp.path().join("notes").join("draft.md");
    f.docs.save_as(&first, &target).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"# Draft\n");
    assert!(!first.is_untitled() && !first.is_dirty());
    assert_eq!(first.title(&i18n), "draft");
    assert_eq!(f.store.borrow().untitled_get(&id).unwrap(), None);
    assert_eq!(
        f.docs.save_as(&second, &target).unwrap_err().message_id(),
        "app-error-already-open"
    );
    // The number 1 is free again now that the first buffer has a file.
    assert_eq!(f.docs.new_untitled().title(&i18n), "Untitled");
}

#[test]
fn a_read_only_file_blocks_saving_until_editing_is_allowed() {
    let f = fixture();
    let path = f.write("locked.md", b"locked\n");
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions).unwrap();
    let entry = f.docs.open(&path).unwrap();
    assert!(entry.is_read_only());
    type_at_end(&entry, "edit\n");
    let error = f.docs.save(&entry).unwrap_err();
    assert_eq!(error.message_id(), "app-error-read-only");
    f.docs.allow_editing(&entry).unwrap();
    assert!(!entry.is_read_only());
    f.docs.save(&entry).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"locked\nedit\n");
}

#[test]
fn the_remembered_caret_is_restored_and_recents_are_recorded() {
    let f = fixture();
    let path = f.write("a.md", "héllo wörld\n".as_bytes());
    let view = ViewState {
        caret: 2,
        scroll_anchor: 0,
        scroll_delta: 0.0,
        folds: vec![],
    };
    f.store
        .borrow()
        .view_state_put(&canonical_path(&path), &view, 1)
        .unwrap();
    let entry = f.docs.open(&path).unwrap();
    // Byte 2 is inside "é": the caret moves to a character boundary.
    assert_eq!(
        entry.document().borrow().primary_selection(),
        Selection::caret(1)
    );
    assert_eq!(f.docs.view_state(&path).unwrap().caret, 2);
    let recent = f.store.borrow().recent_items(RecentKind::File).unwrap();
    assert_eq!(recent[0].path, canonical_path(&path));
}

#[test]
fn text_that_the_encoding_cannot_hold_is_not_written() {
    let f = fixture();
    let path = f.write("a.md", b"plain\n");
    let entry = f.docs.open(&path).unwrap();
    type_at_end(&entry, "日本語\n");
    entry.set_save_encoding(TextEncoding::from_label("windows-1252").unwrap(), true);
    let error = f.docs.save(&entry).unwrap_err();
    assert_eq!(error.message_id(), "app-error-encoding");
    assert_eq!(std::fs::read(&path).unwrap(), b"plain\n");
    entry.set_save_encoding(TextEncoding::Utf16Le, true);
    f.docs.save(&entry).unwrap();
    assert!(std::fs::read(&path).unwrap().starts_with(&[0xFF, 0xFE]));
    entry.set_save_line_ending(ssw_io::encoding::LineEnding::Crlf);
    assert!(entry.is_dirty());
}

#[test]
fn recovered_text_comes_back_as_unsaved_edits() {
    let f = fixture();
    let path = f.write("a.md", b"on disk\n");
    let doc = ssw_io::recovery::RecoveryDoc {
        id: "doc-1",
        kind: RecoveryKind::Dirty,
        original_path: Some(path.as_path()),
        display_name: "a.md",
        text: "recovered edits\n",
        format: TextFormat::default(),
    };
    f.docs.env().recovery.backup(&doc, 1).unwrap();
    let restored = f.docs.env().recovery.restore("doc-1").unwrap();
    let entry = f.docs.restore_recovered(restored);
    assert_eq!(entry.path(), Some(canonical_path(&path)));
    assert_eq!(entry.document().borrow().text(), "recovered edits\n");
    assert!(entry.is_dirty());
    assert_eq!(std::fs::read(&path).unwrap(), b"on disk\n");
    assert!(f.docs.env().recovery.list().unwrap().is_empty());

    let untitled = ssw_io::recovery::RecoveryDoc {
        id: "untitled-7",
        kind: RecoveryKind::Untitled,
        original_path: None,
        display_name: "Untitled",
        text: "scratch\n",
        format: TextFormat::default(),
    };
    f.docs.env().recovery.backup(&untitled, 2).unwrap();
    let restored = f.docs.env().recovery.restore("untitled-7").unwrap();
    let entry = f.docs.restore_recovered(restored);
    assert!(entry.is_untitled() && entry.is_dirty());
    assert_eq!(entry.untitled_id().as_deref(), Some("untitled-7"));
}

#[test]
fn watch_folders_list_each_folder_of_an_open_file_once() {
    let f = fixture();
    let first = f.write("a.md", b"a\n");
    let second = f.write("b.md", b"b\n");
    f.docs.open(&first).unwrap();
    f.docs.open(&second).unwrap();
    let _untitled = f.docs.new_untitled();
    assert_eq!(f.docs.watch_folders().len(), 1);
}

#[test]
fn every_error_has_a_message_id_in_the_app_catalog() {
    let catalog = include_str!("../../../../assets/i18n/en/app.ftl");
    let errors = [
        DocError::Untitled,
        DocError::NeedsSaveAs,
        DocError::Unsupported("x"),
        DocError::AlreadyOpen(PathBuf::from("a")),
        DocError::Convert("x".to_owned()),
        DocError::Fs(FsError::NotFound(PathBuf::from("a"))),
        DocError::Load(LoadError::Fs(FsError::NotFound(PathBuf::from("a")))),
        DocError::Load(LoadError::Import(
            ssw_convert::import::ImportError::Encrypted,
        )),
        DocError::Load(LoadError::Import(
            ssw_convert::import::ImportError::Corrupt("x".to_owned()),
        )),
        DocError::Load(LoadError::Bundle(
            ssw_convert::textbundle::TextBundleError::MissingText,
        )),
        DocError::Load(LoadError::NotText(PathBuf::from("a"))),
        DocError::Save(SaveError::SharingViolation(PathBuf::from("a"))),
        DocError::Save(SaveError::DiskFull(PathBuf::from("a"))),
        DocError::Save(SaveError::AccessDenied(PathBuf::from("a"))),
        DocError::Save(SaveError::Fs(FsError::NotFound(PathBuf::from("a")))),
    ];
    let missing: Vec<&str> = errors
        .iter()
        .map(DocError::message_id)
        .filter(|id| !catalog.contains(&format!("{id} =")))
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
    for kind in [
        ssw_convert::import::LossKind::MergedCells,
        ssw_convert::import::LossKind::CorruptParts,
        ssw_convert::import::LossKind::Comments,
    ] {
        assert!(catalog.contains(&format!("{} =", loss_message_id(kind))));
    }
}

/// A PNG that only has the signature and the header chunk start: enough for the exporters,
/// which read the size of an image from it.
fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes
}

fn set_readonly(path: &Path, readonly: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(readonly);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn every_loss_kind_has_its_own_catalog_message() {
    use ssw_convert::import::LossKind as K;
    let catalog = include_str!("../../../../assets/i18n/en/app.ftl");
    let kinds = [
        K::MergedCells,
        K::HeadersFooters,
        K::Columns,
        K::TextBoxes,
        K::Comments,
        K::TrackedChanges,
        K::VectorShapes,
        K::UnsupportedEquations,
        K::UnsupportedImages,
        K::Alignment,
        K::Underline,
        K::FontsAndColors,
        K::SubSuperscript,
        K::NestedTables,
        K::MultiParagraphCells,
        K::Fields,
        K::HiddenText,
        K::SymbolFontCharacters,
        K::ListNumberingStyle,
        K::SectionBreaks,
        K::CorruptParts,
    ];
    let ids: std::collections::BTreeSet<&str> = kinds.into_iter().map(loss_message_id).collect();
    assert_eq!(ids.len(), kinds.len(), "one message per kind");
    let missing: Vec<&&str> = ids
        .iter()
        .filter(|id| !catalog.contains(&format!("{id} =")))
        .collect();
    assert!(missing.is_empty(), "{missing:?}");
}

#[test]
fn save_as_converts_by_extension_and_later_saves_follow_the_new_source() {
    let f = fixture();
    let notes = f.tmp.path().join("notes");
    let picture = png(3, 2);
    std::fs::write(notes.join("pic.png"), &picture).unwrap();
    std::fs::write(notes.join("readme.txt"), "text").unwrap();
    let markdown = "# Title\n\n![pic](pic.png)\n\n![gone](missing.png)\n\n![web](https://example.com/a.png)\n\n![doc](readme.txt)\n\nInline $x^2$ here.\n";
    let open = |name: &str| f.docs.open(&f.write(name, markdown.as_bytes())).unwrap();

    // Word: the picture next to the document is embedded, the others stay text.
    let entry = open("word.md");
    f.docs.save_as(&entry, &notes.join("out.docx")).unwrap();
    assert_eq!(entry.source(), Source::Converted(ConvertedFormat::Docx));
    assert!(entry.can_save_back());
    let docx = load_file(&RealFs, &notes.join("out.docx"), None).unwrap();
    assert!(docx.text.contains("Title"), "{}", docx.text);
    assert_eq!(docx.assets.len(), 1);
    assert_eq!(docx.assets[0].bytes, picture);

    let entry = open("open.md");
    f.docs.save_as(&entry, &notes.join("out.odt")).unwrap();
    assert_eq!(entry.source(), Source::Converted(ConvertedFormat::Odt));
    let odt = load_file(&RealFs, &notes.join("out.odt"), None).unwrap();
    assert!(odt.text.contains("Title"), "{}", odt.text);

    let entry = open("rich.md");
    f.docs.save_as(&entry, &notes.join("out.rtf")).unwrap();
    assert_eq!(entry.source(), Source::Converted(ConvertedFormat::Rtf));
    let rtf = std::fs::read_to_string(notes.join("out.rtf")).unwrap();
    assert!(rtf.starts_with("{\\rtf"), "{rtf}");
    assert!(rtf.contains("pngblip"), "{rtf}");

    let error = f
        .docs
        .save_as(&open("old.md"), &notes.join("x.doc"))
        .unwrap_err();
    assert_eq!(error.message_id(), "app-error-unsupported");

    // TextPack: later plain saves rewrite the archive.
    let entry = open("pack.md");
    f.docs.save_as(&entry, &notes.join("out.textpack")).unwrap();
    assert_eq!(entry.source(), Source::TextPack);
    type_at_end(&entry, "more\n");
    f.docs.save(&entry).unwrap();
    let pack = load_file(&RealFs, &notes.join("out.textpack"), None).unwrap();
    assert_eq!(pack.text, format!("{markdown}more\n"));

    // TextBundle: later saves rewrite the text file inside the folder.
    let entry = open("bundle.md");
    let bundle = notes.join("out.textbundle");
    f.docs.save_as(&entry, &bundle).unwrap();
    assert_eq!(
        entry.source(),
        Source::TextBundle {
            text_file: "text.markdown".to_owned()
        }
    );
    assert_eq!(entry.path(), Some(canonical_path(&bundle)));
    type_at_end(&entry, "more\n");
    f.docs.save(&entry).unwrap();
    assert_eq!(
        std::fs::read_to_string(bundle.join("text.markdown")).unwrap(),
        format!("{markdown}more\n")
    );

    // Folders of bundles are not watched; the folder of a file is.
    let only = fixture();
    let lone = only.tmp.path().join("notes").join("only.textbundle");
    ssw_convert::textbundle::write_dir(&lone, "# B\n", &[]).unwrap();
    only.docs.open(&lone).unwrap();
    assert!(only.docs.watch_folders().is_empty());
}

#[test]
fn save_as_text_writes_extracted_pictures_beside_it_and_skips_unsafe_ones() {
    let f = fixture();
    let notes = f.tmp.path().join("notes");
    let bytes = std::fs::read(fixture_file("docx/mammoth_tiny-picture.docx")).unwrap();
    let entry = f.docs.open(&f.write("tiny.docx", &bytes)).unwrap();
    for (relative_path, byte) in [("../escape.png", 1), ("deep/er/x.png", 2)] {
        entry.state.borrow_mut().assets.push(Asset {
            relative_path: relative_path.to_owned(),
            bytes: vec![byte],
        });
    }
    f.docs.save_as(&entry, &notes.join("tiny.md")).unwrap();
    assert_eq!(entry.source(), Source::Text);
    assert!(notes.join("tiny_assets").join("image-1.png").is_file());
    assert_eq!(
        std::fs::read(notes.join("deep").join("er").join("x.png")).unwrap(),
        [2]
    );
    assert!(!f.tmp.path().join("escape.png").exists());

    // A TextPack keeps its pictures when it is saved under another name.
    let picture = Asset {
        relative_path: "assets/pic.png".to_owned(),
        bytes: vec![7, 7],
    };
    let packed =
        ssw_convert::textbundle::write_pack("# Pack\n", std::slice::from_ref(&picture)).unwrap();
    let pack = f.docs.open(&f.write("with.textpack", &packed)).unwrap();
    f.docs.save_as(&pack, &notes.join("copy.textpack")).unwrap();
    assert_eq!(pack.source(), Source::TextPack);
    let copy = load_file(&RealFs, &notes.join("copy.textpack"), None).unwrap();
    assert_eq!(copy.assets, [picture]);
}

#[test]
fn save_back_refuses_buffers_and_files_that_cannot_be_replaced() {
    let f = fixture();
    let error = f.docs.save_back(&f.docs.new_untitled()).unwrap_err();
    assert_eq!(error.message_id(), "app-error-untitled");
    let bytes = std::fs::read(fixture_file("docx/mammoth_tiny-picture.docx")).unwrap();
    let path = f.write("locked.docx", &bytes);
    let entry = f.docs.open(&path).unwrap();
    // The picture the importer extracted is embedded again.
    f.docs.save_back(&entry).unwrap();
    assert_eq!(load_file(&RealFs, &path, None).unwrap().assets.len(), 1);
    type_at_end(&entry, "more\n");
    set_readonly(&path, true);
    let error = f.docs.save_back(&entry).unwrap_err();
    set_readonly(&path, false);
    assert_eq!(error.message_id(), "app-error-read-only");
    assert!(entry.is_dirty());
}

#[test]
fn untitled_buffers_have_no_file_to_reload_compare_rebind_or_make_editable() {
    let f = fixture();
    let scratch = f.docs.new_untitled();
    let ids = [
        f.docs.reload(&scratch).unwrap_err().message_id(),
        f.docs.external_text(&scratch).unwrap_err().message_id(),
        f.docs
            .resolve_conflict(&scratch, Resolution::KeepMine)
            .unwrap_err()
            .message_id(),
        f.docs
            .rebind_path(&scratch, Path::new("x.md"))
            .unwrap_err()
            .message_id(),
        f.docs.allow_editing(&scratch).unwrap_err().message_id(),
    ];
    assert_eq!(ids, ["app-error-untitled"; 5]);
    assert!(f.docs.check_external(&scratch).is_none());
    assert!(
        f.docs
            .snapshot(&scratch, SnapshotReason::Periodic)
            .unwrap()
            .is_none()
    );
    let view = ViewState {
        caret: 0,
        scroll_anchor: 0,
        scroll_delta: 0.0,
        folds: vec![],
    };
    f.docs.remember_view(&scratch, &view).unwrap();
    assert!(f.docs.view_state(Path::new("never-opened.md")).is_none());
}

#[test]
fn recovered_text_of_a_vanished_file_becomes_an_untitled_buffer() {
    let f = fixture();
    let gone = f.tmp.path().join("notes").join("gone.md");
    let doc = ssw_io::recovery::RecoveryDoc {
        id: "doc-9",
        kind: RecoveryKind::Dirty,
        original_path: Some(gone.as_path()),
        display_name: "gone.md",
        text: "what was typed\n",
        format: TextFormat::default(),
    };
    f.docs.env().recovery.backup(&doc, 1).unwrap();
    let restored = f.docs.env().recovery.restore("doc-9").unwrap();
    let entry = f.docs.restore_recovered(restored);
    assert!(entry.is_untitled() && entry.is_dirty());
    assert_eq!(entry.untitled_id().as_deref(), Some("doc-9"));
    assert_eq!(entry.document().borrow().text(), "what was typed\n");
}

#[test]
fn mixed_line_endings_are_announced_once_and_debug_output_names_its_parts() {
    let f = fixture();
    let entry = f.docs.open(&f.write("mixed.md", b"a\r\nb\nc\r\n")).unwrap();
    assert!(entry.take_mixed_endings_notice());
    assert!(!entry.take_mixed_endings_notice());
    let env = format!("{:?}", f.docs.env());
    assert!(env.starts_with("DocEnv {"), "{env}");
    assert!(env.contains("tld: None"), "{env}");
    let watches = FolderWatches::new(Arc::new(crate::workers::DeferredWaker::new()));
    assert_eq!(format!("{watches:?}"), "FolderWatches { folders: [], .. }");
}

#[test]
fn a_database_that_refuses_the_recent_list_leaves_no_open_document_to_report() {
    let f = fixture();
    let path = f.write("a.md", b"one\n");
    rusqlite::Connection::open(f.tmp.path().join("state").join("state.db"))
        .unwrap()
        .execute_batch("DROP TABLE recent_items")
        .unwrap();
    let error = f.docs.open(&path).unwrap_err();
    assert_eq!(error.message_id(), "app-error-file");
    assert!(f.docs.all().is_empty());
    // The file stays known to the change tracker, but nobody shows it: its events are ignored.
    std::fs::write(&path, b"two, longer\n").unwrap();
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Modified)]);
    assert!(outcomes.is_empty());
}

#[test]
fn a_rename_the_history_cannot_follow_still_rebinds_the_document() {
    let f = fixture();
    let path = f.write("a.md", b"text\n");
    let entry = f.docs.open(&path).unwrap();
    rusqlite::Connection::open(f.tmp.path().join("state").join("state.db"))
        .unwrap()
        .execute_batch("DROP TABLE history_snapshots")
        .unwrap();
    let moved = path.with_file_name("b.md");
    std::fs::rename(&path, &moved).unwrap();
    let outcomes = f.docs.handle_fs_events(&[Fixture::event(
        &moved,
        FsEventKind::Renamed { from: path.clone() },
    )]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(action_name(&outcomes[0].action), "reload failed");
    let action = format!("{:?}", outcomes[0].action);
    assert!(
        action.contains("no such table: history_snapshots"),
        "{action}"
    );
    assert_eq!(entry.path(), Some(canonical_path(&moved)));
}

fn tiny_picture_docx() -> Vec<u8> {
    std::fs::read(fixture_file("docx/mammoth_tiny-picture.docx")).unwrap()
}

#[test]
fn a_file_that_cannot_be_read_after_it_changed_reports_a_failed_reload_and_is_retried() {
    let fault = Arc::new(FaultFs::new());
    let f = fixture_on(fault.clone());
    let path = f.write("a.md", b"base\n");
    let entry = f.docs.open(&path).unwrap();
    std::fs::write(&path, b"changed and longer\n").unwrap();
    fault.fail(FsOp::Read, FsFailure::SharingViolation, 1);
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Modified)]);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(action_name(&outcomes[0].action), "reload failed");
    let action = format!("{:?}", outcomes[0].action);
    assert!(action.contains("SharingViolation"), "{action}");
    assert_eq!(entry.document().borrow().text(), "base\n");
    // The next look at the file finds it readable.
    let outcome = f.docs.check_external(&entry).unwrap();
    assert_eq!(action_name(&outcome.action), "reloaded");
    assert_eq!(entry.document().borrow().text(), "changed and longer\n");
}

#[test]
fn a_bundle_reloads_and_follows_a_rename_without_being_tracked() {
    let f = fixture();
    let bundle = f.tmp.path().join("notes").join("b.textbundle");
    ssw_convert::textbundle::write_dir(&bundle, "# B\n", &[]).unwrap();
    let entry = f.docs.open(&bundle).unwrap();
    assert_eq!(
        entry.source(),
        Source::TextBundle {
            text_file: "text.markdown".to_owned()
        }
    );
    std::fs::write(bundle.join("text.markdown"), "# B\nedited elsewhere\n").unwrap();
    f.docs.reload(&entry).unwrap();
    assert_eq!(entry.document().borrow().text(), "# B\nedited elsewhere\n");
    assert!(!entry.is_dirty());
    // Nothing tracks the folder: an event for it changes nothing.
    let events = [Fixture::event(&bundle, FsEventKind::Modified)];
    assert!(f.docs.handle_fs_events(&events).is_empty());

    let moved = bundle.with_file_name("moved.textbundle");
    std::fs::rename(&bundle, &moved).unwrap();
    f.docs.rebind_path(&entry, &moved).unwrap();
    assert_eq!(entry.path(), Some(canonical_path(&moved)));
}

#[test]
fn a_deleted_file_cannot_be_compared_and_keep_mine_lets_the_next_save_recreate_it() {
    let f = fixture();
    let path = f.write("a.md", b"base\n");
    let entry = f.docs.open(&path).unwrap();
    type_at_end(&entry, "mine\n");
    std::fs::remove_file(&path).unwrap();
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Removed)]);
    assert_eq!(action_name(&outcomes[0].action), "deleted");
    assert_eq!(
        f.docs.external_text(&entry).unwrap_err().message_id(),
        "app-error-not-found"
    );
    f.docs
        .resolve_conflict(&entry, Resolution::KeepMine)
        .unwrap();
    assert_eq!(entry.external(), ExternalState::InSync);
    assert!(entry.is_dirty());
    f.docs.save(&entry).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"base\nmine\n");
}

#[test]
fn reopening_with_an_encoding_decodes_the_file_again() {
    let f = fixture();
    let path = f.write("latin.md", b"caf\xE9\n");
    let entry = f.docs.open(&path).unwrap();
    let windows_1251 = TextEncoding::from_label("windows-1251").unwrap();
    f.docs.reload_as(&entry, windows_1251).unwrap();
    assert_eq!(entry.document().borrow().text(), "caf\u{439}\n");
    assert_eq!(entry.format().encoding, windows_1251);
    let windows_1252 = TextEncoding::from_label("windows-1252").unwrap();
    f.docs.reload_as(&entry, windows_1252).unwrap();
    assert_eq!(entry.document().borrow().text(), "caf\u{E9}\n");
    assert_eq!(entry.format().encoding, windows_1252);
    assert!(!entry.is_dirty());
}

#[test]
fn a_file_read_elsewhere_joins_the_document_that_is_open_already() {
    let f = fixture();
    let path = f.write("a.md", b"one\n");
    let entry = f.docs.open(&path).unwrap();
    let loaded = load_file(&RealFs, &path, None).unwrap();
    let again = f.docs.open_loaded(&path, loaded).unwrap();
    assert!(Rc::ptr_eq(&entry, &again));
    assert_eq!(entry.users(), 2);
    assert_eq!(f.docs.all().len(), 1);
}

#[test]
fn a_file_that_vanished_cannot_be_made_editable_and_keeps_its_access_until_it_is_back() {
    let f = fixture();
    let path = f.write("locked.md", b"locked\n");
    set_readonly(&path, true);
    let entry = f.docs.open(&path).unwrap();
    assert!(entry.is_read_only());
    set_readonly(&path, false);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        f.docs.allow_editing(&entry).unwrap_err().message_id(),
        "app-error-file"
    );
    f.docs.refresh_access(&entry);
    assert!(entry.is_read_only(), "nothing was learned about the file");
    // The file is back and writable: the next look updates the access.
    std::fs::write(&path, b"back\n").unwrap();
    f.docs.refresh_access(&entry);
    assert!(!entry.is_read_only());
    // A buffer has no file to look at.
    let scratch = f.docs.new_untitled();
    f.docs.refresh_access(&scratch);
    assert!(!scratch.is_read_only());
}

#[test]
fn a_history_that_cannot_store_snapshots_fails_snapshots_and_save_back_replaces_nothing() {
    let f = fixture();
    let bytes = std::fs::read(fixture_file("docx/poi_heading123.docx")).unwrap();
    let path = f.write("report.docx", &bytes);
    let entry = f.docs.open(&path).unwrap();
    type_at_end(&entry, "\nTyped by the test.\n");
    rusqlite::Connection::open(f.tmp.path().join("state").join("state.db"))
        .unwrap()
        .execute_batch("DROP TABLE history_snapshots")
        .unwrap();
    let error = f
        .docs
        .snapshot(&entry, SnapshotReason::Periodic)
        .unwrap_err();
    assert_eq!(error.message_id(), "app-error-file");
    let error = f.docs.save_back(&entry).unwrap_err();
    assert_eq!(error.message_id(), "app-error-file");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(entry.is_dirty());
}

#[test]
fn pictures_outside_an_assets_folder_cannot_go_into_a_bundle() {
    let f = fixture();
    let notes = f.tmp.path().join("notes");
    let path = f.write("tiny.docx", &tiny_picture_docx());
    let entry = f.docs.open(&path).unwrap();
    for extension in ["textpack", "textbundle"] {
        let target = notes.join(format!("tiny.{extension}"));
        let error = f.docs.save_as(&entry, &target).unwrap_err();
        assert_eq!(error.message_id(), "app-error-conversion", "{extension}");
        assert_eq!(
            error.to_string(),
            "conversion failed: invalid asset path: tiny_assets/image-1.png",
            "{extension}"
        );
        assert!(!target.exists(), "{extension}");
    }
    assert_eq!(entry.source(), Source::Converted(ConvertedFormat::Docx));
    assert_eq!(entry.path(), Some(path));
}

#[test]
fn a_textpack_with_an_asset_the_format_cannot_hold_is_not_rewritten() {
    let f = fixture();
    let packed = ssw_convert::textbundle::write_pack("# P\n", &[]).unwrap();
    let path = f.write("a.textpack", &packed);
    let entry = f.docs.open(&path).unwrap();
    entry.state.borrow_mut().assets.push(Asset {
        relative_path: "pic.png".to_owned(),
        bytes: vec![1],
    });
    type_at_end(&entry, "more\n");
    let error = f.docs.save(&entry).unwrap_err();
    assert_eq!(
        error.to_string(),
        "conversion failed: invalid asset path: pic.png"
    );
    assert_eq!(std::fs::read(&path).unwrap(), packed);
    assert!(entry.is_dirty());
}

#[test]
fn a_target_that_cannot_be_replaced_fails_every_kind_of_save_as() {
    let f = fixture();
    let path = f.write("a.md", b"# A\n");
    let entry = f.docs.open(&path).unwrap();
    for name in ["locked.docx", "locked.textpack", "locked.md"] {
        let target = f.write(name, b"locked");
        set_readonly(&target, true);
        let error = f.docs.save_as(&entry, &target).unwrap_err();
        set_readonly(&target, false);
        assert_eq!(error.message_id(), "app-error-read-only", "{name}");
        assert_eq!(std::fs::read(&target).unwrap(), b"locked", "{name}");
    }
    assert_eq!(entry.path(), Some(path));
    assert_eq!(entry.source(), Source::Text);
}

#[test]
fn saving_a_bundle_or_a_pack_reports_a_file_that_cannot_be_replaced() {
    let f = fixture();
    let notes = f.tmp.path().join("notes");
    let packed = ssw_convert::textbundle::write_pack("# P\n", &[]).unwrap();
    let pack_path = f.write("p.textpack", &packed);
    let pack = f.docs.open(&pack_path).unwrap();
    type_at_end(&pack, "more\n");
    set_readonly(&pack_path, true);
    let error = f.docs.save(&pack).unwrap_err();
    set_readonly(&pack_path, false);
    assert_eq!(error.message_id(), "app-error-read-only");
    assert_eq!(std::fs::read(&pack_path).unwrap(), packed);

    let bundle = notes.join("b.textbundle");
    ssw_convert::textbundle::write_dir(&bundle, "# B\n", &[]).unwrap();
    let entry = f.docs.open(&bundle).unwrap();
    type_at_end(&entry, "more\n");
    let text_file = bundle.join("text.markdown");
    set_readonly(&text_file, true);
    let error = f.docs.save(&entry).unwrap_err();
    set_readonly(&text_file, false);
    assert_eq!(error.message_id(), "app-error-read-only");
    assert!(entry.is_dirty());
    f.docs.save(&entry).unwrap();
    assert!(!entry.is_dirty());
    assert_eq!(std::fs::read_to_string(&text_file).unwrap(), "# B\nmore\n");
}

#[test]
fn a_bundle_that_cannot_be_written_or_inspected_is_reported() {
    let fault = Arc::new(FaultFs::new());
    let f = fixture_on(fault.clone());
    let notes = f.tmp.path().join("notes");
    let entry = f.docs.open(&f.write("a.md", b"# A\n")).unwrap();
    // A file is in the way of the folder.
    let blocked = notes.join("blocked.textbundle");
    std::fs::write(&blocked, b"a file").unwrap();
    let error = f.docs.save_as(&entry, &blocked).unwrap_err();
    assert_eq!(error.message_id(), "app-error-conversion");
    assert_eq!(std::fs::read(&blocked).unwrap(), b"a file");
    // The folder is written, then its state cannot be read.
    let target = notes.join("fresh.textbundle");
    fault.fail(FsOp::Metadata, FsFailure::AccessDenied, 1);
    let error = f.docs.save_as(&entry, &target).unwrap_err();
    assert_eq!(error.message_id(), "app-error-file");
    assert!(target.join("text.markdown").is_file());
    assert_eq!(entry.source(), Source::Text);
}

#[test]
fn pictures_that_cannot_be_written_beside_the_new_file_fail_the_save_as() {
    let f = fixture();
    let notes = f.tmp.path().join("notes");
    let path = f.write("tiny.docx", &tiny_picture_docx());
    let entry = f.docs.open(&path).unwrap();
    let target = notes.join("tiny.md");
    // A file where the folder of the pictures should be.
    let folder = notes.join("tiny_assets");
    std::fs::write(&folder, b"a file").unwrap();
    let error = f.docs.save_as(&entry, &target).unwrap_err();
    assert_eq!(error.message_id(), "app-error-file");
    // A locked picture of the same name.
    std::fs::remove_file(&folder).unwrap();
    std::fs::create_dir(&folder).unwrap();
    let picture = folder.join("image-1.png");
    std::fs::write(&picture, b"mine").unwrap();
    set_readonly(&picture, true);
    let error = f.docs.save_as(&entry, &target).unwrap_err();
    set_readonly(&picture, false);
    assert_eq!(error.message_id(), "app-error-read-only");
    assert_eq!(std::fs::read(&picture).unwrap(), b"mine");
    assert_eq!(entry.path(), Some(path));
    // Once the picture can be replaced the save goes through.
    f.docs.save_as(&entry, &target).unwrap();
    assert_ne!(std::fs::read(&picture).unwrap(), b"mine");
    assert_eq!(entry.source(), Source::Text);
}

#[test]
fn save_as_in_an_encoding_that_cannot_hold_the_text_writes_nothing() {
    let f = fixture();
    let path = f.write("a.md", b"plain\n");
    let entry = f.docs.open(&path).unwrap();
    type_at_end(&entry, "\u{65E5}\u{672C}\u{8A9E}\n");
    entry.set_save_encoding(TextEncoding::from_label("windows-1252").unwrap(), false);
    let target = f.tmp.path().join("notes").join("copy.md");
    let error = f.docs.save_as(&entry, &target).unwrap_err();
    assert_eq!(error.message_id(), "app-error-encoding");
    assert!(!target.exists());
    assert_eq!(entry.path(), Some(path));
}
