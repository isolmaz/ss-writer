use std::path::PathBuf;

use ssw_core::edit::{Edit, Transaction};
use ssw_core::i18n::Localizer;
use ssw_io::encoding::TextEncoding;
use ssw_io::fs::RealFs;
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
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("notes")).unwrap();
    let store: SharedStore = Rc::new(RefCell::new(
        Store::open(&tmp.path().join("state").join("state.db")).unwrap(),
    ));
    let clock: Rc<dyn AppClock> = Rc::new(ManualClock::new(1_000_000));
    let env = DocEnv {
        fs: Arc::new(RealFs),
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
    assert!(matches!(outcomes[0].action, ExternalAction::Reloaded));
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
    assert!(matches!(outcomes[0].action, ExternalAction::Conflict));
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
    assert!(matches!(
        f.docs.handle_fs_events(&event)[0].action,
        ExternalAction::Conflict
    ));
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
    assert!(matches!(outcomes[0].action, ExternalAction::Deleted));
    assert_eq!(entry.external(), ExternalState::Deleted);
    // The file comes back with other content: a clean document reloads.
    std::fs::write(&path, b"text again, longer\n").unwrap();
    let outcomes = f
        .docs
        .handle_fs_events(&[Fixture::event(&path, FsEventKind::Created)]);
    assert!(matches!(outcomes[0].action, ExternalAction::Reloaded));
    assert_eq!(entry.external(), ExternalState::InSync);

    let moved = path.with_file_name("b.md");
    std::fs::rename(&path, &moved).unwrap();
    let outcomes = f.docs.handle_fs_events(&[Fixture::event(
        &moved,
        FsEventKind::Renamed { from: path.clone() },
    )]);
    assert!(matches!(outcomes[0].action, ExternalAction::Renamed { .. }));
    assert_eq!(entry.path(), Some(canonical_path(&moved)));
    assert!(f.docs.find(&moved).is_some());
}

#[test]
fn saving_back_to_word_snapshots_first_and_round_trips_the_edit() {
    let f = fixture();
    let bytes = std::fs::read(fixture_file("docx/poi_heading123.docx")).unwrap();
    let path = f.write("report.docx", &bytes);
    let entry = f.docs.open(&path).unwrap();
    assert!(matches!(
        entry.source(),
        Source::Converted(ConvertedFormat::Docx)
    ));
    assert!(entry.can_save_back());
    type_at_end(&entry, "\nTyped by the test.\n");
    assert!(matches!(f.docs.save(&entry), Err(DocError::NeedsSaveAs)));
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
    assert!(matches!(
        f.docs.save_back(&old),
        Err(DocError::Unsupported(_))
    ));
    let target = f.tmp.path().join("notes").join("old.doc.docx");
    f.docs.save_as(&old, &target).unwrap();
    assert!(matches!(
        old.source(),
        Source::Converted(ConvertedFormat::Docx)
    ));
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
    assert!(matches!(f.docs.save(&first), Err(DocError::Untitled)));
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
    assert!(matches!(
        f.docs.save_as(&second, &target),
        Err(DocError::AlreadyOpen(_))
    ));
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
    assert!(
        matches!(error, DocError::Save(SaveError::ReadOnly(_))),
        "{error:?}"
    );
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
    assert!(matches!(error, DocError::Encode(_)), "{error:?}");
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
    let entry = f.docs.restore_recovered(restored).unwrap();
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
    let entry = f.docs.restore_recovered(restored).unwrap();
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
        DocError::Save(SaveError::SharingViolation(PathBuf::from("a"))),
        DocError::Save(SaveError::DiskFull(PathBuf::from("a"))),
        DocError::Save(SaveError::AccessDenied(PathBuf::from("a"))),
        DocError::Save(SaveError::Fs(FsError::NotFound(PathBuf::from("a")))),
    ];
    for error in errors {
        assert!(
            catalog.contains(&format!("{} =", error.message_id())),
            "{}",
            error.message_id()
        );
    }
    for kind in [
        ssw_convert::import::LossKind::MergedCells,
        ssw_convert::import::LossKind::CorruptParts,
        ssw_convert::import::LossKind::Comments,
    ] {
        assert!(catalog.contains(&format!("{} =", loss_message_id(kind))));
    }
}
