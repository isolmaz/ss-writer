//! `FolderWatches` on real folders: the back-off after a failed watch and the recursion flag.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ssw_app::documents::FolderWatches;
use ssw_app::workers::Waker;
use ssw_io::watch::{FsEvent, path_key};

#[derive(Debug)]
struct NoWaker;

impl Waker for NoWaker {
    fn wake(&self) {}
}

fn folder_watches(recursive: bool) -> FolderWatches {
    FolderWatches::new(Arc::new(NoWaker), recursive)
}

const BACK_OFF: Duration = FolderWatches::RETRY_AFTER;
const A_MOMENT: Duration = Duration::from_millis(1);

#[test]
fn a_folder_that_cannot_be_watched_is_left_alone_until_the_back_off_is_over() {
    let tmp = tempfile::tempdir().unwrap();
    let folder = tmp.path().join("later");
    let wanted = [folder.clone()];
    let mut watches = folder_watches(false);
    let start = Duration::from_secs(100);

    let failed = watches.sync(&wanted, start);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, folder);

    // The folder exists from now on: an attempt would succeed, so none is made before the time.
    std::fs::create_dir(&folder).unwrap();
    assert!(
        watches
            .sync(&wanted, start + BACK_OFF - A_MOMENT)
            .is_empty()
    );
    assert!(watches.folders().is_empty());

    assert!(watches.sync(&wanted, start + BACK_OFF).is_empty());
    assert_eq!(watches.folders(), [folder]);
}

#[test]
fn a_folder_that_keeps_failing_is_reported_once_per_back_off() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    let wanted = [missing.clone()];
    let mut watches = folder_watches(false);

    assert_eq!(watches.sync(&wanted, Duration::ZERO).len(), 1);
    assert!(watches.sync(&wanted, BACK_OFF - A_MOMENT).is_empty());
    assert_eq!(watches.sync(&wanted, BACK_OFF).len(), 1);
    assert!(watches.sync(&wanted, BACK_OFF * 2 - A_MOMENT).is_empty());
    assert_eq!(watches.sync(&wanted, BACK_OFF * 2).len(), 1);
    assert!(watches.folders().is_empty());
}

#[test]
fn a_folder_that_was_not_wanted_in_between_is_tried_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("missing");
    let wanted = [missing.clone()];
    let mut watches = folder_watches(false);

    assert_eq!(watches.sync(&wanted, Duration::ZERO).len(), 1);
    assert!(watches.sync(&[], A_MOMENT).is_empty());
    assert_eq!(watches.sync(&wanted, A_MOMENT * 2).len(), 1);
}

#[test]
fn a_watched_folder_is_not_started_again() {
    let tmp = tempfile::tempdir().unwrap();
    let wanted = [tmp.path().to_path_buf()];
    let mut watches = folder_watches(false);

    assert!(watches.sync(&wanted, Duration::ZERO).is_empty());
    assert!(watches.sync(&wanted, BACK_OFF * 3).is_empty());
    assert_eq!(watches.folders().len(), 1);
}

/// Everything `watches` reports until `done` holds for the events so far.
fn drain_until(watches: &FolderWatches, done: impl Fn(&[FsEvent]) -> bool) -> Vec<FsEvent> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while !done(&seen) {
        assert!(Instant::now() < deadline, "no event arrived: {seen:?}");
        seen.extend(watches.drain());
        std::thread::sleep(Duration::from_millis(20));
    }
    seen
}

fn mentions(events: &[FsEvent], path: &Path) -> bool {
    events.iter().any(|e| path_key(&e.path) == path_key(path))
}

#[test]
fn a_flat_set_of_watches_ignores_the_folders_below_and_a_recursive_one_sees_them() {
    let tmp = tempfile::tempdir().unwrap();
    let flat = tmp.path().join("flat");
    let deep = tmp.path().join("deep");
    for root in [&flat, &deep] {
        std::fs::create_dir_all(root.join("sub")).unwrap();
    }
    let mut flat_watches = folder_watches(false);
    let mut deep_watches = folder_watches(true);
    assert!(
        flat_watches
            .sync(std::slice::from_ref(&flat), Duration::ZERO)
            .is_empty()
    );
    assert!(
        deep_watches
            .sync(std::slice::from_ref(&deep), Duration::ZERO)
            .is_empty()
    );

    // The file below comes first: the events of one watcher arrive in the order of the changes.
    for root in [&flat, &deep] {
        std::fs::write(root.join("sub").join("inner.md"), "x").unwrap();
        std::fs::write(root.join("top.md"), "x").unwrap();
    }

    let flat_top = flat.join("top.md");
    let flat_inner = flat.join("sub").join("inner.md");
    let seen = drain_until(&flat_watches, |seen| mentions(seen, &flat_top));
    std::thread::sleep(Duration::from_millis(300));
    let seen: Vec<FsEvent> = seen.into_iter().chain(flat_watches.drain()).collect();
    assert!(!mentions(&seen, &flat_inner), "{seen:?}");

    let deep_inner = deep.join("sub").join("inner.md");
    let seen = drain_until(&deep_watches, |seen| mentions(seen, &deep_inner));
    assert!(mentions(&seen, &deep_inner), "{seen:?}");
}
