//! Integration tests: the real executable with test flags, and real folder watchers.

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::clock::{AppClock, SystemAppClock};
use ssw_app::documents::{DocEnv, Documents, ExternalAction, FolderWatches, SharedStore};
use ssw_app::workers::Waker;
use ssw_io::fs::RealFs;
use ssw_io::recovery::RecoveryStore;
use ssw_io::save::ThreadSleeper;
use ssw_io::store::Store;

#[derive(Debug)]
struct NoWaker;

impl Waker for NoWaker {
    fn wake(&self) {}
}

#[test]
fn the_executable_starts_up_and_leaves_its_data_in_the_configured_folders() {
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("local");
    let roaming = tmp.path().join("roaming");
    let status = Command::new(env!("CARGO_BIN_EXE_SSWriter"))
        .args(["--test-hidden", "--test-exit-after-start"])
        .env("LOCALAPPDATA", &local)
        .env("APPDATA", &roaming)
        .status()
        .unwrap();
    assert!(status.success(), "{status:?}");
    let app_dir = local.join("SS Writer");
    assert!(app_dir.join("state.db").is_file());
    let log = std::fs::read_to_string(app_dir.join("logs").join("ssw.log")).unwrap();
    assert!(log.contains("SS Writer starting"), "{log}");
    assert!(app_dir.join("recovery").is_dir());
    assert!(roaming.join("SS Writer").is_dir());
}

#[test]
fn a_real_watcher_reports_an_external_edit_and_the_document_reloads() {
    let tmp = tempfile::tempdir().unwrap();
    let notes = tmp.path().join("notes");
    std::fs::create_dir(&notes).unwrap();
    let path = notes.join("a.md");
    std::fs::write(&path, "one\ntwo\n").unwrap();

    let store: SharedStore = Rc::new(RefCell::new(
        Store::open(&tmp.path().join("state.db")).unwrap(),
    ));
    let clock: Rc<dyn AppClock> = Rc::new(SystemAppClock::new());
    let docs = Documents::new(DocEnv {
        fs: Arc::new(RealFs),
        sleeper: Arc::new(ThreadSleeper),
        clock,
        store,
        recovery: Arc::new(RecoveryStore::new(
            Arc::new(RealFs),
            tmp.path().join("recovery"),
        )),
        tld: None,
    });
    let entry = docs.open(&path).unwrap();
    let mut watches = FolderWatches::new(Arc::new(NoWaker), false);
    assert!(
        watches
            .sync(&docs.watch_folders(), Duration::ZERO)
            .is_empty()
    );
    assert_eq!(watches.folders().len(), 1);

    std::fs::write(&path, "one\ntwo\nthree, written by another program\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let outcomes = loop {
        let outcomes = docs.handle_fs_events(&watches.drain());
        if !outcomes.is_empty() {
            break outcomes;
        }
        assert!(Instant::now() < deadline, "no change event arrived");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(outcomes[0].action, ExternalAction::Reloaded));
    assert_eq!(
        entry.document().borrow().text(),
        "one\ntwo\nthree, written by another program\n"
    );
    assert!(!entry.is_dirty());

    // Closing the last document stops the watcher.
    assert!(docs.release(&entry));
    assert!(
        watches
            .sync(&docs.watch_folders(), Duration::ZERO)
            .is_empty()
    );
    assert!(watches.folders().is_empty());
}
