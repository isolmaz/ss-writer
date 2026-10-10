//! The command dispatcher with nothing open: the save family needs a document, so it is left to
//! the shell like any other command. Real registry over a temporary profile; the shell is a
//! recording double.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use ssw_app::clock::{AppClock, ManualClock};
use ssw_app::commands::{AppActions, CommandDispatcher, Dispatch};
use ssw_app::documents::{DocEntry, DocEnv, DocError, Documents, SharedStore};
use ssw_core::command::CommandId;
use ssw_io::fs::RealFs;
use ssw_io::recovery::RecoveryStore;
use ssw_io::save::ThreadSleeper;
use ssw_io::store::Store;

/// A shell with no window open: it declines every command and notes what reached it.
#[derive(Default)]
struct NoDocument {
    seen: Vec<CommandId>,
    errors: usize,
}

impl AppActions for NoDocument {
    fn active_document(&self) -> Option<Rc<DocEntry>> {
        None
    }

    fn choose_save_path(&mut self, _entry: &DocEntry) -> Option<PathBuf> {
        None
    }

    fn report_error(&mut self, _error: &DocError) {
        self.errors += 1;
    }

    fn run_in_editor(&mut self, command: CommandId) -> bool {
        self.seen.push(command);
        false
    }

    fn run_view(&mut self, command: CommandId) -> bool {
        self.seen.push(command);
        false
    }

    fn run_app(&mut self, command: CommandId) -> bool {
        self.seen.push(command);
        false
    }
}

fn dispatcher(tmp: &std::path::Path) -> CommandDispatcher {
    let store: SharedStore = Rc::new(RefCell::new(Store::open(&tmp.join("state.db")).unwrap()));
    let clock: Rc<dyn AppClock> = Rc::new(ManualClock::new(1));
    CommandDispatcher::new(Rc::new(Documents::new(DocEnv {
        fs: Arc::new(RealFs),
        sleeper: Arc::new(ThreadSleeper),
        clock,
        store,
        recovery: Arc::new(RecoveryStore::new(Arc::new(RealFs), tmp.join("recovery"))),
        tld: None,
    })))
}

#[test]
fn the_save_family_without_a_document_goes_to_the_shell() {
    let tmp = tempfile::tempdir().unwrap();
    let dispatcher = dispatcher(tmp.path());
    let mut shell = NoDocument::default();
    let save_family = [
        CommandId::FileSave,
        CommandId::FileSaveAs,
        CommandId::FileSaveBackToWord,
    ];
    for command in save_family {
        let outcome = dispatcher.dispatch(command, &mut shell);
        assert!(
            matches!(outcome, Dispatch::Unhandled),
            "{command}: {outcome:?}"
        );
    }
    assert_eq!(shell.seen, save_family, "the shell was asked, in order");
    assert_eq!(shell.errors, 0, "no document, so nothing failed");
}
